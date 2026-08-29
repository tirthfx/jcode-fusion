//! Agent-facing tool for `crate::mission` — Fusion project's Mission Engine
//! work (see jcode-fusion/DESIGN.md §6 item #1 and PROGRESS.md).
//!
//! Slice 1: gave `crate::mission::set`/`update_status`/`checkpoint`/`clear`
//! (which already existed but had zero callers) real callers.
//! Slice 2: `check_budget`, real (not heuristic) budget enforcement.
//! Slice 3 (this one): `success_criteria`/`claim_complete`/
//! `verify_completion` — completion can no longer be self-certified via
//! `status`; it must be claimed with evidence and pass a real, enforced
//! check against declared success criteria. See `crate::mission`'s doc
//! comments on `claim_complete`/`verify_completion` for exactly what is and
//! isn't verified yet (an LLM-based independent review is the natural next
//! step, not yet built).
//!
//! Still not done: an outer supervisor loop tying budget + verification +
//! the continuation-reminder mechanism together into something that
//! actually runs unattended (modeled on `overnight.rs::run_supervisor`).

use super::{Tool, ToolContext, ToolOutput};
use crate::mission::MissionStatus;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct MissionTool;

impl MissionTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct MissionInput {
    action: String,
    #[serde(default)]
    objective: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    criteria: Option<Vec<String>>,
    #[serde(default)]
    evidence: Option<Vec<String>>,
}

#[async_trait]
impl Tool for MissionTool {
    fn name(&self) -> &str {
        "mission"
    }

    fn description(&self) -> &str {
        "Track a long-horizon mission for this session."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["set", "show", "status", "checkpoint", "check_budget", "success_criteria", "claim_complete", "verify_completion", "clear"],
                    "description": "Action."
                },
                "objective": {
                    "type": "string",
                    "description": "Mission objective."
                },
                "status": {
                    "type": "string",
                    "enum": ["active", "paused", "blocked", "needs_decision", "budget_limited", "abandoned"],
                    "description": "New status."
                },
                "summary": {
                    "type": "string",
                    "description": "Checkpoint note."
                },
                "criteria": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Success criteria."
                },
                "evidence": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Completion evidence."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: MissionInput = serde_json::from_value(input)?;
        let action = params.action.clone();
        let result: Result<ToolOutput> = async {
            match action.as_str() {
                "set" => {
                    let objective = params
                        .objective
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("objective is required for set"))?;
                    let mission = crate::mission::set(&ctx.session_id, objective)?;
                    Ok(ToolOutput::new(crate::mission::render_status(&mission))
                        .with_title(mission.objective.clone())
                        .with_metadata(serde_json::to_value(&mission)?))
                }
                "show" => match crate::mission::load(&ctx.session_id)? {
                    Some(mission) => Ok(ToolOutput::new(crate::mission::render_status(&mission))
                        .with_title(mission.objective.clone())
                        .with_metadata(serde_json::to_value(&mission)?)),
                    None => Ok(ToolOutput::new("No mission set for this session.")),
                },
                "status" => {
                    let status_str = params
                        .status
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("status is required for status"))?;
                    let status = MissionStatus::parse(status_str).ok_or_else(|| {
                        anyhow::anyhow!("invalid mission status: {}", status_str)
                    })?;
                    let mission = crate::mission::update_status(&ctx.session_id, status)?
                        .ok_or_else(|| anyhow::anyhow!("no mission set for this session"))?;
                    Ok(ToolOutput::new(crate::mission::render_status(&mission))
                        .with_title(mission.objective.clone())
                        .with_metadata(serde_json::to_value(&mission)?))
                }
                "checkpoint" => {
                    let summary = params
                        .summary
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("summary is required for checkpoint"))?;
                    let mission = crate::mission::checkpoint(&ctx.session_id, summary)?
                        .ok_or_else(|| anyhow::anyhow!("no mission set for this session"))?;
                    Ok(ToolOutput::new(crate::mission::render_status(&mission))
                        .with_title(mission.objective.clone())
                        .with_metadata(serde_json::to_value(&mission)?))
                }
                "check_budget" => {
                    let mission = crate::mission::enforce_budget(&ctx.session_id).await?;
                    match mission {
                        Some(mission) if mission.status == MissionStatus::BudgetLimited => Ok(
                            ToolOutput::new(format!(
                                "Budget limited: a connected provider has hit its usage limit. {}",
                                crate::mission::render_status(&mission)
                            ))
                            .with_title(mission.objective.clone())
                            .with_metadata(serde_json::to_value(&mission)?),
                        ),
                        Some(mission) => Ok(ToolOutput::new(format!(
                            "No connected provider is currently hard-limited. {}",
                            crate::mission::render_status(&mission)
                        ))
                        .with_title(mission.objective.clone())
                        .with_metadata(serde_json::to_value(&mission)?)),
                        None => Ok(ToolOutput::new("No mission set for this session.")),
                    }
                }
                "success_criteria" => {
                    let criteria = params
                        .criteria
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("criteria is required for success_criteria"))?;
                    let mission = crate::mission::set_success_criteria(&ctx.session_id, criteria)?
                        .ok_or_else(|| anyhow::anyhow!("no mission set for this session"))?;
                    Ok(ToolOutput::new(crate::mission::render_status(&mission))
                        .with_title(mission.objective.clone())
                        .with_metadata(serde_json::to_value(&mission)?))
                }
                "claim_complete" => {
                    let evidence = params
                        .evidence
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("evidence is required for claim_complete"))?;
                    let mission = crate::mission::claim_complete(&ctx.session_id, evidence)?
                        .ok_or_else(|| anyhow::anyhow!("no mission set for this session"))?;
                    Ok(ToolOutput::new(format!(
                        "Completion claim recorded, pending verification. Call `verify_completion` \
                         to check it. {}",
                        crate::mission::render_status(&mission)
                    ))
                    .with_title(mission.objective.clone())
                    .with_metadata(serde_json::to_value(&mission)?))
                }
                "verify_completion" => {
                    match crate::mission::verify_completion(&ctx.session_id)? {
                        crate::mission::VerificationOutcome::NoPendingClaim => {
                            Ok(ToolOutput::new(
                                "No pending completion claim for this mission. Use \
                                 `claim_complete` first.",
                            ))
                        }
                        crate::mission::VerificationOutcome::Refuted { reason } => Ok(
                            ToolOutput::new(format!("Completion claim refuted: {}", reason)),
                        ),
                        crate::mission::VerificationOutcome::Confirmed { mission } => Ok(
                            ToolOutput::new(format!(
                                "Completion claim confirmed. {}",
                                crate::mission::render_status(&mission)
                            ))
                            .with_title(mission.objective.clone())
                            .with_metadata(serde_json::to_value(&mission)?),
                        ),
                    }
                }
                "clear" => {
                    let cleared = crate::mission::clear(&ctx.session_id)?;
                    Ok(ToolOutput::new(if cleared {
                        "Mission cleared."
                    } else {
                        "No mission was set for this session."
                    }))
                }
                other => anyhow::bail!("unknown mission action: {}", other),
            }
        }
        .await;
        result.map_err(|err| {
            crate::logging::warn(&format!(
                "[tool:mission] action failed action={} session_id={} error={}",
                action, ctx.session_id, err
            ));
            err
        })
    }
}

#[cfg(test)]
#[path = "mission_tests.rs"]
mod mission_tests;
