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

/// Deterministic, keyword-based adjudication of one ambient-mode permission
/// request. Pure function — no I/O, no async, trivially unit-testable. See
/// module docs for why this only ever denies or defers, never approves.
pub fn adjudicate(request: &PermissionRequest) -> GuardianVerdict {
    let haystack = format!(
        "{} {} {}",
        request.action, request.description, request.rationale
    )
    .to_ascii_lowercase();

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
}
