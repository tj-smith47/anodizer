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
/// publisher landed.
pub(super) fn decide(configured: OnFailureConfig, irreversible_landed: bool) -> FailureAction {
    match (configured, irreversible_landed) {
        (OnFailureConfig::Hold, _) => FailureAction::Hold { degraded: false },
        (OnFailureConfig::Rollback, true) => FailureAction::Hold { degraded: true },
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
}

/// One-way-door evidence for the current run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct BurnEvidence {
    /// Submitter publishers whose publish action landed.
    pub names: Vec<String>,
}

impl BurnEvidence {
    pub(super) fn burned(&self) -> bool {
        !self.names.is_empty()
    }
}

/// Gather burn evidence from every run summary under the dist tree plus
/// the live in-memory publish report.
///
/// The disk pass is what makes per-crate workspace mode safe: each
/// crate's publish run persists its own `dist/<crate>/run-*/summary.json`,
/// so a crate that burned the version before a later crate failed is
/// still seen even though the live report only covers the failing run.
/// Conservative on purpose — any landed Submitter anywhere under this
/// run's dist degrades the rollback.
pub(super) fn gather_burn_evidence(ctx: &Context, log: &StageLogger) -> BurnEvidence {
    let mut names: Vec<String> = Vec::new();
    for path in collect_run_summary_paths(&ctx.config.dist) {
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(serde_json::from_str::<RunSummary>(&text)?))
        {
            Ok(summary) => names.extend(summary.burned_submitter_names()),
            Err(e) => log.warn(&format!(
                "ignoring unreadable run summary {} for failure-policy evaluation: {e:#}",
                path.display()
            )),
        }
    }
    names.extend(RunSummary::from_context(ctx).burned_submitter_names());
    names.sort();
    names.dedup();
    BurnEvidence { names }
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
    let record = match decide(configured, evidence.burned()) {
        FailureAction::Rollback => execute_rollback(opts, configured, &log),
        FailureAction::Hold { degraded } => {
            if degraded {
                log.warn(&format!(
                    "on_failure=rollback DEGRADED to hold — one-way-door publisher(s) already \
                     accepted this version: {}. Those registries never accept the same version \
                     twice, so the version is burned and rolling back the tag would only orphan \
                     the live published state. Fix forward: keep the tag, revert reversible \
                     publishers with `anodizer release --rollback-only --from-run=<id>` if \
                     needed, repair the failure, and cut the NEXT version.",
                    evidence.names.join(", ")
                ));
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
            strict_preflight: false,
            no_post_publish_poll: false,
            no_gate_submitter: false,
            rollback: None,
            simulate_failure: vec![],
            rollback_only: false,
            from_run: None,
            allow_rerun: false,
            allow_nondeterministic: vec![],
            summary_json: None,
            allow_ai_failure: false,
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
                decide(configured, burned),
                expected,
                "decide({configured:?}, irreversible_landed={burned})"
            );
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
            decide(OnFailureConfig::Rollback, evidence.burned()),
            FailureAction::Hold { degraded: true }
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
