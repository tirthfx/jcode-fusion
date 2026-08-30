//! Fusion Phase 1: Guardian, an automatic safety backstop for ambient-mode
//! permission requests (DESIGN.md §6 item #3).
//!
//! **Scope decision** (see PROGRESS.md): ambient-session-only, matching the
//! existing `SafetySystem`/`request_permission` machinery. jcode has no
//! general-session approval-prompt system to intercept (confirmed via a
//! real source read, not assumption) — normal interactive tool calls never
//! go through this path at all, so Guardian only ever sees requests already
//! routed through `crate::tool::ambient::RequestPermissionTool`.
//!
//! **Deliberately conservative first slice: deny-only, never auto-approves.**
//! Codex's real Guardian both auto-approves clearly-safe requests (cutting
//! human interruptions) and auto-denies clearly-bad ones. This slice only
//! implements the second half. A trustworthy "auto-approve" verdict needs
//! either a genuine semantic judge (a real LLM call — not something
//! reliably buildable/testable without live provider credentials in this
//! environment, the same constraint noted throughout this project's
//! PROGRESS.md) or a far more structured risk signal than the free-text
//! `action`/`description`/`rationale` fields callers currently provide.
//! Auto-approving on the mere *absence* of risk keywords would be a much
//! weaker, overclaiming signal than denying on their *presence* — a missed
//! keyword just means "no verdict," never "definitely safe." So: this
//! deterministic keyword layer only ever denies (a real, conservative
//! safety backstop) or defers to the existing human queue, unchanged.
//! Interruption reduction (the auto-approve half) is real follow-up work,
//! not something to fake with a weak heuristic here.

use crate::safety::PermissionRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardianDecision {
    /// Deny immediately — do not queue this for human review at all.
    Deny,
    /// No confident verdict either way. Falls through to the existing
    /// human queue (`SafetySystem::request_permission`), completely
    /// unchanged from today's behavior.
    Undecided,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardianVerdict {
    pub decision: GuardianDecision,
    pub reason: String,
}

/// Codified risk taxonomy, mirroring the categories Codex's own Guardian
/// documents (DESIGN.md §4.2) — chosen for consistency with prior art in
/// this project, not reinvented independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskCategory {
    DestructiveAction,
    CredentialProbing,
    DataExfiltration,
    PersistentSecurityWeakening,
}

impl RiskCategory {
    fn label(self) -> &'static str {
        match self {
            Self::DestructiveAction => "destructive action",
            Self::CredentialProbing => "credential probing",
            Self::DataExfiltration => "data exfiltration",
            Self::PersistentSecurityWeakening => "persistent security weakening",
        }
    }

    /// Deliberately a small, curated, high-confidence keyword list per
    /// category rather than an exhaustive one — false negatives here just
    /// mean "defer to the human queue" (safe), while false positives waste
    /// a human's time re-approving something Guardian wrongly denied. Erring
    /// toward precision over recall for a deny-only first slice.
    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::DestructiveAction => &[
                "rm -rf",
                "drop table",
                "drop database",
                "delete all",
                "force push",
                "git push --force",
                "format the disk",
                "wipe the",
                "truncate table",
                "delete the repository",
                "permanently delete",
            ],
            Self::CredentialProbing => &[
                "private key",
                "ssh key",
                "api key",
                "access token",
                "auth token",
                ".env file",
                "password file",
                "credentials file",
                "secret key",
                "read the .ssh",
                "dump environment variables",
            ],
            Self::DataExfiltration => &[
                "upload to external",
                "send to external",
                "post the contents",
                "exfiltrate",
                "upload the repository",
                "email the contents",
                "publish the source code",
                "share externally",
            ],
            Self::PersistentSecurityWeakening => &[
                "disable the sandbox",
                "disable sandboxing",
                "chmod 777",
                "disable the firewall",
                "disable authentication",
                "remove the permission check",
                "bypass the safety",
                "disable guardian",
                "weaken the sandbox",
            ],
        }
    }
}

const RISK_CATEGORIES: &[RiskCategory] = &[
    RiskCategory::DestructiveAction,
    RiskCategory::CredentialProbing,
    RiskCategory::DataExfiltration,
    RiskCategory::PersistentSecurityWeakening,
];

/// Recursively flatten a JSON value into `out`, space-separated, for keyword
/// matching. `PermissionRequest.context` is caller-supplied free-form JSON
/// (Gemini review, 2026-08-30: a caller could keep `action`/`description`/
/// `rationale` benign while putting the actual risky instruction here, e.g.
/// `action="system_cleanup", context={"commands": ["rm -rf /"]}` — this was
/// previously never inspected at all). Object keys are included too, since
/// a key name itself can carry the risky text (e.g. `{"rm -rf /": true}`).
///
/// **Known, deliberately unfixed gap** (second-pass Gemini review,
/// 2026-08-30): object keys act as word separators in the flattened
/// output, so a multi-word keyword can still be split across sibling
/// values with a key in between -- `{"1": "rm", "2": "-rf"}` flattens to
/// `" 1 rm 2 -rf"`, which no longer contains the contiguous substring
/// `"rm -rf"`. Rated low/medium, not high: this is a keyword-matching
/// precision gap on the *existing* keyword-substring approach, not a new
/// hole opened by this slice's context-inspection fix -- the exact same
/// whitespace-adjacency assumption the top-level `normalize_whitespace`
/// fix already relies on has this same class of limit. Closing it properly
/// needs a real tokenizer over the flattened text (matching keywords as a
/// sequence of tokens, not a literal substring), a bigger change than
/// patching this function; recorded here rather than silently left for a
/// future session to rediscover as a surprise.
fn flatten_json_into(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::String(s) => {
            out.push(' ');
            out.push_str(s);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                flatten_json_into(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                out.push(' ');
                out.push_str(key);
                flatten_json_into(val, out);
            }
        }
        serde_json::Value::Number(n) => {
            out.push(' ');
            out.push_str(&n.to_string());
        }
        serde_json::Value::Bool(b) => {
            out.push(' ');
            out.push_str(if *b { "true" } else { "false" });
        }
        serde_json::Value::Null => {}
    }
}

/// Collapse any run of whitespace — including embedded newlines/tabs, and
/// the double space produced when a keyword is split across two fields
/// with an empty one in between (`action="rm"`, `description=""`,
/// `rationale="-rf"` used to format as `"rm  -rf"`, which does not contain
/// `"rm -rf"`) — into single spaces, so irregular whitespace alone can't
/// defeat a multi-word keyword. Gemini review, 2026-08-30.
fn normalize_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Deterministic, keyword-based adjudication of one ambient-mode permission
/// request. Pure function — no I/O, no async, trivially unit-testable. See
/// module docs for why this only ever denies or defers, never approves.
///
/// **Known, deliberately out-of-scope gap** (Gemini review, 2026-08-30):
/// non-ASCII homoglyph evasion (e.g. a Cyrillic а substituted for a Latin
/// a) is not defended against — `.to_lowercase()` case-folds correctly but
/// does not detect visually-similar different codepoints. Closing that
/// needs a real confusables table (e.g. the `unicode-security` crate), a
/// real new dependency, not something to bolt on inside this pure-logic
/// function. Recorded here rather than silently left unaddressed.
pub fn adjudicate(request: &PermissionRequest) -> GuardianVerdict {
    let mut haystack = format!(
        "{} {} {}",
        request.action, request.description, request.rationale
    );
    if let Some(context) = &request.context {
        flatten_json_into(context, &mut haystack);
    }
    let haystack = normalize_whitespace(&haystack).to_lowercase();

    for category in RISK_CATEGORIES {
        for keyword in category.keywords() {
            if haystack.contains(keyword) {
                return GuardianVerdict {
                    decision: GuardianDecision::Deny,
                    reason: format!(
                        "Guardian auto-denied: matched a {} risk pattern (\"{}\"). \
                         This is a deterministic keyword heuristic, not a semantic \
                         review — if this was a false positive, rephrase the request \
                         to avoid the flagged phrase, or ask the user directly.",
                        category.label(),
                        keyword
                    ),
                };
            }
        }
    }

    GuardianVerdict {
        decision: GuardianDecision::Undecided,
        reason: "no risk pattern matched; deferring to human review".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::Urgency;
    use chrono::Utc;

    fn request(action: &str, description: &str, rationale: &str) -> PermissionRequest {
        PermissionRequest {
            id: "test-id".to_string(),
            action: action.to_string(),
            description: description.to_string(),
            rationale: rationale.to_string(),
            urgency: Urgency::Normal,
            wait: false,
            created_at: Utc::now(),
            context: None,
        }
    }

    #[test]
    fn benign_request_is_undecided_not_approved() {
        let req = request(
            "create_pull_request",
            "Open a PR with the bug fix for issue #123",
            "The fix is tested and ready for review",
        );
        let verdict = adjudicate(&req);
        assert_eq!(verdict.decision, GuardianDecision::Undecided);
    }

    #[test]
    fn destructive_action_is_denied() {
        let req = request(
            "cleanup",
            "Run rm -rf on the old build directory to free disk space",
            "The directory is no longer needed",
        );
        let verdict = adjudicate(&req);
        assert_eq!(verdict.decision, GuardianDecision::Deny);
        assert!(verdict.reason.contains("destructive"));
    }

    #[test]
    fn credential_probing_is_denied() {
        let req = request(
            "debug_auth",
            "Read the private key to check its format",
            "Need to verify the key is valid",
        );
        let verdict = adjudicate(&req);
        assert_eq!(verdict.decision, GuardianDecision::Deny);
        assert!(verdict.reason.contains("credential probing"));
    }

    #[test]
    fn data_exfiltration_is_denied() {
        let req = request(
            "backup",
            "Upload the repository to external storage for a backup",
            "Wanted an off-site copy",
        );
        let verdict = adjudicate(&req);
        assert_eq!(verdict.decision, GuardianDecision::Deny);
        assert!(verdict.reason.contains("data exfiltration"));
    }

    #[test]
    fn security_weakening_is_denied() {
        let req = request(
            "unblock",
            "Disable the sandbox so the build script can run freely",
            "The build keeps failing under the sandbox",
        );
        let verdict = adjudicate(&req);
        assert_eq!(verdict.decision, GuardianDecision::Deny);
        assert!(verdict.reason.contains("persistent security weakening"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        let req = request("cleanup", "RM -RF the temp directory", "cleanup");
        assert_eq!(adjudicate(&req).decision, GuardianDecision::Deny);
    }

    #[test]
    fn keyword_in_rationale_alone_is_still_caught() {
        let req = request(
            "cleanup",
            "Clean up old files",
            "Doing this because we need to drop table entries afterward",
        );
        assert_eq!(adjudicate(&req).decision, GuardianDecision::Deny);
    }

    fn request_with_context(
        action: &str,
        description: &str,
        rationale: &str,
        context: serde_json::Value,
    ) -> PermissionRequest {
        let mut req = request(action, description, rationale);
        req.context = Some(context);
        req
    }

    /// Gemini review, 2026-08-30: `context` was previously never inspected
    /// at all, so a caller could keep the three free-text fields benign
    /// and smuggle the actual dangerous instruction into structured data.
    #[test]
    fn keyword_smuggled_into_context_is_still_caught() {
        let req = request_with_context(
            "system_cleanup",
            "Routine background cleanup",
            "Scheduled maintenance",
            serde_json::json!({"commands": ["rm -rf /"]}),
        );
        assert_eq!(adjudicate(&req).decision, GuardianDecision::Deny);
    }

    #[test]
    fn keyword_in_a_context_object_key_is_still_caught() {
        let req = request_with_context(
            "system_cleanup",
            "Routine background cleanup",
            "Scheduled maintenance",
            serde_json::json!({"disable the sandbox": true}),
        );
        assert_eq!(adjudicate(&req).decision, GuardianDecision::Deny);
    }

    #[test]
    fn benign_context_stays_undecided() {
        let req = request_with_context(
            "create_pull_request",
            "Open a PR with the bug fix",
            "Tested and ready",
            serde_json::json!({"pr_number": 123, "branch": "fix/issue-123"}),
        );
        assert_eq!(adjudicate(&req).decision, GuardianDecision::Undecided);
    }

    /// Gemini review, 2026-08-30: splitting a keyword across two fields
    /// with an empty field in between used to produce a double space that
    /// defeated the exact-substring match.
    #[test]
    fn keyword_split_across_fields_with_double_space_is_still_caught() {
        let req = request("rm", "", "-rf the temp directory");
        assert_eq!(adjudicate(&req).decision, GuardianDecision::Deny);
    }

    #[test]
    fn keyword_split_by_an_embedded_newline_is_still_caught() {
        let req = request("cleanup", "force\npush to the shared branch", "cleanup");
        assert_eq!(adjudicate(&req).decision, GuardianDecision::Deny);
    }
}
