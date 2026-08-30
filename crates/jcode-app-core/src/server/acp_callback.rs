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
//! **Deliberately just this half of the plumbing, same "smallest coherent
//! slice" shape as the adapter-side one**: `send_acp_callback` has **no
//! real caller yet**. The daemon-side tools that would actually need this
//! (`WriteTool`/`ReadTool`, today calling `tokio::fs::write`/`read`
//! directly, in-process, unconditionally) aren't touched here -- routing
//! their I/O through this callback only when a session is ACP-connected is
//! real, separate, higher-blast-radius work (those tools run for every
//! session type, TUI and headless included; a wiring mistake there risks
//! regressing all of them, not just ACP ones). This module exists so that
//! future wiring has a already-built, already-tested primitive to call,
//! rather than needing to invent the callback mechanism itself under time
//! pressure once someone actually starts that riskier change.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{RwLock, oneshot};

use crate::protocol::ServerEvent;

use super::SwarmMember;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;
    use tokio::sync::mpsc;

    fn test_member(event_tx: mpsc::UnboundedSender<ServerEvent>) -> SwarmMember {
        SwarmMember {
            session_id: "sess-1".to_string(),
            event_tx,
            event_txs: StdHashMap::new(),
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
