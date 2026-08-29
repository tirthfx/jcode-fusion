use super::*;

fn test_ctx(session_id: &str, working_dir: &std::path::Path) -> ToolContext {
    ToolContext {
        session_id: session_id.to_string(),
        message_id: "msg1".to_string(),
        tool_call_id: "tool1".to_string(),
        working_dir: Some(working_dir.to_path_buf()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
    }
}

#[tokio::test]
async fn mission_tool_full_round_trip() {
    let _guard = crate::storage::lock_test_env();
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path().join("repo");
    std::fs::create_dir_all(&project).expect("project dir");
    let prev_home = std::env::var_os("JCODE_HOME");
    crate::env::set_var("JCODE_HOME", temp.path());

    let tool = MissionTool::new();
    let ctx = test_ctx("ses_mission_tool", &project);

    // No mission yet.
    let show_empty = tool
        .execute(json!({"action": "show"}), ctx.clone())
        .await
        .expect("show with no mission");
    assert!(show_empty.output.contains("No mission set"));
    assert!(
        crate::mission::active_system_reminder("ses_mission_tool")
            .expect("reminder lookup")
            .is_none(),
        "no reminder should be injected before a mission exists"
    );

    // set
    let set = tool
        .execute(
            json!({"action": "set", "objective": "Ship the mission tool"}),
            ctx.clone(),
        )
        .await
        .expect("set mission");
    assert!(set.output.contains("Ship the mission tool"));
    assert!(set.output.contains("active"));

    // A reminder should now be injected, since status is Active.
    let reminder = crate::mission::active_system_reminder("ses_mission_tool")
        .expect("reminder lookup")
        .expect("reminder should exist for an active mission");
    assert!(reminder.contains("Ship the mission tool"));

    // show reflects the set mission
    let show = tool
        .execute(json!({"action": "show"}), ctx.clone())
        .await
        .expect("show mission");
    assert!(show.output.contains("Ship the mission tool"));

    // checkpoint
    let checkpoint = tool
        .execute(
            json!({"action": "checkpoint", "summary": "Wrote the tool skeleton"}),
            ctx.clone(),
        )
        .await
        .expect("checkpoint");
    assert!(checkpoint.output.contains("Wrote the tool skeleton"));

    // status -> blocked should stop the continuation reminder
    let status = tool
        .execute(json!({"action": "status", "status": "blocked"}), ctx.clone())
        .await
        .expect("status update");
    assert!(status.output.contains("blocked"));
    assert!(
        crate::mission::active_system_reminder("ses_mission_tool")
            .expect("reminder lookup")
            .is_none(),
        "a blocked mission must not keep injecting the continuation reminder"
    );

    // invalid status is rejected
    let bad_status = tool
        .execute(
            json!({"action": "status", "status": "not_a_real_status"}),
            ctx.clone(),
        )
        .await;
    assert!(bad_status.is_err());

    // clear
    let clear = tool
        .execute(json!({"action": "clear"}), ctx.clone())
        .await
        .expect("clear mission");
    assert!(clear.output.contains("cleared"));

    let show_after_clear = tool
        .execute(json!({"action": "show"}), ctx.clone())
        .await
        .expect("show after clear");
    assert!(show_after_clear.output.contains("No mission set"));

    // checkpoint/status against a cleared (nonexistent) mission should fail cleanly
    let checkpoint_missing = tool
        .execute(
            json!({"action": "checkpoint", "summary": "should fail"}),
            ctx.clone(),
        )
        .await;
    assert!(checkpoint_missing.is_err());

    if let Some(prev_home) = prev_home {
        crate::env::set_var("JCODE_HOME", prev_home);
    } else {
        crate::env::remove_var("JCODE_HOME");
    }
}
