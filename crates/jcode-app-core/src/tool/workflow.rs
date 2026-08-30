//! Agent-facing tool for `crate::workflow_template` -- Fusion Phase 3,
//! orchestration-as-script (DESIGN.md §6 item #8). See that module's own
//! doc comments for the full design rationale (why `TaskGraphNodeSpec`, not
//! `PlanItem`; why no Starlark yet).
//!
//! This is the tool-wiring slice `workflow_template.rs` deliberately left
//! for later: `save`/`list` operate purely on local disk (no daemon round
//! trip needed), `run` instantiates a template into real
//! `TaskGraphNodeSpec`s and seeds them via the same `Request::CommSeedGraph`
//! the existing `swarm` tool's `task_graph`/`seed_graph` action uses --
//! reusing `tool::communicate::transport::send_request` (widened to
//! `pub(crate)` for exactly this reuse) rather than re-implementing socket
//! transport.
//!
//! **Deliberately simpler than `task_graph`'s own action**: that action has
//! a duplicate-node-id collision-remap-and-retry loop (renaming a colliding
//! id and resubmitting) for when a plan already has nodes with the same
//! ids. `run` here does not retry on collision -- it surfaces the daemon's
//! error directly. A template's node ids are author-controlled and meant to
//! be stable across runs; silently renaming them on collision would make a
//! template's own `depends_on` graph harder to reason about run to run.
//! Documented simplification, not a silent gap.

use std::collections::HashMap;

use super::{Tool, ToolContext, ToolOutput};
use crate::protocol::{Request, ServerEvent};
use crate::tool::communicate::transport::send_request;
use crate::workflow_template::{TemplateNode, WorkflowParameter, WorkflowTemplate};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub struct WorkflowTool;

impl WorkflowTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
struct WorkflowInput {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<Vec<WorkflowParameter>>,
    #[serde(default)]
    nodes: Option<Vec<TemplateNode>>,
    #[serde(default)]
    values: Option<HashMap<String, String>>,
    #[serde(default)]
    mode: Option<String>,
}

fn check_error(response: &ServerEvent) -> Option<&str> {
    if let ServerEvent::Error { message, .. } = response {
        Some(message)
    } else {
        None
    }
}

fn ensure_success(response: &ServerEvent) -> Result<()> {
    if let Some(message) = check_error(response) {
        Err(anyhow::anyhow!(message.to_string()))
    } else {
        Ok(())
    }
}

#[async_trait]
impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        "workflow"
    }

    fn description(&self) -> &str {
        "Save, list, and run reusable parameterized task-graph templates."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "intent": super::intent_schema_property(),
                "action": {
                    "type": "string",
                    "enum": ["save", "list", "run"],
                    "description": "Action."
                },
                "name": {
                    "type": "string",
                    "description": "Template name."
                },
                "description": {
                    "type": "string",
                    "description": "Template description, for save."
                },
                "parameters": {
                    "type": "array",
                    "description": "For save: declared {{name}} parameters.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "description": {"type": "string"},
                            "default": {"type": "string"}
                        }
                    }
                },
                "nodes": {
                    "type": "array",
                    "description": "For save: template nodes, may reference {{param}}.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "content": {"type": "string"},
                            "kind": {"type": "string"},
                            "priority": {"type": "integer"},
                            "depends_on": {"type": "array", "items": {"type": "string"}}
                        }
                    }
                },
                "values": {
                    "type": "object",
                    "description": "For run: parameter values by name."
                },
                "mode": {
                    "type": "string",
                    "enum": ["deep", "light"],
                    "description": "For run: task-graph mode."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let params: WorkflowInput = serde_json::from_value(input)?;
        let action = params.action.clone();
        let result: Result<ToolOutput> = async {
            match action.as_str() {
                "save" => {
                    let name = params
                        .name
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("'name' is required for save"))?;
                    let nodes = params
                        .nodes
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("'nodes' is required for save"))?;
                    let template = WorkflowTemplate {
                        name: name.clone(),
                        description: params.description.clone().unwrap_or_default(),
                        parameters: params.parameters.clone().unwrap_or_default(),
                        nodes,
                    };
                    crate::workflow_template::save(&template)?;
                    Ok(ToolOutput::new(format!(
                        "Saved workflow template '{}' ({} node(s), {} parameter(s)).",
                        name,
                        template.nodes.len(),
                        template.parameters.len()
                    )))
                }

                "list" => {
                    let names = crate::workflow_template::list()?;
                    if names.is_empty() {
                        Ok(ToolOutput::new("No saved workflow templates."))
                    } else {
                        Ok(ToolOutput::new(format!(
                            "Saved workflow templates ({}):\n{}",
                            names.len(),
                            names
                                .iter()
                                .map(|n| format!("- {n}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )))
                    }
                }

                "run" => {
                    let name = params
                        .name
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("'name' is required for run"))?;
                    let template = crate::workflow_template::load(&name)?;
                    let values = params.values.clone().unwrap_or_default();
                    let node_specs = template.instantiate(&values)?;
                    // Author-controlled, stable ids -- worth handing back so
                    // the caller can immediately query/reference the seeded
                    // nodes without a separate list_channels-style lookup.
                    let node_ids: Vec<String> =
                        node_specs.iter().map(|spec| spec.id.clone()).collect();

                    let request = Request::CommSeedGraph {
                        id: 1,
                        session_id: ctx.session_id.clone(),
                        mode: params.mode.clone(),
                        nodes: node_specs,
                    };
                    let response = send_request(request)
                        .await
                        .map_err(|e| anyhow::anyhow!("Failed to seed task graph: {}", e))?;
                    ensure_success(&response)?;
                    // Echo `template.name` (validated by `load()`, see
                    // that function's doc comment), not the raw `name`
                    // param this action was called with -- Gemini review,
                    // 2026-08-30: using the caller-supplied string here
                    // reintroduced the exact newline/ANSI-escape injection
                    // an earlier fix closed for `list()`'s own output,
                    // just in a sibling code path. `node_ids` are safe to
                    // echo too: `instantiate()` only ever returns ids that
                    // passed `validate()`'s charset check.
                    Ok(ToolOutput::new(format!(
                        "Ran workflow template '{}' ({} node(s) seeded: {}).",
                        template.name,
                        node_ids.len(),
                        node_ids.join(", ")
                    )))
                }

                other => anyhow::bail!("unknown workflow action: {}", other),
            }
        }
        .await;
        result.map_err(|err| {
            crate::logging::warn(&format!(
                "[tool:workflow] action failed action={} session_id={} error={}",
                action, ctx.session_id, err
            ));
            err
        })
    }
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod workflow_tests;
