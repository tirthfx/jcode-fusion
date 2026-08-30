//! The destructive-command gate for the `bash` tool (issue #604).
//!
//! Kept in its own file so the policy seam is easy to find and review: this is
//! the only thing standing between a model's `rm -rf` and the user's data.
//!
//! Fusion Phase 1 layers `crate::execpolicy`'s user-configurable rules on
//! top here (DESIGN.md item #6) — see that module's doc comments for why it
//! lives alongside `jcode-command-risk` rather than inside it, and for the
//! "can only add restriction, never remove it" safety rule. Loaded once per
//! process (`OnceLock`) since bash is a hot path and re-parsing Starlark on
//! every command would be real, avoidable latency; a user editing their
//! policy file mid-session needs to restart jcode-fusion to pick it up —
//! documented here as a known limitation, not a silent gap.

use std::sync::OnceLock;

/// **Test limitation, documented not hidden**: this `OnceLock` is process-
/// global and initializes on first use, so it can't be cleanly reset between
/// `#[test]` functions that share the same test binary process — a test that
/// swaps `JCODE_HOME` to point at a custom policy file can't reliably prove
/// the cache actually picks it up if some *other* test already triggered
/// initialization first (Rust test execution order isn't controlled). The
/// pure logic underneath (`execpolicy::{load_user_rules, matching_rule,
/// combine}`) is fully unit-tested; this thin caching wrapper is not
/// independently integration-tested for that reason, not because it was
/// skipped.
static USER_RULES: OnceLock<Vec<crate::execpolicy::UserRule>> = OnceLock::new();

/// Gemini review, 2026-08-30: this used to be a plain `OnceLock::get_or_init`
/// call, which caches *whatever the closure returns* -- including an empty
/// `Vec` produced by a transient, non-malformed-policy load error (e.g. a
/// momentary I/O hiccup on first use). Because `OnceLock` only ever
/// initializes once, that permanently and silently disabled a perfectly
/// valid `execpolicy.star` for the rest of the process's lifetime, with no
/// retry. Restructured so only a *successful* load (including the normal
/// "no policy file at all" case, which `load_user_rules` itself already
/// treats as `Ok(vec![])`) ever populates the `OnceLock`; a real load error
/// leaves it unset, so the next bash command retries instead of being
/// permanently stuck. The "user needs to restart to pick up a mid-session
/// edit" limitation (see the module docs above) is unchanged and still
/// applies once a load has actually succeeded.
fn user_rules() -> &'static [crate::execpolicy::UserRule] {
    if let Some(rules) = USER_RULES.get() {
        return rules.as_slice();
    }
    let Ok(path) = crate::execpolicy::default_policy_path() else {
        return &[];
    };
    match crate::execpolicy::load_user_rules(&path) {
        Ok(rules) => {
            if !rules.is_empty() {
                crate::logging::info(&format!(
                    "[execpolicy] loaded {} user rule(s) from {}",
                    rules.len(),
                    path.display()
                ));
            }
            // set() can race with another thread also computing this on
            // first use; whichever wins is used -- both computed the same
            // real result from the same file, so it doesn't matter which.
            let _ = USER_RULES.set(rules);
            USER_RULES.get().map(Vec::as_slice).unwrap_or(&[])
        }
        Err(err) => {
            // Fail open: a broken policy file must not block bash
            // entirely. Same convention as pre_tool/sandbox_macos/
            // mission::supervisor_gate elsewhere in this project.
            // Deliberately NOT cached -- see doc comment above.
            crate::logging::warn(&format!(
                "[execpolicy] failed to load {}, continuing without user rules this time: {err}",
                path.display()
            ));
            &[]
        }
    }
}

/// Apply the deterministic destructive-command gate, returning refusal text
/// when the command must not run as-issued.
///
/// Stage 1 is a pure blast-radius assessment; stage 2 turns a `Confirm` verdict
/// into a reflection prompt that a blind retry cannot satisfy. Catastrophic
/// targets (`/`, `$HOME`, credential stores, device nodes) are denied outright.
/// See issue #604.
pub(super) fn destructive_command_refusal(
    command: &str,
    justification: Option<&str>,
    working_dir: Option<std::path::PathBuf>,
) -> Option<String> {
    // Gemini review, 2026-08-30: previously each branch below called
    // `check_user_policy_only` directly, so the "user rules can only
    // escalate, never downgrade a built-in restriction" invariant held only
    // by call-site discipline — a future refactor that reached
    // `check_user_policy_only` from the Deny/Reflect side (even by
    // accident) would have nothing structurally stopping it. Restructured
    // so the built-in verdict's own refusal (if any) is computed first as
    // a plain `Option<String>`, and the user-policy check only ever runs
    // via `.or_else()` — which the compiler guarantees only executes when
    // `built_in_refusal` is `None`. It is now impossible to reach
    // `check_user_policy_only` on a path that already produced a built-in
    // refusal, regardless of how this function gets refactored later.
    let risk_ctx = jcode_command_risk::RiskContext::from_env(working_dir);
    let assessment = jcode_command_risk::assess(command, &risk_ctx);

    let built_in_refusal: Option<String> = if assessment.level.runs_immediately() {
        None
    } else {
        let justification = jcode_command_risk::Justification {
            text: justification.map(str::to_string),
        };
        match jcode_command_risk::gate(&assessment, &justification) {
            jcode_command_risk::GateOutcome::Allow => None,
            jcode_command_risk::GateOutcome::Deny { reason } => {
                crate::logging::warn(&format!("[bash] denied destructive command: {command}"));
                Some(reason)
            }
            jcode_command_risk::GateOutcome::Reflect { prompt } => {
                crate::logging::info(&format!(
                    "[bash] destructive command held for justification: {command}"
                ));
                Some(prompt)
            }
        }
    };

    built_in_refusal.or_else(|| check_user_policy_only(command))
}

/// The built-in gate already said `Allow` — check whether a user execpolicy
/// rule wants to escalate this to `Confirm`/`Deny` anyway. Only ever called
/// when the built-in verdict was `Allow`, so `combine`'s "never loosens an
/// existing restriction" rule is naturally satisfied by construction (there
/// is no existing restriction to loosen at this call site).
fn check_user_policy_only(command: &str) -> Option<String> {
    let rules = user_rules();
    if rules.is_empty() {
        return None;
    }
    let matched = crate::execpolicy::matching_rule(command, rules)?;
    let verdict = crate::execpolicy::combine(
        crate::execpolicy::BuiltInRestrictiveness::Allow,
        Some(matched),
    )?;
    crate::logging::info(&format!(
        "[execpolicy] user rule matched, escalating: prefix={:?} decision={:?} command={command}",
        verdict.prefix, verdict.decision
    ));
    Some(format!(
        "Blocked by execpolicy rule (prefix {:?}): {}",
        verdict.prefix, verdict.reason
    ))
}

/// The `bash` tool's JSON schema, including the `justification` field the
/// destructive-command gate consumes.
///
/// Lives beside the gate so the schema and the policy that reads it stay in
/// sync, and so bash.rs stays inside the code-size budget.
pub(super) fn bash_parameters_schema() -> serde_json::Value {
    let cmd_desc = if cfg!(windows) {
        "The Windows command to execute via cmd.exe. Use cmd.exe syntax and quoting, not Bash syntax."
    } else {
        "The bash command to execute. Put large temp files under `$JCODE_SCRATCH_DIR`, not `/tmp`."
    };
    serde_json::json!({
        "type": "object",
        "required": ["command"],
        "properties": {
            "intent": crate::tool::intent_schema_property(),
            "command": {
                "type": "string",
                "description": cmd_desc
            },
            "timeout": {
                "type": "integer",
                "description": "Timeout in MILLISECONDS (not seconds), e.g. 600000 = 10min; kills with exit 124. Omit for no timeout."
            },
            "run_in_background": {
                "type": "boolean",
                "description": "Run in background. Emit `JCODE_PROGRESS {json}` lines for progress reporting."
            },
            "notify": {
                "type": "boolean",
                "description": "Notify on completion."
            },
            "wake": {
                "type": "boolean",
                "description": "Wake on completion."
            },
            "stall_wake_seconds": {
                "type": "integer",
                "description": "With run_in_background: wake the agent after this many seconds of no output/progress (min 30, resets on activity). Use for long jobs that may hang silently."
            },
            "justification": {
                "type": "string",
                "description": "Only when re-issuing a command the destructive gate refused; explain which user request it serves."
            }
        }
    })
}
