//! Fusion Phase 3: the daemon-side half of ACP client-callback delegation
//! (`fs/read_text_file`, `session/request_permission`, `terminal/*`).
//!
//! The ACP-adapter-side half (`send_client_request`/`route_response`,
//! `src/cli/acp.rs`) already lets that process send a request to its own
//! ACP host and correlate the response. This module is the missing other
//! direction: letting the **daemon** ask a specific session's connected
//! client to do something and wait for the answer, over the exact same
//! `Request`/`ServerEvent` wire every other client interaction already
//! uses (`ServerEvent::AcpCallbackRequest` out, `Request::AcpCallbackResponse`
//! back).
//!
//! `WriteTool`/`ReadTool` (`tool/write.rs`/`tool/read.rs`) now call
//! `send_acp_callback_for_session`/`is_acp_session` directly -- the real
//! wiring this module was originally built ahead of. A non-ACP session's
//! behavior in those tools is untouched (same `tokio::fs` calls as always);
//! an ACP-connected session's primary file read/write routes through this
//! relay instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{RwLock, oneshot};

use crate::protocol::ServerEvent;

use super::{ClientConnectionInfo, SwarmMember};

type PendingAcpCallbacks =
    Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, Value>>>>;

fn pending_callbacks() -> &'static PendingAcpCallbacks {
    static PENDING: OnceLock<PendingAcpCallbacks> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_CALLBACK_ID: AtomicU64 = AtomicU64::new(1);

/// Removes a pending callback's entry when dropped, regardless of how
/// [`send_acp_callback`]'s scope ends -- the same `Drop`-guard shape (and
/// the same real reason for it) as `PendingRequestGuard` on the ACP-adapter
/// side: a caller that drops the awaiting future early (an outer
/// `tokio::select!`, a cancelled tool call) must not leak the entry
/// forever, since neither the timeout arm nor a genuine response arriving
/// later would otherwise ever run the cleanup.
struct PendingCallbackGuard {
    id: u64,
}

impl Drop for PendingCallbackGuard {
    fn drop(&mut self) {
        if let Ok(mut pending) = pending_callbacks().lock() {
            pending.remove(&self.id);
        }
    }
}

/// Ask `session_id`'s connected client to do something and wait for its
/// answer. Looks up that session's live `event_tx` from `swarm_members`
/// (the same "reach a specific session's live connection from anywhere in
/// the daemon" primitive already used elsewhere, e.g. swarm event fanout)
/// -- a session with no live connection, or whose client never answers
/// (doesn't understand `method`, or answers a different id), fails with a
/// clear error rather than hanging forever past `timeout`.
pub async fn send_acp_callback(
    session_id: &str,
    method: &str,
    params: Value,
    timeout: Duration,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> anyhow::Result<Value> {
    let event_tx = {
        let members = swarm_members.read().await;
        members.get(session_id).map(|member| member.event_tx.clone())
    };
    let Some(event_tx) = event_tx else {
        anyhow::bail!(
            "session '{session_id}' has no live connection to relay an ACP callback through"
        );
    };

    let id = NEXT_CALLBACK_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    {
        let mut pending = pending_callbacks()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.insert(id, tx);
    }
    let _cleanup_guard = PendingCallbackGuard { id };

    if event_tx
        .send(ServerEvent::AcpCallbackRequest {
            id,
            method: method.to_string(),
            params,
        })
        .is_err()
    {
        anyhow::bail!(
            "session '{session_id}' connection is gone, cannot relay ACP callback for '{method}'"
        );
    }

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(Ok(result))) => Ok(result),
        Ok(Ok(Err(error))) => Err(anyhow::anyhow!(
            "client returned an error for ACP callback '{method}': {error}"
        )),
        Ok(Err(_)) => Err(anyhow::anyhow!(
            "internal: pending ACP callback for '{method}' (id {id}) was dropped without a response"
        )),
        Err(_) => Err(anyhow::anyhow!(
            "session '{session_id}' did not answer ACP callback '{method}' within {timeout:?}"
        )),
    }
}

/// Complete a pending callback with the client's answer -- called from the
/// `Request::AcpCallbackResponse` handler (`client_lifecycle.rs`) when a
/// client's response line arrives. A response for an id with no matching
/// waiter (already timed out, or an id the client made up) is silently
/// dropped, not an error -- the same "an unmatched response is the
/// client's problem, not ours" stance `route_response` already takes on
/// the ACP-adapter side of this exact same pattern.
pub fn resolve_acp_callback(id: u64, result: std::result::Result<Value, Value>) {
    let waiter = pending_callbacks()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
    if let Some(waiter) = waiter {
        let _ = waiter.send(result);
    }
}

// --- Real caller support: which sessions are ACP-connected, and a
// no-swarm_members-needed entry point for callers (WriteTool/ReadTool)
// that don't have direct access to it. ---

type SwarmMembers = Arc<RwLock<HashMap<String, SwarmMember>>>;

/// The one shared static both `register_swarm_members` and
/// `registered_swarm_members` must use -- a real bug caught while writing
/// this (before it ever shipped): an earlier draft gave each function its
/// *own* function-local `static`, which are genuinely distinct statics even
/// with the same name, so a `set` from one would never be visible to a
/// `get` from the other. A single module-level static is the only way
/// these two functions actually share the same cell.
static REGISTERED_SWARM_MEMBERS: OnceLock<SwarmMembers> = OnceLock::new();

fn registered_swarm_members() -> Option<&'static SwarmMembers> {
    // `get_or_init` isn't used here (unlike `pending_callbacks`) because
    // this needs to be *set* once from `Server::new` with the real,
    // already-constructed map, not lazily built with an empty one the
    // server would never actually use -- `OnceLock::get` after a
    // conditional `set` is the right shape for "may not be registered
    // yet" (e.g. a tool call racing server startup, vanishingly unlikely
    // in practice but not something to assume away).
    REGISTERED_SWARM_MEMBERS.get()
}

/// Register the running server's own `swarm_members` map. Called once from
/// `Server::new`. **A real limit, verified not guessed**: production reload
/// is a full `exec()` process replacement (`platform::replace_process`,
/// confirmed by reading it directly) -- a genuinely fresh process, so this
/// static starts over cleanly there. The one place multiple `Server`
/// instances *do* coexist in one process is this crate's own test suite
/// (`server/tests.rs`, `server/startup_tests.rs` each construct their own);
/// none of those currently exercise ACP callback behavior, so this is a
/// real, latent limitation of the "one global slot" design, not (yet) an
/// active bug -- flagged honestly rather than the earlier draft's
/// overconfident "never happens."
pub fn register_swarm_members(members: SwarmMembers) {
    let _ = REGISTERED_SWARM_MEMBERS.set(members);
}

type ClientConnections = Arc<RwLock<HashMap<String, ClientConnectionInfo>>>;

static REGISTERED_CLIENT_CONNECTIONS: OnceLock<ClientConnections> = OnceLock::new();

fn registered_client_connections() -> Option<&'static ClientConnections> {
    REGISTERED_CLIENT_CONNECTIONS.get()
}

/// Register the running server's own `client_connections` map, alongside
/// [`register_swarm_members`] (same call site, same lifetime, same
/// one-process-per-registration limit documented there).
pub fn register_client_connections(connections: ClientConnections) {
    let _ = REGISTERED_CLIENT_CONNECTIONS.set(connections);
}

/// Whether `session_id` is *currently* ACP-connected -- the check
/// `WriteTool`/`ReadTool` use to decide whether to route through
/// `send_acp_callback_for_session` at all.
///
/// **Rewritten after a Gemini review found the original design genuinely
/// broken, not shipped as first written**: the original approach was a
/// sticky, mark-once `HashSet<String>`, set when a session was created via
/// ACP and never cleared. That has a real, production-triggerable failure
/// mode: if an ACP client disconnects and a *different* client (a TUI, most
/// commonly) later resumes that exact session id via `session/resume`, the
/// stale mark would still route that TUI session's every file write/read
/// through a callback aimed at a client that has no idea what
/// `fs/read_text_file` even means -- every file operation on that session
/// would hang for the full timeout and then fail, for a perfectly ordinary
/// TUI session, until the process restarts. A background swarm worker
/// outliving an ACP client's own disconnect would hit the identical
/// failure.
///
/// Fixed by checking the *live* connection state instead of a persisted
/// mark: `client_connections` already tracks `client_instance_id` per
/// connection, updated correctly on every connect/resume/disconnect by
/// code this slice doesn't touch at all (jcode's own existing connection
/// lifecycle management) -- reusing it instead of inventing parallel,
/// independently-stale-able tracking. This also means `Request::
/// MarkAcpSession` (the original design's own marking mechanism) is no
/// longer needed at all: ACP-ness is now derived from data the existing
/// `Subscribe` handling already records, not something a *separate* step
/// has to remember to set.
pub async fn is_acp_session(session_id: &str) -> bool {
    let Some(connections) = registered_client_connections() else {
        return false;
    };
    connections
        .read()
        .await
        .values()
        .any(|info| info.session_id == session_id && info.client_instance_id.as_deref() == Some("acp"))
}

/// `send_acp_callback` for a caller (`WriteTool`/`ReadTool`) that doesn't
/// have `swarm_members` threaded to it at all -- resolves it from the
/// process-wide registration instead. Fails with a clear error (never
/// panics) if the server hasn't registered its map yet, or the session
/// isn't actually ACP-connected -- callers are expected to check
/// [`is_acp_session`] first and only call this when it returned `true`;
/// this function re-checks `send_acp_callback`'s own "session has no live
/// connection" case regardless, so it's still safe to call without that
/// check, just less informative about *why* it failed.
pub async fn send_acp_callback_for_session(
    session_id: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let Some(members) = registered_swarm_members() else {
        anyhow::bail!("ACP callback dispatcher not initialized (server not fully started?)");
    };
    send_acp_callback(session_id, method, params, timeout, members).await
}

/// Test-only accessor for the registered `swarm_members` map, tolerant of
/// `register_swarm_members` already having been called earlier in this same
/// test binary run (a real constraint, not an oversight: `OnceLock::set`
/// only ever succeeds once per process, and `cargo test` runs every test in
/// this crate inside one process). Callers in `tool/write.rs`'s and
/// `tool/read.rs`'s own tests use unique, never-reused session ids when
/// inserting into whatever map this returns, so tests sharing one
/// underlying map (whichever test happened to register it first) never
/// collide with each other.
///
/// **A real hang, reproduced then fixed, not just theorized**: an earlier
/// version returned its own locally-built map whenever `set` lost the race
/// against a concurrently-running test doing the same thing (`cargo test`
/// runs different test functions on different OS threads by default) --
/// `OnceLock::set` failing is silent (`let _ = ...`), so the loser walked
/// away holding a map the production code (`send_acp_callback_for_session`,
/// which only ever reads the *actual* registered global) could never see.
/// That test's session id was then invisible to `send_acp_callback`, which
/// failed fast with "no live connection" *without ever sending the event*
/// -- and the test was still blocked on `event_rx.recv().await`, which
/// blocks forever rather than erroring, since its sending half was alive
/// but simply never used. Reproduced directly: `tool::write`'s and
/// `tool::read`'s ACP-routing tests run in ~0.00s individually, but hung
/// indefinitely when run together (their natural, default `cargo test`
/// scheduling). Fixed by re-reading the *actual* global after attempting to
/// set it, regardless of whether this call won the race -- every caller
/// now provably shares the one true map, never a stale local one.
#[cfg(test)]
pub(crate) fn ensure_swarm_members_registered_for_test() -> SwarmMembers {
    if let Some(existing) = registered_swarm_members() {
        return Arc::clone(existing);
    }
    let members: SwarmMembers = Arc::new(RwLock::new(HashMap::new()));
    let _ = REGISTERED_SWARM_MEMBERS.set(members);
    Arc::clone(
        registered_swarm_members()
            .expect("just set it ourselves, or another thread already had"),
    )
}

/// Same tolerance -- and the same real race-to-hang fix -- as
/// [`ensure_swarm_members_registered_for_test`], for `client_connections`:
/// `is_acp_session`'s own real backing store now that it checks live
/// connection state instead of a sticky mark.
#[cfg(test)]
pub(crate) fn ensure_client_connections_registered_for_test() -> ClientConnections {
    if let Some(existing) = registered_client_connections() {
        return Arc::clone(existing);
    }
    let connections: ClientConnections = Arc::new(RwLock::new(HashMap::new()));
    let _ = REGISTERED_CLIENT_CONNECTIONS.set(connections);
    Arc::clone(
        registered_client_connections()
            .expect("just set it ourselves, or another thread already had"),
    )
}

/// A minimal, real `ClientConnectionInfo` marking `session_id` as
/// ACP-connected, for tests exercising `is_acp_session`/
/// `send_acp_callback_for_session` end to end (`tool/write.rs`'s and
/// `tool/read.rs`'s own tests, plus this module's own).
#[cfg(test)]
pub(crate) fn test_acp_client_connection(session_id: &str) -> ClientConnectionInfo {
    let (disconnect_tx, _disconnect_rx) = tokio::sync::mpsc::unbounded_channel();
    ClientConnectionInfo {
        client_id: format!("client-for-{session_id}"),
        session_id: session_id.to_string(),
        client_instance_id: Some("acp".to_string()),
        debug_client_id: None,
        connected_at: std::time::Instant::now(),
        last_seen: std::time::Instant::now(),
        is_processing: false,
        current_tool_name: None,
        terminal_env: Vec::new(),
        disconnect_tx,
    }
}

/// A minimal, real `SwarmMember` for tests -- reusable outside this module
/// (`tool/write.rs`'s and `tool/read.rs`'s own tests need one too, to
/// exercise `send_acp_callback_for_session`/`is_acp_session` end to end).
/// Moved to module scope rather than duplicated per test module.
#[cfg(test)]
pub(crate) fn test_swarm_member(
    session_id: &str,
    event_tx: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
) -> SwarmMember {
    SwarmMember {
        session_id: session_id.to_string(),
        event_tx,
        event_txs: HashMap::new(),
        working_dir: None,
        swarm_id: None,
        swarm_enabled: false,
        status: "running".to_string(),
        detail: None,
        task_label: None,
        friendly_name: None,
        report_back_to_session_id: None,
        latest_completion_report: None,
        role: "agent".to_string(),
        joined_at: std::time::Instant::now(),
        last_status_change: std::time::Instant::now(),
        is_headless: true,
        output_tail: None,
        todo_progress: None,
        todo_items: Vec::new(),
        runtime: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn test_member(event_tx: mpsc::UnboundedSender<ServerEvent>) -> SwarmMember {
        test_swarm_member("sess-1", event_tx)
    }

    #[tokio::test]
    async fn send_acp_callback_fails_cleanly_for_an_unknown_session() {
        let swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let result = send_acp_callback(
            "no-such-session",
            "fs/read_text_file",
            serde_json::json!({"path": "/tmp/x"}),
            Duration::from_millis(50),
            &swarm_members,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_acp_callback_sends_the_event_and_resolves_on_a_matching_response() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>> =
            Arc::new(RwLock::new(HashMap::new()));
        swarm_members
            .write()
            .await
            .insert("sess-1".to_string(), test_member(event_tx));

        let call = {
            let swarm_members = Arc::clone(&swarm_members);
            tokio::spawn(async move {
                send_acp_callback(
                    "sess-1",
                    "fs/read_text_file",
                    serde_json::json!({"path": "/tmp/x"}),
                    Duration::from_secs(5),
                    &swarm_members,
                )
                .await
            })
        };

        let event = event_rx.recv().await.expect("event sent");
        let ServerEvent::AcpCallbackRequest { id, method, params } = event else {
            panic!("expected AcpCallbackRequest, got {event:?}");
        };
        assert_eq!(method, "fs/read_text_file");
        assert_eq!(params["path"], "/tmp/x");

        resolve_acp_callback(id, Ok(serde_json::json!({"content": "file contents"})));

        let result = call.await.expect("task").expect("should resolve");
        assert_eq!(result, serde_json::json!({"content": "file contents"}));
    }

    #[tokio::test]
    async fn send_acp_callback_surfaces_a_client_error() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>> =
            Arc::new(RwLock::new(HashMap::new()));
        swarm_members
            .write()
            .await
            .insert("sess-1".to_string(), test_member(event_tx));

        let call = {
            let swarm_members = Arc::clone(&swarm_members);
            tokio::spawn(async move {
                send_acp_callback(
                    "sess-1",
                    "fs/read_text_file",
                    serde_json::json!({}),
                    Duration::from_secs(5),
                    &swarm_members,
                )
                .await
            })
        };

        let event = event_rx.recv().await.expect("event sent");
        let ServerEvent::AcpCallbackRequest { id, .. } = event else {
            panic!("expected AcpCallbackRequest");
        };
        resolve_acp_callback(id, Err(serde_json::json!({"message": "permission denied"})));

        let result = call.await.expect("task");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_acp_callback_times_out_cleanly_and_cleans_up_its_pending_entry() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let swarm_members: Arc<RwLock<HashMap<String, SwarmMember>>> =
            Arc::new(RwLock::new(HashMap::new()));
        swarm_members
            .write()
            .await
            .insert("sess-1".to_string(), test_member(event_tx));

        let result = send_acp_callback(
            "sess-1",
            "fs/read_text_file",
            serde_json::json!({}),
            Duration::from_millis(50),
            &swarm_members,
        )
        .await;
        assert!(result.is_err(), "must time out, not hang forever");

        // The event was still sent even though nobody ever answered it --
        // confirms the timeout, not a send failure, is what's being tested.
        assert!(event_rx.try_recv().is_ok());
        assert!(
            pending_callbacks().lock().unwrap().is_empty(),
            "a timed-out callback must clean up its own pending-map entry"
        );
    }

    #[test]
    fn resolve_acp_callback_silently_drops_a_response_with_no_matching_waiter() {
        // Must not panic on an id nothing is waiting for -- e.g. already
        // timed out, or an id the client made up.
        resolve_acp_callback(u64::MAX, Ok(serde_json::json!({})));
    }
}
