use std::ops::ControlFlow;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::CrateConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{RetryPolicy, retry_sync};
use anodizer_core::stage::Stage;
use anodizer_core::{
    PublishEvidence, PublishReport, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
};

use crate::command::{
    is_retriable_snap_push, resolve_effective_channels, snapcraft_upload_command,
};
use crate::targets::{SnapcraftTarget, collect_snapcraft_targets};

// ---------------------------------------------------------------------------
// SnapcraftPublishStage — uploads previously built .snap artifacts
// ---------------------------------------------------------------------------
//
// `SnapcraftPublishStage` is the load-bearing snapcraft runner. Following
// the Task 15 (commit 026c854) BlobStage pattern, the stage writes its own
// `PublisherResult` directly into `ctx.publish_report` so the Submitter gate
// (and any downstream consumers, e.g. announce-gating, `--rollback-only
// --from-run`) observes outcomes uniformly. A parallel trait-based
// `SnapcraftPublisher` registration would fire `snapcraft upload` a second
// time per release — see
// `.claude/audits/2026-05-15-release-resilience-review.md` finding C3 and
// the doc comment on `stage-publish::registry::configured_publishers`.

pub struct SnapcraftPublishStage;

impl Stage for SnapcraftPublishStage {
    fn name(&self) -> &str {
        "snapcraft-publish"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("snapcraft-publish");
        if ctx.skip_in_snapshot(&log, "snapcraft-publish") {
            // Mirror BlobStage's discipline: snapshot-skip leaves
            // `publish_report` untouched. Recording a `Skipped(Snapshot)`
            // entry here would assymmetrically gate
            // `AnnounceGate::AllPublishers` against snapcraft alone if the
            // announce snapshot-skip-first guard is ever relaxed.
            return Ok(());
        }

        // Submitter-gate check: SnapcraftPublishStage is a Submitter-group
        // surface (irreversible snap-store upload — once a revision is
        // pushed there is no programmatic rollback). When the trait-based
        // dispatch in PublishStage flagged a required Assets/Manager
        // publisher failure, skip the snapcraft upload to avoid the
        // "released to one half-broken surface" failure mode.
        let gate_submitter = ctx.options.gate_submitter.unwrap_or(true);
        if gate_submitter
            && let Some(report) = ctx.publish_report()
            && (report.any_failed(PublisherGroup::Assets, true)
                || report.any_failed(PublisherGroup::Manager, true))
        {
            log.status("snapcraft-publish skipped via submitter-gate");
            record_snapcraft_result(
                ctx,
                None,
                PublisherOutcome::Skipped(SkipReason::SubmitterGated),
            );
            return Ok(());
        }

        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        // Q8.1 — wrap snapcraft upload in retry. Mirrors GR upstream
        // commit eb944f9 (`isRetriableSnapPush`): 5xx Store responses
        // (500/502/503/504) are transient, every other failure is fatal.
        let retry_policy = ctx.retry_policy();

        // Collect crates that have snapcraft config with publish: true
        let crates: Vec<CrateConfig> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.snapcrafts.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            // No work attempted (empty crates) — leave publish_report
            // untouched, matching BlobStage's discipline.
            return Ok(());
        }

        // Collect all snap artifacts that were built
        let snap_artifacts: Vec<Artifact> = ctx
            .artifacts
            .by_kind(ArtifactKind::Snap)
            .into_iter()
            .cloned()
            .collect();

        if snap_artifacts.is_empty() {
            // No work attempted (empty snap_artifacts) — leave
            // publish_report untouched, matching BlobStage's discipline.
            return Ok(());
        }

        // Pre-pass: render every config's `publish.skip` template uniformly
        // BEFORE any upload begins. A template-render failure in this pass
        // is a config error, not an upload outcome — it must fast-fail as a
        // stage error consistently, regardless of which crate iterates
        // first. Folding it into a `Failed(_)` PublisherResult would make
        // `publish_report.json` misrepresent the same bug as an upload
        // failure for some crate orderings and a stage abort for others.
        let skip_decisions = render_skip_decisions(ctx, &crates)?;

        // Capture the resolved per-target snapshot BEFORE we start
        // uploading so a mid-stream failure still leaves the operator a
        // manual channel-management pointer for each snap we attempted to
        // push. The snapshot also feeds
        // `PublishEvidence::extra.snapcraft_targets` on success so
        // `--rollback-only --from-run` consumers can decode the recorded
        // shape.
        let targets = collect_snapcraft_targets(ctx);

        let (attempted, exec_result) = run_uploads(
            ctx,
            &crates,
            &snap_artifacts,
            &skip_decisions,
            &retry_policy,
            dry_run,
            &log,
        );

        if !attempted {
            // Either every config was `publish: false`, or every snap
            // entry was disabled via `skip:`, or every run was dry-run.
            // Mirror BlobStage's "no work, no record" contract. The
            // closure cannot have failed (skip-templates pre-rendered;
            // upload errors flip `attempted=true` first), so
            // `exec_result` is `Ok(())` on this branch — but pass it
            // through for forward-compat in case a future error site is
            // introduced upstream of the first attempted upload.
            return exec_result;
        }

        let outcome = match &exec_result {
            Ok(()) => PublisherOutcome::Succeeded,
            Err(e) => PublisherOutcome::Failed(format!("{e:#}")),
        };
        let evidence = matches!(outcome, PublisherOutcome::Succeeded)
            .then(|| build_snapcraft_evidence(&targets));
        record_snapcraft_result(ctx, evidence, outcome);
        // Per-target upload errors are reported via PublisherResult; they
        // must NOT bail the pipeline because announce-gating and the
        // Submitter gate downstream depend on this stage returning Ok(()).
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Pre-pass: render every config's `publish.skip` template upfront so a
// template error fast-fails as a stage error before any upload begins.
// ---------------------------------------------------------------------------

/// Per-config skip flag, indexed parallel to the
/// `(crate_index, snap_cfg_index)` ordering of
/// `crates[].snapcrafts[]`. `run_uploads` indexes into this Vec rather
/// than re-rendering the template inside the upload loop.
fn render_skip_decisions(ctx: &Context, crates: &[CrateConfig]) -> Result<Vec<bool>> {
    let mut decisions = Vec::new();
    for krate in crates {
        let Some(snap_configs) = krate.snapcrafts.as_ref() else {
            continue;
        };
        for snap_cfg in snap_configs {
            let skip = if let Some(ref d) = snap_cfg.skip {
                d.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| {
                        format!(
                            "snapcraft: render publish.skip template for crate {}",
                            krate.name
                        )
                    })?
            } else {
                false
            };
            decisions.push(skip);
        }
    }
    Ok(decisions)
}

// ---------------------------------------------------------------------------
// Upload loop, extracted from `Stage::run` so the
// (attempted, exec_result) seam is testable in isolation.
// ---------------------------------------------------------------------------

/// Run the snapcraft upload loop and report
/// `(attempted_any_upload, Result<()>)`.
///
/// `attempted_any_upload` becomes `true` the first time we materialize a
/// non-dry-run upload (i.e. shell out to `snapcraft upload`). Callers
/// use it to decide whether to record a `PublisherResult`:
/// - `attempted = false` → nothing to report (BlobStage parity); the
///   `Result<()>` is bubbled as a stage error.
/// - `attempted = true`  → fold the `Result<()>` into a
///   `Succeeded`/`Failed(_)` outcome.
///
/// `skip_decisions` is the pre-pass `publish.skip` flag per
/// `(crate, snap_cfg)` tuple in iteration order — keeps this loop free
/// of template-render side effects.
fn run_uploads(
    ctx: &Context,
    crates: &[CrateConfig],
    snap_artifacts: &[Artifact],
    skip_decisions: &[bool],
    retry_policy: &RetryPolicy,
    dry_run: bool,
    log: &StageLogger,
) -> (bool, Result<()>) {
    let mut attempted_upload = false;
    // IIFE captures `attempted_upload` by mutable reference so the
    // `?`-early-exit on per-target failure preserves the "anything
    // attempted before this point?" answer for the caller.
    let result: Result<()> = (|| -> Result<()> {
        let mut decision_idx = 0usize;
        for krate in crates {
            let Some(snap_configs) = krate.snapcrafts.as_ref() else {
                continue;
            };

            for snap_cfg in snap_configs {
                // Pull the pre-rendered skip flag in parallel with the
                // iteration — every `(crate, snap_cfg)` tuple contributes
                // exactly one entry to `skip_decisions`.
                let cfg_skip = skip_decisions.get(decision_idx).copied().unwrap_or(false);
                decision_idx += 1;

                // Only publish configs that opt in
                if !snap_cfg.publish.unwrap_or(false) {
                    continue;
                }
                if cfg_skip {
                    continue;
                }

                // Find snap artifacts for this crate (optionally filtered by id)
                let matching: Vec<&Artifact> = snap_artifacts
                    .iter()
                    .filter(|a| a.crate_name == krate.name)
                    .filter(|a| {
                        if let Some(ref filter_id) = snap_cfg.id {
                            a.metadata
                                .get("id")
                                .map(|id| id == filter_id)
                                .unwrap_or(false)
                        } else {
                            true
                        }
                    })
                    .collect();

                for artifact in &matching {
                    let snap_path = artifact.path.to_string_lossy();

                    // GoReleaser renders each channel template through the
                    // template engine, filtering out empty results.
                    let rendered_channels: Option<Vec<String>> =
                        snap_cfg.channel_templates.as_ref().map(|templates| {
                            templates
                                .iter()
                                .filter_map(|tmpl| {
                                    ctx.render_template(tmpl).ok().filter(|s| !s.is_empty())
                                })
                                .collect()
                        });
                    // GoReleaser also renders grade through the template engine
                    let rendered_grade: Option<String> = snap_cfg
                        .grade
                        .as_deref()
                        .map(|g| ctx.render_template(g).unwrap_or_else(|_| g.to_string()));
                    let effective_channels = resolve_effective_channels(
                        rendered_channels.as_deref(),
                        rendered_grade.as_deref(),
                    );
                    let upload_args =
                        snapcraft_upload_command(&snap_path, effective_channels.as_deref());

                    if dry_run {
                        log.status(&format!("(dry-run) would run: {}", upload_args.join(" "),));
                        continue;
                    }

                    attempted_upload = true;
                    let max_attempts = retry_policy.max_attempts.max(1);
                    retry_sync(retry_policy, |attempt| {
                        if attempt > 1 {
                            log.warn(&format!(
                                "snapcraft upload attempt {}/{} failed (5xx), retrying…",
                                attempt - 1,
                                max_attempts,
                            ));
                        }
                        log.status(&format!("running: {}", upload_args.join(" ")));
                        let upload_output = match Command::new(&upload_args[0])
                            .args(&upload_args[1..])
                            .output()
                        {
                            Ok(o) => o,
                            Err(e) => {
                                // Spawning snapcraft itself failed (binary missing,
                                // permission denied) — not a transient Store error.
                                return Err(ControlFlow::Break(anyhow::Error::from(e).context(
                                    format!(
                                        "execute snapcraft upload for crate {} snap {}",
                                        krate.name, snap_path
                                    ),
                                )));
                            }
                        };

                        if upload_output.status.success() {
                            return Ok(());
                        }

                        // Review-pending responses from the Snap Store should be
                        // warnings, not fatal errors — the snap was uploaded
                        // successfully but needs human review.
                        const REVIEW_PENDING_STRINGS: &[&str] = &[
                            "Waiting for previous upload",
                            "A human will soon review your snap",
                            "(NEEDS REVIEW)",
                        ];
                        let stderr = String::from_utf8_lossy(&upload_output.stderr);
                        let stdout = String::from_utf8_lossy(&upload_output.stdout);
                        let combined = format!("{}{}", stdout, stderr);
                        if REVIEW_PENDING_STRINGS.iter().any(|s| combined.contains(s)) {
                            log.warn(&format!("snap upload pending review: {}", combined.trim()));
                            return Ok(());
                        }

                        // Materialize the failure as an anyhow::Error via
                        // `log.check_output`, which preserves stderr/stdout for
                        // operators reading the log.
                        let err = match log.check_output(upload_output, "snapcraft upload") {
                            Ok(_) => return Ok(()),
                            Err(e) => e,
                        };
                        if is_retriable_snap_push(&combined) {
                            Err(ControlFlow::Continue(err))
                        } else {
                            // Auth failures, malformed snap, quota errors, etc.
                            // fast-fail without burning retry budget.
                            Err(ControlFlow::Break(err))
                        }
                    })?;
                }
            }
        }
        Ok(())
    })();

    (attempted_upload, result)
}

// ---------------------------------------------------------------------------
// PublisherResult recording
// ---------------------------------------------------------------------------

/// Build the `PublishEvidence` recorded on a successful snapcraft run.
///
/// `primary_ref` points at the first uploaded package's snapcraft.io
/// listing; `extra.snapcraft_targets` carries the full per-target
/// snapshot used by `--rollback-only --from-run` to surface the
/// (package, channel) tuples an operator needs to address manually.
fn build_snapcraft_evidence(targets: &[SnapcraftTarget]) -> PublishEvidence {
    let mut evidence = PublishEvidence::new("snapcraft");
    if let Some(first) = targets.first() {
        evidence.primary_ref = Some(format!("https://snapcraft.io/{}", first.package_name));
    }
    evidence.extra = serde_json::json!({ "snapcraft_targets": targets });
    evidence
}

/// Append a `PublisherResult` for the snapcraft stage to
/// `ctx.publish_report`. Initializes the report when `None` (covers
/// `--publish` runs where the regular `PublishStage` was skipped).
/// Snapcraft is a Submitter-group publisher with `required = false`,
/// matching the trait-based classification before Bundle B2.
///
/// Similar role to `stage-blob::run::record_blob_result`; signature is
/// slightly different — this recorder takes a pre-computed
/// `(evidence, outcome)` pair, while the blob recorder derives both
/// from `(uploaded, &exec_result)`. Different shape is fine; the
/// contract (init `publish_report` if `None`; push one
/// `PublisherResult` with `name="snapcraft"`,
/// `group=PublisherGroup::Submitter`, `required=false`) is identical.
pub(crate) fn record_snapcraft_result(
    ctx: &mut Context,
    evidence: Option<PublishEvidence>,
    outcome: PublisherOutcome,
) {
    if ctx.publish_report.is_none() {
        ctx.publish_report = Some(PublishReport::default());
    }
    let report = ctx
        .publish_report
        .as_mut()
        .expect("publish_report initialized above");
    report.results.push(PublisherResult {
        name: "snapcraft".to_string(),
        group: PublisherGroup::Submitter,
        required: false,
        outcome,
        evidence,
    });
}

#[cfg(test)]
mod publish_stage_tests {
    use super::*;
    use crate::targets::decode_snapcraft_targets;
    use anodizer_core::config::{CrateConfig, SnapcraftConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    fn snap_crate(name: &str, package_name: Option<&str>, channel: Option<&str>) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            snapcrafts: Some(vec![SnapcraftConfig {
                name: package_name.map(|s| s.to_string()),
                publish: Some(true),
                channel_templates: channel.map(|c| vec![c.to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    // ---------------------------------------------------------------
    // build_snapcraft_evidence — pin the success-path wire shape
    // ---------------------------------------------------------------

    #[test]
    fn build_snapcraft_evidence_pins_success_wire_shape() {
        // Success-path evidence is what `--rollback-only --from-run`
        // and any replay consumer reads back. Pin the three load-bearing
        // fields: publisher name, primary_ref pointing at the first
        // package's snapcraft.io listing, and the full per-target
        // snapshot in extra.snapcraft_targets.
        let targets = vec![
            SnapcraftTarget {
                crate_name: "demo".into(),
                package_name: "demo-snap".into(),
                channel: Some("stable".into()),
                revision: None,
            },
            SnapcraftTarget {
                crate_name: "widget".into(),
                package_name: "widget".into(),
                channel: None,
                revision: None,
            },
        ];
        let evidence = build_snapcraft_evidence(&targets);
        assert_eq!(evidence.publisher, "snapcraft");
        assert_eq!(
            evidence.primary_ref.as_deref(),
            Some("https://snapcraft.io/demo-snap")
        );
        let decoded = decode_snapcraft_targets(&evidence.extra);
        assert_eq!(decoded, targets);
    }

    #[test]
    fn build_snapcraft_evidence_handles_empty_targets() {
        // Edge case: success path with no resolved targets — should
        // still produce a well-formed evidence stub with no
        // primary_ref but an empty snapcraft_targets array.
        let evidence = build_snapcraft_evidence(&[]);
        assert_eq!(evidence.publisher, "snapcraft");
        assert!(evidence.primary_ref.is_none());
        assert_eq!(decode_snapcraft_targets(&evidence.extra), Vec::new());
    }

    // ---------------------------------------------------------------
    // PublisherResult recording behavior
    // ---------------------------------------------------------------

    #[test]
    fn snapshot_mode_records_nothing() {
        // BlobStage parity: snapshot-skip leaves publish_report
        // untouched. Recording a `Skipped(Snapshot)` entry would
        // assymmetrically gate `AnnounceGate::AllPublishers` against
        // snapcraft alone if the announce snapshot-skip-first guard
        // is ever relaxed.
        let mut ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, Some("stable"))])
            .snapshot(true)
            .build();
        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("snapshot path returns Ok");

        let recorded_snap = ctx
            .publish_report()
            .map(|r| r.results.iter().any(|r| r.name == "snapcraft"))
            .unwrap_or(false);
        assert!(
            !recorded_snap,
            "snapshot mode must NOT record a snapcraft PublisherResult"
        );
    }

    #[test]
    fn submitter_gate_records_skipped_gated() {
        // Pre-seed the publish report with a required Assets failure so
        // the Submitter-gate path fires. Assert the stage records
        // `Skipped(SubmitterGated)` (the gate is observable in the
        // report, not just silent).
        let mut ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, Some("stable"))])
            .build();
        // Seed a required Assets failure to trip the gate.
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "blob".to_string(),
            group: PublisherGroup::Assets,
            required: true,
            outcome: PublisherOutcome::Failed("simulated upload failure".to_string()),
            evidence: None,
        });
        ctx.publish_report = Some(report);

        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("gate path returns Ok");

        let snap_results: Vec<&PublisherResult> = ctx
            .publish_report()
            .expect("report initialized")
            .results
            .iter()
            .filter(|r| r.name == "snapcraft")
            .collect();
        assert_eq!(snap_results.len(), 1);
        let r = snap_results[0];
        assert_eq!(r.group, PublisherGroup::Submitter);
        assert_eq!(
            r.outcome,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated)
        );
        assert!(r.evidence.is_none(), "gated skip records no evidence");
    }

    #[test]
    fn no_configured_crates_records_nothing() {
        // BlobStage parity: when there is no work to attempt, do NOT
        // append a PublisherResult — the slot stays clean so downstream
        // consumers can distinguish "configured-and-skipped" from
        // "never asked to run".
        let mut ctx = TestContextBuilder::new().build();
        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("no-crates path returns Ok");
        assert!(
            ctx.publish_report().is_none()
                || !ctx
                    .publish_report()
                    .unwrap()
                    .results
                    .iter()
                    .any(|r| r.name == "snapcraft"),
            "no snapcraft entry should be recorded when no crates are configured"
        );
    }

    #[test]
    fn dry_run_with_publishable_config_records_nothing() {
        // Mirrors BlobStage's dry-run contract: we log what WOULD run,
        // but no PublisherResult lands because no upload was attempted.
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::context::ContextOptions;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let crate_cfg = snap_crate("demo", Some("demo"), Some("edge"));
        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![crate_cfg],
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = SnapcraftPublishStage;
        stage.run(&mut ctx).expect("dry-run returns Ok");
        let recorded_snap = ctx
            .publish_report()
            .map(|r| r.results.iter().any(|r| r.name == "snapcraft"))
            .unwrap_or(false);
        assert!(
            !recorded_snap,
            "dry-run path must NOT record a snapcraft PublisherResult"
        );
    }

    // ---------------------------------------------------------------
    // record_snapcraft_result direct seam — Failed(_) entry coverage
    // ---------------------------------------------------------------

    #[test]
    fn record_snapcraft_result_initializes_report_if_missing() {
        // `--publish` subset runs may invoke `SnapcraftPublishStage`
        // before `PublishStage` has populated `ctx.publish_report`.
        // The recorder must initialize the report on first push.
        let mut ctx = TestContextBuilder::new().build();
        assert!(ctx.publish_report.is_none());
        record_snapcraft_result(
            &mut ctx,
            None,
            PublisherOutcome::Failed("simulated upload failure".into()),
        );
        let report = ctx.publish_report.as_ref().expect("report initialized");
        assert_eq!(report.results.len(), 1);
        let r = &report.results[0];
        assert_eq!(r.name, "snapcraft");
        assert_eq!(r.group, PublisherGroup::Submitter);
        assert!(!r.required);
        assert_eq!(
            r.outcome,
            PublisherOutcome::Failed("simulated upload failure".into())
        );
        assert!(r.evidence.is_none());
    }

    #[test]
    fn record_snapcraft_result_failed_entry_announce_gate_visibility() {
        // Load-bearing invariant: a failed snap upload lands as a
        // `Failed(_)` entry, NOT a stage-error bail. This is the
        // property the announce gate (`AnnounceGate::AllPublishers`)
        // and `--rollback-only --from-run` consumers depend on —
        // without this entry, neither downstream surface knows the
        // snap upload tried and failed.
        let mut ctx = TestContextBuilder::new().build();
        // Pre-seed something innocuous so we also verify we APPEND
        // (don't clobber) any existing results.
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "github-release".to_string(),
            group: PublisherGroup::Assets,
            required: false,
            outcome: PublisherOutcome::Succeeded,
            evidence: None,
        });
        ctx.publish_report = Some(report);

        record_snapcraft_result(
            &mut ctx,
            None,
            PublisherOutcome::Failed("snapcraft: 401 unauthorized".into()),
        );

        let report = ctx.publish_report.as_ref().expect("report present");
        assert_eq!(report.results.len(), 2, "appended, did not clobber");
        let snap = report
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry present");
        assert_eq!(snap.group, PublisherGroup::Submitter);
        assert!(!snap.required);
        match &snap.outcome {
            PublisherOutcome::Failed(msg) => {
                assert!(msg.contains("401"), "preserves the underlying error: {msg}")
            }
            other => panic!("expected Failed(_), got {other:?}"),
        }
    }

    #[test]
    fn record_snapcraft_result_succeeded_carries_evidence() {
        // Symmetric coverage: Succeeded path attaches evidence.
        let mut ctx = TestContextBuilder::new().build();
        let evidence = build_snapcraft_evidence(&[SnapcraftTarget {
            crate_name: "demo".into(),
            package_name: "demo".into(),
            channel: Some("stable".into()),
            revision: None,
        }]);
        record_snapcraft_result(
            &mut ctx,
            Some(evidence.clone()),
            PublisherOutcome::Succeeded,
        );
        let report = ctx.publish_report.as_ref().expect("report initialized");
        let snap = report
            .results
            .iter()
            .find(|r| r.name == "snapcraft")
            .expect("snapcraft entry present");
        assert_eq!(snap.outcome, PublisherOutcome::Succeeded);
        assert_eq!(snap.evidence.as_ref(), Some(&evidence));
    }

    // ---------------------------------------------------------------
    // Pre-pass: skip-template render uniformity
    // ---------------------------------------------------------------

    #[test]
    fn skip_template_error_fast_fails_stage_without_recording() {
        // Important #3 invariant: a malformed `publish.skip` template
        // surfaces as a STAGE ERROR (Err(_) from Stage::run), NOT as
        // a `Failed(_)` PublisherResult — and `publish_report` stays
        // untouched. Two operationally-identical config bugs must not
        // produce different pipeline behaviors depending on which
        // crate iterates first.
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::StringOrBool;
        use std::collections::HashMap;
        use std::path::PathBuf;

        // `{{ undefined_var }}` references a template variable that
        // is never set, so Tera errors at render time.
        let snap_cfg = SnapcraftConfig {
            name: Some("demo".to_string()),
            publish: Some(true),
            skip: Some(StringOrBool::String(
                "{{ undefined_var_that_will_not_render }}".to_string(),
            )),
            ..Default::default()
        };
        let crate_cfg = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            snapcrafts: Some(vec![snap_cfg]),
            ..Default::default()
        };
        let config = anodizer_core::config::Config {
            project_name: "demo".to_string(),
            crates: vec![crate_cfg],
            ..Default::default()
        };
        let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        // Need a snap artifact so the stage reaches the pre-pass
        // (early-return on empty snap_artifacts would mask the
        // template error otherwise).
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "demo".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = SnapcraftPublishStage;
        let err = stage
            .run(&mut ctx)
            .expect_err("template-error must surface as stage error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("render publish.skip template"),
            "error preserves the rendering context: {msg}"
        );
        let recorded_snap = ctx
            .publish_report()
            .map(|r| r.results.iter().any(|r| r.name == "snapcraft"))
            .unwrap_or(false);
        assert!(
            !recorded_snap,
            "publish_report must be untouched on stage-error fast-fail"
        );
    }

    #[test]
    fn run_uploads_no_configured_publishers_returns_not_attempted() {
        // White-box test of the (attempted, exec_result) seam:
        // every snap_cfg is `publish: false`, so the loop runs but
        // never flips `attempted_upload`. exec_result is Ok(()).
        let krate = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            snapcrafts: Some(vec![SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(false),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new()
            .crates(vec![krate.clone()])
            .build();
        let log = ctx.logger("snapcraft-publish");
        let crates = vec![krate];
        let skip_decisions = vec![false];
        let retry_policy = ctx.retry_policy();
        let (attempted, result) = run_uploads(
            &ctx,
            &crates,
            &[],
            &skip_decisions,
            &retry_policy,
            false,
            &log,
        );
        assert!(!attempted, "publish:false → no attempted upload");
        assert!(result.is_ok(), "no work done → Ok(())");
    }
}
