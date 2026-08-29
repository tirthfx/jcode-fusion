//! Manual, human-readable demonstration of the `mission` tool (Fusion Phase 0,
//! first slice). Runs the exact same `MissionTool::execute` production code
//! path the agent calls during a real turn — no mocking. Doesn't require any
//! LLM provider credentials, since it drives the tool directly instead of
//! going through a live model conversation.
//!
//! Run with: `cargo run --example mission_tool_demo -p jcode-app-core`

use jcode_app_core::tool::{Tool, ToolContext, ToolExecutionMode};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Isolated, throwaway data dir for this demo run — never touches the
    // user's real ~/.jcode.
    let temp = tempfile::tempdir()?;
    let project = temp.path().join("demo-repo");
    std::fs::create_dir_all(&project)?;
    unsafe {
        std::env::set_var("JCODE_HOME", temp.path());
    }

    let tool = jcode_app_core::tool::mission::MissionTool::new();
    let ctx = ToolContext {
        session_id: "demo-session".to_string(),
        message_id: "demo-msg".to_string(),
        tool_call_id: "demo-tool-call".to_string(),
        working_dir: Some(project),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: ToolExecutionMode::AgentTurn,
    };

    println!("=== 1. show (no mission yet) ===");
    let out = tool.execute(json!({"action": "show"}), ctx.clone()).await?;
    println!("{}\n", out.output);

    println!(
        "=== 2. active_system_reminder before any mission exists (should be None) ==="
    );
    let reminder = jcode_app_core::mission::active_system_reminder(&ctx.session_id)?;
    println!("{:?}\n", reminder);

    println!("=== 3. set (\"Ship the Fusion mission tool\") ===");
    let out = tool
        .execute(
            json!({"action": "set", "objective": "Ship the Fusion mission tool"}),
            ctx.clone(),
        )
        .await?;
    println!("{}\n", out.output);

    println!("=== 4. active_system_reminder now that the mission is Active ===");
    let reminder = jcode_app_core::mission::active_system_reminder(&ctx.session_id)?;
    match &reminder {
        Some(text) => println!("(this is the real XML fragment injected into the next turn)\n{}\n", text),
        None => println!("None — unexpected!\n"),
    }

    println!("=== 5. checkpoint (\"Wrote the demo\") ===");
    let out = tool
        .execute(
            json!({"action": "checkpoint", "summary": "Wrote the demo"}),
            ctx.clone(),
        )
        .await?;
    println!("{}\n", out.output);

    println!("=== 6. status -> blocked ===");
    let out = tool
        .execute(json!({"action": "status", "status": "blocked"}), ctx.clone())
        .await?;
    println!("{}\n", out.output);

    println!("=== 7. active_system_reminder now that status is Blocked (should be None again) ===");
    let reminder = jcode_app_core::mission::active_system_reminder(&ctx.session_id)?;
    println!("{:?}\n", reminder);

    println!("=== 8. status -> not_a_real_status (should error) ===");
    match tool
        .execute(
            json!({"action": "status", "status": "not_a_real_status"}),
            ctx.clone(),
        )
        .await
    {
        Ok(_) => println!("UNEXPECTED: succeeded\n"),
        Err(e) => println!("Correctly rejected: {}\n", e),
    }

    println!("=== 9. clear ===");
    let out = tool.execute(json!({"action": "clear"}), ctx.clone()).await?;
    println!("{}\n", out.output);

    println!("=== 10. show after clear ===");
    let out = tool.execute(json!({"action": "show"}), ctx.clone()).await?;
    println!("{}\n", out.output);

    println!("All steps ran against the real MissionTool/mission.rs production code. No mocks.");
    Ok(())
}
