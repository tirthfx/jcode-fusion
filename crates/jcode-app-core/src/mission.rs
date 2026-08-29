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

pub fn update_status(session_id: &str, status: MissionStatus) -> Result<Option<Mission>> {
    let Some(mut mission) = load(session_id)? else {
        return Ok(None);
    };
    mission.status = status;
    mission.updated_at = Utc::now();
    save(&mission)?;
    Ok(Some(mission))
}

pub fn checkpoint(session_id: &str, summary: &str) -> Result<Option<Mission>> {
    let summary = summary.trim();
    if summary.is_empty() {
        anyhow::bail!("checkpoint summary cannot be empty");
    }
    let Some(mut mission) = load(session_id)? else {
        return Ok(None);
    };
    mission.checkpoints.push(MissionCheckpoint {
        at: Utc::now(),
        summary: summary.to_string(),
    });
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
pub async fn enforce_budget(session_id: &str) -> Result<Option<Mission>> {
    let Some(mission) = load(session_id)? else {
        return Ok(None);
    };
    if !matches!(mission.status, MissionStatus::Active) {
        return Ok(Some(mission));
    }
    let reports = crate::usage::fetch_all_provider_usage().await;
    if any_provider_hard_limited(&reports) {
        return update_status(session_id, MissionStatus::BudgetLimited);
    }
    Ok(Some(mission))
}

/// Pure, network-free decision logic split out from [`enforce_budget`] so it
/// can be unit-tested directly against constructed `ProviderUsage` values
/// instead of needing real provider credentials/network access.
fn any_provider_hard_limited(reports: &[crate::usage::ProviderUsage]) -> bool {
    reports.iter().any(|report| report.hard_limit_reached)
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
