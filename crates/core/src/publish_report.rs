use crate::publish_evidence::PublishEvidence;
use serde::{Deserialize, Serialize};

/// Three-group dispatch classification for publishers. Dispatch order is
/// always Assets → Manager → Submitter. The Submitter gate sits between
/// Manager and Submitter and short-circuits Submitter dispatch when any
/// `required: true` publisher in Assets or Manager failed (so a botched
/// homebrew tap push cannot burn a crates.io version slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PublisherGroup {
    /// Writes uploadable bytes to systems we control end-to-end. Failures
    /// are reversible via API delete (github-release, dockerhub,
    /// artifactory, cloudsmith, blob).
    Assets,
    /// Writes to package-manager state. Server-side deletable, but
    /// consumer machines may have already pulled the artifact
    /// (homebrew, scoop, nix, krew, mcp, our-AUR repos, custom).
    Manager,
    /// Writes to a third-party submission queue, an immutable registry
    /// slot, or a channel position we cannot reclaim. Gated behind the
    /// Submitter gate; rollback is informational only
    /// (cargo, chocolatey, winget, snapcraft, upstream-AUR force-push).
    Submitter,
}

/// Per-publisher terminal state in [`PublishReport`]. Stage-level statuses
/// like `pending-moderation` / `pending-validation` / `announce-gated`
/// live on the run summary, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublisherOutcome {
    /// `Publisher::run` returned `Ok` and the artifact is live.
    Succeeded,
    /// Publisher did not execute; see [`SkipReason`] for why.
    Skipped(SkipReason),
    /// `Publisher::run` returned `Err`; the carried `String` is the error
    /// message (already rendered via `{:#}`).
    Failed(String),
    /// Initially [`PublisherOutcome::Succeeded`], then revert dispatch
    /// successfully reverted the action.
    RolledBack,
    /// Initial run succeeded but revert dispatch failed; manual
    /// intervention required. The carried `String` is the rollback
    /// error message.
    RollbackFailed(String),
    /// Rollback was skipped because the required scope token env var
    /// (per `Publisher::rollback_scope_needed`) is not set in the
    /// environment.
    RollbackSkippedNoScope,
    /// Publisher succeeded but the version is queued for moderation (chocolatey, AUR-like).
    PendingModeration,
    /// Publisher succeeded but a downstream validation step is still polling (winget).
    PendingValidation,
    /// Publisher succeeded; rollback was skipped because `--rollback=none` was set.
    PublishedNoRollback,
}

/// Reason a publisher was [`PublisherOutcome::Skipped`]. Serialized as
/// kebab-case (e.g. `"submitter-gated"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// Skipped because a required Assets/Manager publisher failed; the
    /// Submitter gate closed before this publisher could dispatch.
    /// Preserves rollback safety on irreversible publishers.
    SubmitterGated,
    /// Publisher entry absent from the workspace config; the
    /// `Publisher::run` impl was never invoked.
    NotConfigured,
    /// Pipeline ran in `--snapshot` mode; publishers do not fire.
    Snapshot,
    /// Pipeline ran in `--dry-run` mode; publishers do not fire.
    DryRun,
    /// Pipeline ran in `--nightly` mode and this publisher opts out of
    /// nightly publishes (e.g. homebrew, scoop, aur, krew, nix, every
    /// announcer — the nightly skip-list).
    Nightly,
    /// No artifact in the current crate scope matches this publisher's
    /// applicability rules (e.g. top-level homebrew_casks declared
    /// `binaries: [cfgd]` but the current per-crate iteration is on
    /// `cfgd-core` and has no `cfgd` binary in scope; or cloudsmith
    /// targets `.deb` / `.rpm` / `.apk` but the current crate produces
    /// only library archives). Distinct from `NotConfigured` (where
    /// the publisher block is absent entirely) and from
    /// `PublisherOutcome::Failed` (where the publisher TRIED to run
    /// and hit a real error). Required Manager publishers reporting
    /// `NotApplicable` MUST NOT trigger the submitter gate — there is
    /// nothing to roll back, and the absence of applicable artifacts
    /// is not a publish failure.
    NotApplicable,
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
