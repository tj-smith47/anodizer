//! In-process failure policy for `anodizer release`.
//!
//! On a release-pipeline failure the binary itself evaluates
//! `release.on_failure` and executes the result — no summary.json →
//! workflow-output → `if:` chain is needed on the CI side:
//!
//! - `rollback` (default): delete the run's release tag(s) and revert
//!   the version-bump commit via the same code path as `anodizer tag
//!   rollback`, so the version can be re-cut cleanly.
//! - `hold`: leave everything in place for forensics; the operator
//!   recovers with `release --rollback-only --from-run=<id>` or fixes
//!   forward.
//! - `--publish-only`: the tag and bump commit are permanent (a prior
//!   release created them), so this mode collapses to `hold` regardless
//!   of `on_failure` — see `decide`'s `publish_only` parameter.
//!
//! `rollback` auto-degrades to `hold` the moment any one-way-door
//! (Submitter-group) publisher has landed: crates.io, chocolatey,
//! winget, snapcraft and friends never accept the same version twice,
//! so the version is burned and destructive rollback could only orphan
//! the live published state. The degrade is decided from the run's own
//! evidence — every `summary.json` under the dist tree (which covers
//! prior crates in per-crate workspace mode) plus the live in-memory
//! publish report.
//!
//! The shared `tag rollback` execution path keeps its own
//! published-state guard as defense in depth: it re-derives burn
//! evidence per tag and additionally probes the GitHub Releases API for
//! tags with no summary, which is what protects re-publish runs (a
//! live published release with no local summary refuses rollback).
//!
//! Whatever path is taken is recorded into the run's summaries
//! (`failure_policy` field) so the audit artifact states how the
//! failure was handled. The original pipeline error always propagates —
//! both policies exit nonzero.

use anodizer_core::config::OnFailureConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_stage_publish::run_summary::{
    FailurePolicyRecord, RunSummary, collect_run_summary_paths, record_failure_policy,
    summary_path, write_summary_json,
};
use anyhow::Result;

use super::ReleaseOpts;
use crate::commands::tag::rollback::{Mode, RollbackOpts, Scope};

/// What the policy resolved to for this failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FailureAction {
    /// Roll back reversible state (tag delete + bump revert).
    Rollback,
    /// Leave state in place. `degraded` is true when the configured
    /// policy was `rollback` but a one-way-door publisher landed.
    Hold { degraded: bool },
}

/// Pure policy decision: configured policy × whether any irreversible
/// publisher landed × whether this is a publish-only run.
///
/// `publish_only` forces `Hold` unconditionally: in `--publish-only` the
/// tag and version-bump commit were created and pushed by a *prior*
/// release run and are already permanent (the build artifacts and GitHub
/// release shipped with it). The `Rollback` path reverts the source-repo
/// bump commit and deletes the released tag — destroying history that
/// publish-only never owns. So the only case that would roll back
/// (`rollback` configured, no one-way door burned) holds instead; every
/// other outcome is identical to a normal run — a one-way door that burned
/// still degrades the hold so the operator sees the burned publisher, and an
/// explicit `hold` stays a clean hold. Reversible publisher-level reversals
/// (cargo yank, npm unpublish) recover out-of-band via
/// `release --rollback-only --from-run=<id>`, which never touches the source
/// repo.
pub(super) fn decide(
    configured: OnFailureConfig,
    irreversible_landed: bool,
    publish_only: bool,
) -> FailureAction {
    match (configured, irreversible_landed) {
        (OnFailureConfig::Hold, _) => FailureAction::Hold { degraded: false },
        (OnFailureConfig::Rollback, true) => FailureAction::Hold { degraded: true },
        // The only publish-only divergence: a clean rollback would delete the
        // already-released tag, so publish-only holds instead.
        (OnFailureConfig::Rollback, false) if publish_only => {
            FailureAction::Hold { degraded: false }
        }
        (OnFailureConfig::Rollback, false) => FailureAction::Rollback,
    }
}

/// Whether the failure policy governs this invocation.
///
/// Covered: the full release pipeline, `--publish-only` (both dist
/// layouts), and `--merge` — every mode that reaches upstream
/// publishers. Excluded:
///
/// - `--dry-run` / `--snapshot`: no real tag, nothing upstream.
/// - `--preflight`: check-only mode, mutates nothing.
/// - `--prepare`: contractually local-only (publish stages skipped).
/// - `--split`: a partial build leg of an operator-orchestrated flow;
///   the publishing `--merge` leg carries the policy.
/// - `--announce-only`: re-fires notifications after an already
///   successful publish; a Slack 502 must never destroy the release.
/// - `--rollback-only`: already the recovery path.
pub(super) fn applies(opts: &ReleaseOpts) -> bool {
    !opts.dry_run
        && !opts.snapshot
        && !opts.preflight
        && !opts.prepare
        && !opts.split
        && !opts.announce_only
        && !opts.rollback_only
        // The determinism harness's hermetic replica (which may run with
        // snapshot=false so its dist carries the real version) sets this to
        // suppress the source-repo rollback/hold policy: it builds nothing
        // upstream and must surface a stage failure plainly.
        && !opts.no_failure_policy
}

/// One-way-door evidence for the current run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct BurnEvidence {
    /// Submitter publishers whose publish action landed.
    pub names: Vec<String>,
    /// Where each burned name came from (summary file + the release it
    /// records, or the live report), so a degrade caused by evidence from
    /// the wrong release is diagnosable from the operator message.
    pub sources: Vec<String>,
}

impl BurnEvidence {
    pub(super) fn burned(&self) -> bool {
        !self.names.is_empty()
    }
}

/// Gather burn evidence from the run summaries under the dist tree plus
/// the live in-memory publish report.
///
/// The disk pass is what makes per-crate workspace mode safe: each
/// crate's publish run persists its own `dist/<crate>/run-*/summary.json`,
/// so a crate that burned the version before a later crate failed is
/// still seen even though the live report only covers the failing run.
///
/// Disk-summary filtering is FAIL-CLOSED: a summary is kept unless it
/// provably belongs to a DIFFERENT release — its tag is in the same tag
/// family as this run's tag (equal family prefix, e.g. both `v…` or both
/// `crd-v…`) AND stamps a different base version. That is the
/// `--publish-only` preserved-dist case: a prior attempt of the same
/// family sitting beside this run's summary, whose burn belongs to that
/// other release and must not degrade this rollback. Everything else is
/// kept: sibling per-crate summaries (different family prefixes carry
/// different versions in one release train), tags whose family or
/// version cannot be established, and same-base-version tags whose
/// prerelease/build suffix differs from this run's (a representation
/// mismatch must never weaken the guard). Kept-but-unverifiable
/// summaries name their file in `sources` so a wrong-release degrade is
/// diagnosable from the operator message.
pub(super) fn gather_burn_evidence(ctx: &Context, log: &StageLogger) -> BurnEvidence {
    let current_tag = ctx
        .template_vars()
        .get("Tag")
        .cloned()
        .filter(|t| !t.is_empty());
    let current_family = current_tag
        .as_deref()
        .and_then(anodizer_core::git::split_tag_family);
    let mut names: Vec<String> = Vec::new();
    let mut sources: Vec<String> = Vec::new();
    for path in collect_run_summary_paths(&ctx.config.dist) {
        let summary = match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str::<RunSummary>(&text)?))
        {
            Ok(summary) => summary,
            Err(e) => {
                log.warn(&format!(
                    "ignoring unreadable run summary {} for failure-policy evaluation: {e:#}",
                    path.display()
                ));
                continue;
            }
        };
        let burned = summary.burned_submitter_names();
        if burned.is_empty() {
            continue;
        }
        match (
            anodizer_core::git::split_tag_family(&summary.tag),
            &current_family,
        ) {
            // Provably a different release: same tag family as this run's
            // tag, different base version. Prerelease/build suffixes are
            // deliberately NOT compared — a suffix-only mismatch may be a
            // representation difference within this release, and guessing
            // wrong here flips the guard fail-open.
            (Some((recorded_prefix, recorded_sv)), Some((current_prefix, current_sv)))
                if recorded_prefix == *current_prefix
                    && (recorded_sv.major, recorded_sv.minor, recorded_sv.patch)
                        != (current_sv.major, current_sv.minor, current_sv.patch) =>
            {
                log.verbose(&format!(
                    "ignoring run summary {} for failure-policy evaluation — it records \
                     tag {} ({recorded_prefix}-family version {}), a different release \
                     than the current {}",
                    path.display(),
                    summary.tag,
                    recorded_sv.version_string(),
                    current_sv.version_string()
                ));
                continue;
            }
            (Some(_), _) => sources.push(format!("{} (tag {})", path.display(), summary.tag)),
            (None, _) => sources.push(format!(
                "{} (no release version recorded{} — kept conservatively; verify it \
                 belongs to this release)",
                path.display(),
                if summary.tag.is_empty() {
                    String::new()
                } else {
                    format!("; tag field: {:?}", summary.tag)
                }
            )),
        }
        names.extend(burned);
    }
    let live = RunSummary::from_context(ctx).burned_submitter_names();
    if !live.is_empty() {
        sources.push("this run's live publish report".to_string());
        names.extend(live);
    }
    names.sort();
    names.dedup();
    BurnEvidence { names, sources }
}

/// Route a release-mode outcome through the failure policy. `Ok` passes
/// through untouched; on `Err` the policy is evaluated and executed,
/// the taken path is recorded into the run's summaries, and the
/// ORIGINAL error propagates (rollback and hold both exit nonzero —
/// the release failed either way).
pub(super) fn finish(
    ctx: &Context,
    opts: &ReleaseOpts,
    log: &StageLogger,
    result: Result<()>,
) -> Result<()> {
    let Err(err) = result else {
        return result;
    };
    if !applies(opts) {
        return Err(err);
    }
    let log = log.with_stage("failure-policy");

    // Crate-level `release.on_failure` was already rejected at config
    // load (`validate_on_failure_root_only`), so the root block is the
    // only possible source here.
    let configured = ctx
        .config
        .release
        .as_ref()
        .map(|r| r.resolved_on_failure())
        .unwrap_or_default();
    let evidence = gather_burn_evidence(ctx, &log);
    let publish_only = ctx.is_publish_only();
    let record = match decide(configured, evidence.burned(), publish_only) {
        FailureAction::Rollback => execute_rollback(opts, configured, &log),
        FailureAction::Hold { degraded } => {
            if degraded {
                log.warn(&format!(
                    "on_failure=rollback DEGRADED to hold — one-way-door publisher(s) already \
                     accepted this version: {}. Those registries never accept the same version \
                     twice, so the version is burned and rolling back the tag would only orphan \
                     the live published state. Fix forward: keep the tag, revert reversible \
                     publishers with `anodizer release --rollback-only --from-run=<id>` if \
                     needed, repair the failure, and cut the NEXT version.\n\
                     Burn evidence came from:\n  {}",
                    evidence.names.join(", "),
                    evidence.sources.join("\n  ")
                ));
            } else if publish_only {
                log.status(
                    "publish-only run failed — holding the already-released tag and \
                     version-bump commit in place (they were created by a prior release \
                     run and are permanent; reverting them is never correct here). Recover \
                     reversible publishers with `anodizer release --rollback-only \
                     --from-run=<id>`, then re-run the publish-only backfill once fixed.",
                );
            } else {
                log.status(
                    "holding tags, commits, and published state in place for forensics \
                     (on_failure=hold). Recover with `anodizer release --rollback-only \
                     --from-run=<id>` (reverts reversible publishers) and/or \
                     `anodizer tag rollback` once investigated.",
                );
            }
            FailurePolicyRecord {
                configured: configured.to_string(),
                action: "held".into(),
                degraded,
                burned_publishers: evidence.names.clone(),
                rollback_error: None,
            }
        }
    };
    record_outcome(ctx, &record, &log);
    Err(err)
}

/// Run the shared `tag rollback` path against HEAD (the tagged commit
/// every release mode runs at). Its internal published-state guard
/// stays armed (`force: false`) so cross-run evidence this process
/// cannot see — a live published GitHub release on a re-publish run —
/// still refuses destruction; a refusal or error downgrades the
/// outcome to `rollback-failed` (state held) without masking the
/// pipeline error.
fn execute_rollback(
    opts: &ReleaseOpts,
    configured: OnFailureConfig,
    log: &StageLogger,
) -> FailurePolicyRecord {
    log.status(
        "rolling back this run's release tag(s) and version bump \
         (on_failure=rollback) — no one-way-door publisher landed",
    );
    let rollback = crate::commands::tag::rollback::run(RollbackOpts {
        sha: None,
        dry_run: false,
        no_push: false,
        force: false,
        scope: Scope::All,
        mode: Mode::Revert,
        branch: None,
        verbose: opts.verbose,
        debug: opts.debug,
        quiet: opts.quiet,
    });
    match rollback {
        Ok(()) => {
            log.status("rollback complete — the version can be re-cut once the failure is fixed");
            FailurePolicyRecord {
                configured: configured.to_string(),
                action: "rolled-back".into(),
                degraded: false,
                burned_publishers: Vec::new(),
                rollback_error: None,
            }
        }
        Err(e) => {
            log.warn(&format!(
                "rollback did not complete: {e:#}. State is held; recover manually with \
                 `anodizer tag rollback` and/or `anodizer release --rollback-only \
                 --from-run=<id>` once investigated."
            ));
            FailurePolicyRecord {
                configured: configured.to_string(),
                action: "rollback-failed".into(),
                degraded: false,
                burned_publishers: Vec::new(),
                rollback_error: Some(format!("{e:#}")),
            }
        }
    }
}

/// Persist the taken path into the run's audit trail: stamp every
/// existing summary under the dist tree; when none exists yet (the
/// pipeline failed before any summary write), create one at the run's
/// canonical summary path so the artifact still states how the failure
/// was handled. Best-effort — recording must never mask the release
/// failure.
fn record_outcome(ctx: &Context, record: &FailurePolicyRecord, log: &StageLogger) {
    let mut warn = |msg: &str| log.warn(msg);
    let updated = record_failure_policy(&ctx.config.dist, record, &mut warn);
    if updated > 0 {
        return;
    }
    let Some(path) = summary_path(ctx) else {
        return;
    };
    let mut summary = RunSummary::from_context(ctx);
    summary.failure_policy = Some(record.clone());
    if let Err(e) = write_summary_json(&summary, &path) {
        log.warn(&format!(
            "could not write failure-policy summary at {}: {e:#}",
            path.display()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::Config;
    use anodizer_core::context::ContextOptions;
    use anodizer_core::log::Verbosity;
    use anodizer_core::publish_report::{
        PublishReport, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
    };

    fn release_opts_fixture() -> ReleaseOpts {
        ReleaseOpts {
            crate_names: vec![],
            all: false,
            force: false,
            snapshot: false,
            nightly: false,
            dry_run: false,
            clean: false,
            skip: vec![],
            publishers: vec![],
            token: None,
            verbose: false,
            debug: false,
            quiet: true,
            config_override: None,
            parallelism: 1,
            single_target: None,
            targets: None,
            host_targets: false,
            release_notes: None,
            release_notes_tmpl: None,
            workspace: None,
            draft: false,
            release_header: None,
            release_header_tmpl: None,
            release_footer: None,
            release_footer_tmpl: None,
            fail_fast: false,
            split: false,
            merge: false,
            publish_only: false,
            strict: false,
            prepare: false,
            announce_only: false,
            resume_release: false,
            replace_existing: false,
            preflight: false,
            no_preflight: true,
            preflight_secrets: false,
            strict_preflight: false,
            no_post_publish_poll: false,
            no_gate_submitter: false,
            rollback: None,
            simulate_failure: vec![],
            rollback_only: false,
            from_run: None,
            allow_rerun: false,
            show_skipped: false,
            allow_nondeterministic: vec![],
            summary_json: None,
            allow_ai_failure: false,
            allow_snapshot_publish: false,
            no_failure_policy: false,
        }
    }

    fn result(name: &str, group: PublisherGroup, outcome: PublisherOutcome) -> PublisherResult {
        PublisherResult {
            name: name.into(),
            group,
            required: true,
            outcome,
            evidence: None,
        }
    }

    /// The full decision table: configured policy × irreversible-landed.
    #[test]
    fn decide_covers_every_policy_and_burn_combination() {
        // publish_only=false: the normal (full-pipeline) decision table.
        let cases = [
            (OnFailureConfig::Rollback, false, FailureAction::Rollback),
            (
                OnFailureConfig::Rollback,
                true,
                FailureAction::Hold { degraded: true },
            ),
            (
                OnFailureConfig::Hold,
                false,
                FailureAction::Hold { degraded: false },
            ),
            (
                OnFailureConfig::Hold,
                true,
                FailureAction::Hold { degraded: false },
            ),
        ];
        for (configured, burned, expected) in cases {
            assert_eq!(
                decide(configured, burned, false),
                expected,
                "decide({configured:?}, irreversible_landed={burned}, publish_only=false)"
            );
        }
    }

    /// In publish-only mode the source-repo tag + bump commit are already
    /// permanent (a prior release created them), so `decide` must NEVER
    /// return `Rollback` — that path reverts the bump and deletes the
    /// released tag. It still surfaces a burned one-way door as a *degraded*
    /// hold (the operator must see what burned); the only publish-only
    /// divergence from a normal run is that a would-be clean rollback (no
    /// burn) holds instead of deleting the tag.
    #[test]
    fn decide_publish_only_never_rolls_back() {
        for configured in [OnFailureConfig::Rollback, OnFailureConfig::Hold] {
            for burned in [false, true] {
                let action = decide(configured, burned, true);
                assert_ne!(
                    action,
                    FailureAction::Rollback,
                    "publish-only decide({configured:?}, irreversible_landed={burned}) \
                     must never roll back the released tag/bump"
                );
                // Degraded only when a one-way door burned under a rollback
                // policy — identical to a normal run; publish-only changes
                // only that the non-burned rollback case holds.
                let expected_degraded = configured == OnFailureConfig::Rollback && burned;
                assert_eq!(
                    action,
                    FailureAction::Hold {
                        degraded: expected_degraded
                    },
                    "publish-only decide({configured:?}, irreversible_landed={burned}) \
                     must hold, degraded iff a one-way door burned"
                );
            }
        }
    }

    /// Failure-point coverage through the evidence layer: the decision
    /// input is "did any Submitter land", derived per failure point.
    #[test]
    fn burn_evidence_tracks_failure_point() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dist = tempfile::tempdir().expect("tempdir");

        // Pre-publish failure: no report at all — nothing landed.
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.config.dist = dist.path().to_path_buf();
        assert!(!gather_burn_evidence(&ctx, &log).burned());

        // Publish failed before any Submitter landed (reversible-only
        // outcomes + a failed Submitter): rollback stays permitted.
        let mut report = PublishReport::default();
        report.results.push(result(
            "github-release",
            PublisherGroup::Assets,
            PublisherOutcome::Succeeded,
        ));
        report.results.push(result(
            "cargo",
            PublisherGroup::Submitter,
            PublisherOutcome::Failed("boom".into()),
        ));
        report.results.push(result(
            "winget",
            PublisherGroup::Submitter,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated),
        ));
        ctx.set_publish_report(report);
        assert!(!gather_burn_evidence(&ctx, &log).burned());

        // Post-one-way-door failure: a Submitter landed before the
        // failure — the version is burned.
        let mut report = PublishReport::default();
        report.results.push(result(
            "cargo",
            PublisherGroup::Submitter,
            PublisherOutcome::Succeeded,
        ));
        report.results.push(result(
            "winget",
            PublisherGroup::Submitter,
            PublisherOutcome::Failed("validation".into()),
        ));
        ctx.set_publish_report(report);
        let evidence = gather_burn_evidence(&ctx, &log);
        assert!(evidence.burned());
        assert_eq!(evidence.names, vec!["cargo".to_string()]);
    }

    /// A landed Submitter recorded only on disk (a prior crate's run in
    /// per-crate workspace mode) must degrade the decision even though
    /// the live report knows nothing about it.
    #[test]
    fn burn_evidence_unions_per_crate_summaries_from_disk() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dist = tempfile::tempdir().expect("tempdir");
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.config.dist = dist.path().to_path_buf();

        // Prior crate's persisted summary: cargo landed for crate-a.
        let mut report = PublishReport::default();
        report.results.push(result(
            "cargo",
            PublisherGroup::Submitter,
            PublisherOutcome::Succeeded,
        ));
        let prior = RunSummary::from_context_with_report(&ctx, Some(&report));
        let path = dist
            .path()
            .join("crate-a")
            .join("run-crate-a-v1.0.0")
            .join("summary.json");
        write_summary_json(&prior, &path).expect("write prior crate summary");

        // Live report: the current crate failed reversibly.
        let mut live = PublishReport::default();
        live.results.push(result(
            "github-release",
            PublisherGroup::Assets,
            PublisherOutcome::Failed("upload".into()),
        ));
        ctx.set_publish_report(live);

        let evidence = gather_burn_evidence(&ctx, &log);
        assert!(evidence.burned(), "disk-only burn must be seen");
        assert_eq!(evidence.names, vec!["cargo".to_string()]);
        assert_eq!(
            decide(OnFailureConfig::Rollback, evidence.burned(), false),
            FailureAction::Hold { degraded: true }
        );
    }

    /// A preserved-dist (`--publish-only`) scenario: a prior attempt's
    /// summary for a DIFFERENT version sits under dist. Its burn belongs
    /// to that other release and must not degrade the current rollback.
    #[test]
    fn burn_evidence_ignores_stale_summaries_for_other_versions() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dist = tempfile::tempdir().expect("tempdir");
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.config.dist = dist.path().to_path_buf();
        ctx.template_vars_mut().set("Version", "2.0.0");
        ctx.template_vars_mut().set("Tag", "v2.0.0");

        // Stale prior-attempt summary: cargo burned v1.0.0, not v2.0.0.
        let mut report = PublishReport::default();
        report.results.push(result(
            "cargo",
            PublisherGroup::Submitter,
            PublisherOutcome::Succeeded,
        ));
        let mut stale = RunSummary::from_context_with_report(&ctx, Some(&report));
        stale.tag = "v1.0.0".to_string();
        let path = dist.path().join("run-v1.0.0").join("summary.json");
        write_summary_json(&stale, &path).expect("write stale summary");

        let evidence = gather_burn_evidence(&ctx, &log);
        assert!(
            !evidence.burned(),
            "a burn recorded for a different version must not degrade this \
             release's rollback; got {evidence:?}"
        );

        // Same summary re-stamped for the CURRENT version: now it counts.
        let mut current = RunSummary::from_context_with_report(&ctx, Some(&report));
        current.tag = "v2.0.0".to_string();
        write_summary_json(&current, &path).expect("rewrite summary");
        let evidence = gather_burn_evidence(&ctx, &log);
        assert!(evidence.burned());
        assert_eq!(evidence.names, vec!["cargo".to_string()]);
        assert_eq!(evidence.sources.len(), 1);
        assert!(
            evidence.sources[0].contains("summary.json") && evidence.sources[0].contains("v2.0.0"),
            "the source must name the file and the release it records: {:?}",
            evidence.sources
        );
    }

    /// A summary with no extractable version stamp stays evidence
    /// (conservative), but its file must be named so a wrong-version
    /// degrade is diagnosable from the operator message.
    #[test]
    fn burn_evidence_keeps_unstamped_summaries_and_names_the_source() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dist = tempfile::tempdir().expect("tempdir");
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.config.dist = dist.path().to_path_buf();
        ctx.template_vars_mut().set("Version", "2.0.0");

        let mut report = PublishReport::default();
        report.results.push(result(
            "cargo",
            PublisherGroup::Submitter,
            PublisherOutcome::Succeeded,
        ));
        // Tag empty: written by a run that never resolved a tag.
        let unstamped = RunSummary::from_context_with_report(&ctx, Some(&report));
        assert_eq!(unstamped.tag, "", "fixture precondition: no tag stamp");
        let path = dist.path().join("run-unknown").join("summary.json");
        write_summary_json(&unstamped, &path).expect("write unstamped summary");

        let evidence = gather_burn_evidence(&ctx, &log);
        assert!(
            evidence.burned(),
            "unverifiable evidence must still refuse the destructive path"
        );
        assert_eq!(evidence.names, vec!["cargo".to_string()]);
        assert_eq!(evidence.sources.len(), 1);
        assert!(
            evidence.sources[0].contains("summary.json")
                && evidence.sources[0].contains("kept conservatively"),
            "the source must flag the missing version stamp: {:?}",
            evidence.sources
        );
    }

    /// Per-crate sibling evidence must be KEPT: in one release train the
    /// sibling crates carry different tag families AND different versions
    /// (`a-v1.0.0` beside `b-v2.5.0`), so a version-only filter would
    /// silently drop the exact cross-crate evidence the disk pass exists
    /// to union in.
    #[test]
    fn burn_evidence_keeps_per_crate_sibling_summaries() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dist = tempfile::tempdir().expect("tempdir");
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.config.dist = dist.path().to_path_buf();
        // Current run: crate b at 2.5.0.
        ctx.template_vars_mut().set("Version", "2.5.0");
        ctx.template_vars_mut().set("Tag", "b-v2.5.0");

        // Sibling crate a burned its own (different) version earlier in
        // the same release train.
        let mut report = PublishReport::default();
        report.results.push(result(
            "cargo",
            PublisherGroup::Submitter,
            PublisherOutcome::Succeeded,
        ));
        let mut sibling = RunSummary::from_context_with_report(&ctx, Some(&report));
        sibling.tag = "a-v1.0.0".to_string();
        let path = dist
            .path()
            .join("a")
            .join("run-a-v1.0.0")
            .join("summary.json");
        write_summary_json(&sibling, &path).expect("write sibling summary");

        let evidence = gather_burn_evidence(&ctx, &log);
        assert!(
            evidence.burned(),
            "a sibling crate's burn (different tag family) must be kept; got {evidence:?}"
        );
        assert_eq!(evidence.names, vec!["cargo".to_string()]);
    }

    /// A same-family tag whose base version matches but whose
    /// prerelease/build suffix differs from the current tag's is KEPT: a
    /// suffix-only representation mismatch cannot prove a different
    /// release, and guessing wrong flips the guard fail-open.
    #[test]
    fn burn_evidence_keeps_same_base_version_with_suffix_mismatch() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dist = tempfile::tempdir().expect("tempdir");
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.config.dist = dist.path().to_path_buf();
        ctx.template_vars_mut().set("Version", "2.0.0");
        ctx.template_vars_mut().set("Tag", "v2.0.0");

        let mut report = PublishReport::default();
        report.results.push(result(
            "cargo",
            PublisherGroup::Submitter,
            PublisherOutcome::Succeeded,
        ));
        let mut summary = RunSummary::from_context_with_report(&ctx, Some(&report));
        summary.tag = "v2.0.0-rc.1".to_string();
        let path = dist.path().join("run-v2.0.0-rc.1").join("summary.json");
        write_summary_json(&summary, &path).expect("write prerelease summary");

        let evidence = gather_burn_evidence(&ctx, &log);
        assert!(
            evidence.burned(),
            "same-family same-base-version evidence must be kept despite a \
             prerelease suffix mismatch; got {evidence:?}"
        );
        assert_eq!(evidence.names, vec!["cargo".to_string()]);
    }

    /// When the current run's tag family cannot be established (no Tag
    /// template var), NOTHING is discarded — with no family to compare
    /// against, no summary can be proven to belong to a different release.
    #[test]
    fn burn_evidence_keeps_everything_without_a_current_tag() {
        let log = StageLogger::new("test", Verbosity::Quiet);
        let dist = tempfile::tempdir().expect("tempdir");
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.config.dist = dist.path().to_path_buf();
        ctx.template_vars_mut().set("Version", "2.0.0");

        let mut report = PublishReport::default();
        report.results.push(result(
            "cargo",
            PublisherGroup::Submitter,
            PublisherOutcome::Succeeded,
        ));
        let mut summary = RunSummary::from_context_with_report(&ctx, Some(&report));
        summary.tag = "v1.0.0".to_string();
        let path = dist.path().join("run-v1.0.0").join("summary.json");
        write_summary_json(&summary, &path).expect("write summary");

        let evidence = gather_burn_evidence(&ctx, &log);
        assert!(
            evidence.burned(),
            "with no current tag family the guard must keep everything; got {evidence:?}"
        );
    }

    /// Mode gating: only the modes that reach upstream publishers are
    /// governed by the policy.
    #[test]
    fn applies_excludes_non_publishing_modes() {
        assert!(applies(&release_opts_fixture()), "full release applies");
        assert!(
            applies(&ReleaseOpts {
                publish_only: true,
                ..release_opts_fixture()
            }),
            "publish-only applies"
        );
        assert!(
            applies(&ReleaseOpts {
                merge: true,
                ..release_opts_fixture()
            }),
            "merge applies"
        );
        for (label, opts) in [
            (
                "dry-run",
                ReleaseOpts {
                    dry_run: true,
                    ..release_opts_fixture()
                },
            ),
            (
                "snapshot",
                ReleaseOpts {
                    snapshot: true,
                    ..release_opts_fixture()
                },
            ),
            (
                "preflight",
                ReleaseOpts {
                    preflight: true,
                    ..release_opts_fixture()
                },
            ),
            (
                "prepare",
                ReleaseOpts {
                    prepare: true,
                    ..release_opts_fixture()
                },
            ),
            (
                "split",
                ReleaseOpts {
                    split: true,
                    ..release_opts_fixture()
                },
            ),
            (
                "announce-only",
                ReleaseOpts {
                    announce_only: true,
                    ..release_opts_fixture()
                },
            ),
            (
                "rollback-only",
                ReleaseOpts {
                    rollback_only: true,
                    ..release_opts_fixture()
                },
            ),
            (
                // The determinism harness's hermetic replica sets this even
                // with snapshot=false (CI real-version mode), so it must
                // suppress the policy independent of the snapshot flag.
                "no-failure-policy",
                ReleaseOpts {
                    no_failure_policy: true,
                    ..release_opts_fixture()
                },
            ),
        ] {
            assert!(!applies(&opts), "{label} must not trigger the policy");
        }
    }

    /// Root-level resolution holds in all three config modes: the
    /// policy reads only the top-level `release:` block, so a
    /// single-crate config, a lockstep workspace, and a per-crate
    /// workspace resolve identically.
    #[test]
    fn on_failure_resolves_from_root_release_in_every_config_mode() {
        let single: Config = serde_yaml_ng::from_str(
            r#"
project_name: app
release:
  on_failure: hold
crates:
  - name: app
    path: "."
"#,
        )
        .expect("single-crate config parses");
        let lockstep: Config = serde_yaml_ng::from_str(
            r#"
project_name: ws
release:
  on_failure: hold
workspaces:
  - name: ws
    crates:
      - name: a
        path: crates/a
        tag_template: "v{{ Version }}"
      - name: b
        path: crates/b
        tag_template: "v{{ Version }}"
"#,
        )
        .expect("lockstep workspace config parses");
        let per_crate: Config = serde_yaml_ng::from_str(
            r#"
project_name: ws
release:
  on_failure: hold
workspaces:
  - name: ws
    crates:
      - name: a
        path: crates/a
        tag_template: "a-v{{ Version }}"
      - name: b
        path: crates/b
        tag_template: "b-v{{ Version }}"
"#,
        )
        .expect("per-crate workspace config parses");

        for (label, config) in [
            ("single-crate", single),
            ("lockstep", lockstep),
            ("per-crate", per_crate),
        ] {
            let resolved = config
                .release
                .as_ref()
                .map(|r| r.resolved_on_failure())
                .unwrap_or_default();
            assert_eq!(
                resolved,
                OnFailureConfig::Hold,
                "{label}: root release.on_failure must govern"
            );
        }

        let unset: Config = serde_yaml_ng::from_str("project_name: app\n").expect("minimal config");
        assert_eq!(
            unset
                .release
                .as_ref()
                .map(|r| r.resolved_on_failure())
                .unwrap_or_default(),
            OnFailureConfig::Rollback,
            "unset policy defaults to rollback"
        );
    }
}
