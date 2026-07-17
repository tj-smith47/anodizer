use super::*;

/// Stage→stage handoff state produced by stages and consumed by later
/// stages (as opposed to `config` / `options` which are pipeline inputs,
/// or `artifacts` which has its own registry). The changelog stage
/// writes here, the release stage reads here.
#[derive(Debug, Default)]
pub struct StageOutputs {
    /// Set by the changelog stage when `use: github-native` is configured.
    /// The release stage reads this to set `generate_release_notes(true)`
    /// on the GitHub API.
    pub github_native_changelog: bool,
    /// Per-crate rendered changelog body, keyed by crate name.
    pub changelogs: HashMap<String, String>,
    /// Rendered `changelog.header` value, populated by the changelog stage.
    /// The release stage uses it as a fallback when `release.header` is
    /// unset so YAML-configured changelog headers reach the GitHub release
    /// body (the release-header content-loading behaviour).
    pub changelog_header: Option<String>,
    /// Rendered `changelog.footer` value, populated by the changelog stage.
    /// Same fallback semantics as `changelog_header`.
    pub changelog_footer: Option<String>,
    /// Per-publisher post-publish polling results, written by the publish
    /// stage's chocolatey / winget polling fan-out and consumed by the
    /// release-summary renderer. Stored as opaque JSON to keep core free
    /// of stage-publish types (the `PostPublishResult` type lives in
    /// `anodizer-stage-publish::post_publish::status` and serializes
    /// stably). Empty when polling was disabled or no eligible
    /// publishers ran.
    pub post_publish_results: Vec<serde_json::Value>,
}

/// Callback that re-runs release-content verification against the already
/// published reversible surface and reports whether it passed. Stored on
/// [`Context`] so the publish dispatcher can gate one-way-door publishers on
/// a fresh verify without `stage-publish` depending on `stage-verify-release`.
/// `Arc` so it can be cheaply cloned out of `&mut Context` before invocation.
pub type VerifyGate = std::sync::Arc<dyn Fn(&mut Context) -> anyhow::Result<bool> + Send + Sync>;

impl Context {
    /// Publisher-facing override: when `Publisher::run` returns `Ok`
    /// but the terminal outcome is something other than `Succeeded`
    /// (chocolatey moderation skip, winget/krew/homebrew
    /// PR-already-exists skip, …) call this before returning so
    /// dispatch records the correct `PublisherOutcome` on the report.
    /// Without this, dispatch defaults to `Succeeded` on any Ok and
    /// the summary table silently misreports the skip as success.
    pub fn record_publisher_outcome(&mut self, outcome: crate::PublisherOutcome) {
        self.pending_outcome = Some(outcome);
    }

    /// Dispatch-side consumer: take the pending outcome override (if
    /// any) recorded by the publisher's `run`. Single-shot — the slot
    /// is empty after this call.
    pub fn take_pending_outcome(&mut self) -> Option<crate::PublisherOutcome> {
        self.pending_outcome.take()
    }

    /// Publisher-side recorder: stash the partial evidence accumulated
    /// before a failing `run` returns `Err`, so dispatch can attach it to
    /// the failed report row and rollback has the authoritative record of
    /// what went live. See [`Context::pending_evidence`].
    pub fn record_pending_evidence(&mut self, evidence: crate::PublishEvidence) {
        self.pending_evidence = Some(evidence);
    }

    /// Dispatch-side consumer: take the partial evidence (if any) a
    /// publisher recorded before failing. Single-shot — empty after this
    /// call.
    pub fn take_pending_evidence(&mut self) -> Option<crate::PublishEvidence> {
        self.pending_evidence.take()
    }

    /// Borrow the publisher dispatch report set by `PublishStage::run`,
    /// or `None` if the publish stage hasn't run yet (or was skipped).
    pub fn publish_report(&self) -> Option<&PublishReport> {
        self.publish_report.as_ref()
    }

    /// Whether the publish stage entered its body this run (even if it
    /// aborted before dispatching any publisher).
    pub fn publish_attempted(&self) -> bool {
        self.publish_attempted
    }

    /// Record that the publish stage entered its body. Called by
    /// `PublishStage::run` ahead of its pre-dispatch guards so guard
    /// aborts are distinguishable from a skipped stage.
    pub fn set_publish_attempted(&mut self) {
        self.publish_attempted = true;
    }

    /// Store the publisher dispatch report. Overwrites any prior value.
    ///
    /// Written by the publish stage during a normal release run; rehydrated by
    /// `--announce-only` from the on-disk `<dist>/run-<id>/report.json` so the
    /// announce stage sees an equivalent context without re-publishing.
    pub fn set_publish_report(&mut self, r: PublishReport) {
        self.publish_report = Some(r);
    }

    /// Borrow the set of crate names the build stage actually built, or
    /// `None` if the build stage has not run in this pipeline (merge mode).
    pub fn built_crate_names(&self) -> Option<&std::collections::HashSet<String>> {
        self.built_crate_names.as_ref()
    }

    /// Record the distinct crate names that received at least one in-scope
    /// build job. Called once by the build stage after job planning.
    pub fn set_built_crate_names(&mut self, names: std::collections::HashSet<String>) {
        self.built_crate_names = Some(names);
    }

    /// Record an intentional skip from a per-sub-config loop
    /// (`signs`, `docker_signs`, `publishers`, …). `stage` identifies the
    /// owning stage, `label` identifies the sub-config (id / name / index),
    /// `reason` is short user-facing text. Duplicate (stage, label, reason)
    /// tuples are dropped on insert so a per-artifact inner loop cannot emit
    /// N copies of the same skip message.
    pub fn remember_skip(&self, stage: &str, label: &str, reason: &str) {
        self.skip_memento.remember(stage, label, reason);
    }
}
