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
//! **Second slice, same session (2026-08-30): real extraction, not a new
//! LLM pipeline built from scratch.** `extract_claimed_session` wires the
//! leasing primitive above to jcode's own already-existing, already-shipped
//! `MemoryManager::extract_from_transcript` (Haiku-sidecar-based) — a real
//! function that, before this slice, had **zero callers anywhere in the
//! codebase** (confirmed via grep before writing anything: the exact same
//! "orphaned but shaped right" pattern Mission Engine's own first slice
//! found in `mission.rs`). The only prior caller of the underlying
//! extraction *mechanism* at all was `Agent::extract_session_memories`
//! (`agent/turn_execution.rs`) — but only from one narrow path, an
//! interactive CLI session's own normal exit, never retroactively for a
//! TUI/headless/swarm session that closed, crashed, or was simply never
//! revisited. That gap (`docs/MEMORY_ARCHITECTURE.md`'s own "Retroactive
//! session extraction (crashed/missed sessions)" checklist item) is exactly
//! what this slice closes: the transcript-building logic that lone call
//! site used was factored out to `crate::memory::transcript_from_messages`
//! (jcode-base) so both a live `Agent` and a session freshly loaded from
//! disk with no live agent at all (this module's own case) build the
//! identical transcript shape from one place, not two copies drifting apart.
//!
//! **Still deliberately not in this slice**:
//! - No wiring into the ambient runner. Per the resolved scheduler question
//!   in `PROGRESS.md`, memory consolidation's periodic-loop piece should
//!   model on Ambient Mode's existing runner (a real, working periodic-loop
//!   primitive) rather than Mission Engine's supervisor — but that wiring
//!   is separate work, not attempted here. `extract_claimed_session` is
//!   callable and tested standing alone; nothing calls it automatically yet.
//! - No phase-2 consolidator (the single-global-lock sub-agent that writes
//!   `MEMORY.md`/`skills/`).

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

/// Release a lease this worker currently holds back to [`LeaseStatus::Pending`],
/// touching neither `attempt_count` nor `last_error` — for an outcome that's
/// neither success nor failure (e.g. "too few messages, might still be
/// growing" or "no LLM backend available right now"), just "try again later,
/// no penalty."
///
/// **Real bug fix, caught by a full-repo review**: two call sites used to
/// handle exactly this "try again later, no fault" outcome by simply
/// *leaving* the lease in whatever `Leased` state `claim_next_eligible` had
/// already put it in, relying on it to expire naturally. But once that lease
/// expires, it is indistinguishable from a worker that crashed mid-extraction
/// — `claim_next_eligible`'s own crash-loop protection (`was_crashed_lease`)
/// can't tell the two apart, so it would eventually bump `attempt_count` on
/// every one of these deliberate, no-fault retries too, permanently excluding
/// a session (via `MAX_ATTEMPTS`) that never actually failed once. Resetting
/// to `Pending` immediately — rather than leaving `Leased` to expire on its
/// own — means the lease was never left dangling, so the crash-only branch is
/// never mistakenly entered for this case.
pub fn release_claim(session_id: &str) -> anyhow::Result<()> {
    let _guard = lock_lease_store();
    let mut store = load_store()?;
    let lease = store
        .leases
        .entry(session_id.to_string())
        .or_insert_with(|| SessionLease::pending(session_id));
    lease.status = LeaseStatus::Pending;
    lease.leased_by = None;
    lease.lease_expires_at = None;
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

/// Environment variable gating the ambient-runner wiring below, same
/// opt-in-by-default-off convention every other Fusion feature already
/// uses (`JCODE_FUSION_SWARM_WORKTREES`, `JCODE_FUSION_SANDBOX`) — this one
/// makes real sidecar LLM calls and writes to the user's real memory store
/// automatically in the background, not something to turn on silently for
/// every existing ambient user.
const AMBIENT_WIRING_ENV_VAR: &str = "JCODE_FUSION_MEMORY_CONSOLIDATION";

/// How many session ids `candidate_session_ids` considers per call.
/// Deliberately small: this runs once per ambient cycle (typically minutes
/// apart), and only one session actually gets claimed+extracted per cycle
/// (see `run_one_ambient_extraction`) — the "ambient garden" is meant to
/// tend gradually, not burst through a whole session store at once. A
/// larger batch mostly just gives `claim_next_eligible` more already-
/// `Extracted` sessions to skip past for free.
const CANDIDATE_BATCH_SIZE: usize = 25;

pub fn is_ambient_wiring_enabled() -> bool {
    std::env::var(AMBIENT_WIRING_ENV_VAR)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Cheap, deliberately non-exhaustive listing of session ids that might be
/// worth trying this cycle — **not** "every session on disk." A long-lived
/// install's `sessions/` directory can hold 100k+ entries (see
/// `jcode-base`'s `session/maintenance.rs`, which explicitly profiles an
/// *unconditional* full walk with a per-entry `stat` as the single largest
/// CPU cost at TUI startup). This function is deliberately far cheaper than
/// that: `std::fs::read_dir`'s iterator is lazy, so `.take(limit)` stops
/// reading further entries once satisfied rather than materializing the
/// whole directory, and nothing here calls `entry.metadata()` (no stat
/// syscall per entry at all — just the filename jcode already handed back
/// as part of the directory read itself).
/// How far into the directory listing a call may skip before taking its
/// batch (see the random-skip explanation on [`candidate_session_ids`]
/// itself). Bounded, not proportional to the real directory size -- this
/// module has no cheap way to learn that size without the same full-scan
/// cost it's trying to avoid.
const CANDIDATE_SKIP_WINDOW: usize = 500;

pub fn candidate_session_ids(limit: usize) -> anyhow::Result<Vec<String>> {
    let ids = candidate_session_ids_with_skip(limit, random_skip_offset())?;
    if !ids.is_empty() {
        return Ok(ids);
    }
    // Caught while fixing the stagnation bug below, not shipped blind: a
    // *fixed* random skip of up to CANDIDATE_SKIP_WINDOW overshoots the end
    // of the listing far more often than not for any install with fewer
    // sessions than that window -- almost certainly the common case (a
    // fresh install, a light user) -- which would otherwise turn "fixed on
    // the first 25 forever" into "usually finds nothing at all," a
    // different but equally real regression. An empty skipped read falls
    // back to an unskipped one in the same call, so a small directory still
    // gets real candidates every cycle; a large one only takes this path
    // when the random skip happens to land past the end, not as its normal
    // behavior.
    candidate_session_ids_with_skip(limit, 0)
}

/// A small, dependency-free pseudo-random skip amount, same convention
/// `swarm_worktree.rs::generate_worker_label` already uses for "cheap,
/// good-enough randomness without adding a `rand` dependency for one call
/// site" -- current-time nanoseconds, reduced into range.
fn random_skip_offset() -> usize {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    (nanos % CANDIDATE_SKIP_WINDOW as u128) as usize
}

/// Cheap, deliberately non-exhaustive listing of session ids that might be
/// worth trying this cycle — **not** "every session on disk." A long-lived
/// install's `sessions/` directory can hold 100k+ entries (see
/// `jcode-base`'s `session/maintenance.rs`, which explicitly profiles an
/// *unconditional* full walk with a per-entry `stat` as the single largest
/// CPU cost at TUI startup). This function is deliberately far cheaper than
/// that: `std::fs::read_dir`'s iterator is lazy, so `.skip(n).take(limit)`
/// only walks as many directory entries as needed rather than materializing
/// the whole directory, and nothing here calls `entry.metadata()` (no stat
/// syscall per entry at all — just the filename jcode already handed back
/// as part of the directory read itself).
///
/// **Real bug caught and fixed by an agy (Gemini 3.1 Pro) review, not
/// shipped as originally written**: a plain `.take(limit)` with no skip
/// would return the *same* leading entries on every single call, since
/// `read_dir`'s enumeration order is stable across calls on an unchanged
/// directory. Once those first `limit` sessions were all `Extracted`, every
/// future ambient cycle would find nothing eligible in that fixed prefix
/// and silently stop making progress forever — the other 99,975+ sessions
/// in a large install would simply never get a turn. Fixed with a bounded
/// random skip before the batch: each call starts from a different (if not
/// perfectly uniform) point in the listing, so a session store larger than
/// the skip window still gets churned through over many cycles instead of
/// permanently stalling on one fixed prefix. **Honest limit, not hidden**:
/// this doesn't guarantee full, uniform coverage of an install with far
/// more sessions than `CANDIDATE_SKIP_WINDOW`, only that it can't get
/// permanently stuck the way the original version did — genuinely uniform
/// coverage would need either counting the directory first (the same
/// full-scan cost this function exists to avoid) or a persisted rotating
/// cursor, neither built here.
pub fn candidate_session_ids_with_skip(limit: usize, skip: usize) -> anyhow::Result<Vec<String>> {
    let sessions_dir = crate::storage::jcode_dir()?.join("sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let ids = entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            // Gemini review, 2026-08-30: build the extension/stem check
            // straight off the bare filename rather than `entry.path()`,
            // which allocates a full `PathBuf` (joining the parent
            // directory back on) for every single entry just to discard it
            // a line later.
            let file_name = entry.file_name();
            let name_path = std::path::Path::new(&file_name);
            if name_path.extension().is_some_and(|ext| ext == "json") {
                name_path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .skip(skip)
        .take(limit)
        .collect();
    Ok(ids)
}

/// The actual per-cycle entry point: claim at most one eligible session
/// from a fresh candidate batch and extract it. Returns `Ok(None)` when
/// there was nothing eligible to claim this cycle (normal, not an error)
/// or the wiring is disabled via [`is_ambient_wiring_enabled`]; `Ok(Some(n))`
/// with the memory count on an extraction; extraction errors are already
/// recorded via `mark_failed` inside `extract_claimed_session` and are
/// still surfaced here so the caller can log them.
pub async fn run_one_ambient_extraction(worker_id: &str) -> anyhow::Result<Option<usize>> {
    if !is_ambient_wiring_enabled() {
        return Ok(None);
    }
    let candidates = candidate_session_ids(CANDIDATE_BATCH_SIZE)?;
    let Some(session_id) = claim_next_eligible(&candidates, worker_id, DEFAULT_LEASE_DURATION)?
    else {
        return Ok(None);
    };
    let count = extract_claimed_session(&session_id).await?;
    Ok(Some(count))
}

/// Run a real extraction pass on a session this worker already holds the
/// lease for (via [`claim_next_eligible`]), reporting the outcome back to
/// the leasing primitive itself -- callers don't need to separately call
/// `mark_extracted`/`mark_failed`, this does it for them either way.
///
/// A session with too few messages to be worth extracting from (same
/// threshold `Agent::extract_session_memories` already uses,
/// `MemoryManager::MIN_MESSAGES_FOR_EXTRACTION`) is left entirely
/// unfinalized -- neither `Extracted` nor `Failed` -- since "too few
/// messages right now" can't be told apart from "still actively growing,
/// just caught early" (see this function body's own doc comment on that
/// exact bug and its rejected first fix attempts). The lease is released
/// back to `Pending` via [`release_claim`] (not simply left `Leased` to
/// expire on its own -- see that function's doc comment for why), so it
/// naturally becomes reclaimable on a later cycle with no penalty. The same
/// treatment applies when no LLM backend is available right now.
///
/// Returns the number of memories actually stored on success. Errors
/// (session failed to load, extraction itself failed) are both surfaced to
/// the caller *and* recorded via `mark_failed` before returning, so a
/// caller that just wants "did this work" doesn't have to remember to call
/// `mark_failed` itself on every error path.
pub async fn extract_claimed_session(session_id: &str) -> anyhow::Result<usize> {
    let session = match crate::session::Session::load(session_id) {
        Ok(session) => session,
        Err(err) => {
            let message = format!("failed to load session: {err:#}");
            mark_failed(session_id, &message).ok();
            return Err(anyhow::anyhow!(message));
        }
    };

    if session.messages.len() < crate::memory::MemoryManager::MIN_MESSAGES_FOR_EXTRACTION {
        // **Real bug, caught by a final full-repo review before it could
        // cause silent data loss, then a second real bug in that fix's own
        // first attempt caught by a follow-up review of the fix itself**:
        // unconditionally marking a too-short session `Extracted` here
        // assumed "too few messages" always means a *finished*, genuinely
        // short conversation. But ambient mode claims whatever candidate
        // batch it happens to see this cycle -- that could just as easily
        // be a session someone is actively typing into *right now*, caught
        // mid-conversation before it grew past the threshold. Marking it
        // `Extracted` permanently means ambient mode -- whose whole
        // documented purpose is a retroactive safety net for a session that
        // later crashes -- would never revisit it even after it grows to 50
        // messages and then the terminal dies uncleanly (skipping the
        // interactive CLI-exit extraction hook): exactly the scenario
        // ambient mode exists to cover, silently defeated.
        //
        // The first fix attempt tried a recency check (only finalize a
        // session quiet for a full lease-duration window) -- rejected on
        // review: a user who steps away for a longer break (lunch, a
        // meeting -- anything past that window) and then resumes hits the
        // *identical* bug, just with a longer trigger window instead of a
        // fixed one. There is no timeout that can distinguish "paused, will
        // resume" from "actually finished" from message count and recency
        // alone.
        //
        // Fixed properly by never finalizing a too-short session at all --
        // release the lease back to `Pending` instead (see
        // [`release_claim`]'s own doc comment for why that's not simply
        // "leave it `Leased` to expire on its own" -- that first version of
        // this fix reintroduced a second, subtler permanent-exclusion bug
        // via the crash-loop protection above, caught by a later review).
        // This means a session that's genuinely finished and short gets
        // re-checked again every lease window, forever -- but that check is
        // just a cheap session load and message count, no LLM call, nowhere
        // near the wasteful-retry cost the original design's own comment
        // was actually worried about (which was about a *failed*
        // extraction attempt being retried under backoff, not this
        // near-free case) -- a real, bounded cost, and a far smaller one
        // than the silent, permanent data loss this now avoids.
        release_claim(session_id).ok();
        return Ok(0);
    }

    if !crate::memory::memory_llm_judge_available() {
        // Real bug fix, caught by a full-repo review: `extract_from_transcript`
        // itself treats "LLM judge unavailable right now" as a graceful
        // `Ok(Vec::new())` no-op -- indistinguishable, at this call site,
        // from "the LLM ran and genuinely found nothing worth extracting."
        // The match below used to treat both identically and call
        // `mark_extracted` either way, which is correct for the real
        // no-op-with-no-memories case but wrong here: a session claimed
        // during an outage window (no backend configured, sidecar
        // unreachable) would be permanently marked `Extracted` and never
        // revisited even after the backend comes back. Checking this
        // up front and releasing the lease (not finalizing at all, same
        // "try again later, no fault" treatment as the too-short case
        // above) means it stays eligible for a real extraction attempt once
        // the backend is actually available again.
        release_claim(session_id).ok();
        return Ok(0);
    }

    let transcript = crate::memory::transcript_from_messages(&session.messages);
    let manager = session
        .working_dir
        .as_deref()
        .map(|dir| crate::memory::MemoryManager::new().with_project_dir(dir))
        .unwrap_or_default();

    match manager.extract_from_transcript(&transcript, session_id).await {
        Ok(extracted) => {
            // Gemini review, 2026-08-30: extraction genuinely succeeded by
            // this point -- any memories it found are already persisted by
            // extract_from_transcript itself. A `?` here would let a
            // failure to write the *lease bookkeeping* (e.g. a transient
            // disk error) masquerade as an extraction failure and discard
            // the real, already-true success. Log and proceed instead: the
            // lease simply stays Leased and naturally becomes reclaimable
            // once it expires (see `is_eligible`), which at worst means a
            // future retry re-extracts the same session -- a bounded,
            // self-healing outcome (possible duplicate memories), not lost
            // extraction work or a permanently stuck lease.
            if let Err(err) = mark_extracted(session_id) {
                crate::logging::warn(&format!(
                    "extract_claimed_session: extraction for '{session_id}' succeeded but \
                     recording it as Extracted failed ({err:#}) -- lease will expire and may \
                     be retried"
                ));
            }
            Ok(extracted.len())
        }
        Err(err) => {
            let message = format!("extraction failed: {err:#}");
            mark_failed(session_id, &message).ok();
            Err(anyhow::anyhow!(message))
        }
    }
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

    // --- extract_claimed_session ---

    fn seeded_session(session_id: &str, message_count: usize) {
        let mut session =
            crate::session::Session::create_with_id(session_id.to_string(), None, None);
        for i in 0..message_count {
            let role = if i % 2 == 0 {
                jcode_message_types::Role::User
            } else {
                jcode_message_types::Role::Assistant
            };
            session.add_message(
                role,
                vec![jcode_message_types::ContentBlock::Text {
                    text: format!("message {i}"),
                    cache_control: None,
                }],
            );
        }
        session.save().expect("save seeded session");
    }


    #[tokio::test]
    async fn extract_claimed_session_never_finalizes_a_too_short_session() {
        // Regression for a real, high-severity bug (a final full-repo
        // review caught the original version, then a follow-up review of
        // that fix's own first attempt caught a second, subtler version of
        // the identical bug): a too-short session must never be marked
        // `Extracted` -- not unconditionally (the original bug: it might
        // just be caught mid-conversation, not actually finished), and not
        // even after a fixed "quiet long enough" window (the first fix's
        // own bug: a user paused longer than any chosen window, e.g. over
        // lunch, and then resumed, hits the identical failure). No
        // message-count-plus-recency heuristic can tell "paused, will
        // resume" apart from "actually done" -- so this proves the lease is
        // released back to `Pending` (naturally reclaimable on a later
        // cycle, forever, at near-zero cost, with `attempt_count` left
        // untouched) rather than ever finalized, for a too-short session
        // regardless of how long it's been quiet. Ambient mode's whole
        // documented purpose is a retroactive safety net for a session that
        // crashes later -- permanently marking a too-short session
        // `Extracted` on any timeline would silently defeat that for
        // exactly the sessions caught earliest.
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());

        seeded_session("short", 2); // below MIN_MESSAGES_FOR_EXTRACTION (4)
        claim_next_eligible(&["short".to_string()], "worker-1", DEFAULT_LEASE_DURATION)
            .expect("claim");

        let count = extract_claimed_session("short")
            .await
            .expect("must succeed, not error, even when deferring a decision");
        assert_eq!(count, 0);

        let lease = lease_status("short").expect("status").expect("exists");
        assert_eq!(
            lease.status,
            LeaseStatus::Pending,
            "a too-short session must never be finalized -- it may still be growing, on any timeline"
        );
        assert_eq!(
            lease.attempt_count, 0,
            "deferring a decision on a too-short session is not a fault -- must not count \
             toward MAX_ATTEMPTS"
        );
    }

    #[test]
    fn a_released_lease_reclaimed_after_expiry_does_not_accumulate_attempts() {
        // Regression for the bug this fix's own first attempt (see the test
        // above) would have reintroduced: leaving the lease `Leased` to
        // expire on its own looks identical, once expired, to a crashed
        // worker -- `claim_next_eligible`'s crash-loop protection would then
        // bump `attempt_count` on every single reclaim, permanently
        // excluding a session that never actually failed once it later
        // grows past the threshold. Releasing to `Pending` immediately means
        // repeated claim/release cycles never trip that crash-only branch.
        with_isolated_home(|| {
            for _ in 0..(MAX_ATTEMPTS + 2) {
                claim_next_eligible(&["short".to_string()], "worker-1", DEFAULT_LEASE_DURATION)
                    .expect("claim");
                release_claim("short").expect("release");
            }
            let lease = lease_status("short").expect("status").expect("exists");
            assert_eq!(
                lease.attempt_count, 0,
                "a deliberately-released lease must never accumulate crash-loop attempts"
            );
            assert!(
                lease.is_eligible(SystemTime::now()),
                "must still be eligible after many release cycles, not permanently excluded"
            );
        });
    }

    #[tokio::test]
    async fn extract_claimed_session_marks_failed_when_the_session_cannot_be_loaded() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());

        // Never seeded/saved -- Session::load must fail for it.
        claim_next_eligible(
            &["nonexistent".to_string()],
            "worker-1",
            DEFAULT_LEASE_DURATION,
        )
        .expect("claim");

        let result = extract_claimed_session("nonexistent").await;
        assert!(result.is_err(), "must surface the load failure to the caller");

        let lease = lease_status("nonexistent")
            .expect("status")
            .expect("exists");
        assert_eq!(lease.status, LeaseStatus::Failed);
        assert_eq!(lease.attempt_count, 1);
    }

    #[tokio::test]
    async fn extract_claimed_session_releases_the_lease_when_the_llm_judge_is_unavailable() {
        // Regression for a real bug a full-repo review caught: this used to
        // assert `LeaseStatus::Extracted` here, codifying the very bug it
        // exposed -- a session claimed during an LLM-outage window (no
        // backend configured, sidecar unreachable) was permanently marked
        // `Extracted` with zero memories and never revisited, even after the
        // backend later became available. A long-enough session in a test
        // environment with no LLM backend configured exercises the real,
        // non-mocked unavailability check, matching this project's usual
        // "genuinely tests without needing live credentials" bar.
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());

        seeded_session("long-enough", 6);
        claim_next_eligible(
            &["long-enough".to_string()],
            "worker-1",
            DEFAULT_LEASE_DURATION,
        )
        .expect("claim");

        let count = extract_claimed_session("long-enough")
            .await
            .expect("must succeed cleanly with no LLM backend configured");
        assert_eq!(
            count, 0,
            "no backend available -- must not attempt extraction at all"
        );

        let lease = lease_status("long-enough")
            .expect("status")
            .expect("exists");
        assert_eq!(
            lease.status,
            LeaseStatus::Pending,
            "must be released, not finalized, so a later cycle retries once a backend is available"
        );
        assert_eq!(lease.attempt_count, 0, "an outage is not this session's fault");
    }

    // --- candidate_session_ids / ambient wiring ---

    fn sessions_dir_for_test() -> std::path::PathBuf {
        let dir = crate::storage::jcode_dir().expect("jcode dir").join("sessions");
        std::fs::create_dir_all(&dir).expect("mkdir sessions");
        dir
    }

    #[test]
    fn candidate_session_ids_lists_json_session_files() {
        with_isolated_home(|| {
            let dir = sessions_dir_for_test();
            std::fs::write(dir.join("session-a.json"), "{}").expect("write");
            std::fs::write(dir.join("session-b.json"), "{}").expect("write");

            let mut ids = candidate_session_ids(10).expect("list");
            ids.sort();
            assert_eq!(ids, vec!["session-a".to_string(), "session-b".to_string()]);
        });
    }

    #[test]
    fn candidate_session_ids_ignores_non_json_files() {
        with_isolated_home(|| {
            let dir = sessions_dir_for_test();
            std::fs::write(dir.join("session-a.json"), "{}").expect("write");
            std::fs::write(dir.join("session-a.bak"), "{}").expect("write");
            std::fs::write(dir.join("sessions-bak-prune.stamp"), "").expect("write");

            let ids = candidate_session_ids(10).expect("list");
            assert_eq!(ids, vec!["session-a".to_string()]);
        });
    }

    #[test]
    fn candidate_session_ids_respects_the_limit() {
        with_isolated_home(|| {
            let dir = sessions_dir_for_test();
            for i in 0..10 {
                std::fs::write(dir.join(format!("session-{i}.json")), "{}").expect("write");
            }
            let ids = candidate_session_ids(3).expect("list");
            assert_eq!(ids.len(), 3, "must stop at the limit, not return all 10");
        });
    }

    #[test]
    fn candidate_session_ids_returns_empty_when_sessions_dir_is_missing() {
        with_isolated_home(|| {
            // Deliberately not calling sessions_dir_for_test() -- the
            // directory itself doesn't exist yet in a fresh JCODE_HOME.
            let ids = candidate_session_ids(10).expect("must not error");
            assert!(ids.is_empty());
        });
    }

    #[test]
    fn candidate_session_ids_with_skip_stops_returning_the_same_fixed_prefix() {
        // Regression for the real stagnation bug an agy review caught: a
        // plain `.take(limit)` with no variation would return the exact
        // same leading entries on every call. Two different skip values
        // over the same 10-entry directory must be able to select
        // different windows (not required to be *disjoint*, just capable
        // of differing) -- proving the skip parameter actually changes
        // which entries come back, not just accepted-but-ignored.
        with_isolated_home(|| {
            let dir = sessions_dir_for_test();
            for i in 0..10 {
                std::fs::write(dir.join(format!("session-{i}.json")), "{}").expect("write");
            }
            let first_window = candidate_session_ids_with_skip(3, 0).expect("list");
            let later_window = candidate_session_ids_with_skip(3, 5).expect("list");
            assert_ne!(
                first_window, later_window,
                "a nonzero skip must actually shift which entries are returned"
            );
        });
    }

    #[test]
    fn candidate_session_ids_falls_back_to_unskipped_when_the_random_skip_overshoots() {
        // Regression for a second bug caught while fixing the first one:
        // a *fixed* random skip window (up to CANDIDATE_SKIP_WINDOW) lands
        // past the end of a small directory far more often than not --
        // almost certainly the common case (a light user, a fresh
        // install). Without a fallback, that would silently turn "always
        // stuck on the same 25" into "usually finds nothing at all,"
        // starving the common case instead of the large-install case.
        with_isolated_home(|| {
            let dir = sessions_dir_for_test();
            std::fs::write(dir.join("only-session.json"), "{}").expect("write");

            // The public entry point applies its own random skip
            // internally, which for a single-entry directory overshoots on
            // every call except the rare exact-zero roll -- across many
            // calls, it must still reliably fall back to real candidates
            // rather than returning empty most of the time.
            for _ in 0..20 {
                let ids = candidate_session_ids(10).expect("list");
                assert_eq!(ids, vec!["only-session".to_string()]);
            }
        });
    }

    #[test]
    fn is_ambient_wiring_enabled_is_off_by_default() {
        let _guard = crate::storage::lock_test_env();
        crate::env::remove_var(AMBIENT_WIRING_ENV_VAR);
        assert!(!is_ambient_wiring_enabled());
    }

    #[test]
    fn is_ambient_wiring_enabled_reflects_the_env_var() {
        let _guard = crate::storage::lock_test_env();
        crate::env::set_var(AMBIENT_WIRING_ENV_VAR, "1");
        assert!(is_ambient_wiring_enabled());
        crate::env::remove_var(AMBIENT_WIRING_ENV_VAR);
    }

    #[tokio::test]
    async fn run_one_ambient_extraction_is_a_noop_when_wiring_is_disabled() {
        let _guard = crate::storage::lock_test_env();
        crate::env::remove_var(AMBIENT_WIRING_ENV_VAR);
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());

        // Even with an eligible session sitting right there, disabled means
        // disabled -- must not touch it at all.
        seeded_session("untouched", 6);

        let result = run_one_ambient_extraction("ambient-test")
            .await
            .expect("must not error when disabled");
        assert_eq!(result, None);
        assert!(
            lease_status("untouched").expect("status").is_none(),
            "a disabled wiring pass must not create a lease for anything"
        );
    }

    #[tokio::test]
    async fn run_one_ambient_extraction_claims_and_extracts_when_enabled() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());
        crate::env::set_var(AMBIENT_WIRING_ENV_VAR, "1");

        seeded_session("ready-for-extraction", 6);

        let result = run_one_ambient_extraction("ambient-test").await;
        crate::env::remove_var(AMBIENT_WIRING_ENV_VAR);
        let count = result.expect("must succeed with no LLM backend configured");
        assert_eq!(
            count,
            Some(0),
            "no backend available in the test environment -- extract_claimed_session's own \
             up-front unavailability check"
        );

        let lease = lease_status("ready-for-extraction")
            .expect("status")
            .expect("a claim must have happened");
        assert_eq!(
            lease.status,
            LeaseStatus::Pending,
            "no backend available -- must release, not finalize, so a later cycle retries \
             once a backend is available"
        );
    }

    #[tokio::test]
    async fn run_one_ambient_extraction_returns_none_when_nothing_is_eligible() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());
        crate::env::set_var(AMBIENT_WIRING_ENV_VAR, "1");

        // No sessions/ directory at all -- candidate_session_ids returns
        // empty, so there's nothing for claim_next_eligible to pick.
        let result = run_one_ambient_extraction("ambient-test").await;
        crate::env::remove_var(AMBIENT_WIRING_ENV_VAR);
        assert_eq!(result.expect("must not error"), None);
    }
}
