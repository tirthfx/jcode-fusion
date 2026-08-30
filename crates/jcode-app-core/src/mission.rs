use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::prompt::MISSION_CONTINUATION_TEMPLATE;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MissionStatus {
    Active,
    Paused,
    Blocked,
    NeedsDecision,
    BudgetLimited,
    Complete,
    Abandoned,
}

impl MissionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::NeedsDecision => "needs_decision",
            Self::BudgetLimited => "budget_limited",
            Self::Complete => "complete",
            Self::Abandoned => "abandoned",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "active" => Some(Self::Active),
            "paused" => Some(Self::Paused),
            "blocked" => Some(Self::Blocked),
            "needs_decision" | "needs-decision" => Some(Self::NeedsDecision),
            "budget_limited" | "budget-limited" => Some(Self::BudgetLimited),
            "complete" | "completed" => Some(Self::Complete),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MissionCheckpoint {
    pub at: DateTime<Utc>,
    pub summary: String,
}

/// A self-reported claim that the mission is complete, pending independent
/// verification. Recording a claim does NOT change `status` — see
/// [`claim_complete`] and [`verify_completion`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletionClaim {
    pub evidence: Vec<String>,
    pub claimed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mission {
    pub session_id: String,
    pub objective: String,
    pub long_horizon_intent: String,
    pub status: MissionStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub semantic_expansion: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub success_criteria: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_plan: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<MissionCheckpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_completion_claim: Option<CompletionClaim>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub fn load(session_id: &str) -> Result<Option<Mission>> {
    let path = mission_path(session_id)?;
    if !path.exists() {
        return Ok(None);
    }
    crate::storage::read_json(&path)
}

pub fn set(session_id: &str, objective: &str) -> Result<Mission> {
    let objective = objective.trim();
    if objective.is_empty() {
        anyhow::bail!("mission objective cannot be empty");
    }
    let _guard = lock_mission_store();
    let now = Utc::now();
    let mut mission = load(session_id)?.unwrap_or_else(|| Mission {
        session_id: session_id.to_string(),
        objective: String::new(),
        long_horizon_intent: String::new(),
        status: MissionStatus::Active,
        semantic_expansion: Vec::new(),
        success_criteria: Vec::new(),
        validation_plan: Vec::new(),
        checkpoints: Vec::new(),
        pending_completion_claim: None,
        created_at: now,
        updated_at: now,
    });
    mission.objective = objective.to_string();
    mission.long_horizon_intent = default_long_horizon_intent(objective);
    mission.status = MissionStatus::Active;
    mission.updated_at = now;
    save(&mission)?;
    Ok(mission)
}

/// Declare (or replace) the mission's success criteria — the contract that
/// [`verify_completion`] checks a completion claim against. Without this,
/// there is nothing for completion verification to verify against, so a
/// mission with no criteria can never pass `verify_completion` (see
/// `claim_meets_bar`).
pub fn set_success_criteria(session_id: &str, criteria: Vec<String>) -> Result<Option<Mission>> {
    let criteria: Vec<String> = criteria
        .into_iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    let _guard = lock_mission_store();
    let Some(mut mission) = load(session_id)? else {
        return Ok(None);
    };
    mission.success_criteria = criteria;
    mission.updated_at = Utc::now();
    save(&mission)?;
    Ok(Some(mission))
}

/// Update mission status. **Cannot be used to reach `Complete`** — that
/// transition must go through [`claim_complete`] + [`verify_completion`]
/// (Fusion Phase 0, third slice: completion verification). Direct
/// self-certified completion is exactly the gap this project set out to
/// close (see jcode-fusion/DESIGN.md §6 item #1, Grok Build's `/goal`
/// adversarial-verifier idea) — allowing `update_status(.., Complete)` to
/// work would silently defeat the whole feature.
pub fn update_status(session_id: &str, status: MissionStatus) -> Result<Option<Mission>> {
    if status == MissionStatus::Complete {
        anyhow::bail!(
            "cannot set status to complete directly; use claim_complete (with evidence) \
             followed by verify_completion instead — completion must be claimed with \
             evidence and independently verified, not self-certified"
        );
    }
    let _guard = lock_mission_store();
    let Some(mut mission) = load(session_id)? else {
        return Ok(None);
    };
    mission.status = status;
    mission.updated_at = Utc::now();
    save(&mission)?;
    Ok(Some(mission))
}

/// Minimum length, after trimming, for a single piece of completion
/// evidence to be considered substantive. Mirrors the anti-rubber-stamp
/// pattern already used elsewhere in jcode for exactly this purpose (see
/// `jcode-command-risk`'s `Justification::is_substantive()`, a ~25-char
/// minimum for destructive-command justifications) rather than inventing a
/// new convention.
const MIN_EVIDENCE_LEN: usize = 20;

fn evidence_is_substantive(item: &str) -> bool {
    let trimmed = item.trim();
    if trimmed.chars().count() < MIN_EVIDENCE_LEN {
        return false;
    }
    const BARE_AFFIRMATIONS: &[&str] = &[
        "done",
        "yes",
        "complete",
        "completed",
        "finished",
        "ok",
        "okay",
        "it works",
        "all good",
        "should be fine",
    ];
    let lower = trimmed.to_ascii_lowercase();
    !BARE_AFFIRMATIONS.contains(&lower.as_str())
}

/// Step 1 of completion verification: record a self-reported claim that the
/// mission is complete, with evidence. **Does not change `status`** — the
/// mission stays wherever it was (typically `Active`) until
/// [`verify_completion`] actually confirms it. Each evidence item must be
/// substantive (see [`evidence_is_substantive`]); bare affirmations like
/// "done" are rejected outright rather than silently accepted.
pub fn claim_complete(session_id: &str, evidence: Vec<String>) -> Result<Option<Mission>> {
    if evidence.is_empty() {
        anyhow::bail!("completion claim requires at least one piece of evidence");
    }
    if let Some(weak) = evidence.iter().find(|e| !evidence_is_substantive(e)) {
        anyhow::bail!(
            "evidence item is not substantive enough (must be a real, specific claim, \
             not a bare affirmation): \"{}\"",
            weak.trim()
        );
    }
    let _guard = lock_mission_store();
    let Some(mut mission) = load(session_id)? else {
        return Ok(None);
    };
    mission.pending_completion_claim = Some(CompletionClaim {
        evidence,
        claimed_at: Utc::now(),
    });
    mission.updated_at = Utc::now();
    save(&mission)?;
    Ok(Some(mission))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationOutcome {
    /// No completion claim is currently pending for this mission.
    NoPendingClaim,
    /// The claim was refuted — the mission stays exactly as it was (still
    /// carrying the pending claim, still not `Complete`).
    Refuted { reason: String },
    /// The claim was confirmed — the mission has been transitioned to
    /// `Complete` and the pending claim cleared.
    Confirmed { mission: Mission },
}

/// Step 2 of completion verification: independently assess a pending
/// completion claim and, only if it holds up, actually transition the
/// mission to `Complete`.
///
/// **Important limitation, documented deliberately rather than silently
/// shipped as if solved**: this first slice's verification is a real,
/// enforced, code-level structural check (does the evidence plausibly cover
/// the declared success criteria?) — not yet a genuinely independent LLM
/// review of the actual evidence's truth. A real semantic verifier (e.g.
/// spawning a fresh, separate `Agent` via `Agent::run_once_capture`, the
/// same primitive `overnight.rs::run_supervisor` already uses, with a
/// tightly-scoped "try to refute this claim" prompt) is the natural next
/// step once this scaffold is in place — see PROGRESS.md. This function is
/// also, today, callable by the same session/turn that filed the claim;
/// nothing yet enforces that a *different* identity must call it. Both
/// gaps are intentional scope boundaries for this slice, not oversights.
pub fn verify_completion(session_id: &str) -> Result<VerificationOutcome> {
    let _guard = lock_mission_store();
    let Some(mission) = load(session_id)? else {
        return Ok(VerificationOutcome::NoPendingClaim);
    };
    let Some(claim) = mission.pending_completion_claim.clone() else {
        return Ok(VerificationOutcome::NoPendingClaim);
    };
    if mission.success_criteria.is_empty() {
        return Ok(VerificationOutcome::Refuted {
            reason: "mission has no success criteria set (use the `success_criteria` action) \
                      — completion cannot be verified against nothing"
                .to_string(),
        });
    }
    if claim.evidence.len() < mission.success_criteria.len() {
        return Ok(VerificationOutcome::Refuted {
            reason: format!(
                "{} success criteria declared but only {} evidence item(s) provided — \
                 not every criterion appears to be addressed",
                mission.success_criteria.len(),
                claim.evidence.len()
            ),
        });
    }
    let mut confirmed = mission;
    confirmed.status = MissionStatus::Complete;
    confirmed.pending_completion_claim = None;
    confirmed.updated_at = Utc::now();
    save(&confirmed)?;
    Ok(VerificationOutcome::Confirmed { mission: confirmed })
}

pub fn checkpoint(session_id: &str, summary: &str) -> Result<Option<Mission>> {
    let summary = summary.trim();
    if summary.is_empty() {
        anyhow::bail!("checkpoint summary cannot be empty");
    }
    let _guard = lock_mission_store();
    let Some(mut mission) = load(session_id)? else {
        return Ok(None);
    };
    mission.checkpoints.push(MissionCheckpoint {
        at: Utc::now(),
        summary: summary.to_string(),
    });
    // Gemini review, 2026-08-30: cap rather than let this grow unbounded --
    // see MAX_CHECKPOINTS's own doc comment.
    if mission.checkpoints.len() > MAX_CHECKPOINTS {
        let overflow = mission.checkpoints.len() - MAX_CHECKPOINTS;
        mission.checkpoints.drain(0..overflow);
    }
    mission.updated_at = Utc::now();
    save(&mission)?;
    Ok(Some(mission))
}

/// Real (not heuristic) budget enforcement — Fusion Phase 0, second slice.
///
/// Checks actual provider usage (`crate::usage::fetch_all_provider_usage`,
/// which hits real provider APIs, unlike Ambient Mode's dead local
/// `UsageLog`/`AdaptiveScheduler` or Overnight's advisory-only one-shot
/// projection — see jcode-fusion/DESIGN.md §6 item #1) and, if any connected
/// provider has actually hit its hard usage limit, transitions the mission
/// to `BudgetLimited` and persists it. This is a genuine halt: once
/// `BudgetLimited`, `active_system_reminder` stops injecting the
/// continuation prompt (same mechanism already covered by
/// `tool/mission_tests.rs`'s Blocked-status case).
///
/// Known, deliberate simplification for this first slice: this checks
/// whether *any* connected provider is hard-limited, not specifically the
/// provider the current session is actually using — mission.rs is
/// provider-agnostic and doesn't currently have a session→active-provider
/// lookup available to it. Refine to session-specific scoping in a later
/// pass rather than blocking this slice on it; documented here so it isn't
/// mistaken for an oversight.
///
/// Bounded by [`ENFORCE_BUDGET_USAGE_FETCH_TIMEOUT`] (Gemini review,
/// 2026-08-30): this is called once per turn from `supervisor_gate`, which
/// itself runs at the top of `overnight.rs::run_supervisor`'s loop before
/// every coordinator turn — a hung or slow provider-usage endpoint here
/// previously had no timeout at all and would have stalled the entire
/// overnight run indefinitely, with no way to recover. The timeout lives at
/// this call site, not inside `crate::usage::fetch_all_provider_usage`
/// itself, since that's jcode's own shared upstream function used by other
/// callers (e.g. Overnight's own usage projection) this project doesn't own
/// and shouldn't change the blocking behavior of for everyone.
pub async fn enforce_budget(session_id: &str) -> Result<Option<Mission>> {
    let Some(mission) = load(session_id)? else {
        return Ok(None);
    };
    if !matches!(mission.status, MissionStatus::Active) {
        return Ok(Some(mission));
    }
    let reports = match tokio::time::timeout(
        ENFORCE_BUDGET_USAGE_FETCH_TIMEOUT,
        crate::usage::fetch_all_provider_usage(),
    )
    .await
    {
        Ok(reports) => reports,
        Err(_) => {
            // Fail open: same convention as `pre_tool`/`sandbox_macos`/this
            // module's own `supervisor_gate` elsewhere — a slow usage check
            // must not itself become the reason a mission can never make
            // progress. This turn's budget check is simply skipped; the
            // next turn tries again.
            crate::logging::warn(&format!(
                "[mission] enforce_budget: fetch_all_provider_usage timed out after {:?} for \
                 session {session_id}, skipping budget check this turn",
                ENFORCE_BUDGET_USAGE_FETCH_TIMEOUT
            ));
            return Ok(Some(mission));
        }
    };
    if any_provider_hard_limited(&reports) {
        return update_status(session_id, MissionStatus::BudgetLimited);
    }
    Ok(Some(mission))
}

/// Generous enough for a real multi-provider usage fetch (network calls
/// across however many accounts are connected) but bounded, so a hung
/// endpoint can't stall the overnight supervisor loop indefinitely.
const ENFORCE_BUDGET_USAGE_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Pure, network-free decision logic split out from [`enforce_budget`] so it
/// can be unit-tested directly against constructed `ProviderUsage` values
/// instead of needing real provider credentials/network access.
fn any_provider_hard_limited(reports: &[crate::usage::ProviderUsage]) -> bool {
    reports.iter().any(|report| report.hard_limit_reached)
}

/// Fusion Phase 0, fourth slice: the outer supervisor gate.
///
/// This is what actually ties budget enforcement + completion verification +
/// the continuation-reminder mechanism together into something that can run
/// unattended — the piece DESIGN.md describes as "a driver loop modeled on
/// `overnight.rs::run_supervisor`". Rather than duplicating `run_supervisor`'s
/// substantial `Agent`/`Session`/`Provider` construction machinery in a
/// parallel implementation, this is designed to be called **from inside**
/// `run_supervisor`'s existing loop, once per turn — see the call site added
/// to `overnight.rs`.
///
/// Returns `Ok(Some(reason))` if the caller should stop the supervisor loop
/// because of the mission's state (budget exhausted, or completion verified);
/// `Ok(None)` if the loop should keep going. A session with no mission at all
/// always returns `Ok(None)` — Mission Engine is opt-in, not a behavior
/// change for Overnight runs that don't use it.
///
/// Note on completion: a *refuted* claim does **not** stop the loop — the
/// mission stays `Active` with the claim still pending, and the agent is
/// expected to keep working and can re-claim later. Only a *confirmed*
/// claim stops it.
pub async fn supervisor_gate(session_id: &str) -> Result<Option<String>> {
    let Some(mission) = load(session_id)? else {
        return Ok(None);
    };

    if mission.status == MissionStatus::BudgetLimited {
        return Ok(Some(
            "mission is budget_limited: a connected provider was already hard-limited"
                .to_string(),
        ));
    }

    if mission.status != MissionStatus::Active {
        // Paused/Blocked/NeedsDecision/Abandoned/Complete: not this
        // function's job to decide whether that should stop the loop —
        // Complete in particular is already a natural stop condition the
        // caller can check via `mission.status` directly without going
        // through this gate at all.
        return Ok(None);
    }

    // Gemini review, 2026-08-30: this used to be `enforce_budget(...).await?`
    // -- any transient error from the budget check (a disk I/O hiccup
    // reading/writing mission.json, not just the timeout case
    // `enforce_budget` itself already fails open on) made the whole
    // function return `Err` before ever reaching the completion-claim
    // check below, even though "fail open" was meant to apply to the
    // budget check specifically, not to skip verification entirely. A
    // mission with a claim that would have verified successfully this
    // turn could silently miss that, with the caller's own fail-open
    // handling just logging a warning and running another full coordinator
    // turn instead.
    match enforce_budget(session_id).await {
        Ok(Some(updated)) if updated.status == MissionStatus::BudgetLimited => {
            return Ok(Some(
                "mission transitioned to budget_limited: a connected provider is hard-limited"
                    .to_string(),
            ));
        }
        Ok(_) => {}
        Err(err) => {
            crate::logging::warn(&format!(
                "[mission] supervisor_gate: enforce_budget failed for session {session_id}, \
                 continuing to check completion without a budget verdict this turn: {err}"
            ));
        }
    }

    if mission.pending_completion_claim.is_some() {
        match verify_completion(session_id)? {
            VerificationOutcome::Confirmed { .. } => {
                return Ok(Some(
                    "mission completion claim verified — objective achieved".to_string(),
                ));
            }
            VerificationOutcome::Refuted { reason } => {
                crate::logging::info(&format!(
                    "[mission] completion claim for session {session_id} refuted, \
                     continuing: {reason}"
                ));
            }
            VerificationOutcome::NoPendingClaim => {}
        }
    }

    Ok(None)
}

pub fn clear(session_id: &str) -> Result<bool> {
    let path = mission_path(session_id)?;
    if path.exists() {
        std::fs::remove_file(path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn render_status(mission: &Mission) -> String {
    let mut out = format!(
        "Mission **{}**\n\nStatus: **{}**\n\nLong-horizon intent: {}",
        mission.objective,
        mission.status.as_str(),
        mission.long_horizon_intent
    );
    if let Some(last) = mission.checkpoints.last() {
        out.push_str(&format!("\n\nLast checkpoint: {}", last.summary));
    }
    out.push_str("\n\nMission loop: keep updating todos, expand adjacent work, validate progress, and continue until complete, blocked, paused, or a decision is needed.");
    out
}

pub fn active_system_reminder(session_id: &str) -> Result<Option<String>> {
    let Some(mission) = load(session_id)? else {
        return Ok(None);
    };
    if !matches!(mission.status, MissionStatus::Active) {
        return Ok(None);
    }
    Ok(Some(render_mission_continuation_prompt(&mission)))
}

pub fn render_mission_continuation_prompt(mission: &Mission) -> String {
    MISSION_CONTINUATION_TEMPLATE
        .replace("{{ objective }}", &escape_xml_text(&mission.objective))
        .replace(
            "{{ long_horizon_intent }}",
            &escape_xml_text(&mission.long_horizon_intent),
        )
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Serializes every load -> mutate -> save mutation across every mission.
/// Gemini review, 2026-08-30: `set`/`set_success_criteria`/`update_status`/
/// `claim_complete`/`checkpoint`/`verify_completion` each do an unguarded
/// load-mutate-save round trip to a plain JSON file, so two concurrent
/// invocations for the same `session_id` (parallel subagents, or the
/// agent's own tool call racing with `supervisor_gate`'s periodic checks)
/// could silently lose one write. Same fix, same simplicity tradeoff
/// (process-wide, not per-`session_id`) already applied to
/// `rewind_store.rs`'s equivalent race.
static MISSION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_mission_store() -> std::sync::MutexGuard<'static, ()> {
    MISSION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Checkpoints are an append-only log with no natural cap of their own
/// (Gemini review, 2026-08-30: an accidental or adversarial retry loop
/// hammering the `checkpoint` tool action could otherwise grow
/// `mission.json` without bound, since every load/save round-trips the
/// whole file). Keeps the most recent entries, drops the oldest — a
/// checkpoint log is a rolling window for context, not an audit trail that
/// needs every entry preserved forever.
const MAX_CHECKPOINTS: usize = 200;

fn save(mission: &Mission) -> Result<()> {
    crate::storage::write_json_fast(&mission_path(&mission.session_id)?, mission)
}

fn mission_path(session_id: &str) -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?
        .join("missions")
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

fn default_long_horizon_intent(objective: &str) -> String {
    format!(
        "Interpret `{}` broadly: pursue the literal objective, continuously refresh the todo frontier, include semantically adjacent work that improves the outcome, and preserve long-term quality.",
        objective
    )
}

#[cfg(test)]
mod completion_verification_tests {
    use super::*;

    #[test]
    fn bare_affirmations_are_not_substantive() {
        for weak in ["done", "Yes", "COMPLETE", "  ok  ", "it works", "all good"] {
            assert!(
                !evidence_is_substantive(weak),
                "expected {:?} to be rejected as not substantive",
                weak
            );
        }
    }

    #[test]
    fn short_strings_are_not_substantive() {
        assert!(!evidence_is_substantive("ran the tests ok"));
    }

    #[test]
    fn real_evidence_is_substantive() {
        assert!(evidence_is_substantive(
            "Ran `cargo test -p jcode-app-core mission`, all 19 tests pass"
        ));
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
    fn claim_complete_rejects_empty_and_weak_evidence() {
        with_isolated_home(|| {
            let session_id = "ses_claim_reject";
            set(session_id, "Ship the verifier").expect("set mission");

            assert!(claim_complete(session_id, vec![]).is_err());
            assert!(claim_complete(session_id, vec!["done".to_string()]).is_err());

            // A weak claim must not have mutated the mission.
            let mission = load(session_id).expect("load").expect("exists");
            assert!(mission.pending_completion_claim.is_none());
        });
    }

    #[test]
    fn update_status_cannot_reach_complete_directly() {
        with_isolated_home(|| {
            let session_id = "ses_no_direct_complete";
            set(session_id, "Ship the verifier").expect("set mission");
            assert!(update_status(session_id, MissionStatus::Complete).is_err());
            let mission = load(session_id).expect("load").expect("exists");
            assert_eq!(mission.status, MissionStatus::Active);
        });
    }

    #[test]
    fn verify_completion_refutes_without_success_criteria() {
        with_isolated_home(|| {
            let session_id = "ses_no_criteria";
            set(session_id, "Ship the verifier").expect("set mission");
            claim_complete(
                session_id,
                vec!["Ran the full test suite and everything passed cleanly".to_string()],
            )
            .expect("claim");

            let outcome = verify_completion(session_id).expect("verify");
            match outcome {
                VerificationOutcome::Refuted { reason } => {
                    assert!(reason.contains("no success criteria"));
                }
                other => panic!("expected Refuted, got {:?}", other),
            }
            // Refuted claims stay pending, not silently dropped.
            let mission = load(session_id).expect("load").expect("exists");
            assert!(mission.pending_completion_claim.is_some());
            assert_eq!(mission.status, MissionStatus::Active);
        });
    }

    #[test]
    fn verify_completion_refutes_insufficient_evidence_coverage() {
        with_isolated_home(|| {
            let session_id = "ses_insufficient_evidence";
            set(session_id, "Ship the verifier").expect("set mission");
            set_success_criteria(
                session_id,
                vec![
                    "All unit tests pass".to_string(),
                    "Manually verified in a live run".to_string(),
                ],
            )
            .expect("set criteria");
            claim_complete(
                session_id,
                vec!["Ran the full test suite and everything passed cleanly".to_string()],
            )
            .expect("claim");

            let outcome = verify_completion(session_id).expect("verify");
            assert!(matches!(outcome, VerificationOutcome::Refuted { .. }));
        });
    }

    #[test]
    fn verify_completion_confirms_when_claim_meets_the_bar() {
        with_isolated_home(|| {
            let session_id = "ses_confirmed";
            set(session_id, "Ship the verifier").expect("set mission");
            set_success_criteria(
                session_id,
                vec![
                    "All unit tests pass".to_string(),
                    "Manually verified in a live run".to_string(),
                ],
            )
            .expect("set criteria");
            claim_complete(
                session_id,
                vec![
                    "Ran `cargo test -p jcode-app-core mission`, all tests passed".to_string(),
                    "Ran the mission_tool_demo example end to end and inspected the output"
                        .to_string(),
                ],
            )
            .expect("claim");

            let outcome = verify_completion(session_id).expect("verify");
            match outcome {
                VerificationOutcome::Confirmed { mission } => {
                    assert_eq!(mission.status, MissionStatus::Complete);
                    assert!(mission.pending_completion_claim.is_none());
                }
                other => panic!("expected Confirmed, got {:?}", other),
            }

            // Once Complete, the reminder must stop (same guarantee already
            // covered for Blocked in tool/mission_tests.rs).
            assert!(active_system_reminder(session_id).expect("reminder").is_none());
        });
    }

    #[test]
    fn verify_completion_with_no_pending_claim() {
        with_isolated_home(|| {
            let session_id = "ses_no_claim";
            set(session_id, "Ship the verifier").expect("set mission");
            let outcome = verify_completion(session_id).expect("verify");
            assert_eq!(outcome, VerificationOutcome::NoPendingClaim);
        });
    }
}

#[cfg(test)]
mod checkpoint_and_concurrency_tests {
    use super::*;

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

    /// Gemini review, 2026-08-30: an accumulating log with no cap could
    /// grow `mission.json` without bound under a retry-loop-style misuse.
    #[test]
    fn checkpoints_beyond_the_cap_drop_the_oldest_not_the_newest() {
        with_isolated_home(|| {
            let session_id = "ses_checkpoint_cap";
            set(session_id, "Cap the checkpoint log").expect("set mission");
            for i in 0..(MAX_CHECKPOINTS + 10) {
                checkpoint(session_id, &format!("checkpoint {i}")).expect("checkpoint");
            }
            let mission = load(session_id).expect("load").expect("mission exists");
            assert_eq!(mission.checkpoints.len(), MAX_CHECKPOINTS);
            assert_eq!(
                mission.checkpoints.first().unwrap().summary,
                format!("checkpoint {}", 10),
                "the oldest surviving checkpoint should be #10 -- #0..#9 were dropped"
            );
            assert_eq!(
                mission.checkpoints.last().unwrap().summary,
                format!("checkpoint {}", MAX_CHECKPOINTS + 9),
                "the newest checkpoint must always survive"
            );
        });
    }

    /// Gemini review, 2026-08-30: `checkpoint`'s load -> mutate -> save was
    /// previously unguarded; two concurrent callers for the same session
    /// could both load the same prior mission, each append their own
    /// checkpoint, and save -- whichever save landed last would silently
    /// discard the other's checkpoint. Real concurrent threads, same
    /// pattern already proven for `rewind_store.rs`'s equivalent race.
    #[test]
    fn concurrent_checkpoints_for_the_same_mission_never_lose_one() {
        with_isolated_home(|| {
            let session_id = "ses_concurrent_checkpoint";
            set(session_id, "Survive concurrent checkpoints").expect("set mission");
            const WRITERS: usize = 20;
            std::thread::scope(|scope| {
                for i in 0..WRITERS {
                    scope.spawn(move || {
                        checkpoint(session_id, &format!("from thread {i}")).expect("checkpoint");
                    });
                }
            });
            let mission = load(session_id).expect("load").expect("mission exists");
            assert_eq!(
                mission.checkpoints.len(),
                WRITERS,
                "every concurrent checkpoint call must land -- none may be silently lost"
            );
        });
    }
}

#[cfg(test)]
mod supervisor_gate_tests {
    use super::*;

    // Each test below sets up its own isolated JCODE_HOME inline rather than
    // via a shared sync helper, since these are #[tokio::test] async fns and
    // a synchronous FnOnce-based helper (as used elsewhere in this file for
    // non-async tests) doesn't fit cleanly here.

    #[tokio::test]
    async fn no_mission_never_stops_the_loop() {
        // Isolated JCODE_HOME with no mission ever set for this session id.
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());
        let outcome = supervisor_gate("ses_never_existed").await.expect("gate");
        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn active_mission_with_no_budget_issue_and_no_claim_continues() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());
        let session_id = "ses_gate_normal";
        set(session_id, "Ship the supervisor gate").expect("set mission");

        // No credentials configured in this isolated home, so
        // fetch_all_provider_usage() returns an empty Vec without touching
        // the network -- this really does exercise enforce_budget's async
        // path, not skip it.
        let outcome = supervisor_gate(session_id).await.expect("gate");
        assert_eq!(outcome, None);
        let mission = load(session_id).expect("load").expect("exists");
        assert_eq!(mission.status, MissionStatus::Active);
    }

    #[tokio::test]
    async fn already_budget_limited_stops_immediately_without_reenforcing() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());
        let session_id = "ses_gate_already_limited";
        set(session_id, "Ship the supervisor gate").expect("set mission");
        // Force into BudgetLimited directly via the persistence layer (not
        // through update_status, which only forbids reaching Complete —
        // BudgetLimited is a legitimate direct transition).
        update_status(session_id, MissionStatus::BudgetLimited).expect("force budget_limited");

        let outcome = supervisor_gate(session_id).await.expect("gate");
        match outcome {
            Some(reason) => assert!(reason.contains("budget_limited")),
            None => panic!("expected the gate to stop the loop"),
        }
    }

    #[tokio::test]
    async fn non_active_non_budget_status_passes_through_without_checking_claim() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());
        let session_id = "ses_gate_paused";
        set(session_id, "Ship the supervisor gate").expect("set mission");
        update_status(session_id, MissionStatus::Paused).expect("pause");

        let outcome = supervisor_gate(session_id).await.expect("gate");
        assert_eq!(outcome, None);
    }

    #[tokio::test]
    async fn confirmed_completion_claim_stops_the_loop() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());
        let session_id = "ses_gate_confirmed";
        set(session_id, "Ship the supervisor gate").expect("set mission");
        set_success_criteria(session_id, vec!["Gate stops on confirmed completion".to_string()])
            .expect("set criteria");
        claim_complete(
            session_id,
            vec!["Wrote and ran supervisor_gate_tests, all passing".to_string()],
        )
        .expect("claim");

        let outcome = supervisor_gate(session_id).await.expect("gate");
        match outcome {
            Some(reason) => assert!(reason.to_lowercase().contains("verified")),
            None => panic!("expected the gate to stop the loop on confirmed completion"),
        }
        let mission = load(session_id).expect("load").expect("exists");
        assert_eq!(mission.status, MissionStatus::Complete);
    }

    #[tokio::test]
    async fn refuted_completion_claim_does_not_stop_the_loop() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());
        let session_id = "ses_gate_refuted";
        set(session_id, "Ship the supervisor gate").expect("set mission");
        // No success criteria set -> verify_completion will refute.
        claim_complete(
            session_id,
            vec!["Wrote and ran supervisor_gate_tests, all passing".to_string()],
        )
        .expect("claim");

        let outcome = supervisor_gate(session_id).await.expect("gate");
        assert_eq!(outcome, None, "a refuted claim must not stop the loop");
        let mission = load(session_id).expect("load").expect("exists");
        assert_eq!(mission.status, MissionStatus::Active);
        assert!(
            mission.pending_completion_claim.is_some(),
            "refuted claim should stay pending, not be dropped"
        );
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::usage::ProviderUsage;

    fn report(hard_limit_reached: bool) -> ProviderUsage {
        ProviderUsage {
            hard_limit_reached,
            ..Default::default()
        }
    }

    #[test]
    fn no_reports_is_not_limited() {
        assert!(!any_provider_hard_limited(&[]));
    }

    #[test]
    fn all_under_limit_is_not_limited() {
        assert!(!any_provider_hard_limited(&[report(false), report(false)]));
    }

    #[test]
    fn one_hard_limited_provider_trips_it() {
        assert!(any_provider_hard_limited(&[report(false), report(true)]));
    }
}
