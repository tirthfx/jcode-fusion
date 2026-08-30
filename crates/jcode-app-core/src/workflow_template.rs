//! Fusion Phase 3: orchestration-as-script, first slice (DESIGN.md §6 item
//! #8).
//!
//! **Scoping note, source-verified before writing any of this**: the
//! original size estimate assumed a new `rhai` dependency for this. That's
//! now stale -- Phase 1's execpolicy slice already added `starlark = "0.14"`
//! to this crate. Reusing that instead of adding a second embedded-scripting
//! interpreter is a real, source-grounded win worth recording: **this
//! module deliberately does NOT add Starlark yet**. `PROGRESS.md`'s own
//! Phase 3/4 findings already scoped the *real* first slice correctly:
//! `VersionedPlan` is already durably persisted (`server/swarm_persistence.rs`)
//! with ~27 call sites across nearly every plan mutation -- orchestration-
//! as-script doesn't need to build persistence from scratch, it needs a new
//! capability on top: turning a plan into a reusable, parameterized,
//! replayable **template**. That's what this slice builds. A follow-up
//! slice can layer Starlark on top for real conditional/loop logic inside a
//! template, the same "extend, don't replace" pattern execpolicy used for
//! `jcode-command-risk` -- not needed for a template that's just
//! parameterized text substitution.
//!
//! **Deliberately out of scope for this slice** (documented, not silently
//! skipped): no agent tool wiring yet (no `workflow` tool action to save/run
//! a template from a live session -- this slice is types + persistence +
//! instantiation logic only, the same "smallest coherent first slice" shape
//! Phase 0's Mission Engine work started with). No Starlark scripting inside
//! a template (see above). No template versioning/migration if the shape
//! changes later. No listing/discovery UI.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::plan::PlanItem;

/// A declared parameter a template's nodes can reference via `{{name}}`
/// placeholders in `content`/`subsystem`/`file_scope`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowParameter {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Used when the caller doesn't supply a value for this parameter.
    /// `None` means the parameter is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// One task node in a template, pre-substitution. Mirrors the subset of
/// [`PlanItem`] a template actually needs to declare -- `status` and
/// `assigned_to` are deliberately not template fields, since every
/// instantiation always starts a node at `"pending"`/unassigned regardless
/// of what a prior run of the same template ended up with.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TemplateNode {
    pub id: String,
    pub content: String,
    #[serde(default = "default_priority")]
    pub priority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subsystem: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

fn default_priority() -> String {
    "medium".to_string()
}

/// A saved, reusable, parameterized swarm plan. Persisted at
/// `~/.jcode/workflows/<name>.json` via [`save`]/[`load`] -- atomic writes
/// with corruption recovery come for free from `jcode_storage`'s existing
/// `write_json_fast`/`read_json`, the same primitives `rewind_store.rs`
/// already relies on. Nothing new to build or test for the storage layer
/// itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowTemplate {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: Vec<WorkflowParameter>,
    pub nodes: Vec<TemplateNode>,
}

/// Placeholder syntax: `{{param_name}}`. Deliberately not full Starlark/Rhai
/// interpolation -- see module docs on why a scripting engine isn't part of
/// this slice.
fn placeholder(name: &str) -> String {
    format!("{{{{{name}}}}}")
}

impl WorkflowTemplate {
    /// Validate structural invariants that are cheap to check up front and
    /// would otherwise surface as a confusing failure much later (a
    /// dangling `blocked_by` reference breaking swarm plan construction, a
    /// duplicate node id silently shadowing one node with another).
    /// Called by both [`save`] (refuse to persist something already known
    /// to be broken) and [`instantiate`] (refuse to hand back a plan built
    /// from a template that was somehow saved before this check existed, or
    /// edited by hand on disk).
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            anyhow::bail!("workflow template name must not be empty");
        }
        if self.nodes.is_empty() {
            anyhow::bail!("workflow template '{}' has no nodes", self.name);
        }

        let mut seen_ids = std::collections::HashSet::new();
        for node in &self.nodes {
            if node.id.trim().is_empty() {
                anyhow::bail!(
                    "workflow template '{}' has a node with an empty id",
                    self.name
                );
            }
            if !seen_ids.insert(node.id.as_str()) {
                anyhow::bail!(
                    "workflow template '{}' has a duplicate node id '{}'",
                    self.name,
                    node.id
                );
            }
        }
        let known_ids: std::collections::HashSet<&str> =
            self.nodes.iter().map(|n| n.id.as_str()).collect();
        for node in &self.nodes {
            for dep in &node.blocked_by {
                if !known_ids.contains(dep.as_str()) {
                    anyhow::bail!(
                        "workflow template '{}': node '{}' is blocked_by unknown node '{}'",
                        self.name,
                        node.id,
                        dep
                    );
                }
            }
        }

        let mut seen_params = std::collections::HashSet::new();
        for param in &self.parameters {
            if param.name.trim().is_empty() {
                anyhow::bail!(
                    "workflow template '{}' has a parameter with an empty name",
                    self.name
                );
            }
            if !seen_params.insert(param.name.as_str()) {
                anyhow::bail!(
                    "workflow template '{}' declares parameter '{}' more than once",
                    self.name,
                    param.name
                );
            }
        }
        Ok(())
    }

    /// Substitute every declared parameter's `{{name}}` placeholder across
    /// all nodes and produce a real `Vec<PlanItem>`, ready to hand to
    /// whatever already builds a `VersionedPlan` from plan items.
    ///
    /// Refuses (does not silently proceed) on:
    /// - a required parameter (no `default`) with no value supplied,
    /// - any `{{...}}`-shaped placeholder left over after substitution --
    ///   this means the template referenced a parameter it never declared,
    ///   which is a template authoring bug, not something to paper over by
    ///   leaving the literal placeholder text in a real task's content.
    pub fn instantiate(&self, values: &HashMap<String, String>) -> Result<Vec<PlanItem>> {
        self.validate()?;

        let mut missing = Vec::new();
        let mut resolved: HashMap<&str, String> = HashMap::new();
        for param in &self.parameters {
            match values.get(param.name.as_str()) {
                Some(v) => {
                    resolved.insert(param.name.as_str(), v.clone());
                }
                None => match &param.default {
                    Some(default) => {
                        resolved.insert(param.name.as_str(), default.clone());
                    }
                    None => missing.push(param.name.clone()),
                },
            }
        }
        if !missing.is_empty() {
            anyhow::bail!(
                "workflow template '{}' is missing required parameter(s): {}",
                self.name,
                missing.join(", ")
            );
        }

        let substitute = |text: &str| -> String {
            let mut out = text.to_string();
            for (name, value) in &resolved {
                out = out.replace(&placeholder(name), value);
            }
            out
        };

        let mut items = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let content = substitute(&node.content);
            let subsystem = node.subsystem.as_deref().map(substitute);
            let file_scope: Vec<String> = node.file_scope.iter().map(|f| substitute(f)).collect();

            for (label, text) in [("content", content.as_str())]
                .into_iter()
                .chain(subsystem.as_deref().map(|s| ("subsystem", s)))
                .chain(file_scope.iter().map(|f| ("file_scope entry", f.as_str())))
            {
                if let Some(leftover) = find_unresolved_placeholder(text) {
                    anyhow::bail!(
                        "workflow template '{}': node '{}' {} references \
                         undeclared parameter '{}'",
                        self.name,
                        node.id,
                        label,
                        leftover
                    );
                }
            }

            items.push(PlanItem {
                content,
                status: "pending".to_string(),
                priority: node.priority.clone(),
                id: node.id.clone(),
                subsystem,
                file_scope,
                blocked_by: node.blocked_by.clone(),
                assigned_to: None,
            });
        }
        Ok(items)
    }
}

/// Find a leftover `{{...}}`-shaped placeholder in already-substituted text,
/// if any. Deliberately simple (no nested-brace handling -- templates are
/// flat text, not a real expression language) since this only needs to
/// catch "an undeclared parameter was referenced," not parse arbitrary
/// mustache-like syntax.
fn find_unresolved_placeholder(text: &str) -> Option<&str> {
    let start = text.find("{{")?;
    let rest = &text[start + 2..];
    let end = rest.find("}}")?;
    Some(&rest[..end])
}

fn workflows_dir() -> Result<PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("workflows"))
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn template_path(name: &str) -> Result<PathBuf> {
    Ok(workflows_dir()?.join(format!("{}.json", sanitize_name(name))))
}

/// Persist a template, refusing (not silently saving) anything that fails
/// [`WorkflowTemplate::validate`] -- catch a broken template at save time,
/// not the next time someone tries to run it.
pub fn save(template: &WorkflowTemplate) -> Result<()> {
    template.validate()?;
    let dir = workflows_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating workflows dir {}", dir.display()))?;
    crate::storage::write_json_fast(&template_path(&template.name)?, template)
}

/// Load a previously-saved template by name.
pub fn load(name: &str) -> Result<WorkflowTemplate> {
    let path = template_path(name)?;
    if !path.exists() {
        anyhow::bail!("no workflow template named '{}'", name);
    }
    crate::storage::read_json(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_template() -> WorkflowTemplate {
        WorkflowTemplate {
            name: "review-and-fix".to_string(),
            description: "Review a subsystem, then fix what it finds.".to_string(),
            parameters: vec![
                WorkflowParameter {
                    name: "subsystem".to_string(),
                    description: "Which subsystem to review".to_string(),
                    default: None,
                },
                WorkflowParameter {
                    name: "severity".to_string(),
                    description: "Minimum severity to act on".to_string(),
                    default: Some("medium".to_string()),
                },
            ],
            nodes: vec![
                TemplateNode {
                    id: "review".to_string(),
                    content: "Review {{subsystem}} for {{severity}}+ issues".to_string(),
                    priority: "high".to_string(),
                    subsystem: Some("{{subsystem}}".to_string()),
                    file_scope: vec!["{{subsystem}}/**".to_string()],
                    blocked_by: vec![],
                },
                TemplateNode {
                    id: "fix".to_string(),
                    content: "Fix what the {{subsystem}} review found".to_string(),
                    priority: "medium".to_string(),
                    subsystem: Some("{{subsystem}}".to_string()),
                    file_scope: vec![],
                    blocked_by: vec!["review".to_string()],
                },
            ],
        }
    }

    #[test]
    fn instantiate_substitutes_declared_and_default_parameters() {
        let template = sample_template();
        let mut values = HashMap::new();
        values.insert("subsystem".to_string(), "auth".to_string());
        // severity deliberately omitted -- must fall back to its default.

        let items = template.instantiate(&values).expect("instantiate");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].content, "Review auth for medium+ issues");
        assert_eq!(items[0].subsystem.as_deref(), Some("auth"));
        assert_eq!(items[0].file_scope, vec!["auth/**".to_string()]);
        assert_eq!(items[1].content, "Fix what the auth review found");
        assert_eq!(items[1].blocked_by, vec!["review".to_string()]);
        // Every instantiation starts fresh, regardless of any prior run.
        assert!(items.iter().all(|i| i.status == "pending"));
        assert!(items.iter().all(|i| i.assigned_to.is_none()));
    }

    #[test]
    fn instantiate_refuses_when_a_required_parameter_is_missing() {
        let template = sample_template();
        let values = HashMap::new(); // subsystem never supplied, no default.

        let err = template.instantiate(&values).unwrap_err().to_string();
        assert!(err.contains("subsystem"), "got: {err}");
    }

    #[test]
    fn instantiate_refuses_a_reference_to_an_undeclared_parameter() {
        let mut template = sample_template();
        template.nodes[0].content = "Review {{subsystem}} owned by {{owner}}".to_string();
        let mut values = HashMap::new();
        values.insert("subsystem".to_string(), "auth".to_string());

        let err = template.instantiate(&values).unwrap_err().to_string();
        assert!(err.contains("owner"), "got: {err}");
    }

    #[test]
    fn validate_rejects_a_dangling_blocked_by_reference() {
        let mut template = sample_template();
        template.nodes[1].blocked_by = vec!["does-not-exist".to_string()];
        let err = template.validate().unwrap_err().to_string();
        assert!(err.contains("does-not-exist"), "got: {err}");
    }

    #[test]
    fn validate_rejects_duplicate_node_ids() {
        let mut template = sample_template();
        template.nodes[1].id = template.nodes[0].id.clone();
        let err = template.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[test]
    fn validate_rejects_duplicate_parameter_names() {
        let mut template = sample_template();
        template.parameters.push(WorkflowParameter {
            name: "subsystem".to_string(),
            description: "duplicate".to_string(),
            default: None,
        });
        let err = template.validate().unwrap_err().to_string();
        assert!(err.contains("subsystem"), "got: {err}");
    }

    #[test]
    fn validate_rejects_a_template_with_no_nodes() {
        let mut template = sample_template();
        template.nodes.clear();
        assert!(template.validate().is_err());
    }

    #[tokio::test]
    async fn save_and_load_round_trip_through_real_disk() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());

        let template = sample_template();
        save(&template).expect("save");

        let loaded = load(&template.name).expect("load");
        assert_eq!(loaded, template);
    }

    #[tokio::test]
    async fn save_refuses_an_invalid_template_rather_than_persisting_it() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());

        let mut template = sample_template();
        template.nodes.clear();

        assert!(save(&template).is_err());
        assert!(
            load(&template.name).is_err(),
            "nothing should have been written to disk"
        );
    }

    #[tokio::test]
    async fn load_fails_cleanly_for_an_unknown_template_name() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());

        assert!(load("never-saved").is_err());
    }

    #[test]
    fn sanitize_name_strips_path_traversal_shaped_input() {
        // Defensive: a template name is used to build a filesystem path --
        // confirm it can't be used to escape the workflows directory.
        let sanitized = sanitize_name("../../etc/passwd");
        assert!(!sanitized.contains('/'));
        assert!(!sanitized.contains(".."));
    }
}
