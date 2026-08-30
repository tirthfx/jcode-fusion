//! Fusion Phase 4: memory consolidation (DESIGN.md item #9, "two-phase
//! consolidation" — formalizing jcode's own documented "Ambient Garden" TODO,
//! `docs/MEMORY_ARCHITECTURE.md` §Phase 8, confirmed genuinely unimplemented
//! via real code read: only embedding backfill is wired today, fire-and-
//! forget after each ambient cycle).
//!
//! **This is Phase 1 of the two-phase design only: the leasing/claiming
//! primitive.** Real background-job semantics — a session gets claimed by
//! exactly one worker, with retry backoff on failure and reclaim on a
//! crashed/expired lease — built as pure, isolated, well-tested logic
//! first, same shape Mission Engine's own first slice took (`mission.rs`
//! shipped the write path alone before budget/verification/supervisor
//! followed in later slices).
//!
//! **Deliberately not in this slice, not silently pretended solved**:
//! - No actual memory-extraction logic. `claim_next_eligible` only decides
//!   *which* session a worker should process next — it doesn't run an LLM
//!   turn or touch `MemoryManager`/`MemoryGraph` itself. That's the natural
//!   next slice, once this primitive exists to build it on.
//! - No wiring into the ambient runner. Per the resolved scheduler question
//!   in `PROGRESS.md`, memory consolidation's periodic-loop piece should
//!   model on Ambient Mode's existing runner (a real, working periodic-loop
//!   primitive) rather than Mission Engine's supervisor — but that wiring
//!   is separate work, not attempted here.
//! - No phase-2 consolidator (the single-global-lock sub-agent that writes
//!   `MEMORY.md`/`skills/`). This module only covers phase 1's leasing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// How long a claimed lease is valid before another worker may reclaim it
/// (e.g. the worker that claimed it crashed mid-extraction). Generous on
/// purpose — a real extraction involves at least one LLM turn, which can
/// legitimately take minutes; reclaiming too eagerly would risk two workers
/// extracting the same session concurrently, which the leasing primitive
/// exists specifically to prevent.
pub const DEFAULT_LEASE_DURATION: Duration = Duration::from_secs(600);

/// Base backoff after a failed extraction attempt. Doubles per attempt
/// (capped at [`MAX_BACKOFF`]) — the same shape as any standard retry
/// backoff, not a novel scheme invented for this module.
const BASE_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(3600 * 6);
/// After this many failed attempts, a session is treated as permanently
/// ineligible rather than retried forever — a session whose transcript
/// genuinely can't be extracted (e.g. malformed/corrupted) shouldn't
/// occupy a claim slot on every single ambient cycle indefinitely.
const MAX_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseStatus {
    /// Never attempted, or a previous lease expired without completing.
    Pending,
    /// Currently claimed by a worker; see `leased_by`/`lease_expires_at`.
    Leased,
    /// Extraction completed successfully. Terminal — never reclaimed.
    Extracted,
    /// Extraction failed at least once. May still be eligible again after
    /// `next_eligible_at`, unless `attempt_count >= MAX_ATTEMPTS`.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLease {
    pub session_id: String,
    pub status: LeaseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leased_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<SystemTime>,
    #[serde(default)]
    pub attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_eligible_at: Option<SystemTime>,
}

impl SessionLease {
    fn pending(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            status: LeaseStatus::Pending,
            leased_by: None,
            lease_expires_at: None,
            attempt_count: 0,
            last_error: None,
            next_eligible_at: None,
        }
    }

    /// Whether this session can be claimed *right now*, given `now`.
    fn is_eligible(&self, now: SystemTime) -> bool {
        // A lease that's been reclaimed from a crashed worker enough times
        // (bumped in `claim_next_eligible`, since a crash never reaches
        // `mark_failed`) is just as permanently ineligible as one that
        // failed cleanly `MAX_ATTEMPTS` times -- checked once, up front,
        // for every status, rather than duplicating this gate per arm.
        if self.attempt_count >= MAX_ATTEMPTS {
            return false;
        }
        match self.status {
            LeaseStatus::Extracted => false,
            LeaseStatus::Pending => true,
            LeaseStatus::Leased => self
                .lease_expires_at
                .is_none_or(|expires| now >= expires),
            LeaseStatus::Failed => self.next_eligible_at.is_none_or(|at| now >= at),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LeaseStore {
    leases: HashMap<String, SessionLease>,
}

/// Serializes every claim/complete/fail against the on-disk lease store —
/// same reasoning and shape as `rewind_store.rs`'s own `REWIND_STORE_LOCK`
/// (Gemini review, 2026-08-30, on that module): an unguarded load-mutate-
/// save round trip would let two concurrent callers both see the same
/// session as eligible and both claim it, exactly the race this module
/// exists to prevent. One process-wide mutex, not a per-session map — lease
/// operations are not a hot path.
///
/// **In-process only, by design — verified against how this daemon actually
/// runs background work, not assumed**: every background loop in this
/// codebase (ambient mode, `server.rs`'s `tokio::spawn(ambient_handle.
/// run_loop(...))`, and every per-session `Agent`) lives inside one daemon
/// process, not separate OS processes — the same model `rewind_store.rs`'s
/// already-reviewed lock relies on. A `std::sync::Mutex` is therefore
/// sufficient for how this module is actually invoked today. **If memory
/// consolidation is ever also triggered via a separate CLI invocation
/// running alongside a live daemon** (a real, plausible future gap, raised
/// in review — not fixed here since nothing calls this module that way
/// yet), this in-process lock would not protect against that second
/// process; revisit with real cross-process file locking if/when that
/// becomes a real call path, not preemptively for a hypothetical one.
static LEASE_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_lease_store() -> std::sync::MutexGuard<'static, ()> {
    LEASE_STORE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn store_path() -> anyhow::Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?
        .join("memory_consolidation")
        .join("leases.json"))
}

fn load_store() -> anyhow::Result<LeaseStore> {
    let path = store_path()?;
    if !path.exists() {
        return Ok(LeaseStore::default());
    }
    crate::storage::read_json(&path)
}

fn save_store(store: &LeaseStore) -> anyhow::Result<()> {
    let path = store_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    crate::storage::write_json_fast(&path, store)
}

/// Claim the first eligible session from `candidates`, in order, marking it
/// [`LeaseStatus::Leased`] by `worker_id` for `lease_duration` before
/// returning. Candidates already `Extracted`, currently leased by someone
/// else (lease not yet expired), or `Failed` within their backoff window
/// are skipped, not errored on.
///
/// Returns `Ok(None)` if nothing in `candidates` is currently eligible —
/// a normal, expected outcome (e.g. an ambient cycle running between
/// extractions), not a failure.
pub fn claim_next_eligible(
    candidates: &[String],
    worker_id: &str,
    lease_duration: Duration,
) -> anyhow::Result<Option<String>> {
    let _guard = lock_lease_store();
    let mut store = load_store()?;
    let now = SystemTime::now();

    for session_id in candidates {
        let Some(existing) = store.leases.get(session_id) else {
            // Never seen before -- eligible by definition, no reclaim.
            let lease = store
                .leases
                .entry(session_id.clone())
                .or_insert_with(|| SessionLease::pending(session_id));
            claim(lease, worker_id, now, lease_duration);
            save_store(&store)?;
            return Ok(Some(session_id.clone()));
        };
        if !existing.is_eligible(now) {
            continue;
        }
        // Gemini review, 2026-08-30: a session whose worker crashed/panicked
        // mid-extraction (never reaching mark_failed) previously left
        // attempt_count untouched -- its lease would simply expire and get
        // reclaimed again, forever, never hitting MAX_ATTEMPTS. Reclaiming
        // an expired Leased lease now counts as an attempt too, so a
        // silently crash-looping session still eventually stops being
        // retried, the same as one that fails cleanly.
        let was_crashed_lease = existing.status == LeaseStatus::Leased;
        let lease = store.leases.get_mut(session_id).expect("checked above");
        if was_crashed_lease {
            lease.attempt_count = lease.attempt_count.saturating_add(1);
        }
        claim(lease, worker_id, now, lease_duration);
        save_store(&store)?;
        return Ok(Some(session_id.clone()));
    }
    Ok(None)
}

fn claim(lease: &mut SessionLease, worker_id: &str, now: SystemTime, lease_duration: Duration) {
    lease.status = LeaseStatus::Leased;
    lease.leased_by = Some(worker_id.to_string());
    lease.lease_expires_at = Some(now + lease_duration);
}

/// Mark a session's extraction as successfully completed. Terminal — a
/// session marked `Extracted` is never claimed again by
/// [`claim_next_eligible`].
pub fn mark_extracted(session_id: &str) -> anyhow::Result<()> {
    let _guard = lock_lease_store();
    let mut store = load_store()?;
    let lease = store
        .leases
        .entry(session_id.to_string())
        .or_insert_with(|| SessionLease::pending(session_id));
    lease.status = LeaseStatus::Extracted;
    lease.leased_by = None;
    lease.lease_expires_at = None;
    lease.last_error = None;
    lease.attempt_count = 0;
    lease.next_eligible_at = None;
    save_store(&store)
}

/// Mark a session's extraction as failed, applying exponential backoff
/// before it becomes eligible again (`BASE_BACKOFF * 2^(attempt_count - 1)`,
/// so the first failure waits exactly `BASE_BACKOFF`; capped at
/// `MAX_BACKOFF`). After `MAX_ATTEMPTS` failures the session
/// becomes permanently ineligible (`is_eligible` returns `false`
/// regardless of `next_eligible_at`) rather than retried forever.
pub fn mark_failed(session_id: &str, error: &str) -> anyhow::Result<()> {
    let _guard = lock_lease_store();
    let mut store = load_store()?;
    let lease = store
        .leases
        .entry(session_id.to_string())
        .or_insert_with(|| SessionLease::pending(session_id));
    lease.status = LeaseStatus::Failed;
    lease.leased_by = None;
    lease.lease_expires_at = None;
    lease.attempt_count = lease.attempt_count.saturating_add(1);
    lease.last_error = Some(error.to_string());
    // Gemini review, 2026-08-30: attempt_count is already incremented above
    // by the time this runs, so shifting by attempt_count itself doubled
    // once too many (attempt 1 -> 120s instead of the intended 60s). Shift
    // by attempt_count - 1 so the first failure gets exactly BASE_BACKOFF.
    let backoff_secs = BASE_BACKOFF
        .as_secs()
        .saturating_mul(1u64 << lease.attempt_count.saturating_sub(1).min(10))
        .min(MAX_BACKOFF.as_secs());
    lease.next_eligible_at = Some(SystemTime::now() + Duration::from_secs(backoff_secs));
    save_store(&store)
}

/// Current lease state for a session, if any exists. Read-only —
/// deliberately doesn't take the lock, since a snapshot read racing a
/// concurrent claim is fine for observability (e.g. a status/debug
/// command), unlike the mutating operations above.
pub fn lease_status(session_id: &str) -> anyhow::Result<Option<SessionLease>> {
    let store = load_store()?;
    Ok(store.leases.get(session_id).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_isolated_home<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());
        f()
    }

    #[test]
    fn claims_the_first_eligible_candidate_in_order() {
        with_isolated_home(|| {
            let candidates = vec!["a".to_string(), "b".to_string()];
            let claimed = claim_next_eligible(&candidates, "worker-1", DEFAULT_LEASE_DURATION)
                .expect("claim");
            assert_eq!(claimed, Some("a".to_string()));

            let lease = lease_status("a").expect("status").expect("exists");
            assert_eq!(lease.status, LeaseStatus::Leased);
            assert_eq!(lease.leased_by.as_deref(), Some("worker-1"));
        });
    }

    #[test]
    fn a_leased_unexpired_session_is_not_claimed_by_a_second_worker() {
        with_isolated_home(|| {
            let candidates = vec!["a".to_string()];
            claim_next_eligible(&candidates, "worker-1", DEFAULT_LEASE_DURATION).expect("claim");

            let second = claim_next_eligible(&candidates, "worker-2", DEFAULT_LEASE_DURATION)
                .expect("claim");
            assert_eq!(second, None, "worker-2 must not claim what worker-1 already holds");
        });
    }

    #[test]
    fn an_expired_lease_is_reclaimable() {
        with_isolated_home(|| {
            let candidates = vec!["a".to_string()];
            // A lease duration of zero expires immediately -- simulates a
            // crashed worker whose lease has long since lapsed, without
            // needing to actually wait real wall-clock time in a test.
            claim_next_eligible(&candidates, "worker-1", Duration::from_secs(0))
                .expect("claim")
                .expect("first claim succeeds");

            let reclaimed = claim_next_eligible(&candidates, "worker-2", DEFAULT_LEASE_DURATION)
                .expect("claim");
            assert_eq!(reclaimed, Some("a".to_string()));
            let lease = lease_status("a").expect("status").expect("exists");
            assert_eq!(lease.leased_by.as_deref(), Some("worker-2"));
        });
    }

    #[test]
    fn an_extracted_session_is_never_claimed_again() {
        with_isolated_home(|| {
            let candidates = vec!["a".to_string()];
            claim_next_eligible(&candidates, "worker-1", DEFAULT_LEASE_DURATION).expect("claim");
            mark_extracted("a").expect("mark extracted");

            let claimed = claim_next_eligible(&candidates, "worker-2", DEFAULT_LEASE_DURATION)
                .expect("claim");
            assert_eq!(claimed, None);
            let lease = lease_status("a").expect("status").expect("exists");
            assert_eq!(lease.status, LeaseStatus::Extracted);
        });
    }

    #[test]
    fn a_failed_session_is_not_immediately_reclaimable() {
        with_isolated_home(|| {
            let candidates = vec!["a".to_string()];
            claim_next_eligible(&candidates, "worker-1", DEFAULT_LEASE_DURATION).expect("claim");
            mark_failed("a", "transcript parse error").expect("mark failed");

            // BASE_BACKOFF is 60s -- immediately after a failure, a fresh
            // claim attempt must not succeed.
            let claimed = claim_next_eligible(&candidates, "worker-2", DEFAULT_LEASE_DURATION)
                .expect("claim");
            assert_eq!(claimed, None, "must respect backoff, not retry instantly");

            let lease = lease_status("a").expect("status").expect("exists");
            assert_eq!(lease.status, LeaseStatus::Failed);
            assert_eq!(lease.attempt_count, 1);
            assert_eq!(lease.last_error.as_deref(), Some("transcript parse error"));
        });
    }

    #[test]
    fn first_failure_backs_off_by_exactly_base_backoff() {
        with_isolated_home(|| {
            claim_next_eligible(&["a".to_string()], "worker-1", DEFAULT_LEASE_DURATION)
                .expect("claim");
            mark_failed("a", "transient").expect("mark failed");

            let lease = lease_status("a").expect("status").expect("exists");
            assert_eq!(lease.attempt_count, 1);
            // Off-by-one regression check (Gemini review, 2026-08-30):
            // the first failure must back off by exactly BASE_BACKOFF
            // (60s), not BASE_BACKOFF * 2. Just before the window: still
            // ineligible. At/after it: eligible again.
            let just_before = lease.next_eligible_at.expect("has backoff") - Duration::from_secs(1);
            assert!(!lease.is_eligible(just_before));
            assert!(lease.is_eligible(lease.next_eligible_at.expect("has backoff")));
        });
    }

    #[test]
    fn a_crashed_worker_that_never_calls_mark_failed_still_eventually_stops_being_retried() {
        with_isolated_home(|| {
            // The very first claim of a never-before-seen session is a
            // fresh claim, not a reclaim -- it correctly doesn't count as
            // an "attempt" (nothing has failed yet). Each claim *after*
            // that finds an already-Leased-but-expired lease -- a crashed
            // worker's reclaim -- which does count. So reaching
            // MAX_ATTEMPTS via pure crash-looping (mark_failed never
            // called) takes one initial claim plus MAX_ATTEMPTS reclaims.
            for _ in 0..=MAX_ATTEMPTS {
                claim_next_eligible(&["a".to_string()], "worker-1", Duration::from_secs(0))
                    .expect("claim")
                    .expect("still eligible on this attempt");
            }
            let lease = lease_status("a").expect("status").expect("exists");
            assert_eq!(
                lease.attempt_count, MAX_ATTEMPTS,
                "reclaiming an expired (never-failed) lease must still count as an attempt"
            );
            let claimed = claim_next_eligible(&["a".to_string()], "worker-2", Duration::from_secs(0))
                .expect("claim");
            assert_eq!(
                claimed, None,
                "a crash-looping session must stop being retried once MAX_ATTEMPTS is hit, \
                 even though mark_failed was never called"
            );
        });
    }

    #[test]
    fn a_session_past_max_attempts_becomes_permanently_ineligible() {
        with_isolated_home(|| {
            for _ in 0..MAX_ATTEMPTS {
                claim_next_eligible(&["a".to_string()], "worker-1", Duration::from_secs(0))
                    .expect("claim");
                mark_failed("a", "still broken").expect("mark failed");
            }
            let lease = lease_status("a").expect("status").expect("exists");
            assert_eq!(lease.attempt_count, MAX_ATTEMPTS);
            assert!(
                !lease.is_eligible(SystemTime::now() + MAX_BACKOFF * 2),
                "must stay ineligible even long after the last backoff window, once max attempts is hit"
            );
        });
    }

    #[test]
    fn skips_ineligible_candidates_to_claim_a_later_eligible_one() {
        with_isolated_home(|| {
            claim_next_eligible(&["a".to_string()], "worker-1", DEFAULT_LEASE_DURATION)
                .expect("claim");
            mark_extracted("a").expect("mark extracted");

            let candidates = vec!["a".to_string(), "b".to_string()];
            let claimed = claim_next_eligible(&candidates, "worker-2", DEFAULT_LEASE_DURATION)
                .expect("claim");
            assert_eq!(claimed, Some("b".to_string()), "must skip 'a' and claim 'b'");
        });
    }

    #[test]
    fn lease_status_returns_none_for_a_session_never_seen() {
        with_isolated_home(|| {
            assert!(lease_status("never-claimed").expect("status").is_none());
        });
    }
}
