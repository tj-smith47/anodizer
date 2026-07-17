//! Run summary JSON writer + end-of-pipeline status table.
//!
//! Emits a stable JSON document describing every publisher outcome,
//! gate flags, and the runtime/compile-time non-determinism allowlist.
//! Consumed by CI (parse `summary.json` to gate further jobs) and by
//! operators (status table prints to stderr so non-CI runs see the
//! per-publisher result at a glance).
//!
//! The schema is `deny_unknown_fields` so a downstream consumer
//! fails loudly if a future anodize version adds a field they don't
//! understand — preferable to silent shape drift.

use anodizer_core::context::Context;
use anodizer_core::publish_evidence::PublishEvidence;
use anodizer_core::publish_report::{PublisherGroup, PublisherOutcome, SkipReason};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// `skip_serializing_if` predicate for `retry_backoff_secs`: a run that never
/// backed off omits the field entirely rather than emitting `0.0`.
fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
}

// No `Eq`: `retry_backoff_secs` is an f64 (wall-clock seconds), which is
// PartialEq but not Eq.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RunSummary {
    pub schema_version: u32,
    pub anodize_version: String,
    pub tag: String,
    pub submitter_gated: bool,
    pub announce_gated: bool,
    /// Count of publishers whose outcome left durable published state
    /// in the world: `succeeded`, `pending-moderation`,
    /// `pending-validation`, `published-no-rollback`,
    /// `rollback-failed` AND `rollback-skipped-no-scope` (in both, the
    /// publish landed and nothing withdrew it — the state is presumed
    /// live). The counting is intentionally conservative: when in
    /// doubt, an outcome counts as published.
    ///
    /// `#[serde(default)]` keeps summaries written by older anodize
    /// versions parseable by newer readers.
    #[serde(default)]
    pub publishers_succeeded: u32,
    /// Count of publishers with a `failed` outcome.
    #[serde(default)]
    pub publishers_failed: u32,
    /// True when any Submitter-group publisher's publish action landed
    /// at the remote — the one-way door. Submitter targets (crates.io,
    /// chocolatey, winget, snapcraft, ...) never accept the same
    /// version twice, so once this is true the version is burned and a
    /// same-version re-cut is impossible: recovery tooling must refuse
    /// destructive rollback (tag delete, revert push) and fix forward
    /// instead. Counts EVERY landed outcome, including `rolled-back` —
    /// a cargo yank withdraws the artifact but does NOT reopen the
    /// version slot. Reversible groups (Assets, Manager) never set
    /// this; their state can be deleted and the same version re-cut.
    ///
    /// `#[serde(default)]` keeps summaries written by older anodize
    /// versions parseable by newer readers.
    #[serde(default)]
    pub irreversibly_published: bool,
    /// Outcome of the in-process failure policy (`release.on_failure`),
    /// recorded after a release-pipeline failure so the summary states
    /// which recovery path the run took. `None` on successful runs and
    /// on summaries written before the policy executed.
    ///
    /// `#[serde(default)]` keeps summaries written by older anodize
    /// versions parseable by newer readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_policy: Option<FailurePolicyRecord>,
    /// Outcome of the post-publish verification gate (`verify_release:`),
    /// recorded on a SEPARATE axis from the publisher rows. The gate runs
    /// LAST — after the irreversible publish — so the publisher rows still
    /// read `succeeded` while this field states whether the published
    /// release has unverified defects to investigate. `None` when the gate
    /// did not run (disabled / skipped / dry-run / snapshot) and on
    /// summaries written before the gate executed.
    ///
    /// `#[serde(default)]` keeps summaries written by older anodize
    /// versions parseable by newer readers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_release: Option<VerifyReleaseRecord>,
    /// Wall-clock seconds the run spent sleeping between retry attempts of
    /// failed operations — the retry engine plus the bespoke publisher
    /// ladders (github asset upload + octocrab, cargo-publish propagation,
    /// gh-pr-create, sign/notarize/rate-limit backoff). Excludes deliberate
    /// pacing (`upload_pace`) and readiness polls (crates.io index wait,
    /// post-publish moderation) — those are expected waiting, not backoff
    /// after a failure. A high value flags a flaky remote worth investigating
    /// even when the run ultimately succeeded.
    ///
    /// `#[serde(default)]` keeps summaries written by older anodize
    /// versions parseable by newer readers; `skip_serializing_if` omits it
    /// from the JSON on runs that never backed off.
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub retry_backoff_secs: f64,
    /// Per-publisher/stage breakdown of `retry_backoff_secs`, biggest offender
    /// first — so a summary reader sees WHICH remote was flaky, not just that
    /// the run backed off. Sums to `retry_backoff_secs`; empty on runs that
    /// never backed off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retry_by_scope: Vec<RetryScopeStat>,
    pub results: Vec<RunSummaryResult>,
    pub determinism_allowlist: DeterminismAllowlist,
}

/// One publisher/stage's retry tally within a run. `scope` is the publisher
/// name (`cargo`, `homebrew`, …) or the stage label (`release`); `retries` is
/// the number of backoff sleeps it incurred and `backoff_secs` their summed
/// wait. See `RunSummary::retry_by_scope`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetryScopeStat {
    pub scope: String,
    pub retries: u32,
    pub backoff_secs: f64,
}

/// The verify-release gate's verdict for the run. See `verify_release:`.
///
/// `passed` is the headline boolean (`issue_count == 0`); `issues` carries the
/// human-readable defect strings so the summary reader sees the same detail the
/// gate logged before it bailed. Each issue string already names the offending
/// `crate '<name>'`, so attribution survives the workspace fan-out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifyReleaseRecord {
    /// True when the gate found no defects (`issue_count == 0`).
    pub passed: bool,
    /// Number of defects the gate reported.
    pub issue_count: u32,
    /// One message per detected defect; empty on a clean pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<String>,
}

/// What the in-process failure policy decided and executed after a
/// release-pipeline failure. See `release.on_failure`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FailurePolicyRecord {
    /// Configured policy: `rollback` or `hold`.
    pub configured: String,
    /// Action actually taken: `rolled-back`, `held`, or
    /// `rollback-failed` (rollback was attempted and refused/errored;
    /// state is effectively held).
    pub action: String,
    /// True when a configured `rollback` degraded to hold because a
    /// one-way-door publisher had already landed.
    pub degraded: bool,
    /// Submitter-group publishers whose publish landed and burned the
    /// version (the degrade evidence). Empty unless `degraded`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub burned_publishers: Vec<String>,
    /// Error from the rollback execution when `action` is
    /// `rollback-failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunSummaryResult {
    pub name: String,
    pub group: PublisherGroup,
    pub required: bool,
    /// Kebab-case status string per the spec's "Status Set".
    pub status: String,
    pub evidence: Option<PublishEvidence>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DeterminismAllowlist {
    pub compile_time: Vec<DeterminismAllowlistEntry>,
    pub runtime: Vec<DeterminismAllowlistEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeterminismAllowlistEntry {
    pub artifact: String,
    pub reason: String,
}

impl RunSummary {
    pub const CURRENT_SCHEMA_VERSION: u32 = 1;

    /// Build a `RunSummary` from `Context`. Pulls per-publisher results
    /// from `ctx.publish_report`, the compile-time and runtime
    /// allowlists from `ctx.determinism` (which the BuildStage seeds
    /// from the operator's `--allow-nondeterministic` flags + the spec
    /// contract), and the tag from the `Tag` template var (which the
    /// pipeline sets in `Context::apply_git_info` so it's stable across
    /// stages).
    ///
    /// Reading from `ctx.determinism` (not `ctx.options`) keeps the
    /// audit trail unified: every downstream consumer (release body,
    /// determinism harness, run summary) reads the same allow-list,
    /// rather than each consumer re-deriving it from `ctx.options`.
    /// When `ctx.determinism` is `None` (e.g. snapshot mode without a
    /// resolvable SDE), the runtime list falls back to
    /// `ctx.options.runtime_nondeterministic_allowlist` so the operator
    /// still gets an audit row in the summary.
    pub fn from_context(ctx: &Context) -> Self {
        Self::from_context_with_report(ctx, ctx.publish_report.as_ref())
    }

    /// Like [`from_context`](Self::from_context) but with an explicit
    /// report, for callers that hold the in-progress `PublishReport`
    /// before it is installed on the `Context` (the dispatch loop's
    /// per-publisher snapshot writes).
    pub fn from_context_with_report(
        ctx: &Context,
        report: Option<&anodizer_core::publish_report::PublishReport>,
    ) -> Self {
        let results = report
            .map(|r| {
                r.results
                    .iter()
                    .map(|res| RunSummaryResult {
                        name: res.name.clone(),
                        group: res.group,
                        required: res.required,
                        status: outcome_to_status_string(&res.outcome),
                        evidence: res.evidence.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();

        let (compile_time, runtime) = match ctx.determinism.as_ref() {
            Some(state) => (
                state
                    .compile_time_allowlist
                    .iter()
                    .map(|(name, reason)| DeterminismAllowlistEntry {
                        artifact: name.clone(),
                        reason: reason.clone(),
                    })
                    .collect(),
                state
                    .runtime_allowlist
                    .iter()
                    .map(|(name, reason)| DeterminismAllowlistEntry {
                        artifact: name.clone(),
                        reason: reason.clone(),
                    })
                    .collect(),
            ),
            None => (
                Vec::new(),
                ctx.options
                    .runtime_nondeterministic_allowlist
                    .iter()
                    .map(|(name, reason)| DeterminismAllowlistEntry {
                        artifact: name.clone(),
                        reason: reason.clone(),
                    })
                    .collect(),
            ),
        };

        let (publishers_succeeded, publishers_failed) = report
            .map(|r| count_publish_state(&r.results))
            .unwrap_or((0, 0));
        let irreversibly_published = report.is_some_and(|r| {
            r.results
                .iter()
                .any(|p| p.group == PublisherGroup::Submitter && outcome_landed(&p.outcome))
        });

        let verify_release = ctx.verify_release.as_ref().map(|v| VerifyReleaseRecord {
            passed: v.issues.is_empty(),
            issue_count: v.issues.len() as u32,
            issues: v.issues.clone(),
        });

        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            anodize_version: env!("CARGO_PKG_VERSION").to_string(),
            tag,
            submitter_gated: report.is_some_and(|r| r.submitter_gated),
            announce_gated: report.is_some_and(|r| r.announce_gated),
            publishers_succeeded,
            publishers_failed,
            irreversibly_published,
            failure_policy: None,
            verify_release,
            retry_backoff_secs: anodizer_core::retry::total_retry_backoff().as_secs_f64(),
            retry_by_scope: anodizer_core::retry::retry_scope_breakdown()
                .into_iter()
                .map(|(scope, retries, backoff)| RetryScopeStat {
                    scope,
                    retries,
                    backoff_secs: backoff.as_secs_f64(),
                })
                .collect(),
            results,
            determinism_allowlist: DeterminismAllowlist {
                compile_time,
                runtime,
            },
        }
    }
}

impl RunSummary {
    /// Names of Submitter-group publishers whose publish action landed —
    /// the version-burning set behind
    /// [`irreversibly_published`](Self::irreversibly_published).
    ///
    /// Read-side mirror of the build-side rule (status-string based, so
    /// it works on deserialized summaries): every status except `failed`
    /// and `skipped-*` means the publish reached the remote. Non-empty
    /// on summaries written BEFORE the `irreversibly_published` field
    /// existed too, so recovery tooling reading an old summary still
    /// sees the burn.
    pub fn burned_submitter_names(&self) -> Vec<String> {
        self.results
            .iter()
            .filter(|r| r.group == PublisherGroup::Submitter && status_landed(&r.status))
            .map(|r| r.name.clone())
            .collect()
    }
}

/// Status-string twin of [`outcome_landed`], for deserialized summaries
/// where only the kebab-case status survives.
fn status_landed(status: &str) -> bool {
    status != "failed" && !status.starts_with("skipped-")
}

/// Fold per-publisher outcomes into the top-level
/// `(publishers_succeeded, publishers_failed)` pair.
///
/// "Succeeded" means durable published state exists somewhere in the
/// world that a destructive recovery (tag delete, revert push) would
/// orphan. `RollbackFailed` and `RollbackSkippedNoScope` count as
/// succeeded for that reason: the publish landed and nothing withdrew
/// it, so the published state is presumed live. `RolledBack` does NOT
/// count — the state was published and then verifiably withdrawn.
fn count_publish_state(results: &[anodizer_core::publish_report::PublisherResult]) -> (u32, u32) {
    let mut succeeded = 0u32;
    let mut failed = 0u32;
    for r in results {
        match r.outcome {
            PublisherOutcome::Succeeded
            | PublisherOutcome::PendingModeration
            | PublisherOutcome::PendingValidation
            | PublisherOutcome::PublishedNoRollback
            | PublisherOutcome::RollbackFailed(_)
            | PublisherOutcome::RollbackSkippedNoScope => succeeded += 1,
            PublisherOutcome::Failed(_) => failed += 1,
            PublisherOutcome::Skipped(_) | PublisherOutcome::RolledBack => {}
        }
    }
    (succeeded, failed)
}

/// True when the outcome records that the publish ACTION landed at the
/// remote at some point — regardless of any later rollback. Only
/// `skipped-*` (never ran) and `failed` (ran, did not land) are
/// non-landed. Distinct from [`count_publish_state`]'s "durable state"
/// rule: a `rolled-back` publisher has no live state left, but for a
/// Submitter target the landing itself burned the version slot.
fn outcome_landed(outcome: &PublisherOutcome) -> bool {
    !matches!(
        outcome,
        PublisherOutcome::Skipped(_) | PublisherOutcome::Failed(_)
    )
}

/// Map a `PublisherOutcome` to the kebab-case status string defined
/// by the spec's "Status Set" enumeration. The `announce-gated`
/// status is intentionally not produced here because it is a
/// top-level flag on the report rather than a per-publisher outcome.
fn outcome_to_status_string(outcome: &PublisherOutcome) -> String {
    match outcome {
        PublisherOutcome::Succeeded => "succeeded".into(),
        PublisherOutcome::Skipped(reason) => match reason {
            SkipReason::SubmitterGated => "skipped-submitter-gated".into(),
            SkipReason::NotConfigured => "skipped-not-configured".into(),
            SkipReason::Snapshot => "skipped-snapshot".into(),
            SkipReason::DryRun => "skipped-dry-run".into(),
            SkipReason::Nightly => "skipped-nightly".into(),
            SkipReason::NotApplicable => "skipped-not-applicable".into(),
            SkipReason::AlreadyPublished => "skipped-already-published".into(),
            SkipReason::Deselected => "skipped-deselected".into(),
            SkipReason::VerifyGateBlocked => "skipped-verify-gate-blocked".into(),
            SkipReason::ConfigSkipped => "skipped-config".into(),
        },
        PublisherOutcome::Failed(_) => "failed".into(),
        PublisherOutcome::RolledBack => "rolled-back".into(),
        PublisherOutcome::RollbackFailed(_) => "rollback-failed".into(),
        PublisherOutcome::RollbackSkippedNoScope => "rollback-skipped-no-scope".into(),
        PublisherOutcome::PendingModeration => "pending-moderation".into(),
        PublisherOutcome::PendingValidation => "pending-validation".into(),
        PublisherOutcome::PublishedNoRollback => "published-no-rollback".into(),
    }
}

/// Write the run summary JSON to the given path. Creates parent
/// directories if missing. Pretty-prints so operators reading the
/// file directly do not have to pipe through `jq`.
///
/// Returns `true` when the summary was written, `false` when an
/// existing file was PRESERVED: a summary with no publisher results
/// never overwrites one that has them. Report-less pipelines
/// (standalone `announce`, `release --split`) resolve the same tag —
/// and therefore the same run dir — as the release run that preceded
/// them; letting their empty summary clobber the real one would erase
/// the burn evidence the rollback guard keys on (fail-open).
pub fn write_summary_json(summary: &RunSummary, path: &Path) -> Result<bool> {
    if summary.results.is_empty()
        && let Ok(existing) = fs::read_to_string(path)
        && serde_json::from_str::<RunSummary>(&existing)
            .is_ok_and(|prior| !prior.results.is_empty())
    {
        return Ok(false);
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "create parent directory {} for summary.json",
                parent.display()
            )
        })?;
    }
    let text = serde_json::to_string_pretty(summary).context("serialize run summary")?;
    anodizer_core::fs_atomic::atomic_write_str(path, &text)
        .with_context(|| format!("write run summary to {}", path.display()))?;
    Ok(true)
}

/// Resolve where this run's `summary.json` belongs.
///
/// An explicit `--summary-json=<path>` wins in every mode. Otherwise the
/// default is `<dist>/run-<id>/summary.json` (next to the publish stage's
/// `report.json` for the same run; `<dist>` is the per-crate dist in
/// per-crate workspace mode, so each published crate gets its own
/// summary), suppressed only for snapshot / dry-run pipelines — those are
/// not real releases and must not pollute `dist/run-*/`.
///
/// The default deliberately does NOT require `ctx.publish_report`: a real
/// release that fails BEFORE the publish stage (tag resolution, build,
/// release-asset upload) must still leave machine-readable state on disk —
/// CI reads the summary post-mortem to decide whether destructive recovery
/// (tag rollback) is safe, and "no file" forces it to guess. A report-less
/// summary carries the tag, empty publisher results, and
/// `irreversibly_published: false`. Report-less pipelines that share a run
/// dir with a prior real run (standalone `announce`, `release --split`)
/// cannot erase that run's publish state: [`write_summary_json`] refuses
/// to clobber a results-bearing summary with an empty one.
pub fn summary_path(ctx: &Context) -> Option<std::path::PathBuf> {
    if let Some(explicit) = ctx.options.summary_json_path.clone() {
        return Some(explicit);
    }
    if ctx.is_snapshot() || ctx.is_dry_run() {
        return None;
    }
    let run_id = crate::derive_run_id(ctx);
    Some(crate::run_dir(ctx, &run_id).join(anodizer_core::dist::SUMMARY_JSON))
}

/// Persist a point-in-time summary for an in-progress publish run.
///
/// Called by the dispatch loop after every publisher completes so a
/// hard kill (SIGKILL, OOM, runner eviction) mid-publish still leaves
/// the last-known per-publisher state on disk for recovery tooling.
/// The pipeline-end `emit_summary` rewrite supersedes the final
/// snapshot on every orderly exit (success or Err).
///
/// No-op when [`summary_path`] resolves to `None` (snapshot / dry-run).
pub fn persist_summary_snapshot(
    ctx: &Context,
    report: &anodizer_core::publish_report::PublishReport,
) -> Result<()> {
    let Some(path) = summary_path(ctx) else {
        return Ok(());
    };
    let summary = RunSummary::from_context_with_report(ctx, Some(report));
    write_summary_json(&summary, &path).map(|_| ())
}

/// Every `summary.json` under `<dist>/run-*/` (single-crate / lockstep
/// layout) and `<dist>/<crate>/run-*/` (per-crate workspace layout).
/// The two layouts are the writer-side contract of [`summary_path`]:
/// per-crate publish runs re-anchor `dist` onto `dist/<crate>/`, so a
/// reader that wants the whole run's evidence must walk both levels.
pub fn collect_run_summary_paths(dist: &Path) -> Vec<std::path::PathBuf> {
    fn summaries_in(dir: &Path) -> Vec<std::path::PathBuf> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(anodizer_core::dist::RUN_DIR_PREFIX)
            })
            .map(|e| e.path().join(anodizer_core::dist::SUMMARY_JSON))
            .filter(|p| p.is_file())
            .collect()
    }

    let mut paths = summaries_in(dist);
    if let Ok(entries) = fs::read_dir(dist) {
        for entry in entries.flatten().filter(|e| e.path().is_dir()) {
            paths.extend(summaries_in(&entry.path()));
        }
    }
    paths
}

/// Stamp `record` onto every parseable run summary under `dist` (both
/// layout levels — see [`collect_run_summary_paths`]) and rewrite them
/// in place. Returns the number of summaries updated. Unreadable or
/// unparseable files are skipped via `warn`: the record is a secondary
/// observability channel and must never mask the release failure that
/// triggered it.
pub fn record_failure_policy(
    dist: &Path,
    record: &FailurePolicyRecord,
    warn: &mut dyn FnMut(&str),
) -> usize {
    let mut updated = 0;
    for path in collect_run_summary_paths(dist) {
        let parsed: Result<RunSummary> = fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str(&text)?));
        match parsed {
            Ok(mut summary) => {
                summary.failure_policy = Some(record.clone());
                match write_summary_json(&summary, &path) {
                    Ok(_) => updated += 1,
                    Err(e) => warn(&format!(
                        "failure-policy record write failed for {}: {e:#}",
                        path.display()
                    )),
                }
            }
            Err(e) => warn(&format!(
                "failure-policy record skipped unreadable summary {}: {e:#}",
                path.display()
            )),
        }
    }
    updated
}

/// How far the publish stage got this run, for the zero-results
/// placeholder row in [`status_table_rows`].
///
/// Derived at summary time from the `(publish_attempted, publish_report)`
/// context pair: report present means [`Ran`](Self::Ran); attempted with
/// no report means a pre-dispatch guard aborted the stage; neither means
/// the stage never started (snapshot / `--skip=publish`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishDisposition {
    /// Publish stages never started — operator skip (snapshot mode /
    /// `--skip=publish`), a stage set that excludes publish, or an
    /// earlier stage failing before publish was reached. The context
    /// pair cannot distinguish these, so the row wording stays neutral:
    /// "did not run".
    Skipped,
    /// The publish stage started but aborted before dispatching any
    /// publisher (e.g. rerun refusal, runtime allowlist validation).
    Aborted,
    /// The publisher dispatcher ran to completion.
    Ran,
}

/// Build the end-of-pipeline per-publisher status rows as `(key, value)`
/// pairs in the log's kv register — the caller feeds each pair to
/// `StageLogger::kv` with a shared key width so the value column aligns.
///
/// Output shape once rendered (operator-facing):
///
/// ```text
/// • github-release   Assets     required  succeeded
/// • homebrew         Manager    optional  failed
/// • cargo            Submitter  required  skipped-submitter-gated
/// • run flags        submitter_gated=false announce_gated=true
/// ```
///
/// With zero publisher results a single placeholder row stands in for
/// the per-publisher block, so the summary still states *why* it is
/// empty instead of rendering a bare header. `disposition` selects the
/// cause — skipped stage, pre-dispatch abort, and a zero-publisher
/// configuration read very differently to an operator:
///
/// ```text
/// • publishers   none ran (publish stages did not run)
/// • run flags    submitter_gated=false announce_gated=false
/// ```
pub fn status_table_rows(
    summary: &RunSummary,
    disposition: PublishDisposition,
) -> Vec<(String, String)> {
    // Cap the name column so a pathological publisher name (e.g. an
    // operator pastes a URL into `publishers.custom[].name`) cannot
    // blow out the terminal width in CI logs. 40 chars covers every
    // realistic publisher name in the built-in registry (longest is
    // `homebrew-formula` at 16) plus generous headroom for custom
    // ones; anything longer is truncated with an ellipsis so the
    // remaining columns still line up.
    const NAME_CAP: usize = 40;

    let truncate_name = |name: &str| -> String {
        // `char_indices` keeps the truncation UTF-8-safe; slicing by
        // byte index inside a multi-byte char would panic. The cap
        // operates on char count, not byte count, so a name with
        // multibyte characters fits the visual width consistently.
        if name.chars().count() > NAME_CAP {
            let cutoff = name
                .char_indices()
                .nth(NAME_CAP - 1)
                .map(|(i, _)| i)
                .unwrap_or(name.len());
            format!("{}…", &name[..cutoff])
        } else {
            name.to_string()
        }
    };

    let mut rows: Vec<(String, String)> = Vec::new();
    if summary.results.is_empty() {
        let why = match disposition {
            PublishDisposition::Ran => "none ran (no publishers configured)",
            PublishDisposition::Aborted => "none ran (publish stage aborted before dispatch)",
            PublishDisposition::Skipped => "none ran (publish stages did not run)",
        };
        rows.push(("publishers".to_string(), why.to_string()));
    } else {
        let group_width = summary
            .results
            .iter()
            .map(|r| format!("{:?}", r.group).len())
            .max()
            .unwrap_or(0);
        for r in &summary.results {
            // "required" and "optional" are both 8 chars, so the status
            // column aligns without padding the requirement cell.
            let requirement = if r.required { "required" } else { "optional" };
            rows.push((
                truncate_name(&r.name),
                format!(
                    "{:<group_width$}  {requirement}  {}",
                    format!("{:?}", r.group),
                    r.status,
                ),
            ));
        }
    }
    // Verify-release verdict on a separate axis from the publisher rows: the
    // gate runs after the irreversible publish, so a FAILED verdict here does
    // NOT mean a publish failed. The explicit "the release IS published"
    // wording stops an operator misreading the row as a publish failure.
    if let Some(vr) = summary.verify_release.as_ref() {
        let value = if vr.passed {
            "passed".to_string()
        } else {
            format!(
                "FAILED ({} issue(s)) — the release IS published; investigate",
                vr.issue_count
            )
        };
        rows.push(("verify-release".to_string(), value));
        // Fold each defect into its own sub-row so the operator sees the
        // specifics inline rather than having to open summary.json.
        for issue in &vr.issues {
            rows.push((String::new(), format!("- {issue}")));
        }
    }
    // Only surface the retry-backoff row when the run actually backed off:
    // a clean run reads noise-free, while a flaky remote leaves a visible
    // "spent Ns retrying" trace even though the run ultimately succeeded.
    if summary.retry_backoff_secs > 0.0 {
        rows.push((
            "retry backoff".to_string(),
            format!("spent {:.1}s in retry backoff", summary.retry_backoff_secs),
        ));
        // Fold each publisher/stage that backed off into its own indented
        // sub-row (biggest offender first) so the operator sees WHICH remote
        // was flaky without opening summary.json.
        for s in &summary.retry_by_scope {
            rows.push((
                String::new(),
                format!("{}  {} retries  {:.1}s", s.scope, s.retries, s.backoff_secs),
            ));
        }
    }
    rows.push((
        "run flags".to_string(),
        format!(
            "submitter_gated={} announce_gated={}",
            summary.submitter_gated, summary.announce_gated,
        ),
    ));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::publish_evidence::PublishEvidence;
    use anodizer_core::publish_report::{PublishReport, PublisherResult};

    fn populated_summary() -> RunSummary {
        RunSummary {
            schema_version: RunSummary::CURRENT_SCHEMA_VERSION,
            anodize_version: "0.0.0-test".to_string(),
            tag: "v1.2.3".to_string(),
            submitter_gated: true,
            announce_gated: false,
            publishers_succeeded: 1,
            publishers_failed: 0,
            irreversibly_published: false,
            failure_policy: None,
            verify_release: None,
            retry_backoff_secs: 0.0,
            retry_by_scope: vec![],
            results: vec![
                RunSummaryResult {
                    name: "github-release".to_string(),
                    group: PublisherGroup::Assets,
                    required: true,
                    status: "succeeded".to_string(),
                    evidence: Some(PublishEvidence {
                        schema_version: 1,
                        publisher: "github-release".to_string(),
                        primary_ref: Some("https://example.com/r/v1".to_string()),
                        artifact_paths: vec![],
                        nondeterministic: None,
                        extra: anodizer_core::PublishEvidenceExtra::Empty,
                    }),
                },
                RunSummaryResult {
                    name: "cargo".to_string(),
                    group: PublisherGroup::Submitter,
                    required: true,
                    status: "skipped-submitter-gated".to_string(),
                    evidence: None,
                },
            ],
            determinism_allowlist: DeterminismAllowlist {
                compile_time: vec![],
                runtime: vec![DeterminismAllowlistEntry {
                    artifact: "anodizer.tar.gz".to_string(),
                    reason: "embedded build date".to_string(),
                }],
            },
        }
    }

    #[test]
    fn run_summary_schema_v1_roundtrips_through_json() {
        let s = populated_summary();
        let text = serde_json::to_string(&s).expect("serialize");
        let back: RunSummary = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, s);
    }

    #[test]
    fn run_summary_rejects_unknown_fields() {
        let bad = r#"{
            "schema_version": 1,
            "anodize_version": "0.0.0-test",
            "tag": "v0.0.0",
            "submitter_gated": false,
            "announce_gated": false,
            "results": [],
            "determinism_allowlist": {"compile_time": [], "runtime": []},
            "future_field": "should reject"
        }"#;
        let parsed: std::result::Result<RunSummary, _> = serde_json::from_str(bad);
        assert!(parsed.is_err(), "unknown fields must be denied");
    }

    #[test]
    fn run_summary_parses_legacy_json_without_publish_counts() {
        // Summaries written before the publish-state counts existed
        // must still deserialize; the counts default to zero.
        let legacy = r#"{
            "schema_version": 1,
            "anodize_version": "0.0.0-test",
            "tag": "v0.0.0",
            "submitter_gated": false,
            "announce_gated": false,
            "results": [],
            "determinism_allowlist": {"compile_time": [], "runtime": []}
        }"#;
        let parsed: RunSummary = serde_json::from_str(legacy).expect("legacy summary parses");
        assert_eq!(parsed.publishers_succeeded, 0);
        assert_eq!(parsed.publishers_failed, 0);
        assert!(
            !parsed.irreversibly_published,
            "missing irreversibly_published must default to false"
        );
    }

    #[test]
    fn run_summary_roundtrips_with_verify_release_present() {
        // The new Option field serializes + deserializes losslessly when set.
        let mut s = populated_summary();
        s.verify_release = Some(VerifyReleaseRecord {
            passed: false,
            issue_count: 2,
            issues: vec![
                "install smoke-test failed for crate 'app' (app.deb)".to_string(),
                "produced artifact missing for crate 'app': app.rpm".to_string(),
            ],
        });
        let text = serde_json::to_string(&s).expect("serialize");
        let back: RunSummary = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, s);
        let vr = back
            .verify_release
            .expect("verify_release survives round-trip");
        assert!(!vr.passed);
        assert_eq!(vr.issue_count, 2);
        assert_eq!(vr.issues.len(), 2);
    }

    #[test]
    fn run_summary_parses_legacy_json_without_verify_release() {
        // A summary written before the verify_release field existed must still
        // deserialize; the field defaults to None (no spurious row).
        let legacy = r#"{
            "schema_version": 1,
            "anodize_version": "0.0.0-test",
            "tag": "v0.0.0",
            "submitter_gated": false,
            "announce_gated": false,
            "results": [],
            "determinism_allowlist": {"compile_time": [], "runtime": []}
        }"#;
        let parsed: RunSummary = serde_json::from_str(legacy).expect("legacy summary parses");
        assert!(
            parsed.verify_release.is_none(),
            "missing verify_release must default to None"
        );
        // And re-serializing must NOT emit a `verify_release` key (skip_serializing_if).
        let text = serde_json::to_string(&parsed).expect("serialize");
        assert!(
            !text.contains("verify_release"),
            "None verify_release must be omitted from JSON: {text}"
        );
    }

    #[test]
    fn status_table_renders_failed_verify_release_row() {
        // A failing verify-release verdict produces a FAILED row plus a sub-row
        // per issue — the publisher rows stay untouched (separate axis).
        let mut s = populated_summary();
        s.verify_release = Some(VerifyReleaseRecord {
            passed: false,
            issue_count: 1,
            issues: vec!["install smoke-test failed for crate 'app'".to_string()],
        });
        let rows = status_table_rows(&s, PublishDisposition::Ran);
        let verdict = rows
            .iter()
            .find(|(k, _)| k == "verify-release")
            .expect("a verify-release row must be present");
        assert!(
            verdict.1.contains("FAILED (1 issue(s))")
                && verdict.1.contains("the release IS published"),
            "FAILED verdict wording: {}",
            verdict.1
        );
        assert!(
            rows.iter()
                .any(|(_, v)| v.contains("install smoke-test failed for crate 'app'")),
            "the issue detail must appear as a sub-row: {rows:?}"
        );
    }

    #[test]
    fn status_table_renders_passed_verify_release_row() {
        let mut s = populated_summary();
        s.verify_release = Some(VerifyReleaseRecord {
            passed: true,
            issue_count: 0,
            issues: vec![],
        });
        let rows = status_table_rows(&s, PublishDisposition::Ran);
        let verdict = rows
            .iter()
            .find(|(k, _)| k == "verify-release")
            .expect("a verify-release row must be present");
        assert_eq!(verdict.1, "passed");
    }

    #[test]
    fn status_table_omits_verify_release_row_when_absent() {
        let s = populated_summary(); // verify_release: None
        let rows = status_table_rows(&s, PublishDisposition::Ran);
        assert!(
            !rows.iter().any(|(k, _)| k == "verify-release"),
            "no verify-release row when the gate did not run: {rows:?}"
        );
    }

    fn result_in(group: PublisherGroup, outcome: PublisherOutcome) -> PublisherResult {
        PublisherResult {
            name: "p".to_string(),
            group,
            required: false,
            outcome,
            evidence: None,
        }
    }

    #[test]
    fn count_publish_state_classifies_every_outcome() {
        let result = |outcome: PublisherOutcome| result_in(PublisherGroup::Manager, outcome);
        let results = vec![
            result(PublisherOutcome::Succeeded),
            result(PublisherOutcome::PendingModeration),
            result(PublisherOutcome::PendingValidation),
            result(PublisherOutcome::PublishedNoRollback),
            result(PublisherOutcome::RollbackFailed("boom".into())),
            result(PublisherOutcome::Failed("boom".into())),
            result(PublisherOutcome::Skipped(SkipReason::NotConfigured)),
            result(PublisherOutcome::RolledBack),
            result(PublisherOutcome::RollbackSkippedNoScope),
        ];
        let (succeeded, failed) = count_publish_state(&results);
        assert_eq!(
            succeeded, 6,
            "published-state outcomes: Succeeded + 2 Pending + PublishedNoRollback \
             + RollbackFailed + RollbackSkippedNoScope"
        );
        assert_eq!(failed, 1, "only Failed counts as failed");
    }

    #[test]
    fn outcome_landed_classifies_every_outcome() {
        // Landed = the publish action reached the remote at some point.
        // Only never-ran (skipped) and ran-but-did-not-land (failed) are
        // non-landed; a rolled-back Submitter publish still burned its
        // version slot.
        for (outcome, landed) in [
            (PublisherOutcome::Succeeded, true),
            (PublisherOutcome::PendingModeration, true),
            (PublisherOutcome::PendingValidation, true),
            (PublisherOutcome::PublishedNoRollback, true),
            (PublisherOutcome::RollbackFailed("boom".into()), true),
            (PublisherOutcome::RolledBack, true),
            (PublisherOutcome::RollbackSkippedNoScope, true),
            (PublisherOutcome::Failed("boom".into()), false),
            (PublisherOutcome::Skipped(SkipReason::NotConfigured), false),
        ] {
            assert_eq!(
                outcome_landed(&outcome),
                landed,
                "outcome_landed({outcome:?})"
            );
        }
    }

    #[test]
    fn outcome_to_status_string_for_each_variant() {
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::Succeeded),
            "succeeded"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::Skipped(SkipReason::SubmitterGated)),
            "skipped-submitter-gated"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::Skipped(SkipReason::NotConfigured)),
            "skipped-not-configured"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::Skipped(SkipReason::Snapshot)),
            "skipped-snapshot"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::Skipped(SkipReason::DryRun)),
            "skipped-dry-run"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::Failed("boom".into())),
            "failed"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::RolledBack),
            "rolled-back"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::RollbackFailed("oops".into())),
            "rollback-failed"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::RollbackSkippedNoScope),
            "rollback-skipped-no-scope"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::PendingModeration),
            "pending-moderation"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::PendingValidation),
            "pending-validation"
        );
        assert_eq!(
            outcome_to_status_string(&PublisherOutcome::PublishedNoRollback),
            "published-no-rollback"
        );
    }

    #[test]
    fn from_context_captures_runtime_allowlist() {
        // Fallback path: when `ctx.determinism` is None, the runtime
        // allowlist falls back to `ctx.options` so the audit row still
        // appears in the summary.
        let mut ctx = anodizer_core::context::Context::test_fixture();
        ctx.options.runtime_nondeterministic_allowlist = vec![
            (
                "anodizer.tar.gz".to_string(),
                "embedded build date".to_string(),
            ),
            ("anodizer.deb".to_string(), "dpkg timestamp".to_string()),
        ];
        let s = RunSummary::from_context(&ctx);
        assert_eq!(s.determinism_allowlist.runtime.len(), 2);
        assert_eq!(
            s.determinism_allowlist.runtime[0].artifact,
            "anodizer.tar.gz"
        );
        assert_eq!(
            s.determinism_allowlist.runtime[0].reason,
            "embedded build date"
        );
        assert_eq!(s.determinism_allowlist.runtime[1].artifact, "anodizer.deb");
        assert!(s.determinism_allowlist.compile_time.is_empty());
    }

    #[test]
    fn allow_nondeterministic_appears_in_run_summary() {
        // Primary path: when `ctx.determinism` is populated, the
        // run-summary reads both compile-time and runtime entries from
        // there (the single source of truth).
        let mut ctx = anodizer_core::context::Context::test_fixture();
        let mut state =
            anodizer_core::DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        state.append_runtime(
            "anodizer.tar.gz".to_string(),
            "embedded build date".to_string(),
        );
        ctx.determinism = Some(state);
        let s = RunSummary::from_context(&ctx);
        assert!(
            s.determinism_allowlist
                .runtime
                .iter()
                .any(|e| e.artifact == "anodizer.tar.gz" && e.reason == "embedded build date"),
            "runtime entry must round-trip into the summary: {:?}",
            s.determinism_allowlist.runtime,
        );
        assert!(
            !s.determinism_allowlist.compile_time.is_empty(),
            "compile_time entries must be mirrored from DeterminismState (seed_from_commit \
             populates the spec contract list)",
        );
    }

    #[test]
    fn compile_time_wins_on_collision_in_report() {
        // Operator passes `--allow-nondeterministic foo.flatpak=local-override`,
        // but the compile-time list also covers `*.flatpak`. Both entries
        // must appear in the run-summary allow-list arrays so the audit
        // trail is complete; the per-artifact precedence is verified
        // separately via `DeterminismState::resolve_reason`. `*.flatpak` is
        // the stable anchor: it is intrinsically non-reproducible (OSTree
        // commit metadata) and thus permanently compile-time allow-listed,
        // unlike `.crate`/`.rpm`/`.deb`/`.snap` which are now gated.
        let mut ctx = anodizer_core::context::Context::test_fixture();
        let mut state =
            anodizer_core::DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        state.append_runtime("foo.flatpak".to_string(), "local-override".to_string());
        ctx.determinism = Some(state.clone());
        let s = RunSummary::from_context(&ctx);
        // Compile-time list has the `*.flatpak` glob.
        assert!(
            s.determinism_allowlist
                .compile_time
                .iter()
                .any(|e| e.artifact == "*.flatpak"),
            "compile-time `*.flatpak` entry must appear in the summary",
        );
        // Runtime list has the operator's overlapping entry.
        assert!(
            s.determinism_allowlist
                .runtime
                .iter()
                .any(|e| e.artifact == "foo.flatpak" && e.reason == "local-override"),
            "runtime override must appear in the summary alongside the compile-time entry",
        );
        // Per-artifact precedence: compile-time wins.
        let reason = state.resolve_reason("foo.flatpak").unwrap();
        assert!(
            reason.contains("OSTree"),
            "compile-time reason must win on collision, got: {reason}",
        );
    }

    #[test]
    fn from_context_pulls_tag_and_gate_flags() {
        let mut ctx = anodizer_core::context::Context::test_fixture();
        let report = PublishReport {
            submitter_gated: true,
            announce_gated: true,
            verify_gate_blocked: false,
            verify_gate_evaluated: false,
            results: vec![PublisherResult {
                name: "homebrew".to_string(),
                group: PublisherGroup::Manager,
                required: false,
                outcome: PublisherOutcome::Failed("boom".to_string()),
                evidence: None,
            }],
        };
        ctx.publish_report = Some(report);
        let s = RunSummary::from_context(&ctx);
        assert_eq!(s.tag, "v0.0.0-test");
        assert!(s.submitter_gated);
        assert!(s.announce_gated);
        assert_eq!(s.results.len(), 1);
        assert_eq!(s.results[0].status, "failed");
        assert_eq!(s.results[0].name, "homebrew");
        assert_eq!(s.publishers_succeeded, 0);
        assert_eq!(s.publishers_failed, 1);
        assert!(!s.irreversibly_published);
    }

    #[test]
    fn from_context_without_publish_report_yields_empty_results_and_default_flags() {
        let ctx = anodizer_core::context::Context::test_fixture();
        let s = RunSummary::from_context(&ctx);
        assert!(s.results.is_empty());
        assert!(!s.submitter_gated);
        assert!(!s.announce_gated);
        assert!(!s.irreversibly_published);
    }

    #[test]
    fn irreversibly_published_keys_on_submitter_group_only() {
        // A fully-successful run of REVERSIBLE publishers (Assets,
        // Manager) must not flag the version as burned — every one of
        // them can be deleted and the same version re-cut. The flag
        // flips only when a Submitter (one-way-door) publish landed.
        let mut ctx = anodizer_core::context::Context::test_fixture();
        ctx.publish_report = Some(PublishReport {
            submitter_gated: false,
            announce_gated: false,
            verify_gate_blocked: false,
            verify_gate_evaluated: false,
            results: vec![
                result_in(PublisherGroup::Assets, PublisherOutcome::Succeeded),
                result_in(PublisherGroup::Manager, PublisherOutcome::Succeeded),
                result_in(
                    PublisherGroup::Submitter,
                    PublisherOutcome::Skipped(SkipReason::SubmitterGated),
                ),
            ],
        });
        let s = RunSummary::from_context(&ctx);
        assert_eq!(s.publishers_succeeded, 2);
        assert!(
            !s.irreversibly_published,
            "reversible-group successes must not burn the version"
        );

        ctx.publish_report = Some(PublishReport {
            submitter_gated: false,
            announce_gated: false,
            verify_gate_blocked: false,
            verify_gate_evaluated: false,
            results: vec![result_in(
                PublisherGroup::Submitter,
                PublisherOutcome::Succeeded,
            )],
        });
        let s = RunSummary::from_context(&ctx);
        assert!(
            s.irreversibly_published,
            "Submitter success burns the version"
        );
    }

    #[test]
    fn irreversibly_published_counts_rolled_back_submitter() {
        // cargo yank withdraws the artifact but the version slot stays
        // burned — a same-version re-publish is rejected by crates.io.
        let mut ctx = anodizer_core::context::Context::test_fixture();
        ctx.publish_report = Some(PublishReport {
            submitter_gated: false,
            announce_gated: false,
            verify_gate_blocked: false,
            verify_gate_evaluated: false,
            results: vec![result_in(
                PublisherGroup::Submitter,
                PublisherOutcome::RolledBack,
            )],
        });
        let s = RunSummary::from_context(&ctx);
        assert!(s.irreversibly_published);
    }

    #[test]
    fn burned_submitter_names_agrees_with_irreversibly_published() {
        // The read-side (status-string) rule must agree with the
        // build-side (outcome) rule across every outcome, through a full
        // serialize → deserialize round-trip.
        let all_outcomes = [
            PublisherOutcome::Succeeded,
            PublisherOutcome::PendingModeration,
            PublisherOutcome::PendingValidation,
            PublisherOutcome::PublishedNoRollback,
            PublisherOutcome::RollbackFailed("boom".into()),
            PublisherOutcome::RolledBack,
            PublisherOutcome::RollbackSkippedNoScope,
            PublisherOutcome::Failed("boom".into()),
            PublisherOutcome::Skipped(SkipReason::SubmitterGated),
            PublisherOutcome::Skipped(SkipReason::NotConfigured),
            PublisherOutcome::Skipped(SkipReason::Snapshot),
            PublisherOutcome::Skipped(SkipReason::DryRun),
            PublisherOutcome::Skipped(SkipReason::Nightly),
            PublisherOutcome::Skipped(SkipReason::NotApplicable),
            PublisherOutcome::Skipped(SkipReason::AlreadyPublished),
            PublisherOutcome::Skipped(SkipReason::Deselected),
            PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked),
            PublisherOutcome::Skipped(SkipReason::ConfigSkipped),
        ];
        for outcome in all_outcomes {
            let mut ctx = anodizer_core::context::Context::test_fixture();
            ctx.publish_report = Some(PublishReport {
                submitter_gated: false,
                announce_gated: false,
                verify_gate_blocked: false,
                verify_gate_evaluated: false,
                results: vec![result_in(PublisherGroup::Submitter, outcome.clone())],
            });
            let s = RunSummary::from_context(&ctx);
            let back: RunSummary =
                serde_json::from_str(&serde_json::to_string(&s).expect("serialize"))
                    .expect("deserialize");
            assert_eq!(
                back.irreversibly_published,
                !back.burned_submitter_names().is_empty(),
                "rules disagree for {outcome:?}"
            );
            if back.irreversibly_published {
                assert_eq!(back.burned_submitter_names(), vec!["p".to_string()]);
            }
        }
    }

    #[test]
    fn write_summary_json_creates_parent_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let nested = tmp.path().join("a").join("b").join("summary.json");
        let s = populated_summary();
        write_summary_json(&s, &nested).expect("write");
        assert!(nested.exists());
        let back: RunSummary =
            serde_json::from_str(&fs::read_to_string(&nested).expect("read")).expect("parse");
        assert_eq!(back, s);
    }

    #[test]
    fn write_summary_json_overwrites_existing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("summary.json");
        fs::write(&path, "stale").expect("seed");
        let s = populated_summary();
        assert!(write_summary_json(&s, &path).expect("write"), "must write");
        let back: RunSummary =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(back, s);
    }

    #[test]
    fn write_summary_json_preserves_results_bearing_file_from_empty_summary() {
        // A report-less summary (results: []) must never clobber an
        // existing summary that carries publisher results — that file
        // is the burn evidence the rollback guard keys on.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("summary.json");
        let real = populated_summary();
        assert!(write_summary_json(&real, &path).expect("seed real summary"));

        let mut empty = populated_summary();
        empty.results.clear();
        empty.publishers_succeeded = 0;
        empty.irreversibly_published = false;
        assert!(
            !write_summary_json(&empty, &path).expect("preserve must not error"),
            "empty summary over results-bearing file must be skipped"
        );

        let back: RunSummary =
            serde_json::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        assert_eq!(back, real, "original summary must survive");

        // Empty-over-empty (and empty-over-missing) still writes: there
        // is no evidence to protect.
        let empty_path = tmp.path().join("fresh.json");
        assert!(write_summary_json(&empty, &empty_path).expect("write fresh"));
        assert!(write_summary_json(&empty, &empty_path).expect("rewrite empty over empty"));
    }

    #[test]
    fn status_table_rows_render_per_publisher_and_run_flags() {
        let s = populated_summary();
        let rows = status_table_rows(&s, PublishDisposition::Ran);
        // One row per publisher result, plus the trailing run-flags row.
        assert_eq!(rows.len(), s.results.len() + 1, "rows: {rows:?}");
        assert!(
            rows.iter().any(|(_, v)| v.contains("succeeded")),
            "succeeded row missing: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|(_, v)| v.contains("skipped-submitter-gated")),
            "submitter-gated row missing: {rows:?}"
        );
        // The required bool renders as the words required/optional, not
        // true/false.
        assert!(
            rows.iter()
                .any(|(_, v)| v.contains("required") || v.contains("optional")),
            "requirement cell missing: {rows:?}"
        );
        assert_eq!(
            rows.last().expect("non-empty"),
            &(
                "run flags".to_string(),
                "submitter_gated=true announce_gated=false".to_string()
            ),
            "run-flags row must close the table: {rows:?}"
        );
    }

    #[test]
    fn retry_backoff_row_appears_only_when_nonzero_and_precedes_run_flags() {
        // Zero backoff: no row (the baseline render test already asserts the
        // row count is results + 1). Nonzero: exactly one "retry backoff" row,
        // sitting immediately before the closing "run flags" row so the two
        // run-level rows stay grouped at the foot of the table.
        let mut s = populated_summary();
        assert!(
            !status_table_rows(&s, PublishDisposition::Ran)
                .iter()
                .any(|(k, _)| k == "retry backoff"),
            "a zero-backoff run must not emit a retry-backoff row"
        );

        s.retry_backoff_secs = 12.4;
        let rows = status_table_rows(&s, PublishDisposition::Ran);
        let idx = rows
            .iter()
            .position(|(k, _)| k == "retry backoff")
            .expect("nonzero backoff must emit a retry-backoff row");
        assert_eq!(
            rows[idx].1, "spent 12.4s in retry backoff",
            "row text: {rows:?}"
        );
        // With no per-scope breakdown, the total row is immediately followed
        // by run flags.
        assert_eq!(
            rows[idx + 1].0,
            "run flags",
            "retry-backoff row must sit directly before run flags: {rows:?}"
        );

        // Per-scope breakdown renders as indented sub-rows (blank key), in the
        // order given, between the total row and run flags.
        s.retry_by_scope = vec![
            RetryScopeStat {
                scope: "release".to_string(),
                retries: 3,
                backoff_secs: 130.0,
            },
            RetryScopeStat {
                scope: "cargo".to_string(),
                retries: 2,
                backoff_secs: 15.0,
            },
        ];
        let rows = status_table_rows(&s, PublishDisposition::Ran);
        let idx = rows
            .iter()
            .position(|(k, _)| k == "retry backoff")
            .expect("total row present");
        assert_eq!(
            rows[idx + 1],
            (String::new(), "release  3 retries  130.0s".to_string())
        );
        assert_eq!(
            rows[idx + 2],
            (String::new(), "cargo  2 retries  15.0s".to_string())
        );
        assert_eq!(
            rows[idx + 3].0,
            "run flags",
            "sub-rows precede run flags: {rows:?}"
        );
    }

    #[test]
    fn retry_backoff_secs_round_trips_and_omits_zero() {
        // Nonzero persists across a JSON round-trip; zero is omitted from the
        // serialized form (skip_serializing_if) yet deserializes back to 0.0
        // via `#[serde(default)]`, so older summaries stay readable.
        let mut s = populated_summary();
        s.retry_backoff_secs = 7.5;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"retry_backoff_secs\":7.5"), "json: {json}");
        let back: RunSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.retry_backoff_secs, 7.5);

        let zero = populated_summary();
        let zjson = serde_json::to_string(&zero).unwrap();
        assert!(
            !zjson.contains("retry_backoff_secs"),
            "zero backoff must be omitted: {zjson}"
        );
        let zback: RunSummary = serde_json::from_str(&zjson).unwrap();
        assert_eq!(zback.retry_backoff_secs, 0.0);
    }

    #[test]
    fn status_table_rows_empty_results_state_why() {
        // Zero publisher results must yield an explicit placeholder row,
        // not an empty table — the operator should read WHY nothing is
        // listed.
        let s = RunSummary {
            schema_version: RunSummary::CURRENT_SCHEMA_VERSION,
            anodize_version: "0.0.0-test".to_string(),
            tag: "v0.0.0".to_string(),
            submitter_gated: false,
            announce_gated: false,
            publishers_succeeded: 0,
            publishers_failed: 0,
            irreversibly_published: false,
            failure_policy: None,
            verify_release: None,
            retry_backoff_secs: 0.0,
            retry_by_scope: vec![],
            results: vec![],
            determinism_allowlist: DeterminismAllowlist::default(),
        };
        let rows = status_table_rows(&s, PublishDisposition::Skipped);
        assert_eq!(
            rows,
            vec![
                (
                    "publishers".to_string(),
                    "none ran (publish stages did not run)".to_string()
                ),
                (
                    "run flags".to_string(),
                    "submitter_gated=false announce_gated=false".to_string()
                ),
            ],
        );
        // Same empty results, but publish actually ran: the cause is a
        // zero-publisher configuration, not a skipped stage.
        let rows = status_table_rows(&s, PublishDisposition::Ran);
        assert_eq!(
            rows.first().expect("placeholder row"),
            &(
                "publishers".to_string(),
                "none ran (no publishers configured)".to_string()
            ),
        );
    }

    #[test]
    fn status_table_rows_empty_results_abort_state_why() {
        // A pre-dispatch guard abort (rerun refusal, runtime allowlist)
        // is neither "skipped" nor "zero publishers configured"; the
        // placeholder row must name the abort so the failure-path
        // summary doesn't mislabel the cause.
        let s = RunSummary {
            schema_version: RunSummary::CURRENT_SCHEMA_VERSION,
            anodize_version: "0.0.0-test".to_string(),
            tag: "v0.0.0".to_string(),
            submitter_gated: false,
            announce_gated: false,
            publishers_succeeded: 0,
            publishers_failed: 0,
            irreversibly_published: false,
            failure_policy: None,
            verify_release: None,
            retry_backoff_secs: 0.0,
            retry_by_scope: vec![],
            results: vec![],
            determinism_allowlist: DeterminismAllowlist::default(),
        };
        let rows = status_table_rows(&s, PublishDisposition::Aborted);
        assert_eq!(
            rows.first().expect("placeholder row"),
            &(
                "publishers".to_string(),
                "none ran (publish stage aborted before dispatch)".to_string()
            ),
        );
    }

    #[test]
    fn status_table_rows_keep_long_names_untruncated_under_cap() {
        // 29-char publisher name (longer than the historical 20-char
        // fixed width but under the 40-char cap) must survive as the row
        // key verbatim; the caller pads keys to the widest one so the
        // value column aligns.
        let s = RunSummary {
            schema_version: RunSummary::CURRENT_SCHEMA_VERSION,
            anodize_version: "0.0.0-test".to_string(),
            tag: "v0.0.0".to_string(),
            submitter_gated: false,
            announce_gated: false,
            publishers_succeeded: 1,
            publishers_failed: 0,
            irreversibly_published: false,
            failure_policy: None,
            verify_release: None,
            retry_backoff_secs: 0.0,
            retry_by_scope: vec![],
            results: vec![
                RunSummaryResult {
                    name: "custom-publisher-with-long-id".to_string(), // 29 chars
                    group: PublisherGroup::Manager,
                    required: false,
                    status: "succeeded".to_string(),
                    evidence: None,
                },
                RunSummaryResult {
                    name: "gh".to_string(),
                    group: PublisherGroup::Assets,
                    required: true,
                    status: "succeeded".to_string(),
                    evidence: None,
                },
            ],
            determinism_allowlist: DeterminismAllowlist::default(),
        };
        let rows = status_table_rows(&s, PublishDisposition::Ran);
        // The full long name is the row key, untruncated at this length.
        assert_eq!(
            rows[0].0, "custom-publisher-with-long-id",
            "long name must survive as the key untruncated: {rows:?}"
        );
        // Within the values, the group cell is padded to the widest group
        // so the requirement/status columns align across rows.
        let req_at = |v: &str| v.find("required").or_else(|| v.find("optional"));
        assert_eq!(
            req_at(&rows[0].1),
            req_at(&rows[1].1),
            "requirement column must align across rows: {rows:?}"
        );
    }

    #[test]
    fn status_table_rows_truncate_extremely_long_names() {
        // 60-char publisher name exceeds the 40-char cap; the row key
        // must replace the tail with an ellipsis so the caller's key
        // padding (and thus the value column) stays bounded in CI logs.
        let long_name = "x".repeat(60);
        let s = RunSummary {
            schema_version: RunSummary::CURRENT_SCHEMA_VERSION,
            anodize_version: "0.0.0-test".to_string(),
            tag: "v0.0.0".to_string(),
            submitter_gated: false,
            announce_gated: false,
            publishers_succeeded: 1,
            publishers_failed: 0,
            irreversibly_published: false,
            failure_policy: None,
            verify_release: None,
            retry_backoff_secs: 0.0,
            retry_by_scope: vec![],
            results: vec![RunSummaryResult {
                name: long_name.clone(),
                group: PublisherGroup::Assets,
                required: true,
                status: "succeeded".to_string(),
                evidence: None,
            }],
            determinism_allowlist: DeterminismAllowlist::default(),
        };
        let rows = status_table_rows(&s, PublishDisposition::Ran);
        let key = &rows[0].0;
        assert!(
            !key.contains(&long_name),
            "full 60-char name must NOT appear verbatim: {key:?}",
        );
        assert!(
            key.ends_with('…'),
            "ellipsis must mark the truncation: {key:?}"
        );
        // The cap is in chars (the `…` is one char), so the key stays at
        // the 40-char visual width.
        assert_eq!(
            key.chars().count(),
            40,
            "truncated key must sit at the 40-char cap: {key:?}"
        );
    }

    #[test]
    fn summary_anodize_version_is_cargo_pkg_version() {
        let ctx = anodizer_core::context::Context::test_fixture();
        let s = RunSummary::from_context(&ctx);
        assert_eq!(s.anodize_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn missing_failure_policy_field_defaults_to_none() {
        // Summaries written before the failure-policy field existed must
        // stay parseable by newer readers.
        let mut value = serde_json::to_value(populated_summary()).unwrap();
        value.as_object_mut().unwrap().remove("failure_policy");
        let parsed: RunSummary = serde_json::from_value(value).unwrap();
        assert!(parsed.failure_policy.is_none());
    }

    #[test]
    fn failure_policy_record_round_trips() {
        let mut summary = populated_summary();
        summary.failure_policy = Some(FailurePolicyRecord {
            configured: "rollback".to_string(),
            action: "held".to_string(),
            degraded: true,
            burned_publishers: vec!["cargo".to_string()],
            rollback_error: None,
        });
        let text = serde_json::to_string(&summary).unwrap();
        let parsed: RunSummary = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, summary);
        // Optional sub-fields stay off the wire when empty.
        assert!(!text.contains("rollback_error"));
    }

    #[test]
    fn collect_run_summary_paths_walks_both_layouts() {
        let dist = tempfile::tempdir().unwrap();
        let root_run = dist.path().join("run-v1.0.0");
        let crate_run = dist.path().join("crate-a").join("run-crate-a-v1.0.0");
        for dir in [&root_run, &crate_run] {
            fs::create_dir_all(dir).unwrap();
            fs::write(dir.join("summary.json"), "{}").unwrap();
        }
        // Distractors that must NOT be picked up: a non-run dir and a
        // run dir without a summary.
        fs::create_dir_all(dist.path().join("not-a-run")).unwrap();
        fs::create_dir_all(dist.path().join("run-empty")).unwrap();

        let mut paths = collect_run_summary_paths(dist.path());
        paths.sort();
        assert_eq!(
            paths,
            vec![
                crate_run.join("summary.json"),
                root_run.join("summary.json")
            ]
        );
    }

    #[test]
    fn record_failure_policy_stamps_every_summary_in_both_layouts() {
        let dist = tempfile::tempdir().unwrap();
        let root_path = dist.path().join("run-v1.0.0").join("summary.json");
        let crate_path = dist
            .path()
            .join("crate-a")
            .join("run-crate-a-v1.0.0")
            .join("summary.json");
        write_summary_json(&populated_summary(), &root_path).unwrap();
        write_summary_json(&populated_summary(), &crate_path).unwrap();

        let record = FailurePolicyRecord {
            configured: "rollback".to_string(),
            action: "rolled-back".to_string(),
            degraded: false,
            burned_publishers: vec![],
            rollback_error: None,
        };
        let mut warnings: Vec<String> = Vec::new();
        let updated =
            record_failure_policy(dist.path(), &record, &mut |m| warnings.push(m.to_string()));
        assert_eq!(updated, 2, "warnings: {warnings:?}");
        for path in [&root_path, &crate_path] {
            let parsed: RunSummary =
                serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
            assert_eq!(parsed.failure_policy.as_ref(), Some(&record));
            // Stamping must not disturb the publish results.
            assert_eq!(parsed.results.len(), 2);
        }
    }

    #[test]
    fn record_failure_policy_skips_unparseable_summary_with_warning() {
        let dist = tempfile::tempdir().unwrap();
        let bad = dist.path().join("run-v1.0.0").join("summary.json");
        fs::create_dir_all(bad.parent().unwrap()).unwrap();
        fs::write(&bad, "not json").unwrap();

        let record = FailurePolicyRecord {
            configured: "hold".to_string(),
            action: "held".to_string(),
            degraded: false,
            burned_publishers: vec![],
            rollback_error: None,
        };
        let mut warnings: Vec<String> = Vec::new();
        let updated =
            record_failure_policy(dist.path(), &record, &mut |m| warnings.push(m.to_string()));
        assert_eq!(updated, 0);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("summary"), "got: {warnings:?}");
    }
}
