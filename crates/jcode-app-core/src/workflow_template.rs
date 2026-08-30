//! Fusion Phase 3: orchestration-as-script (DESIGN.md §6 item #8).
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
//! replayable **template**. That's what this module builds. A follow-up
//! slice can layer Starlark on top for real conditional/loop logic inside a
//! template, the same "extend, don't replace" pattern execpolicy used for
//! `jcode-command-risk` -- not needed for a template that's just
//! parameterized text substitution.
//!
//! **Real correction made mid-implementation, not shipped wrong on
//! purpose**: the first version of this module modeled a template node on
//! `PlanItem` (id/content/priority-as-string/subsystem/file_scope/
//! blocked_by). Reading `tool/communicate.rs`'s existing `task_graph`/
//! `seed_graph` action (the real integration point a template's output
//! needs to feed into, via `Request::CommSeedGraph`) found that action
//! actually takes `TaskGraphNodeSpec { id, content, kind, depends_on,
//! priority: u8 }` -- a different, narrower shape than `PlanItem`.
//! `subsystem`/`file_scope` are explicitly fields "the engine does not own"
//! (see `jcode_plan::bridge::apply_task_graph`'s own doc comment) at this
//! integration point, and `kind` (explore/implement/verify/fix/synthesize/
//! critique) has no `PlanItem` equivalent at all. Retargeted `TemplateNode`
//! to match `TaskGraphNodeSpec` before any tool wiring was built on the
//! wrong shape, rather than carrying the mismatch forward.
//!
//! **Deliberately out of scope still**: no Starlark scripting inside a
//! template (see above). No template versioning/migration if the shape
//! changes later.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::protocol::TaskGraphNodeSpec;

/// A declared parameter a template's nodes can reference via `{{name}}`
/// placeholders in `content`.
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

/// One task node in a template, pre-substitution. Matches
/// [`TaskGraphNodeSpec`] field-for-field (the type `tool_run` below actually
/// hands to `Request::CommSeedGraph`), plus `content`'s placeholder support.
/// `kind` mirrors `jcode_plan::bridge::parse_kind`'s own vocabulary
/// (explore/implement/verify/fix/synthesize/critique, default explore) --
/// deliberately a plain `Option<String>` here rather than re-declaring that
/// enum, so this module doesn't need to track it if it ever changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TemplateNode {
    pub id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// 0 = high, 2 = low, anything else (including the default, 1) = medium
    /// -- matches `jcode_plan::bridge::priority_string`'s exact mapping.
    #[serde(default = "default_priority")]
    pub priority: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

fn default_priority() -> u8 {
    1
}

/// A saved, reusable, parameterized swarm task graph. Persisted at
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

/// The safe charset for anything echoed back into tool-output text or
/// interpolated into an error string: `name` (template), node `id`, and
/// parameter `name`. Gemini review, 2026-08-30: the original charset check
/// only covered the template's own `name` -- node ids and parameter names
/// remained unrestricted despite being echoed into `run`'s success message
/// (joined node ids) and into duplicate-id/dangling-dependency/missing-
/// parameter error strings, reopening the same newline/ANSI-escape
/// injection class for two sibling identifiers.
fn is_safe_identifier_charset(value: &str) -> bool {
    value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Strip every ASCII control character (0x00-0x1F and 0x7F — includes
/// newline/CR/tab and, critically, ESC, the character that starts every
/// ANSI escape sequence) before embedding an otherwise-unvalidated,
/// caller-supplied string into output-facing text. Second-pass Gemini
/// review, 2026-08-30: the fix for the `run`/`list` injection class missed
/// `load()`'s own "no workflow template named '{name}'" error — a lookup
/// by name that fails never goes through [`WorkflowTemplate::validate`] at
/// all (there's no template to validate), so the raw, never-checked input
/// was still reaching tool-output/error text unfiltered. Used only for
/// *display*; never for the actual filesystem lookup or comparison, so this
/// can't be used to smuggle a match past `template_path`'s own sanitizing.
fn sanitize_for_display(value: &str) -> String {
    value.chars().filter(|c| !c.is_ascii_control()).collect()
}

impl WorkflowTemplate {
    /// Validate structural invariants that are cheap to check up front and
    /// would otherwise surface as a confusing failure much later (a
    /// dangling `depends_on` reference breaking swarm plan construction, a
    /// duplicate node id silently shadowing one node with another).
    /// Called by both [`save`] (refuse to persist something already known
    /// to be broken) and [`instantiate`] (refuse to hand back a plan built
    /// from a template that was somehow saved before this check existed, or
    /// edited by hand on disk).
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            anyhow::bail!("workflow template name must not be empty");
        }
        // The name isn't just a filename component (already handled by
        // `sanitize_name` at the storage layer) -- it's also echoed back
        // verbatim in `list()`'s output text, which an agent reads as
        // normal tool output. A name containing a newline could inject a
        // fake extra list entry; ANSI escapes could spoof terminal UI.
        // Restrict to the same safe charset `sanitize_name` already uses,
        // so a validated template's name is never in need of sanitizing.
        if !is_safe_identifier_charset(&self.name) {
            anyhow::bail!(
                "workflow template name must contain only letters, digits, '-', or '_'"
            );
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
            if !is_safe_identifier_charset(&node.id) {
                anyhow::bail!(
                    "workflow template '{}' has a node id that must contain only letters, \
                     digits, '-', or '_'",
                    self.name
                );
            }
            if !seen_ids.insert(node.id.as_str()) {
                anyhow::bail!(
                    "workflow template '{}' has a duplicate node id",
                    self.name
                );
            }
        }
        let known_ids: std::collections::HashSet<&str> =
            self.nodes.iter().map(|n| n.id.as_str()).collect();
        for node in &self.nodes {
            for dep in &node.depends_on {
                if !known_ids.contains(dep.as_str()) {
                    anyhow::bail!(
                        "workflow template '{}': node '{}' depends_on unknown node '{}'",
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
            if !is_safe_identifier_charset(&param.name) {
                anyhow::bail!(
                    "workflow template '{}' has a parameter name that must contain only \
                     letters, digits, '-', or '_'",
                    self.name
                );
            }
            // Safe to echo `param.name` from here on: the charset check
            // just above already guarantees it can't inject a newline or
            // ANSI escape into this (or any later) tool-output text.
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
    /// all nodes and produce real `TaskGraphNodeSpec`s, ready to hand
    /// straight to `Request::CommSeedGraph`.
    ///
    /// Refuses (does not silently proceed) on:
    /// - a required parameter (no `default`) with no value supplied,
    /// - any `{{...}}`-shaped placeholder left over after substitution --
    ///   this means the template referenced a parameter it never declared,
    ///   which is a template authoring bug, not something to paper over by
    ///   leaving the literal placeholder text in a real task's content.
    pub fn instantiate(&self, values: &HashMap<String, String>) -> Result<Vec<TaskGraphNodeSpec>> {
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

        let mut specs = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let content = substitute(&node.content);
            if let Some(leftover) = find_unresolved_placeholder(&content) {
                anyhow::bail!(
                    "workflow template '{}': node '{}' content references \
                     undeclared parameter '{}'",
                    self.name,
                    node.id,
                    leftover
                );
            }

            specs.push(TaskGraphNodeSpec {
                id: node.id.clone(),
                content,
                kind: node.kind.clone(),
                depends_on: node.depends_on.clone(),
                priority: node.priority,
            });
        }
        Ok(specs)
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
///
/// **Re-validates before returning** (Gemini review, 2026-08-30): `save()`
/// validates before writing, but a hand-edited, imported, or legacy file on
/// disk bypasses that entirely -- without this, `load()` could hand back a
/// `WorkflowTemplate` whose `name`/node `id`s/parameter names never went
/// through the safe-charset check, exactly the gap that made it possible
/// for a caller of `run` to still end up echoing an unsafe identifier into
/// tool-output text even after that call site was fixed to use the loaded
/// `template.name` instead of the raw, unvalidated input. `list()` (below)
/// reads files directly by path rather than by name, so it can't route
/// through this function, but applies the same `.validate()` check itself
/// for the identical reason.
pub fn load(name: &str) -> Result<WorkflowTemplate> {
    let path = template_path(name)?;
    if !path.exists() {
        anyhow::bail!("no workflow template named '{}'", sanitize_for_display(name));
    }
    let template: WorkflowTemplate = crate::storage::read_json(&path)?;
    template.validate().with_context(|| {
        format!(
            "workflow template file at {} failed validation on load",
            path.display()
        )
    })?;
    Ok(template)
}

/// List the names of every saved template, sorted. Reads the template's own
/// `name` field out of each file rather than trusting the sanitized
/// filename to round-trip back to the original name (it doesn't always --
/// `sanitize_name` is lossy for names containing characters outside
/// `[A-Za-z0-9_-]`).
///
/// **Re-validates each template before including its name** (Gemini
/// review, 2026-08-30): previously read `name` straight from disk with no
/// check at all, so a hand-edited/imported/legacy file with an unsafe name
/// bypassed the charset check entirely and was echoed verbatim into this
/// function's tool-output text. A template that fails validation is
/// silently skipped, same as one that fails to parse at all just above --
/// this is a listing, not a place to surface a half-broken file's error.
pub fn list() -> Result<Vec<String>> {
    let dir = workflows_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .with_context(|| format!("reading workflows dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(template) = crate::storage::read_json::<WorkflowTemplate>(&path)
            && template.validate().is_ok()
        {
            names.push(template.name);
        }
    }
    names.sort();
    Ok(names)
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
                    kind: Some("critique".to_string()),
                    priority: 0,
                    depends_on: vec![],
                },
                TemplateNode {
                    id: "fix".to_string(),
                    content: "Fix what the {{subsystem}} review found".to_string(),
                    kind: Some("fix".to_string()),
                    priority: 1,
                    depends_on: vec!["review".to_string()],
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

        let specs = template.instantiate(&values).expect("instantiate");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].content, "Review auth for medium+ issues");
        assert_eq!(specs[0].kind.as_deref(), Some("critique"));
        assert_eq!(specs[0].priority, 0);
        assert_eq!(specs[1].content, "Fix what the auth review found");
        assert_eq!(specs[1].depends_on, vec!["review".to_string()]);
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
    fn validate_rejects_a_dangling_depends_on_reference() {
        let mut template = sample_template();
        template.nodes[1].depends_on = vec!["does-not-exist".to_string()];
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

    /// Gemini review, 2026-08-30: the charset check originally covered only
    /// the template `name` -- node `id` was unrestricted despite being
    /// echoed into `run`'s success message and into error text.
    #[test]
    fn validate_rejects_a_node_id_with_unsafe_characters() {
        let mut template = sample_template();
        template.nodes[0].id = "legit\n- fake-injected-entry".to_string();
        let err = template.validate().unwrap_err().to_string();
        assert!(err.contains("letters, digits"), "got: {err}");
    }

    /// Same gap, parameter names.
    #[test]
    fn validate_rejects_a_parameter_name_with_unsafe_characters() {
        let mut template = sample_template();
        template.parameters[0].name = "legit\n- fake-injected-entry".to_string();
        let err = template.validate().unwrap_err().to_string();
        assert!(err.contains("letters, digits"), "got: {err}");
    }

    /// Gemini review, 2026-08-30: `save()` validates before writing, but a
    /// hand-edited/imported/legacy file bypasses that -- `load()` must
    /// re-validate on the way back out, not trust whatever is on disk.
    #[tokio::test]
    async fn load_rejects_a_hand_edited_file_with_an_unsafe_name() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());

        let mut unsafe_template = sample_template();
        unsafe_template.name = "legit\n- fake-injected-entry".to_string();
        let dir = workflows_dir().expect("workflows dir");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // Bypass save()'s own validate() call entirely -- simulating a
        // hand-edited or imported file, not one this module ever wrote.
        crate::storage::write_json_fast(
            &dir.join("hand-edited.json"),
            &unsafe_template,
        )
        .expect("write raw file");

        let result = load("hand-edited");
        assert!(
            result.is_err(),
            "load() must refuse a template that fails validation, not hand it back trusted"
        );
    }

    /// Same gap, `list()`'s own separate read path.
    #[tokio::test]
    async fn list_skips_a_hand_edited_file_that_fails_validation() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());

        let mut unsafe_template = sample_template();
        unsafe_template.name = "legit\n- fake-injected-entry".to_string();
        let dir = workflows_dir().expect("workflows dir");
        std::fs::create_dir_all(&dir).expect("mkdir");
        crate::storage::write_json_fast(&dir.join("hand-edited.json"), &unsafe_template)
            .expect("write raw file");

        let mut valid = sample_template();
        valid.name = "a-valid-one".to_string();
        save(&valid).expect("save valid");

        assert_eq!(
            list().expect("list"),
            vec!["a-valid-one".to_string()],
            "an unsafe on-disk template must be silently skipped, not echoed into the listing"
        );
    }

    #[test]
    fn validate_rejects_a_template_with_no_nodes() {
        let mut template = sample_template();
        template.nodes.clear();
        assert!(template.validate().is_err());
    }

    /// Regression test for a real finding from an external review: `name`
    /// is echoed verbatim in `list()`'s tool-output text, so a newline or
    /// ANSI escape sequence in it could inject a fake list entry or spoof
    /// terminal output for whoever reads that response (a human, or an
    /// agent treating it as normal tool text).
    #[test]
    fn validate_rejects_a_name_with_unsafe_characters() {
        let mut template = sample_template();
        template.name = "legit\n- fake-injected-entry".to_string();
        let err = template.validate().unwrap_err().to_string();
        assert!(err.contains("letters, digits"), "got: {err}");
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

    /// Second-pass Gemini review, 2026-08-30: a lookup-by-name that fails
    /// never reaches `validate()` (there's no template to validate) --
    /// the raw, unvalidated `name` argument was still landing unfiltered
    /// in this error's text, reopening the same class of issue the
    /// `run`/`list` fixes closed elsewhere.
    #[tokio::test]
    async fn load_error_for_an_unknown_name_never_echoes_raw_control_characters() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());

        let err = load("legit\n\x1b[31m-fake-entry").unwrap_err().to_string();
        assert!(
            !err.contains('\n') && !err.contains('\x1b'),
            "error text must never carry through an embedded newline or ESC byte verbatim, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn list_returns_saved_names_sorted() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());

        assert_eq!(list().expect("list empty"), Vec::<String>::new());

        let mut b = sample_template();
        b.name = "zzz-last".to_string();
        save(&b).expect("save b");
        let mut a = sample_template();
        a.name = "aaa-first".to_string();
        save(&a).expect("save a");

        assert_eq!(
            list().expect("list"),
            vec!["aaa-first".to_string(), "zzz-last".to_string()]
        );
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
