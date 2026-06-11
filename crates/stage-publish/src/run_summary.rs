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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    pub results: Vec<RunSummaryResult>,
    pub determinism_allowlist: DeterminismAllowlist,
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

        Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            anodize_version: env!("CARGO_PKG_VERSION").to_string(),
            tag,
            submitter_gated: report.is_some_and(|r| r.submitter_gated),
            announce_gated: report.is_some_and(|r| r.announce_gated),
            publishers_succeeded,
            publishers_failed,
            irreversibly_published,
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
    Some(
        ctx.config
            .dist
            .join(format!("run-{run_id}"))
            .join("summary.json"),
    )
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

/// Pretty-print a per-publisher status table to the supplied writer
/// (typically `stderr`).
///
/// Output shape (operator-facing):
///
/// ```text
/// Publisher status:
///   name              group     required  status
///   github-release    Assets    true      succeeded
///   homebrew          Manager   false     failed
///   cargo             Submitter true      skipped-submitter-gated
///
/// Run flags: submitter_gated=false announce_gated=true
/// ```
pub fn print_status_table(
    summary: &RunSummary,
    out: &mut dyn std::io::Write,
) -> std::io::Result<()> {
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

    let name_width = summary
        .results
        .iter()
        .map(|r| truncate_name(&r.name).chars().count())
        .max()
        .unwrap_or(0)
        .max("name".len());
    let group_width = summary
        .results
        .iter()
        .map(|r| format!("{:?}", r.group).len())
        .max()
        .unwrap_or(0)
        .max("group".len());
    let required_width = "required".len();

    writeln!(out, "Publisher status:")?;
    writeln!(
        out,
        "  {:<nw$} {:<gw$} {:<rw$} status",
        "name",
        "group",
        "required",
        nw = name_width,
        gw = group_width,
        rw = required_width,
    )?;
    for r in &summary.results {
        writeln!(
            out,
            "  {:<nw$} {:<gw$} {:<rw$} {}",
            truncate_name(&r.name),
            format!("{:?}", r.group),
            r.required,
            r.status,
            nw = name_width,
            gw = group_width,
            rw = required_width,
        )?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "Run flags: submitter_gated={} announce_gated={}",
        summary.submitter_gated, summary.announce_gated,
    )?;
    Ok(())
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
        // Operator passes `--allow-nondeterministic foo.crate=local-override`,
        // but the compile-time list also covers `*.crate`. Both entries
        // must appear in the run-summary allow-list arrays so the audit
        // trail is complete; the per-artifact precedence is verified
        // separately via `DeterminismState::resolve_reason`.
        let mut ctx = anodizer_core::context::Context::test_fixture();
        let mut state =
            anodizer_core::DeterminismState::seed_from_commit(1_715_000_000).expect("non-negative");
        state.append_runtime("foo.crate".to_string(), "local-override".to_string());
        ctx.determinism = Some(state.clone());
        let s = RunSummary::from_context(&ctx);
        // Compile-time list has the `*.crate` glob.
        assert!(
            s.determinism_allowlist
                .compile_time
                .iter()
                .any(|e| e.artifact == "*.crate"),
            "compile-time `*.crate` entry must appear in the summary",
        );
        // Runtime list has the operator's overlapping entry.
        assert!(
            s.determinism_allowlist
                .runtime
                .iter()
                .any(|e| e.artifact == "foo.crate" && e.reason == "local-override"),
            "runtime override must appear in the summary alongside the compile-time entry",
        );
        // Per-artifact precedence: compile-time wins.
        let reason = state.resolve_reason("foo.crate").unwrap();
        assert!(
            reason.contains("cargo package non-determinism"),
            "compile-time reason must win on collision, got: {reason}",
        );
    }

    #[test]
    fn from_context_pulls_tag_and_gate_flags() {
        let mut ctx = anodizer_core::context::Context::test_fixture();
        let report = PublishReport {
            submitter_gated: true,
            announce_gated: true,
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
        ];
        for outcome in all_outcomes {
            let mut ctx = anodizer_core::context::Context::test_fixture();
            ctx.publish_report = Some(PublishReport {
                submitter_gated: false,
                announce_gated: false,
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
    fn print_status_table_renders_human_readable() {
        let s = populated_summary();
        let mut buf: Vec<u8> = Vec::new();
        print_status_table(&s, &mut buf).expect("print");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("Publisher status:"), "header missing: {text}");
        assert!(text.contains("succeeded"), "succeeded row missing: {text}");
        assert!(
            text.contains("skipped-submitter-gated"),
            "submitter-gated row missing: {text}"
        );
        assert!(
            text.contains("Run flags: submitter_gated=true announce_gated=false"),
            "run-flags line missing: {text}"
        );
    }

    #[test]
    fn print_status_table_widens_for_long_publisher_names() {
        // 25-char publisher name (longer than the historical 20-char
        // fixed width). The header and the row must agree on column
        // boundaries: the `group` header should start at the same
        // offset as the row's group column.
        let s = RunSummary {
            schema_version: RunSummary::CURRENT_SCHEMA_VERSION,
            anodize_version: "0.0.0-test".to_string(),
            tag: "v0.0.0".to_string(),
            submitter_gated: false,
            announce_gated: false,
            publishers_succeeded: 1,
            publishers_failed: 0,
            irreversibly_published: false,
            results: vec![RunSummaryResult {
                name: "custom-publisher-with-long-id".to_string(), // 29 chars
                group: PublisherGroup::Manager,
                required: false,
                status: "succeeded".to_string(),
                evidence: None,
            }],
            determinism_allowlist: DeterminismAllowlist::default(),
        };
        let mut buf: Vec<u8> = Vec::new();
        print_status_table(&s, &mut buf).expect("print");
        let text = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = text.lines().collect();
        // Lines: "Publisher status:", header, row.
        let header = lines[1];
        let row = lines[2];
        // Find `group` in header and `Manager` in row; their starting
        // byte offsets must match (column alignment).
        let header_group_at = header.find("group").expect("group header present");
        let row_group_at = row.find("Manager").expect("Manager cell present");
        assert_eq!(
            header_group_at, row_group_at,
            "header `group` column at {header_group_at} must align with row `Manager` cell at \
             {row_group_at}\nheader: {header:?}\nrow:    {row:?}",
        );
        // And the full long name must appear (no truncation at this length).
        assert!(
            row.contains("custom-publisher-with-long-id"),
            "long name must render untruncated: {row:?}",
        );
    }

    #[test]
    fn print_status_table_truncates_extremely_long_names() {
        // 60-char publisher name exceeds the 40-char cap; the rendered
        // row must replace the tail with an ellipsis so the remaining
        // columns still line up in the CI log.
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
            results: vec![RunSummaryResult {
                name: long_name.clone(),
                group: PublisherGroup::Assets,
                required: true,
                status: "succeeded".to_string(),
                evidence: None,
            }],
            determinism_allowlist: DeterminismAllowlist::default(),
        };
        let mut buf: Vec<u8> = Vec::new();
        print_status_table(&s, &mut buf).expect("print");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(
            !text.contains(&long_name),
            "full 60-char name must NOT appear verbatim: {text}",
        );
        assert!(
            text.contains('…'),
            "ellipsis must mark the truncation: {text}",
        );
        // Header and row must still align — compare by visual (char)
        // offset, not byte offset, since `…` is a 3-byte UTF-8 char.
        let lines: Vec<&str> = text.lines().collect();
        let header = lines[1];
        let row = lines[2];
        let char_offset = |line: &str, needle: &str| -> usize {
            let byte_at = line.find(needle).expect("needle present");
            line[..byte_at].chars().count()
        };
        let header_group_at = char_offset(header, "group");
        let row_group_at = char_offset(row, "Assets");
        assert_eq!(
            header_group_at, row_group_at,
            "truncated row must still align (char offsets): header {header_group_at} vs row \
             {row_group_at}\nheader: {header:?}\nrow:    {row:?}",
        );
    }

    #[test]
    fn summary_anodize_version_is_cargo_pkg_version() {
        let ctx = anodizer_core::context::Context::test_fixture();
        let s = RunSummary::from_context(&ctx);
        assert_eq!(s.anodize_version, env!("CARGO_PKG_VERSION"));
    }
}
