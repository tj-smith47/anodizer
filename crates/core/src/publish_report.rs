use crate::publish_evidence::PublishEvidence;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PublisherGroup {
    Assets,
    Manager,
    Submitter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublisherOutcome {
    Succeeded,
    Skipped(SkipReason),
    Failed(String),
    RolledBack,
    RollbackFailed(String),
    RollbackSkippedNoScope,
    /// Publisher succeeded but the version is queued for moderation (chocolatey, AUR-like).
    PendingModeration,
    /// Publisher succeeded but a downstream validation step is still polling (winget).
    PendingValidation,
    /// Publisher succeeded; rollback was skipped because `--rollback=none` was set.
    PublishedNoRollback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    SubmitterGated,
    NotConfigured,
    Snapshot,
    DryRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublisherResult {
    pub name: String,
    pub group: PublisherGroup,
    pub required: bool,
    pub outcome: PublisherOutcome,
    pub evidence: Option<PublishEvidence>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishReport {
    pub results: Vec<PublisherResult>,
    #[serde(default)]
    pub submitter_gated: bool,
    #[serde(default)]
    pub announce_gated: bool,
}

impl PublishReport {
    pub fn required_failures(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.required && matches!(r.outcome, PublisherOutcome::Failed(_)))
            .count()
    }

    /// Returns true if any publisher in `group` failed.
    ///
    /// When `required_only` is true, only publishers with `required: true` count.
    /// The Submitter gate consults this with `required_only = true` against the
    /// Assets and Manager groups to decide whether to skip Submitter dispatch.
    pub fn any_failed(&self, group: PublisherGroup, required_only: bool) -> bool {
        self.results.iter().any(|r| {
            r.group == group
                && (!required_only || r.required)
                && matches!(r.outcome, PublisherOutcome::Failed(_))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_report_has_zero_failures() {
        let r = PublishReport::default();
        assert!(r.results.is_empty());
        assert!(!r.submitter_gated);
        assert_eq!(r.required_failures(), 0);
    }

    #[test]
    fn required_failures_counts_only_required() {
        let mut r = PublishReport::default();
        r.results.push(PublisherResult {
            name: "optional-pub".to_string(),
            group: PublisherGroup::Manager,
            required: false,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        });
        r.results.push(PublisherResult {
            name: "required-pub".to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        });
        assert_eq!(r.required_failures(), 1);
    }

    #[test]
    fn skip_reason_serializes_as_kebab_case() {
        let s = serde_json::to_string(&SkipReason::SubmitterGated).expect("serialize");
        assert_eq!(s, "\"submitter-gated\"");
    }

    #[test]
    fn publisher_group_serializes_pascal_case() {
        let s = serde_json::to_string(&PublisherGroup::Submitter).expect("serialize");
        assert_eq!(s, "\"Submitter\"");
    }

    #[test]
    fn publisher_outcome_succeeded_serializes_as_bare_string() {
        let s = serde_json::to_string(&PublisherOutcome::Succeeded).expect("serialize");
        assert_eq!(s, "\"Succeeded\"");
    }

    #[test]
    fn publisher_outcome_failed_serializes_as_externally_tagged() {
        let s = serde_json::to_string(&PublisherOutcome::Failed("boom".into())).expect("serialize");
        assert_eq!(s, r#"{"Failed":"boom"}"#);
    }

    #[test]
    fn any_failed_returns_true_only_for_required_when_required_only_is_true() {
        let mut r = PublishReport::default();
        r.results.push(PublisherResult {
            name: "required-mgr".to_string(),
            group: PublisherGroup::Manager,
            required: true,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        });
        r.results.push(PublisherResult {
            name: "optional-mgr".to_string(),
            group: PublisherGroup::Manager,
            required: false,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        });
        assert!(r.any_failed(PublisherGroup::Manager, true));

        let mut r = PublishReport::default();
        r.results.push(PublisherResult {
            name: "optional-mgr".to_string(),
            group: PublisherGroup::Manager,
            required: false,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        });
        assert!(!r.any_failed(PublisherGroup::Manager, true));
        assert!(r.any_failed(PublisherGroup::Manager, false));
    }
}
