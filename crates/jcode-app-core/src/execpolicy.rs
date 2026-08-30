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
    /// `Confirm` sorts before `Deny` (derived `Ord`, declaration order).
    /// **Doc correction, Gemini review 2026-08-30**: an earlier version of
    /// this comment claimed `combine` "takes the max" of a `Confirm`/`Deny`
    /// pair -- it never has; `combine` just passes through whichever single
    /// rule `matching_rule` already picked (first-match-wins), it doesn't
    /// compare or resolve multiple matches at all. Corrected here rather
    /// than left to mislead a future implementer into assuming a rule-order-
    /// independent conflict resolution that doesn't exist. See the enum
    /// doc comment: `Confirm` currently behaves identically to `Deny`
    /// regardless.
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
/// A policy file is a handful of `"prefix|decision|reason"` lines -- a few
/// KiB at most for even a large rule set. Generous cap, real bound.
const MAX_POLICY_FILE_BYTES: u64 = 1024 * 1024;

pub fn load_user_rules(path: &std::path::Path) -> anyhow::Result<Vec<UserRule>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    // Gemini review, 2026-08-30: `read_to_string` had no size cap after
    // only a `path.exists()` check (a TOCTOU-prone pattern on its own) --
    // if `~/.jcode/execpolicy.star` is or becomes a symlink to an
    // unbounded or special file, reading it could block or exhaust memory
    // before the Starlark parser (which itself now has its own tick/heap
    // limits, but only once evaluation actually starts) ever gets a chance
    // to fail closed. Checking metadata size first doesn't fully close the
    // TOCTOU window (the file could still change between the check and the
    // read) but bounds the failure mode to "read up to the cap," not
    // "block/exhaust memory on an arbitrarily large target."
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_POLICY_FILE_BYTES {
        anyhow::bail!(
            "execpolicy.star is {} bytes, over the {}-byte limit -- refusing to read it",
            metadata.len(),
            MAX_POLICY_FILE_BYTES
        );
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
/// Ticks (roughly: one function call or one loop backedge) and heap bytes
/// permitted for one policy-file evaluation. Generous for what this file is
/// meant to hold (a `RULES` list, at most built with simple loops/
/// comprehensions) but real bounds: Gemini review, 2026-08-30, found this
/// evaluator previously had none at all, so a huge string multiplication or
/// a large comprehension in `~/.jcode/execpolicy.star` (malicious or just a
/// mistake) could hang the process or exhaust memory — worse than the
/// documented "fail closed to the built-in classifier" behavior, since it
/// never gets the chance to fail at all.
const MAX_POLICY_EVAL_TICKS: u64 = 1_000_000;
const MAX_POLICY_EVAL_HEAP_BYTES: usize = 16 * 1024 * 1024;

fn parse_policy_source(content: &str) -> anyhow::Result<Vec<UserRule>> {
    let ast = AstModule::parse("execpolicy.star", content.to_string(), &Dialect::Standard)
        .map_err(|err| anyhow::anyhow!("failed to parse execpolicy.star: {err}"))?;
    let globals = Globals::standard();

    Module::with_temp_heap(|module| -> anyhow::Result<Vec<UserRule>> {
        let mut eval = Evaluator::new(&module);
        // Both limits are enforced by starlark-rust itself during
        // eval_module (verified against the crate's own test,
        // `test_tick_count_limit`), not something this module has to poll
        // for manually — an over-limit script surfaces as a real `Err`
        // here, which the existing fail-open handling at the call site
        // (`tool/bash_destructive_gate.rs::user_rules`) already treats the
        // same as any other malformed policy file.
        eval.set_max_tick_count(MAX_POLICY_EVAL_TICKS)
            .expect("tick limit is set exactly once, non-zero");
        eval.set_max_heap_size(MAX_POLICY_EVAL_HEAP_BYTES)
            .expect("heap limit is set exactly once, non-zero");
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
    let prefix = prefix.trim().to_string();
    // Gemini review, 2026-08-30: a rule line with an empty/whitespace-only
    // prefix segment (e.g. `" | deny | reason"`) previously parsed
    // successfully to `prefix == ""` -- since every string starts with the
    // empty string, `matching_rule` would then match and block literally
    // every bash command for the rest of the process. Refuse this at parse
    // time instead of letting it silently become a footgun.
    if prefix.is_empty() {
        anyhow::bail!("rule prefix must not be empty (would match every command): {raw:?}");
    }
    let decision = match decision.trim().to_ascii_lowercase().as_str() {
        "confirm" => UserDecision::Confirm,
        "deny" => UserDecision::Deny,
        other => anyhow::bail!("unknown decision {other:?} (expected \"confirm\" or \"deny\")"),
    };
    Ok(UserRule {
        prefix,
        decision,
        reason: reason.trim().to_string(),
    })
}

/// Find the first user rule whose prefix matches `command` (case-sensitive,
/// simple substring-of-prefix match on the trimmed command). Pure and
/// testable independent of Starlark evaluation.
/// Gemini review, 2026-08-30: a raw `starts_with` check on the command
/// as-issued let irregular whitespace defeat an intended rule -- a rule
/// prefix of `"rm "` (author intent: match `rm` followed by anything) would
/// not match `"rm\t-rf"` or `"rm  -rf"` (a tab, or a double space), since
/// the literal bytes right after `rm` don't equal a single space. Collapsing
/// runs of whitespace in the command to single spaces before matching
/// closes that specific evasion. Deliberately narrow: this does not add
/// full tokenization or a "must match a whole word" boundary check (e.g.
/// whether prefix `"rm"` should also match `"rmdir"`) -- that's a
/// legitimate but separate design question about how specific a prefix
/// rule is meant to be, not a bug, and changing it risks silently altering
/// the meaning of existing policy files that rely on short prefixes on
/// purpose.
pub fn matching_rule<'a>(command: &str, rules: &'a [UserRule]) -> Option<&'a UserRule> {
    let normalized = command.split_whitespace().collect::<Vec<_>>().join(" ");
    rules
        .iter()
        .find(|rule| normalized.starts_with(rule.prefix.as_str()))
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

    /// Gemini review, 2026-08-30: `load_user_rules` previously had no size
    /// cap after only a `path.exists()` check.
    #[test]
    fn load_user_rules_refuses_a_file_over_the_size_cap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("execpolicy.star");
        let oversized = "#".repeat((MAX_POLICY_FILE_BYTES + 1) as usize);
        std::fs::write(&path, oversized).expect("write oversized file");

        let err = load_user_rules(&path).unwrap_err();
        assert!(err.to_string().contains("limit"), "got: {err}");
    }

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

    /// Gemini review, 2026-08-30: an empty prefix previously parsed
    /// successfully and, since every string starts with the empty string,
    /// would have matched -- and blocked -- every single bash command.
    #[test]
    fn an_empty_prefix_is_rejected_not_silently_accepted() {
        let err = parse_policy_source(r#"RULES = [" |deny|reason"]"#).unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "got: {err}"
        );
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

    /// Gemini review, 2026-08-30: a rule intended to escalate "rm " (rm
    /// followed by anything) previously failed to match irregular
    /// whitespace between the command and its arguments.
    #[test]
    fn matching_rule_survives_irregular_inner_whitespace() {
        let rules = vec![UserRule {
            prefix: "rm ".to_string(),
            decision: UserDecision::Deny,
            reason: "no rm allowed".to_string(),
        }];
        assert!(matching_rule("rm -rf /tmp/x", &rules).is_some());
        assert!(
            matching_rule("rm  -rf /tmp/x", &rules).is_some(),
            "a double space must not evade the rule"
        );
        assert!(
            matching_rule("rm\t-rf /tmp/x", &rules).is_some(),
            "a tab must not evade the rule"
        );
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

    /// Gemini review, 2026-08-30: the evaluator previously had no tick/heap
    /// limit at all, so a policy script that just burns CPU (no I/O, no
    /// network — a plain loop) could hang the process indefinitely rather
    /// than failing closed like every other malformed-policy case.
    #[test]
    fn a_runaway_loop_is_stopped_by_the_tick_limit_not_left_to_hang() {
        let source = r#"
RULES = []
for i in range(100000000):
    RULES = RULES + [str(i)]
"#;
        let result = parse_policy_source(source);
        assert!(
            result.is_err(),
            "a script that would run far longer than the tick budget must fail, not hang"
        );
    }

    #[test]
    fn an_ordinary_policy_well_within_the_limits_still_parses_fine() {
        let source = r#"
RULES = ["prefix" + str(i) + "|confirm|reason" for i in range(50)]
"#;
        let rules = parse_policy_source(source).expect("a modest, realistic policy must parse");
        assert_eq!(rules.len(), 50);
    }
}
