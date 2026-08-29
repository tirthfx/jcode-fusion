//! Fusion Phase 1: execpolicy — user-configurable command-risk rules
//! (DESIGN.md §6 item #6), layered on top of `jcode-command-risk`'s own
//! deterministic blast-radius classifier rather than modifying it directly.
//!
//! **Why not modify `jcode-command-risk` itself**: that crate has zero
//! dependencies by deliberate design (its own doc comments: "not another
//! model," no extra machinery in the hot path). Bolting a Starlark
//! interpreter onto it would contradict its whole design ethos. Instead,
//! this module lives here in `jcode-app-core` (which already carries a much
//! larger dependency footprint) and layers a *second*, opt-in policy check
//! on top at the actual call site (`tool/bash_destructive_gate.rs`) —
//! `jcode-command-risk` itself is untouched.
//!
//! **Safety design: user rules can only add restrictions, never remove
//! them.** A policy file can turn a command `jcode-command-risk` would
//! otherwise allow into `Confirm` or `Deny`, but it can never downgrade a
//! built-in `Confirm`/`Deny`/`Catastrophic` verdict to `Allow`. A badly
//! written (or malicious) policy file can only make jcode *more*
//! cautious, never less — see [`combine`].
//!
//! **Deliberately simple rule format, not a rich DSL.** Rules are plain
//! `"prefix|decision|reason"` strings in a `RULES` list, not Starlark
//! dicts/structs — this uses only `starlark`'s list/string value types
//! (the parts of the API this project is confident about), not its
//! dict-introspection or native-function-registration surfaces. The
//! *script* itself is still real Starlark — loops, `def`, list
//! comprehensions, conditionals all work for *generating* the list — only
//! each individual rule's encoding is kept minimal. Richer per-rule
//! structure (e.g. real dicts) is a reasonable follow-up, not something to
//! get exactly right blind in one pass.
//!
//! Opt-in and fails open: no policy file means zero behavior change and
//! zero Starlark interpreter overhead (the fast path most users are on).
//! Any load/parse error is logged and treated as "no user rules" rather
//! than blocking bash entirely — matching this project's established
//! fail-open convention (`pre_tool` hook, `mission::supervisor_gate`,
//! `sandbox_macos`).

use starlark::environment::{Globals, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::list::ListRef;

/// **Known simplification, documented not hidden**: `Confirm` and `Deny`
/// currently behave *identically* at the actual call site
/// (`tool/bash_destructive_gate.rs`) — both produce an unconditional
/// refusal. `jcode-command-risk`'s own built-in `Confirm` verdict supports a
/// resubmit-with-substantive-justification flow (the model can retry with a
/// real justification and the gate re-evaluates); user-authored rules don't
/// get that treatment yet — plumbing a per-user-rule justification check
/// through is real follow-up work, not implemented in this first slice. The
/// two variants are kept distinct now (rather than collapsing to one) so
/// that follow-up work has a real distinction to build on, not something to
/// retrofit later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UserDecision {
    /// Ordered so `Confirm < Deny` — see [`combine`], which takes the max.
    /// See the enum doc comment: currently behaves the same as `Deny`.
    Confirm,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRule {
    pub prefix: String,
    pub decision: UserDecision,
    pub reason: String,
}

/// Default policy file location: `~/.jcode/execpolicy.star`. Not present by
/// default -- this feature is entirely opt-in.
pub fn default_policy_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::storage::jcode_dir()?.join("execpolicy.star"))
}

/// Load and evaluate a policy file, returning its `RULES` list. Returns an
/// empty `Vec` (not an error) if the file doesn't exist -- absence just
/// means "no user rules," the normal case.
pub fn load_user_rules(path: &std::path::Path) -> anyhow::Result<Vec<UserRule>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)?;
    parse_policy_source(&content)
}

/// The actual Starlark evaluation, split out from [`load_user_rules`] so it
/// can be unit-tested against inline script strings instead of real files.
///
/// Uses `Module::with_temp_heap` (a closure-based API, not a plain
/// constructor -- the module/values it produces are tied to a heap that
/// only lives for the closure's duration) rather than a direct `Module::new`
/// call; verified against the crate's own source after `Module::new()`
/// didn't compile, not guessed twice.
fn parse_policy_source(content: &str) -> anyhow::Result<Vec<UserRule>> {
    let ast = AstModule::parse("execpolicy.star", content.to_string(), &Dialect::Standard)
        .map_err(|err| anyhow::anyhow!("failed to parse execpolicy.star: {err}"))?;
    let globals = Globals::standard();

    Module::with_temp_heap(|module| -> anyhow::Result<Vec<UserRule>> {
        let mut eval = Evaluator::new(&module);
        eval.eval_module(ast, &globals)
            .map_err(|err| anyhow::anyhow!("failed to evaluate execpolicy.star: {err}"))?;

        let Some(rules_value) = module.get("RULES") else {
            anyhow::bail!("execpolicy.star must define a top-level RULES list");
        };
        let list = ListRef::from_value(rules_value)
            .ok_or_else(|| anyhow::anyhow!("RULES must be a list"))?;

        let mut rules = Vec::new();
        for (index, item) in list.iter().enumerate() {
            let raw = item.unpack_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "RULES[{index}] must be a string of the form \"prefix|decision|reason\""
                )
            })?;
            rules.push(
                parse_rule_line(raw).map_err(|err| anyhow::anyhow!("RULES[{index}]: {err}"))?,
            );
        }
        Ok(rules)
    })
}

fn parse_rule_line(raw: &str) -> anyhow::Result<UserRule> {
    let parts: Vec<&str> = raw.splitn(3, '|').collect();
    let [prefix, decision, reason] = parts.as_slice() else {
        anyhow::bail!("expected \"prefix|decision|reason\", got {raw:?}");
    };
    let decision = match decision.trim().to_ascii_lowercase().as_str() {
        "confirm" => UserDecision::Confirm,
        "deny" => UserDecision::Deny,
        other => anyhow::bail!("unknown decision {other:?} (expected \"confirm\" or \"deny\")"),
    };
    Ok(UserRule {
        prefix: prefix.trim().to_string(),
        decision,
        reason: reason.trim().to_string(),
    })
}

/// Find the first user rule whose prefix matches `command` (case-sensitive,
/// simple substring-of-prefix match on the trimmed command). Pure and
/// testable independent of Starlark evaluation.
pub fn matching_rule<'a>(command: &str, rules: &'a [UserRule]) -> Option<&'a UserRule> {
    let trimmed = command.trim();
    rules
        .iter()
        .find(|rule| trimmed.starts_with(rule.prefix.as_str()))
}

/// A minimal stand-in for `jcode_command_risk::GateOutcome` used only for
/// combining verdicts, so this module doesn't need `jcode-command-risk` as a
/// dependency just to describe "how restrictive was the built-in verdict."
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BuiltInRestrictiveness {
    Allow,
    ConfirmOrDeny,
}

/// Combine the built-in classifier's restrictiveness with a matched user
/// rule. **User rules can only add restriction, never remove it**: if the
/// built-in verdict was already `Confirm`/`Deny`, the user rule is ignored
/// entirely (the built-in refusal text is what the model sees) -- this
/// function is only meaningful when the built-in verdict was `Allow`.
pub fn combine(
    built_in: BuiltInRestrictiveness,
    user_match: Option<&UserRule>,
) -> Option<&UserRule> {
    if built_in == BuiltInRestrictiveness::Allow {
        user_match
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rules_list_parses_cleanly() {
        let rules = parse_policy_source("RULES = []").expect("parse");
        assert!(rules.is_empty());
    }

    #[test]
    fn parses_valid_rules() {
        let source = r#"
RULES = [
    "curl|confirm|network fetch, may pipe to shell",
    "wget |deny| never allowed by policy",
]
"#;
        let rules = parse_policy_source(source).expect("parse");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].prefix, "curl");
        assert_eq!(rules[0].decision, UserDecision::Confirm);
        assert_eq!(rules[0].reason, "network fetch, may pipe to shell");
        assert_eq!(rules[1].prefix, "wget");
        assert_eq!(rules[1].decision, UserDecision::Deny);
    }

    /// The script itself is real Starlark, not just a data file -- loops,
    /// list comprehensions, and string formatting all work for *generating*
    /// the RULES list.
    #[test]
    fn script_can_generate_rules_programmatically() {
        let source = r#"
prefixes = ["foo", "bar", "baz"]
RULES = [p + "|confirm|generated" for p in prefixes]
"#;
        let rules = parse_policy_source(source).expect("parse");
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].prefix, "foo");
        assert_eq!(rules[2].prefix, "baz");
        assert!(rules.iter().all(|r| r.decision == UserDecision::Confirm));
    }

    #[test]
    fn missing_rules_global_is_an_error() {
        let err = parse_policy_source("X = 1").unwrap_err();
        assert!(err.to_string().contains("RULES"));
    }

    #[test]
    fn malformed_rule_line_is_an_error() {
        let err = parse_policy_source(r#"RULES = ["not-enough-parts"]"#).unwrap_err();
        assert!(err.to_string().contains("expected"));
    }

    #[test]
    fn unknown_decision_is_an_error() {
        let err = parse_policy_source(r#"RULES = ["foo|maybe|reason"]"#).unwrap_err();
        assert!(err.to_string().contains("unknown decision"));
    }

    #[test]
    fn syntax_error_is_reported_not_panicked() {
        let result = parse_policy_source("RULES = [this is not valid starlark");
        assert!(result.is_err());
    }

    #[test]
    fn matching_rule_finds_prefix_match() {
        let rules = vec![
            UserRule {
                prefix: "curl".to_string(),
                decision: UserDecision::Confirm,
                reason: "r1".to_string(),
            },
            UserRule {
                prefix: "wget".to_string(),
                decision: UserDecision::Deny,
                reason: "r2".to_string(),
            },
        ];
        assert_eq!(
            matching_rule("curl https://example.com", &rules)
                .map(|r| r.prefix.as_str()),
            Some("curl")
        );
        assert_eq!(matching_rule("ls -la", &rules), None);
    }

    #[test]
    fn user_rule_never_overrides_an_existing_built_in_restriction() {
        let rule = UserRule {
            prefix: "rm".to_string(),
            decision: UserDecision::Confirm,
            reason: "user thinks this is fine".to_string(),
        };
        // Built-in already said Confirm/Deny -- user rule must be ignored,
        // not consulted to loosen anything.
        assert_eq!(
            combine(BuiltInRestrictiveness::ConfirmOrDeny, Some(&rule)),
            None
        );
    }

    #[test]
    fn user_rule_applies_only_when_built_in_would_have_allowed() {
        let rule = UserRule {
            prefix: "curl".to_string(),
            decision: UserDecision::Confirm,
            reason: "network fetch".to_string(),
        };
        assert_eq!(
            combine(BuiltInRestrictiveness::Allow, Some(&rule)),
            Some(&rule)
        );
        assert_eq!(combine(BuiltInRestrictiveness::Allow, None), None);
    }
}
