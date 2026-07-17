use crate::publish_evidence::PublishEvidence;
use serde::{Deserialize, Serialize};

/// Three-group dispatch classification for publishers. Dispatch order is
/// always Assets → Manager → Submitter. The one-way-door gate (historically
/// "the submitter gate") arms once any `required: true` publisher in an
/// already-run group fails, and from that point skips **both** the Manager
/// and Submitter groups — every publisher that writes a surface we cannot
/// cleanly reclaim. Only the reversible Assets group runs ungated. This is
/// why a botched blob mirror or homebrew tap push cannot burn a crates.io
/// version slot, and why a failed required blob upload no longer lets the
/// homebrew/scoop/nix/AUR/MCP one-way doors fire against an incomplete
/// release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PublisherGroup {
    /// Writes uploadable bytes to systems we control end-to-end. Failures
    /// are reversible via API delete (github-release, dockerhub,
    /// artifactory, cloudsmith, blob).
    Assets,
    /// Writes to package-manager state. Server-side deletable AND cleanly
    /// re-cuttable at the same version, so a botched write can be overwritten
    /// (homebrew, scoop, nix, krew, mcp, our-AUR repos, custom). Immutable
    /// registries whose SAME-version slot can never be reclaimed do NOT
    /// belong here — they are Submitter, so the rollback guard sees them.
    Manager,
    /// Writes to a third-party submission queue, an immutable registry
    /// slot, or a channel position we cannot reclaim. Gated behind the
    /// Submitter gate. Rollback is informational-only for most members
    /// (chocolatey, winget, snapcraft, upstream-AUR force-push); **cargo**,
    /// **npm**, and **pypi** are immutable registries whose landed publish
    /// burns the version (npm/pypi rollback is warn-only; cargo has a real
    /// programmatic `yank`). The one exception with a programmatic rollback
    /// is **cargo**. A multi-crate
    /// `cargo publish` that succeeds on crate A then fails on crate B
    /// records A and opts in via
    /// [`Publisher::programmatic_rollback_on_failure`](crate::Publisher::programmatic_rollback_on_failure),
    /// so the rollback path issues `cargo yank` for A even though the row's
    /// outcome is `Failed`.
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

impl PublisherOutcome {
    /// Whether this outcome is a terminal failure of a *required* publisher —
    /// the failure class that must fail the release and close the submitter
    /// gate. True for [`PublisherOutcome::Failed`] (the publish itself failed)
    /// and [`PublisherOutcome::RollbackFailed`] (the publish ran, rollback was
    /// attempted, and the rollback also failed, leaving a half-published
    /// surface needing manual intervention).
    ///
    /// Written as an exhaustive `match` (not `matches!`) so a future
    /// hard-failure variant cannot silently slip past a gate: adding one
    /// forces a compile error here and a conscious classification decision.
    pub fn is_required_release_failure(&self) -> bool {
        match self {
            PublisherOutcome::Failed(_) | PublisherOutcome::RollbackFailed(_) => true,
            PublisherOutcome::Succeeded
            | PublisherOutcome::Skipped(_)
            | PublisherOutcome::RolledBack
            | PublisherOutcome::RollbackSkippedNoScope
            | PublisherOutcome::PendingModeration
            | PublisherOutcome::PendingValidation
            | PublisherOutcome::PublishedNoRollback => false,
        }
    }
}

/// Reason a publisher was [`PublisherOutcome::Skipped`]. Serialized as
/// kebab-case (e.g. `"submitter-gated"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// Skipped because a required publisher in an already-run group
    /// failed and the one-way-door gate closed before this publisher
    /// could dispatch. Recorded for gated Manager AND Submitter
    /// publishers (the variant keeps its historical name). Preserves
    /// rollback safety by never firing an irreversible publisher past a
    /// known required failure.
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
    /// This version was already published to the target registry/store on a
    /// prior run, so the publisher detected it and skipped re-publishing
    /// (idempotent re-run). Distinct from `PublisherOutcome::Succeeded`
    /// because nothing was published THIS run — and distinct from
    /// `PublisherOutcome::Failed` because an already-published version is the
    /// desired end-state, not an error. A publisher reporting
    /// `AlreadyPublished` records NO rollback evidence: the version it found
    /// was published by an earlier run (or another actor), and deleting it on
    /// rollback would destroy state this run never created.
    AlreadyPublished,
    /// Excluded by `--skip` (the unified stage/publisher denylist) or absent
    /// from a non-empty `--publishers` allowlist. The operator opted this
    /// publisher out of the run; it was never invoked. Distinct from
    /// `NotConfigured` (the publisher block is absent from config) and from
    /// `NotApplicable` (the publisher is configured but no in-scope artifact
    /// matches it) — here the config and artifacts may both be present, and the
    /// publisher would otherwise have run, but the operator's selection
    /// suppressed it.
    Deselected,
    /// The pre-submitter verify-release gate ([`Context::verify_gate`](crate::context::Context::verify_gate))
    /// blocked every Submitter-group publisher this run: the gate returned
    /// `Ok(false)` (asset-content defects found against the just-published
    /// release) or `Err` (the check itself could not run and the dispatcher
    /// blocks as a precaution). Distinct from `SubmitterGated` — that variant
    /// means a required publisher already FAILED; this one means no publisher
    /// failed, but the post-publish content check did not clear before any
    /// one-way door could fire. Never applied to the Assets or Manager
    /// groups: the gate runs only once, at Submitter-group entry, after both
    /// reversible groups have already dispatched.
    VerifyGateBlocked,
    /// The publisher was registered (a config block exists) but every
    /// configured entry evaluated skip-inactive under the CURRENT
    /// config/env — `skip:`/`skip_upload:` truthy or `if:` falsy on all of
    /// them. Assigned directly by the dispatch chokepoint
    /// ([`crate::Publisher::config_fully_inactive`]) BEFORE `run()` is
    /// ever invoked, so a `run()` that unconditionally returns
    /// `Ok(evidence)` even with zero active entries can never be recorded
    /// as `Succeeded`. Distinct from `NotConfigured` (the config block is
    /// absent entirely) and from `Deselected` (an operator `--skip` /
    /// `--publishers` choice, not a config-derived inactivity).
    ConfigSkipped,
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
    /// Set once, at most, per report: the pre-submitter verify-release gate
    /// blocked the Submitter group this run. Kept separate from
    /// `submitter_gated` (which means a required publisher already failed)
    /// so the run summary and `report.json` can distinguish "a publisher
    /// broke" from "nothing broke, but the post-publish content check never
    /// cleared" — the two skip reasons a Submitter-group row can carry.
    #[serde(default)]
    pub verify_gate_blocked: bool,
    /// Whether [`ensure_verify_gate_evaluated`] has already run the
    /// pre-submitter verify-release gate this release. Distinct from
    /// `verify_gate_blocked`: that bit alone cannot tell "the gate ran and
    /// passed" apart from "the gate never ran" — both leave it `false` — so
    /// a second call site (e.g. `SnapcraftPublishStage`, which runs as its
    /// own pipeline stage outside the in-dispatch Submitter loop) needs this
    /// flag to know whether it must invoke the gate itself or may trust
    /// `verify_gate_blocked` as already authoritative.
    #[serde(default)]
    pub verify_gate_evaluated: bool,
}

impl PublishReport {
    pub fn required_failures(&self) -> usize {
        self.required_failure_names().len()
    }

    /// Names of every *required* publisher that finished in a terminal
    /// failure state ([`PublisherOutcome::is_required_release_failure`]).
    /// The one filter behind [`Self::required_failures`] and the
    /// required-failure exit gate ([`gate_required_failures`]), so the
    /// count, the gate, and the operator-facing name list cannot diverge.
    pub fn required_failure_names(&self) -> Vec<&str> {
        self.results
            .iter()
            .filter(|r| r.required && r.outcome.is_required_release_failure())
            .map(|r| r.name.as_str())
            .collect()
    }

    /// Names of every *required* publisher that was
    /// [`SkipReason::VerifyGateBlocked`] — never attempted because the
    /// pre-submitter verify-release gate blocked the Submitter group before
    /// this publisher could dispatch. Distinct from
    /// [`Self::required_failure_names`]: a blocked row never ran, so it is
    /// not [`PublisherOutcome::is_required_release_failure`] and would
    /// otherwise let [`gate_required_failures`] exit 0 on a required
    /// publisher that silently never published. [`gate_required_failures`]
    /// bails on this list too, closing that hole.
    pub fn required_gate_blocked_names(&self) -> Vec<&str> {
        self.results
            .iter()
            .filter(|r| {
                r.required
                    && matches!(
                        r.outcome,
                        PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked)
                    )
            })
            .map(|r| r.name.as_str())
            .collect()
    }

    /// A concise, run-wide human summary of the required failure(s) that
    /// triggered a rollback — each failed required publisher rendered as
    /// `<name>: <error>` and joined with `; `. Empty when no required
    /// publisher failed.
    ///
    /// Threaded into the `on_rollback` hook surface as `{{ .Reason }}` /
    /// `ANODIZER_ROLLBACK_REASON` so a reverted-but-never-failed publisher's
    /// hook learns WHY the unwind fired (which sibling failure), a fact
    /// `{{ .Error }}` (that publisher's own revert error, empty on a clean
    /// revert) cannot carry. Shares the required-failure filter with
    /// [`Self::required_failure_names`] so the reason names exactly the set
    /// the exit gate reports.
    pub fn required_failure_reason(&self) -> String {
        self.results
            .iter()
            .filter(|r| r.required && r.outcome.is_required_release_failure())
            .map(|r| {
                // Exhaustive match (not `_ =>`) mirrors
                // `is_required_release_failure`: a future message-carrying
                // required-failure variant must be compile-forced to extract
                // its message here rather than silently rendering `<name>: `
                // with an empty reason.
                let msg = match &r.outcome {
                    PublisherOutcome::Failed(m) | PublisherOutcome::RollbackFailed(m) => m.as_str(),
                    PublisherOutcome::Succeeded
                    | PublisherOutcome::Skipped(_)
                    | PublisherOutcome::RolledBack
                    | PublisherOutcome::RollbackSkippedNoScope
                    | PublisherOutcome::PendingModeration
                    | PublisherOutcome::PendingValidation
                    | PublisherOutcome::PublishedNoRollback => "",
                };
                format!("{}: {}", r.name, msg)
            })
            .collect::<Vec<_>>()
            .join("; ")
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

    /// The single authoritative one-way-door gate predicate: `true` when
    /// any already-run **required** publisher failed and every downstream
    /// one-way door (Manager or Submitter) must therefore be skipped.
    ///
    /// A required failure in Assets, Manager, **or the Submitter group
    /// itself** closes the gate. Both the intra-Manager and intra-Submitter
    /// checks are load-bearing: each group runs sequentially, and a
    /// required failure partway through must stop the remaining
    /// irreversible publishers in that group and every later group from
    /// pushing against an incomplete release — the gate is not a single
    /// boundary, it is a running "any required publish already broke" check
    /// that every remaining one-way door consults.
    ///
    /// Every gate site — the in-dispatch Manager+Submitter loop, the
    /// `SnapcraftPublishStage` (a Submitter that runs as its own stage after
    /// the trait dispatch), and the `BlobStage` self-check — calls this one
    /// predicate so the rule cannot drift between copies. (The name is
    /// historical: the gate originally covered only the Submitter group.)
    pub fn submitter_gate_closed(&self) -> bool {
        self.any_failed(PublisherGroup::Assets, true)
            || self.any_failed(PublisherGroup::Manager, true)
            || self.any_failed(PublisherGroup::Submitter, true)
    }

    /// The complete one-way-door gate: `true` when Submitter-group
    /// publishers must be skipped for EITHER reason — a required publisher
    /// already failed ([`Self::submitter_gate_closed`]), or the pre-submitter
    /// verify-release check blocked the group
    /// ([`Self::verify_gate_blocked`]).
    ///
    /// The live per-publisher check sites (the in-dispatch Submitter loop,
    /// `SnapcraftPublishStage`) deliberately do NOT call this combined form:
    /// each needs to distinguish which reason applies so it can record the
    /// precise [`SkipReason`] (`SubmitterGated` vs `VerifyGateBlocked`)
    /// rather than losing that distinction behind an OR, so they check
    /// [`Self::submitter_gate_closed`] and [`Self::verify_gate_blocked`]
    /// separately, in that order. This method is the read-only aggregate for
    /// callers — tests, and any future consumer — that only need "is the
    /// Submitter group blocked at all" without caring why.
    pub fn one_way_door_gate_closed(&self) -> bool {
        self.submitter_gate_closed() || self.verify_gate_blocked
    }
}

/// Runs the pre-submitter verify-release gate exactly once per release,
/// coordinating every call site that might be first to reach a
/// Submitter-group publisher: the in-dispatch Submitter loop
/// (`stage-publish::dispatch`) and `SnapcraftPublishStage`, which runs as
/// its own pipeline stage AFTER trait-based dispatch and is therefore
/// invisible to dispatch's own view of the Submitter group — a release that
/// configures only `snapcraft:` never puts a single publisher through
/// dispatch's Submitter loop, so dispatch's lazy eval would never fire and
/// an unverified snap would ship unless snapcraft also has a way to trigger
/// the check.
///
/// Persisting the "already evaluated" bit on `report` (rather than a
/// per-call-scope local, which is what each call site used before this
/// function existed) is what makes "exactly once" hold across that crate
/// boundary: whichever call site reaches a live Submitter-group publisher
/// first pays for the (network) gate check and records both bits; the
/// other call site observes `verify_gate_evaluated` already `true` and
/// returns immediately, deferring to the `verify_gate_blocked` bit the
/// first caller already recorded.
///
/// No-op (marks evaluated, leaves `verify_gate_blocked` untouched) when
/// `ctx.verify_gate` is `None` — no CLI pipeline installed one (bare
/// `dispatch()` callers, most unit tests) — matching the behaviour of a
/// release that never installs a gate at all.
///
/// Takes `report` as a caller-owned `&mut PublishReport` rather than
/// reading `ctx.publish_report` itself: `stage-publish::dispatch` holds its
/// report as a local (taken out of `ctx.publish_report` for the duration of
/// the dispatch loop, restored by its caller afterward) specifically so the
/// gate closure can freely borrow `&mut ctx` without aliasing the report
/// it's mutating; a caller-owned parameter lets both `dispatch` and
/// `SnapcraftPublishStage` share this exact discipline instead of each
/// re-deriving it.
pub fn ensure_verify_gate_evaluated(
    ctx: &mut crate::context::Context,
    report: &mut PublishReport,
    scope: &'static str,
) {
    if report.verify_gate_evaluated {
        return;
    }
    report.verify_gate_evaluated = true;
    let Some(gate) = ctx.verify_gate.clone() else {
        return;
    };
    let passed = gate(ctx).unwrap_or_else(|err| {
        ctx.logger(scope).status(&format!(
            "verify-release gate check failed: {err:#} — blocking one-way-door publishers"
        ));
        false
    });
    if !passed {
        report.verify_gate_blocked = true;
        ctx.logger(scope).status(
            "one-way-door publishers blocked — verify-release did not pass against the published release",
        );
    }
}

/// Required-failure exit gate: bail when any *required* publisher finished
/// in a terminal failure state, so the caller exits non-zero even though
/// the pipeline body ran to completion.
///
/// One definition serves both layers of the defense — the publish stage's
/// in-stage bail (so any embedding of the stage cannot report green over a
/// failed required publisher) and the CLI's end-of-pipeline gate (so shell
/// / CI callers see a non-zero exit). `ran_context` is the caller-specific
/// sentence describing what completed before this error; everything else —
/// the snapshot / dry-run skip, the failure filter, the name list, the
/// recovery hint — is shared so the two layers cannot drift.
///
/// **Snapshot / dry-run skip**: publishers don't actually publish in those
/// modes, so a recorded failure there must not abort the preview pipeline;
/// the explicit skip is defense-in-depth in case a future stage starts
/// recording publisher results in those modes.
pub fn gate_required_failures(
    ctx: &crate::context::Context,
    ran_context: &str,
) -> anyhow::Result<()> {
    if ctx.is_snapshot() || ctx.is_dry_run() {
        return Ok(());
    }
    let Some(report) = ctx.publish_report() else {
        return Ok(());
    };
    let failed = report.required_failure_names();
    if !failed.is_empty() {
        anyhow::bail!(
            "{} required publisher(s) failed: {}. {} Inspect dist/run-<id>/report.json \
             for details and use --rollback-only --from-run=<id> to retry rollback.",
            failed.len(),
            failed.join(", "),
            ran_context
        );
    }
    // A required publisher can also be missing not because it FAILED but
    // because the pre-submitter verify-release gate blocked the whole
    // Submitter group before it ever dispatched (e.g. a transient GH API
    // error during the gate check). `Skipped(_)` is deliberately never
    // `is_required_release_failure()` (that predicate means "ran and
    // failed"), so without this second check a required cargo/npm/pypi
    // publisher blocked by the gate would exit 0 — a silent partial release.
    let blocked = report.required_gate_blocked_names();
    if !blocked.is_empty() {
        anyhow::bail!(
            "{} required publisher(s) were blocked by the pre-submitter verify-release \
             gate and never attempted to publish: {}. {} The post-publish content check \
             did not clear before these one-way-door publishers could run; inspect the \
             verify-release stage log and re-run once the underlying issue is fixed.",
            blocked.len(),
            blocked.join(", "),
            ran_context
        );
    }
    Ok(())
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

    /// The name list feeding the operator-facing gate message applies the
    /// SAME filter as the count — a required Failed/RollbackFailed row is
    /// named, a non-required or non-terminal row is not.
    #[test]
    fn required_failure_names_matches_count_filter() {
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
        r.results.push(PublisherResult {
            name: "required-rollback-failed".to_string(),
            group: PublisherGroup::Manager,
            required: true,
            outcome: PublisherOutcome::RollbackFailed("cleanup".to_string()),
            evidence: None,
        });
        assert_eq!(
            r.required_failure_names(),
            vec!["required-pub", "required-rollback-failed"]
        );
        assert_eq!(r.required_failures(), 2);
    }

    #[test]
    fn required_failures_counts_rollback_failed() {
        // Regression: a required publisher whose publish succeeded but whose
        // rollback then failed leaves a half-published surface and MUST count
        // as a required failure. The prior `matches!(Failed(_))`-only filter
        // silently dropped it.
        let mut r = PublishReport::default();
        r.results.push(PublisherResult {
            name: "required-rollback-failed".to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome: PublisherOutcome::RollbackFailed("manual cleanup needed".to_string()),
            evidence: None,
        });
        assert_eq!(r.required_failures(), 1);
    }

    #[test]
    fn is_required_release_failure_classifies_terminal_failures() {
        assert!(PublisherOutcome::Failed("boom".into()).is_required_release_failure());
        assert!(PublisherOutcome::RollbackFailed("boom".into()).is_required_release_failure());
        assert!(!PublisherOutcome::Succeeded.is_required_release_failure());
        assert!(!PublisherOutcome::RolledBack.is_required_release_failure());
        assert!(!PublisherOutcome::RollbackSkippedNoScope.is_required_release_failure());
        assert!(!PublisherOutcome::PendingModeration.is_required_release_failure());
        assert!(!PublisherOutcome::PendingValidation.is_required_release_failure());
        assert!(!PublisherOutcome::PublishedNoRollback.is_required_release_failure());
        assert!(!PublisherOutcome::Skipped(SkipReason::DryRun).is_required_release_failure());
    }

    #[test]
    fn skip_reason_serializes_as_kebab_case() {
        let s = serde_json::to_string(&SkipReason::SubmitterGated).expect("serialize");
        assert_eq!(s, "\"submitter-gated\"");
    }

    #[test]
    fn skip_reason_deselected_serializes_as_kebab_case() {
        let s = serde_json::to_string(&SkipReason::Deselected).expect("serialize");
        assert_eq!(s, "\"deselected\"");
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

    fn failed(name: &str, group: PublisherGroup, required: bool) -> PublisherResult {
        PublisherResult {
            name: name.to_string(),
            group,
            required,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        }
    }

    #[test]
    fn submitter_gate_closes_on_required_failure_in_any_group() {
        for group in [
            PublisherGroup::Assets,
            PublisherGroup::Manager,
            PublisherGroup::Submitter,
        ] {
            let mut r = PublishReport::default();
            r.results.push(failed("p", group, true));
            assert!(
                r.submitter_gate_closed(),
                "a required failure in {group:?} must close the submitter gate"
            );
        }
    }

    #[test]
    fn submitter_gate_closes_on_required_intra_submitter_failure() {
        // The load-bearing case for the v0.8.0 fix: a required cargo
        // (Submitter) failure must close the gate so later irreversible
        // submitters (winget, snapcraft) are skipped.
        let mut r = PublishReport::default();
        r.results
            .push(failed("cargo", PublisherGroup::Submitter, true));
        assert!(r.submitter_gate_closed());
    }

    #[test]
    fn submitter_gate_stays_open_on_optional_failures_only() {
        let mut r = PublishReport::default();
        r.results.push(failed("a", PublisherGroup::Assets, false));
        r.results.push(failed("m", PublisherGroup::Manager, false));
        r.results
            .push(failed("cargo", PublisherGroup::Submitter, false));
        assert!(
            !r.submitter_gate_closed(),
            "optional failures must not close the gate (continue-on-error)"
        );
    }

    #[test]
    fn submitter_gate_stays_open_on_all_success() {
        let mut r = PublishReport::default();
        r.results.push(PublisherResult {
            name: "github-release".to_string(),
            group: PublisherGroup::Assets,
            required: true,
            outcome: PublisherOutcome::Succeeded,
            evidence: None,
        });
        assert!(!r.submitter_gate_closed());
    }

    #[test]
    fn one_way_door_gate_closed_by_verify_gate_alone() {
        let r = PublishReport {
            verify_gate_blocked: true,
            ..Default::default()
        };
        assert!(
            !r.submitter_gate_closed(),
            "no required publisher failed, so the required-failure gate must stay open"
        );
        assert!(
            r.one_way_door_gate_closed(),
            "the verify-gate block alone must close the combined predicate"
        );
    }

    #[test]
    fn one_way_door_gate_closed_by_required_failure_alone() {
        let mut r = PublishReport::default();
        r.results.push(PublisherResult {
            name: "cargo".to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome: PublisherOutcome::Failed("boom".to_string()),
            evidence: None,
        });
        assert!(!r.verify_gate_blocked);
        assert!(r.one_way_door_gate_closed());
    }

    #[test]
    fn one_way_door_gate_open_when_neither_reason_applies() {
        let r = PublishReport::default();
        assert!(!r.submitter_gate_closed());
        assert!(!r.verify_gate_blocked);
        assert!(!r.one_way_door_gate_closed());
    }

    #[test]
    fn required_gate_blocked_names_finds_only_required_verify_gate_blocked_rows() {
        let mut r = PublishReport::default();
        r.results.push(PublisherResult {
            name: "cargo".to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome: PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked),
            evidence: None,
        });
        r.results.push(PublisherResult {
            name: "chocolatey".to_string(),
            group: PublisherGroup::Submitter,
            required: false,
            outcome: PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked),
            evidence: None,
        });
        r.results.push(PublisherResult {
            name: "npm".to_string(),
            group: PublisherGroup::Submitter,
            required: true,
            outcome: PublisherOutcome::Skipped(SkipReason::Deselected),
            evidence: None,
        });
        assert_eq!(r.required_gate_blocked_names(), vec!["cargo"]);
    }
}
