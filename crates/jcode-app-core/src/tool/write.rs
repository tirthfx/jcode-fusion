use super::{Tool, ToolContext, ToolOutput};
use crate::bus::{Bus, BusEvent, FileOp, FileTouch};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use std::path::Path;

const FILE_TOUCH_PREVIEW_MAX_LINES: usize = 6;
const FILE_TOUCH_PREVIEW_MAX_BYTES: usize = 240;

pub struct WriteTool;

impl WriteTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize)]
struct WriteInput {
    #[serde(default)]
    intent: Option<String>,
    file_path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write a file."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["file_path", "content"],
            "properties": {
                "intent": super::intent_schema_property(),
                "file_path": {
                    "type": "string",
                    "description": "File path."
                },
                "content": {
                    "type": "string",
                    "description": "File content."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: WriteInput = serde_json::from_value(input)?;

        let path = ctx.resolve_path(Path::new(&params.file_path));

        // Create parent directories if needed. Common to both paths below --
        // an empty directory's existence doesn't clash with any editor
        // buffer state an ACP host might be holding, unlike the file's own
        // content.
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Fusion Phase 3, ACP client-callback delegation, real wiring: when
        // this session is ACP-connected, route both the "read old content
        // for the diff" and the actual write through the ACP host
        // (`fs/read_text_file`/`fs/write_text_file`) instead of touching
        // the filesystem directly in-process. This is the whole point of
        // the relay, not an incidental detail: the host may have this
        // exact file open with unsaved editor changes, which a direct
        // `tokio::fs::write` would silently clobber and a direct
        // `tokio::fs::read_to_string` would silently miss (reading stale
        // on-disk content instead of the host's live buffer). A non-ACP
        // session's behavior below is completely unchanged -- same calls,
        // same order, same tolerance for a read failure meaning "treat as
        // a new file."
        let is_acp = crate::server::acp_callback::is_acp_session(&ctx.session_id).await;

        let (existed, old_content) = if is_acp {
            acp_read_existing_content(&ctx.session_id, &path).await
        } else {
            // Check if file existed before and read old content for diff
            let existed = path.exists();
            let old_content = if existed {
                tokio::fs::read_to_string(&path).await.ok()
            } else {
                None
            };
            (existed, old_content)
        };

        // Write the file
        if is_acp {
            // **Real bug fix, caught by a full-repo review**: `is_acp` was
            // checked once above, but the read callback and this write
            // callback each independently re-resolve the session's ACP
            // client -- if it disconnects in the window between the two
            // (e.g. an editor window closed mid-turn), this used to
            // propagate the failure via `?` and hard-fail the whole write,
            // even though a write that would have silently succeeded before
            // ACP relay existed at all (a plain local `tokio::fs::write`
            // doesn't care whether some remote client is still connected).
            // Falling back to a direct local write on a relay failure
            // means a disconnect mid-turn degrades to "the host's live
            // editor buffer wasn't updated, but the file on disk still is"
            // rather than losing the write outright.
            if let Err(err) = acp_write_content(&ctx.session_id, &path, &params.content).await {
                crate::logging::warn(&format!(
                    "WriteTool: ACP write callback failed for session '{}' ({err:#}), \
                     falling back to a direct local write",
                    ctx.session_id
                ));
                tokio::fs::write(&path, &params.content).await?;
            }
        } else {
            tokio::fs::write(&path, &params.content).await?;
        }

        let _new_len = params.content.len();
        let line_count = params.content.lines().count();
        let diff = if let Some(old) = old_content.as_deref() {
            generate_diff_summary(old, &params.content)
        } else {
            generate_diff_summary("", &params.content)
        };
        let detail = build_file_touch_preview(&diff);

        // Publish file touch event for swarm coordination
        Bus::global().publish(BusEvent::FileTouch(FileTouch {
            session_id: ctx.session_id.clone(),
            path: path.to_path_buf(),
            op: FileOp::Write,
            intent: params
                .intent
                .clone()
                .filter(|value| !value.trim().is_empty()),
            summary: Some(if existed {
                format!("overwrote file ({} lines)", line_count)
            } else {
                format!("created new file ({} lines)", line_count)
            }),
            detail,
        }));

        let mut body = if existed {
            format!(
                "Updated {} ({} lines){}\n{}",
                params.file_path,
                line_count,
                if diff.is_empty() { "" } else { ":" },
                diff
            )
        } else {
            // For new files, show all lines as additions
            let diff = generate_diff_summary("", &params.content);
            format!(
                "Created {} ({} lines):\n{}",
                params.file_path, line_count, diff
            )
        };

        // A write that lands on the active config.toml states exactly which
        // settings changed and whether they are live, so neither the agent nor
        // the user has to guess whether the edit took effect.
        super::config_edit_notice::append_config_edit_notice(
            &mut body,
            &path,
            old_content.as_deref().unwrap_or(""),
            &params.content,
        );

        Ok(ToolOutput::new(body).with_title(params.file_path.clone()))
    }
}

/// How long to wait for the ACP host to answer a file read/write callback.
/// Shorter than `session/request_permission`'s own 120s (`src/cli/acp.rs`)
/// on purpose -- a file operation shouldn't need a human in the loop, so a
/// slow answer here is much more likely a stuck host than someone thinking,
/// and callers (an agent turn waiting on this tool call) shouldn't be held
/// open for minutes over what's meant to be routine I/O.
const ACP_FILE_CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Read a file's current content via the ACP host's own `fs/read_text_file`
/// (Phase 3, client-callback delegation), rather than this process's own
/// filesystem view -- the host may hold this exact file open with unsaved
/// editor changes a direct disk read would miss entirely.
///
/// **Wire shape not yet verified against a real ACP host** -- this project's
/// standing biggest unverified assumption applies here specifically: no
/// live ACP client has exercised this path. `{"path", "sessionId"}` in,
/// `{"content"}` out is this implementation's best-effort reading of the
/// protocol, not confirmed interop.
///
/// Tolerant on any failure (missing file, host error, timeout) for
/// `old_content` -- same as the non-ACP path's own `.ok()` on
/// `read_to_string`, which also silently swallows every read failure (not
/// just "not found") down to "no old content available."
///
/// **Real bug fix, caught by a full-repo review**: `existed` used to
/// collapse to `false` on *any* callback failure too -- unlike the non-ACP
/// branch, where `existed` comes from `path.exists()` independently of
/// whether the subsequent content read succeeds, so a transient read
/// failure on a file that genuinely exists still correctly reports
/// `existed = true` there. A transient ACP callback failure (a timeout, a
/// momentary host error -- not necessarily "the host says this file
/// doesn't exist") was reporting `existed = false` regardless, so
/// `WriteTool` would claim "Created" instead of "Updated" and diff against
/// empty content instead of the real prior content for a file it just
/// couldn't get an answer about.
///
/// On a callback failure, `existed` now falls back to this process's own
/// `path.exists()` -- not authoritative for an ACP session (the host's
/// live editor buffer is still the real source of truth when reachable),
/// but a callback failure means that source of truth is unavailable right
/// now, and "what's actually on local disk" is a strictly more informed
/// guess than unconditionally assuming the file doesn't exist. `old_content`
/// stays `None` either way -- this fallback only restores accurate
/// existence for the "Updated"/"Created" label and the file-touch summary,
/// it does not fabricate content this process was never given.
async fn acp_read_existing_content(session_id: &str, path: &Path) -> (bool, Option<String>) {
    let params = json!({
        "path": path.display().to_string(),
        "sessionId": session_id,
    });
    let result = crate::server::acp_callback::send_acp_callback_for_session(
        session_id,
        "fs/read_text_file",
        params,
        ACP_FILE_CALLBACK_TIMEOUT,
    )
    .await;
    match result {
        Ok(value) => {
            let content = value
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string);
            (content.is_some(), content)
        }
        Err(_) => (path.exists(), None),
    }
}

/// Write `content` via the ACP host's own `fs/write_text_file`, so an open,
/// unsaved editor buffer for this file gets the host's own save semantics
/// rather than being silently overwritten by a direct disk write underneath
/// it. Same "wire shape not yet verified against a real host" caveat as
/// [`acp_read_existing_content`]. Unlike the read side, a write failure
/// propagates as a real tool error -- silently pretending a write succeeded
/// when it didn't would be a correctness problem, not just a missed
/// diff-preview nicety.
async fn acp_write_content(session_id: &str, path: &Path, content: &str) -> Result<()> {
    let params = json!({
        "path": path.display().to_string(),
        "content": content,
        "sessionId": session_id,
    });
    crate::server::acp_callback::send_acp_callback_for_session(
        session_id,
        "fs/write_text_file",
        params,
        ACP_FILE_CALLBACK_TIMEOUT,
    )
    .await?;
    Ok(())
}

/// Generate a compact diff: "42- old" / "42+ new" (max 20 lines)
fn generate_diff_summary(old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut output = String::new();
    let mut lines_shown = 0;
    const MAX_LINES: usize = 20;

    let mut old_line = 1usize;
    let mut new_line = 1usize;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                old_line += 1;
                new_line += 1;
                continue;
            }
            ChangeTag::Delete => {
                let content = change.value().trim();
                old_line += 1;
                if content.is_empty() {
                    continue;
                }
                if lines_shown >= MAX_LINES {
                    output.push_str("...\n");
                    break;
                }
                output.push_str(&format!("{}- {}\n", old_line - 1, content));
                lines_shown += 1;
            }
            ChangeTag::Insert => {
                let content = change.value().trim();
                new_line += 1;
                if content.is_empty() {
                    continue;
                }
                if lines_shown >= MAX_LINES {
                    output.push_str("...\n");
                    break;
                }
                output.push_str(&format!("{}+ {}\n", new_line - 1, content));
                lines_shown += 1;
            }
        }
    }

    output.trim_end().to_string()
}

fn build_file_touch_preview(diff: &str) -> Option<String> {
    let trimmed = diff.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut lines = trimmed.lines();
    let mut preview = lines
        .by_ref()
        .take(FILE_TOUCH_PREVIEW_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    let mut truncated = lines.next().is_some();

    if preview.len() > FILE_TOUCH_PREVIEW_MAX_BYTES {
        preview = crate::util::truncate_str(&preview, FILE_TOUCH_PREVIEW_MAX_BYTES)
            .trim_end()
            .to_string();
        truncated = true;
    }

    if truncated {
        preview.push_str("\n…");
    }

    Some(preview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolExecutionMode;

    #[test]
    fn test_generate_diff_summary_single_change() {
        let old = "hello world";
        let new = "hello rust";
        let diff = generate_diff_summary(old, new);

        // Compact format: "1- content" / "1+ content"
        assert!(diff.contains("1- hello world"), "Should show deleted line");
        assert!(diff.contains("1+ hello rust"), "Should show added line");
    }

    #[test]
    fn test_generate_diff_summary_multi_line() {
        let old = "line one\nline two\nline three";
        let new = "line one\nchanged two\nline three";
        let diff = generate_diff_summary(old, new);

        assert!(diff.contains("2- line two"), "Should show deleted line");
        assert!(diff.contains("2+ changed two"), "Should show added line");
        // Equal lines should not appear
        assert!(
            !diff.contains("line one"),
            "Should not show unchanged lines"
        );
    }

    #[test]
    fn test_generate_diff_summary_new_file() {
        let old = "";
        let new = "line one\nline two\nline three";
        let diff = generate_diff_summary(old, new);

        assert!(diff.contains("1+ line one"), "Should show line 1 added");
        assert!(diff.contains("2+ line two"), "Should show line 2 added");
        assert!(diff.contains("3+ line three"), "Should show line 3 added");
    }

    #[test]
    fn test_generate_diff_summary_truncation() {
        // Create old and new with more than 20 changed lines
        let old = (1..=25)
            .map(|i| format!("old line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (1..=25)
            .map(|i| format!("new line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = generate_diff_summary(&old, &new);

        assert!(diff.contains("..."), "Should truncate after 20 lines");
    }

    #[test]
    fn test_generate_diff_summary_line_number_format() {
        let old = "old";
        let new = "new";
        let diff = generate_diff_summary(old, new);

        // Compact format: no padding
        assert!(
            diff.contains("1- old"),
            "Should have line number directly before minus"
        );
        assert!(
            diff.contains("1+ new"),
            "Should have line number directly before plus"
        );
    }

    #[test]
    fn test_generate_diff_summary_empty_result() {
        let old = "same content";
        let new = "same content";
        let diff = generate_diff_summary(old, new);

        assert!(diff.is_empty(), "No changes should produce empty diff");
    }

    // --- ACP client-callback delegation (Phase 3, real wiring) ---

    fn make_ctx(session_id: &str, working_dir: std::path::PathBuf) -> ToolContext {
        ToolContext {
            session_id: session_id.to_string(),
            message_id: "test-message".to_string(),
            tool_call_id: "test-call".to_string(),
            working_dir: Some(working_dir),
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::Direct,
        }
    }

    #[tokio::test]
    async fn write_tool_routes_through_acp_callback_for_an_acp_connected_session() {
        // Proves the ACP-routed path actually goes through the relay end
        // to end, not just that the branch condition compiles: this
        // session never touches the real filesystem at all -- both the
        // pre-write read and the write itself are answered entirely via
        // simulated ACP host responses, and the test asserts the file
        // never actually landed on disk.
        let session_id = "acp-write-test-session";
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let members = crate::server::acp_callback::ensure_swarm_members_registered_for_test();
        members.write().await.insert(
            session_id.to_string(),
            crate::server::acp_callback::test_swarm_member(session_id, event_tx),
        );
        let connections = crate::server::acp_callback::ensure_client_connections_registered_for_test();
        connections.write().await.insert(
            session_id.to_string(),
            crate::server::acp_callback::test_acp_client_connection(session_id),
        );

        let temp = tempfile::tempdir().expect("tempdir");
        let ctx = make_ctx(session_id, temp.path().to_path_buf());
        let input = json!({
            "file_path": "new_file.txt",
            "content": "hello from acp\n",
        });

        let call = tokio::spawn(async move { WriteTool::new().execute(input, ctx).await });

        // First callback: the pre-write "read old content" check. Answer
        // with no `content` field -- the write-tool side treats that as
        // "doesn't exist", matching a brand-new file.
        let event = event_rx.recv().await.expect("read callback sent");
        let crate::protocol::ServerEvent::AcpCallbackRequest { id, method, params } = event else {
            panic!("expected AcpCallbackRequest, got {event:?}");
        };
        assert_eq!(method, "fs/read_text_file");
        assert!(params["path"].as_str().unwrap().ends_with("new_file.txt"));
        crate::server::acp_callback::resolve_acp_callback(id, session_id, Ok(json!({})));

        // Second callback: the actual write.
        let event = event_rx.recv().await.expect("write callback sent");
        let crate::protocol::ServerEvent::AcpCallbackRequest { id, method, params } = event else {
            panic!("expected AcpCallbackRequest, got {event:?}");
        };
        assert_eq!(method, "fs/write_text_file");
        assert_eq!(params["content"], "hello from acp\n");
        crate::server::acp_callback::resolve_acp_callback(id, session_id, Ok(json!({})));

        let output = call
            .await
            .expect("task")
            .expect("execute should succeed");
        assert!(output.output.contains("Created"));

        // The whole point: this must never have touched the real disk.
        assert!(
            !temp.path().join("new_file.txt").exists(),
            "the write must have gone through the ACP callback, not the local filesystem"
        );
    }

    #[tokio::test]
    async fn write_tool_falls_back_to_a_local_write_when_the_acp_write_callback_fails() {
        // Regression test for a real bug a full-repo review caught: `is_acp`
        // is checked once up front, but the read callback and the write
        // callback each independently re-resolve the session's ACP client.
        // If the client disconnects in the window between them (simulated
        // here by deregistering it right after answering the read
        // callback), the write callback used to fail and propagate via `?`,
        // hard-failing the whole tool call -- even though a plain local
        // write would have succeeded fine. This proves the write actually
        // lands on disk via the local fallback instead.
        let session_id = "acp-write-fallback-test-session";
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let members = crate::server::acp_callback::ensure_swarm_members_registered_for_test();
        members.write().await.insert(
            session_id.to_string(),
            crate::server::acp_callback::test_swarm_member(session_id, event_tx),
        );
        let connections = crate::server::acp_callback::ensure_client_connections_registered_for_test();
        connections.write().await.insert(
            session_id.to_string(),
            crate::server::acp_callback::test_acp_client_connection(session_id),
        );

        let temp = tempfile::tempdir().expect("tempdir");
        let ctx = make_ctx(session_id, temp.path().to_path_buf());
        let input = json!({
            "file_path": "fallback_file.txt",
            "content": "written after disconnect\n",
        });

        let call = tokio::spawn(async move { WriteTool::new().execute(input, ctx).await });

        // Answer the read callback normally...
        let event = event_rx.recv().await.expect("read callback sent");
        let crate::protocol::ServerEvent::AcpCallbackRequest { id, .. } = event else {
            panic!("expected AcpCallbackRequest, got {event:?}");
        };
        crate::server::acp_callback::resolve_acp_callback(id, session_id, Ok(json!({})));

        // ...then simulate the client disconnecting before the write
        // callback goes out: deregister it from both maps, exactly what
        // real disconnect cleanup does.
        members.write().await.remove(session_id);
        connections.write().await.remove(session_id);

        let output = call
            .await
            .expect("task")
            .expect("execute should succeed via the local fallback, not hard-fail");
        assert!(output.output.contains("Created"));

        let written = std::fs::read_to_string(temp.path().join("fallback_file.txt"))
            .expect("the fallback must have actually written the file to local disk");
        assert_eq!(written, "written after disconnect\n");
    }

    #[tokio::test]
    async fn write_tool_reports_updated_not_created_when_the_acp_read_callback_fails_for_an_existing_file()
     {
        // Regression test for a real bug a full-repo review caught:
        // `acp_read_existing_content` used to collapse *every* callback
        // failure (a transient timeout, a host error -- not just "the file
        // genuinely doesn't exist") into `existed = false`, unlike the
        // non-ACP path where `existed` comes from `path.exists()`
        // independently of whether the content read itself succeeds. A
        // callback failure on a file that actually exists (simulated here
        // by deregistering the ACP client before the read callback can be
        // answered, so the callback fails outright) must still report
        // "Updated", not falsely claim "Created".
        let session_id = "acp-read-failure-existing-file-session";
        // Deliberately register only `connections`, not `members` --
        // `is_acp_session` (checked once, up front) consults `connections`
        // only, so it still returns `true` and the ACP branch is taken; but
        // `send_acp_callback_for_session`'s own client-connection lookup
        // has nothing to relay the read callback through (no matching
        // `members` entry), so it fails fast with no event ever sent, the
        // same shape a real disconnected-but-not-yet-cleaned-up client
        // would produce.
        crate::server::acp_callback::ensure_swarm_members_registered_for_test();
        let connections = crate::server::acp_callback::ensure_client_connections_registered_for_test();
        connections.write().await.insert(
            session_id.to_string(),
            crate::server::acp_callback::test_acp_client_connection(session_id),
        );

        let temp = tempfile::tempdir().expect("tempdir");
        // A real file already on local disk -- the ACP callback can never
        // answer for it (nothing to relay through), so the only way
        // `existed` can come out `true` is the local `path.exists()`
        // fallback.
        std::fs::write(temp.path().join("existing.txt"), "stale disk content\n")
            .expect("seed local file");

        let ctx = make_ctx(session_id, temp.path().to_path_buf());
        let input = json!({
            "file_path": "existing.txt",
            "content": "new content\n",
        });

        let output = WriteTool::new()
            .execute(input, ctx)
            .await
            .expect("execute should succeed via the local write fallback");
        assert!(
            output.output.contains("Updated"),
            "must report Updated for a file that genuinely exists, not Created: {}",
            output.output
        );
    }

    #[tokio::test]
    async fn write_tool_still_writes_locally_for_a_non_acp_session() {
        // The other half of "provably unaffected": a session never marked
        // ACP must still behave exactly as before this slice -- a real
        // local write, no callback involved at all.
        let temp = tempfile::tempdir().expect("tempdir");
        let ctx = make_ctx("not-an-acp-session", temp.path().to_path_buf());
        let input = json!({
            "file_path": "plain_file.txt",
            "content": "hello from disk\n",
        });

        WriteTool::new()
            .execute(input, ctx)
            .await
            .expect("execute should succeed");

        let written = std::fs::read_to_string(temp.path().join("plain_file.txt"))
            .expect("file must exist locally");
        assert_eq!(written, "hello from disk\n");
    }
}
