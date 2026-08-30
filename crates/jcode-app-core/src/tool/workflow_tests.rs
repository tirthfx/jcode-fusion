use super::*;

fn test_ctx(session_id: &str) -> ToolContext {
    ToolContext {
        session_id: session_id.to_string(),
        message_id: "msg1".to_string(),
        tool_call_id: "tool1".to_string(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    }
}

fn sample_nodes() -> Value {
    json!([
        {"id": "review", "content": "Review {{subsystem}}", "kind": "critique", "priority": 0},
        {"id": "fix", "content": "Fix {{subsystem}}", "kind": "fix", "priority": 1, "depends_on": ["review"]}
    ])
}

#[tokio::test]
async fn save_and_list_round_trip() {
    let _guard = crate::storage::lock_test_env();
    let jcode_home = tempfile::tempdir().expect("tempdir");
    crate::env::set_var("JCODE_HOME", jcode_home.path());

    let tool = WorkflowTool::new();
    let ctx = test_ctx("ses_workflow_tool");

    let empty_list = tool
        .execute(json!({"action": "list"}), ctx.clone())
        .await
        .expect("list before any save");
    assert!(empty_list.output.contains("No saved workflow templates"));

    let save = tool
        .execute(
            json!({
                "action": "save",
                "name": "review-and-fix",
                "description": "Review then fix.",
                "parameters": [
                    {"name": "subsystem", "description": "target", "default": null}
                ],
                "nodes": sample_nodes(),
            }),
            ctx.clone(),
        )
        .await
        .expect("save");
    assert!(save.output.contains("review-and-fix"));
    assert!(save.output.contains("2 node"));

    let list = tool
        .execute(json!({"action": "list"}), ctx.clone())
        .await
        .expect("list after save");
    assert!(list.output.contains("review-and-fix"));

    // `list`'s output only confirms the *name* made it to disk -- load the
    // template directly (through the tool's own persistence layer, not a
    // second write) to confirm description/parameters/nodes actually
    // round-tripped too, not just the name.
    let loaded = crate::workflow_template::load("review-and-fix").expect("load");
    assert_eq!(loaded.description, "Review then fix.");
    assert_eq!(loaded.parameters.len(), 1);
    assert_eq!(loaded.parameters[0].name, "subsystem");
    assert_eq!(loaded.nodes.len(), 2);
}

#[tokio::test]
async fn save_rejects_an_empty_nodes_array() {
    let _guard = crate::storage::lock_test_env();
    let jcode_home = tempfile::tempdir().expect("tempdir");
    crate::env::set_var("JCODE_HOME", jcode_home.path());

    let tool = WorkflowTool::new();
    let ctx = test_ctx("ses_workflow_tool");

    let result = tool
        .execute(
            json!({"action": "save", "name": "empty", "nodes": []}),
            ctx.clone(),
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn save_requires_name_and_nodes() {
    let _guard = crate::storage::lock_test_env();
    let jcode_home = tempfile::tempdir().expect("tempdir");
    crate::env::set_var("JCODE_HOME", jcode_home.path());

    let tool = WorkflowTool::new();
    let ctx = test_ctx("ses_workflow_tool");

    let missing_name = tool
        .execute(json!({"action": "save", "nodes": sample_nodes()}), ctx.clone())
        .await;
    assert!(missing_name.is_err());

    let missing_nodes = tool
        .execute(json!({"action": "save", "name": "x"}), ctx.clone())
        .await;
    assert!(missing_nodes.is_err());
}

#[tokio::test]
async fn save_rejects_an_invalid_template() {
    let _guard = crate::storage::lock_test_env();
    let jcode_home = tempfile::tempdir().expect("tempdir");
    crate::env::set_var("JCODE_HOME", jcode_home.path());

    let tool = WorkflowTool::new();
    let ctx = test_ctx("ses_workflow_tool");

    // A node depends_on a node id that doesn't exist -- validate() must
    // catch this at save time, through the tool, not just in the module's
    // own unit tests.
    let result = tool
        .execute(
            json!({
                "action": "save",
                "name": "broken",
                "nodes": [
                    {"id": "a", "content": "do a", "depends_on": ["does-not-exist"]}
                ],
            }),
            ctx.clone(),
        )
        .await;
    assert!(result.is_err());

    let list = tool
        .execute(json!({"action": "list"}), ctx.clone())
        .await
        .expect("list");
    assert!(
        list.output.contains("No saved workflow templates"),
        "an invalid template must never be persisted, got: {}",
        list.output
    );
}

#[tokio::test]
async fn run_fails_cleanly_for_an_unknown_template_before_touching_the_daemon() {
    let _guard = crate::storage::lock_test_env();
    let jcode_home = tempfile::tempdir().expect("tempdir");
    crate::env::set_var("JCODE_HOME", jcode_home.path());

    let tool = WorkflowTool::new();
    let ctx = test_ctx("ses_workflow_tool");

    // No daemon is running in this test process at all -- if `run` reached
    // for `send_request` before `load()` failed, this would hang or error
    // on a socket-connect instead of a clean "no such template" message.
    let result = tool
        .execute(
            json!({"action": "run", "name": "never-saved", "values": {}}),
            ctx.clone(),
        )
        .await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("never-saved"), "got: {err}");
}

#[tokio::test]
async fn run_fails_cleanly_when_a_required_parameter_is_missing_before_touching_the_daemon() {
    let _guard = crate::storage::lock_test_env();
    let jcode_home = tempfile::tempdir().expect("tempdir");
    crate::env::set_var("JCODE_HOME", jcode_home.path());

    let tool = WorkflowTool::new();
    let ctx = test_ctx("ses_workflow_tool");

    tool.execute(
        json!({
            "action": "save",
            "name": "needs-param",
            "parameters": [{"name": "subsystem", "default": null}],
            "nodes": sample_nodes(),
        }),
        ctx.clone(),
    )
    .await
    .expect("save");

    // `values` deliberately omits `subsystem` -- instantiate() must refuse
    // before `run` ever reaches for the daemon socket.
    let result = tool
        .execute(
            json!({"action": "run", "name": "needs-param", "values": {}}),
            ctx.clone(),
        )
        .await;
    let err = result.unwrap_err().to_string();
    assert!(err.contains("subsystem"), "got: {err}");
}

#[test]
fn schema_lists_all_three_actions() {
    let tool = WorkflowTool::new();
    let schema = tool.parameters_schema();
    let actions = schema["properties"]["action"]["enum"]
        .as_array()
        .expect("action enum");
    let names: Vec<&str> = actions.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(names, vec!["save", "list", "run"]);
}
