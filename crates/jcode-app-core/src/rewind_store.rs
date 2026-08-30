//! Fusion Phase 0: provable-safe rewind (DESIGN.md §6 item #4).
//!
//! jcode's existing `/rewind` (`Agent::rewind_to_message`/`undo_rewind`,
//! `agent/turn_execution.rs`) had three specific, documented gaps versus
//! Grok Build's "refuse rather than guess" design: the undo snapshot was
//! **in-memory only** (lost on restart), held **only one level** (a second
//! rewind overwrote the first, no stack), and had **no integrity check**
//! before trusting a snapshot. This module closes the first two directly and
//! gives the third a real, enforced meaning: a persisted, multi-level undo
//! stack, and a snapshot is verified against its own content hash before
//! being restored — corruption is *refused*, not silently applied.
//!
//! Deliberately out of scope for this slice (documented, not silently
//! skipped): filesystem/tool-side-effect awareness. This only covers
//! conversation state (`Session.messages` + provider session ids), exactly
//! the same scope the pre-existing `/rewind` had — it doesn't yet know about
//! or revert file writes made by tool calls between the snapshot and now.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::session::StoredMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindSnapshot {
    pub messages: Vec<StoredMessage>,
    pub provider_session_id: Option<String>,
    pub session_provider_session_id: Option<String>,
    pub visible_message_count: usize,
    pub created_at: DateTime<Utc>,
    /// SHA-256 of the other fields (see `compute_hash`), checked before this
    /// snapshot is ever restored. Not a security boundary — a corruption/
    /// integrity check, so a truncated write or manual file edit is refused
    /// rather than silently applied as if it were a trustworthy prior state.
    ///
    /// **Explicitly not defense against deliberate tampering** (Gemini
    /// review, 2026-08-30, sharpening this doc comment rather than the
    /// mechanism itself): this is a plain, unkeyed hash — anything with
    /// filesystem write access to `~/.jcode/rewind/` can edit the stored
    /// messages and simply recompute a matching hash. A real
    /// tamper-resistant check needs an HMAC with a locally-held secret,
    /// which needs its own key-management story (an OS keychain, most
    /// plausibly) — a genuinely bigger feature, not a drop-in change to
    /// this struct. "Provable-safe" in this module's own docs has always
    /// meant "provably not corrupted," never "provably not tampered with by
    /// something that already has local filesystem access" — recorded
    /// explicitly here so that distinction isn't assumed away by a future
    /// reader.
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RewindStack {
    #[serde(default)]
    snapshots: Vec<RewindSnapshot>,
}

/// What happened when popping the most recent snapshot off the stack.
#[derive(Debug)]
pub enum PopOutcome {
    /// No snapshots on the stack for this session.
    Empty,
    /// A snapshot was found but failed its integrity check. **Left in place
    /// on disk, not discarded** — refusing to restore corrupt data is not
    /// the same as deciding it's worthless; a human can still inspect
    /// `~/.jcode/rewind/<session>.json` directly if needed.
    Corrupt { reason: String },
    Popped(Box<RewindSnapshot>),
}

/// Gemini review, 2026-08-30: `created_at` was previously omitted from the
/// hash payload entirely (it wasn't even known yet at the point `push_snapshot`
/// used to call this — `created_at: Utc::now()` was set *after* the hash was
/// computed), so that field could be altered on disk without the integrity
/// check ever noticing. Now a required input, computed once by the caller
/// and used for both the hash and the stored field, so they can never
/// diverge.
fn compute_hash(
    messages: &[StoredMessage],
    provider_session_id: &Option<String>,
    session_provider_session_id: &Option<String>,
    visible_message_count: usize,
    created_at: DateTime<Utc>,
) -> Result<String> {
    #[derive(Serialize)]
    struct HashPayload<'a> {
        messages: &'a [StoredMessage],
        provider_session_id: &'a Option<String>,
        session_provider_session_id: &'a Option<String>,
        visible_message_count: usize,
        created_at: DateTime<Utc>,
    }
    let bytes = serde_json::to_vec(&HashPayload {
        messages,
        provider_session_id,
        session_provider_session_id,
        visible_message_count,
        created_at,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Serializes every push/pop against every session's persisted rewind
/// stack. Gemini review, 2026-08-30: `push_snapshot`/`pop_snapshot`
/// previously did an unguarded load -> mutate -> save round trip, relying
/// entirely on an *external* lock (the caller's own `Mutex<Agent>`) — a
/// second concurrent caller for the same session (e.g. two live client
/// connections attached to the same session, or a future call path that
/// simply forgets to hold that lock) could cause a lost snapshot, or pop
/// the same top snapshot twice while the on-disk stack only shrinks once.
/// One process-wide mutex (not a per-`session_id` map, since rewind is not
/// a hot path — the small, harmless over-serialization across unrelated
/// sessions isn't worth a lock-map's own lifecycle complexity) makes this
/// module safe against concurrent callers by construction, not by caller
/// discipline.
static REWIND_STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire [`REWIND_STORE_LOCK`], recovering from poisoning rather than
/// panicking: a panic *elsewhere* while the lock was held must not
/// permanently wedge every future rewind operation in the process.
fn lock_rewind_store() -> std::sync::MutexGuard<'static, ()> {
    REWIND_STORE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Push a new snapshot onto this session's undo stack. Returns the new
/// stack depth (number of available undo levels, including this one).
pub fn push_snapshot(
    session_id: &str,
    messages: Vec<StoredMessage>,
    provider_session_id: Option<String>,
    session_provider_session_id: Option<String>,
    visible_message_count: usize,
) -> Result<usize> {
    let created_at = Utc::now();
    let content_hash = compute_hash(
        &messages,
        &provider_session_id,
        &session_provider_session_id,
        visible_message_count,
        created_at,
    )?;
    let _guard = lock_rewind_store();
    let mut stack = load_stack(session_id)?;
    stack.snapshots.push(RewindSnapshot {
        messages,
        provider_session_id,
        session_provider_session_id,
        visible_message_count,
        created_at,
        content_hash,
    });
    let depth = stack.snapshots.len();
    save_stack(session_id, &stack)?;
    Ok(depth)
}

/// Pop and integrity-check the most recent snapshot. On success, the
/// snapshot is removed from the persisted stack. On a failed integrity
/// check, the stack is left untouched (see [`PopOutcome::Corrupt`]).
pub fn pop_snapshot(session_id: &str) -> Result<PopOutcome> {
    let _guard = lock_rewind_store();
    let mut stack = load_stack(session_id)?;
    let Some(snapshot) = stack.snapshots.pop() else {
        return Ok(PopOutcome::Empty);
    };

    let expected_hash = compute_hash(
        &snapshot.messages,
        &snapshot.provider_session_id,
        &snapshot.session_provider_session_id,
        snapshot.visible_message_count,
        snapshot.created_at,
    )?;
    if expected_hash != snapshot.content_hash {
        // Refuse rather than guess: do NOT save `stack` (which has this
        // entry already popped in memory) -- the on-disk file still has it,
        // untouched, since we return before calling save_stack.
        return Ok(PopOutcome::Corrupt {
            reason: format!(
                "rewind snapshot integrity check failed (stored hash {}, recomputed {}) \
                 -- refusing to restore possibly-corrupt conversation state",
                snapshot.content_hash, expected_hash
            ),
        });
    }

    save_stack(session_id, &stack)?;
    Ok(PopOutcome::Popped(Box::new(snapshot)))
}

/// Number of undo levels currently available for this session.
pub fn depth(session_id: &str) -> Result<usize> {
    Ok(load_stack(session_id)?.snapshots.len())
}

/// Discard the entire undo stack for this session (e.g. on session end).
pub fn clear(session_id: &str) -> Result<()> {
    let _guard = lock_rewind_store();
    let path = stack_path(session_id)?;
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    // Gemini review, 2026-08-30: `write_json_fast` (jcode-storage, upstream)
    // preserves the previous version as a `.bak` hard link on every write
    // (`path.with_extension("bak")`) -- `clear` removed only the primary
    // file, leaving a full copy of the "discarded" conversation history
    // readable on disk. Best-effort: a `.bak` that never existed, or that's
    // already gone, is not an error.
    let bak_path = path.with_extension("bak");
    if bak_path.exists() {
        std::fs::remove_file(&bak_path)?;
    }
    Ok(())
}

fn load_stack(session_id: &str) -> Result<RewindStack> {
    let path = stack_path(session_id)?;
    if !path.exists() {
        return Ok(RewindStack::default());
    }
    crate::storage::read_json(&path)
}

fn save_stack(session_id: &str, stack: &RewindStack) -> Result<()> {
    crate::storage::write_json_fast(&stack_path(session_id)?, stack)
}

fn stack_path(session_id: &str) -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?
        .join("rewind")
        .join(format!("{}.json", sanitize_session_id(session_id))))
}

fn sanitize_session_id(session_id: &str) -> String {
    session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str) -> StoredMessage {
        StoredMessage {
            id: format!("msg-{text}"),
            role: crate::message::Role::User,
            content: vec![crate::message::ContentBlock::Text {
                text: text.to_string(),
                cache_control: None,
            }],
            display_role: None,
            timestamp: None,
            tool_duration_ms: None,
            token_usage: None,
        }
    }

    fn with_isolated_home<F: FnOnce()>(f: F) {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());
        f();
        if let Some(prev_home) = prev_home {
            crate::env::set_var("JCODE_HOME", prev_home);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    #[test]
    fn empty_stack_reports_empty() {
        with_isolated_home(|| {
            let outcome = pop_snapshot("ses_empty").expect("pop");
            assert!(matches!(outcome, PopOutcome::Empty));
            assert_eq!(depth("ses_empty").expect("depth"), 0);
        });
    }

    #[test]
    fn push_then_pop_round_trips() {
        with_isolated_home(|| {
            let session_id = "ses_roundtrip";
            let depth_after_push =
                push_snapshot(session_id, vec![msg("hello")], None, None, 1).expect("push");
            assert_eq!(depth_after_push, 1);
            assert_eq!(depth(session_id).expect("depth"), 1);

            match pop_snapshot(session_id).expect("pop") {
                PopOutcome::Popped(snapshot) => {
                    assert_eq!(snapshot.visible_message_count, 1);
                }
                other => panic!("expected Popped, got {:?}", other),
            }
            assert_eq!(depth(session_id).expect("depth"), 0);
        });
    }

    #[test]
    fn multi_level_stack_pops_in_lifo_order() {
        with_isolated_home(|| {
            let session_id = "ses_multilevel";
            push_snapshot(session_id, vec![msg("first")], None, None, 1).expect("push 1");
            push_snapshot(session_id, vec![msg("second")], None, None, 2).expect("push 2");
            push_snapshot(session_id, vec![msg("third")], None, None, 3).expect("push 3");
            assert_eq!(depth(session_id).expect("depth"), 3);

            let first_pop = pop_snapshot(session_id).expect("pop");
            match first_pop {
                PopOutcome::Popped(s) => assert_eq!(s.visible_message_count, 3),
                other => panic!("expected Popped, got {:?}", other),
            }
            let second_pop = pop_snapshot(session_id).expect("pop");
            match second_pop {
                PopOutcome::Popped(s) => assert_eq!(s.visible_message_count, 2),
                other => panic!("expected Popped, got {:?}", other),
            }
            assert_eq!(depth(session_id).expect("depth"), 1);
        });
    }

    #[test]
    fn tampered_snapshot_is_refused_not_applied() {
        with_isolated_home(|| {
            let session_id = "ses_tampered";
            push_snapshot(session_id, vec![msg("original")], None, None, 1).expect("push");

            // Simulate corruption/tampering: load the stack file, mutate a
            // message in place, save it back with the (now-stale) hash.
            let path = stack_path(session_id).expect("path");
            let mut stack: RewindStack = crate::storage::read_json(&path).expect("read");
            stack.snapshots[0].messages[0] = msg("tampered");
            crate::storage::write_json_fast(&path, &stack).expect("write tampered");

            match pop_snapshot(session_id).expect("pop") {
                PopOutcome::Corrupt { reason } => {
                    assert!(reason.contains("integrity check failed"));
                }
                other => panic!("expected Corrupt, got {:?}", other),
            }
            // Refused, not discarded: the entry must still be there.
            assert_eq!(
                depth(session_id).expect("depth"),
                1,
                "a refused/corrupt snapshot must not be silently dropped from the stack"
            );
        });
    }

    /// Gemini review, 2026-08-30: `created_at` used to be excluded from the
    /// hash payload -- altering it on disk previously passed the integrity
    /// check silently.
    #[test]
    fn tampering_with_created_at_alone_is_also_caught() {
        with_isolated_home(|| {
            let session_id = "ses_tampered_timestamp";
            push_snapshot(session_id, vec![msg("original")], None, None, 1).expect("push");

            let path = stack_path(session_id).expect("path");
            let mut stack: RewindStack = crate::storage::read_json(&path).expect("read");
            stack.snapshots[0].created_at =
                stack.snapshots[0].created_at + chrono::Duration::days(365);
            crate::storage::write_json_fast(&path, &stack).expect("write tampered");

            match pop_snapshot(session_id).expect("pop") {
                PopOutcome::Corrupt { .. } => {}
                other => panic!(
                    "expected Corrupt after tampering with created_at alone, got {:?}",
                    other
                ),
            }
        });
    }

    /// Gemini review, 2026-08-30: push_snapshot's load -> mutate -> save
    /// was previously unguarded; two concurrent pushes for the same
    /// session could both load the same prior stack, each append their own
    /// snapshot, and save -- whichever save landed last would silently
    /// discard the other. Real concurrent threads (not just a sequential
    /// simulation), same process -- exactly the "two callers for the same
    /// session_id" scenario the finding describes.
    #[test]
    fn concurrent_pushes_for_the_same_session_never_lose_a_snapshot() {
        with_isolated_home(|| {
            let session_id = "ses_concurrent";
            const PUSHES: usize = 20;
            std::thread::scope(|scope| {
                for i in 0..PUSHES {
                    scope.spawn(move || {
                        push_snapshot(session_id, vec![msg(&format!("m{i}"))], None, None, i)
                            .expect("push");
                    });
                }
            });
            assert_eq!(
                depth(session_id).expect("depth"),
                PUSHES,
                "every concurrent push must land -- none may be silently lost to a race"
            );
        });
    }

    #[test]
    fn clear_removes_the_whole_stack() {
        with_isolated_home(|| {
            let session_id = "ses_clear";
            push_snapshot(session_id, vec![msg("a")], None, None, 1).expect("push");
            push_snapshot(session_id, vec![msg("b")], None, None, 2).expect("push");
            assert_eq!(depth(session_id).expect("depth"), 2);
            clear(session_id).expect("clear");
            assert_eq!(depth(session_id).expect("depth"), 0);
        });
    }
}
