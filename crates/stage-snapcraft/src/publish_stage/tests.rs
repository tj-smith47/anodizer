use super::*;
use crate::targets::{SnapcraftTarget, decode_snapcraft_targets};
use anodizer_core::config::{CrateConfig, SnapcraftConfig};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PublishReport, PublisherGroup, PublisherResult};

fn snap_crate(name: &str, package_name: Option<&str>, channel: Option<&str>) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        snapcrafts: Some(vec![SnapcraftConfig {
            name: package_name.map(|s| s.to_string()),
            publish: Some(true),
            channel_templates: channel.map(|c| vec![c.to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    }
}

/// A Linux Snap artifact for `crate_name`, matching what `stage-build`
/// registers before this stage runs. The `crates.is_empty()` /
/// `snap_artifacts.is_empty()` early-return checks in `Stage::run`
/// require BOTH a `publish: true` snapcraft config AND a matching
/// artifact before any of the gate/upload machinery below them
/// executes — a test that only sets up the crate config, without this,
/// exercises the "no work, no record" early-return path instead.
fn snap_artifact(crate_name: &str) -> anodizer_core::artifact::Artifact {
    anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::Snap,
        name: String::new(),
        path: std::path::PathBuf::from(format!("/tmp/dist/{crate_name}.snap")),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: crate_name.to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    }
}

// ---------------------------------------------------------------
// Idempotent retry floor — the snapcraft upload is an opaque subprocess
// (`run_capture_timeout`) with no in-process retry-mock seam, so the
// strongest feasible proof is that the effective upload policy equals
// `max(global, IDEMPOTENT_PUT_ATTEMPTS)`. This is the same expression the
// production upload site applies (`retry_policy.with_idempotent_floor()`),
// so reverting the floor fails this test.
// ---------------------------------------------------------------

#[test]
fn upload_policy_applies_idempotent_floor() {
    use anodizer_core::retry::{IDEMPOTENT_PUT_ATTEMPTS, RetryPolicy};
    use std::time::Duration;

    // A `--publish-only`-shaped policy (attempts: 1) must be raised to the
    // shared idempotent floor so a transient 5xx still retries.
    let capped = RetryPolicy {
        max_attempts: 1,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
    };
    assert_eq!(
        capped.with_idempotent_floor().max_attempts,
        IDEMPOTENT_PUT_ATTEMPTS,
        "a single-attempt snapcraft policy must be floored to the idempotent minimum"
    );

    // An operator-set cap above the floor must be preserved, never lowered.
    let generous = RetryPolicy {
        max_attempts: 9,
        ..capped
    };
    assert_eq!(
        generous.with_idempotent_floor().max_attempts,
        9,
        "an operator cap above the floor must be preserved"
    );
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
            ..Default::default()
        },
        SnapcraftTarget {
            crate_name: "widget".into(),
            package_name: "widget".into(),
            channel: None,
            revision: None,
            ..Default::default()
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
    // asymmetrically gate `AnnounceGate::AllPublishers` against
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
fn submitter_gate_fires_on_required_cargo_submitter_failure() {
    // v0.8.0 intra-Submitter fix: snapcraft runs as its own Submitter
    // stage AFTER the trait dispatch. A required cargo (Submitter)
    // failure recorded by that dispatch must close the gate here too —
    // before the fix, snapcraft only consulted Assets/Manager and would
    // have pushed against a half-published crates.io release.
    let mut ctx = TestContextBuilder::new()
        .crates(vec![snap_crate("demo", None, Some("stable"))])
        .build();
    let mut report = PublishReport::default();
    report.results.push(PublisherResult {
        name: "cargo".to_string(),
        group: PublisherGroup::Submitter,
        required: true,
        outcome: PublisherOutcome::Failed("crate-b failed after crate-a published".to_string()),
        evidence: None,
    });
    ctx.publish_report = Some(report);

    let stage = SnapcraftPublishStage;
    stage.run(&mut ctx).expect("gate path returns Ok");

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry present");
    assert_eq!(
        snap.outcome,
        PublisherOutcome::Skipped(SkipReason::SubmitterGated),
        "a required cargo (Submitter) failure must gate snapcraft"
    );
}

#[test]
fn submitter_gate_stays_open_on_optional_upstream_failure() {
    // Continue-on-error preserved: an OPTIONAL upstream failure must NOT
    // gate snapcraft. The stage must not record Skipped(SubmitterGated),
    // and — positively — the gate predicate it consults must report open.
    let mut ctx = TestContextBuilder::new()
        .crates(vec![snap_crate("demo", None, Some("stable"))])
        .build();
    let mut report = PublishReport::default();
    report.results.push(PublisherResult {
        name: "blob".to_string(),
        group: PublisherGroup::Assets,
        required: false,
        outcome: PublisherOutcome::Failed("optional blob boom".to_string()),
        evidence: None,
    });
    ctx.publish_report = Some(report);

    let stage = SnapcraftPublishStage;
    stage.run(&mut ctx).expect("ungated path returns Ok");

    let gated = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .any(|r| {
            r.name == "snapcraft"
                && matches!(
                    r.outcome,
                    PublisherOutcome::Skipped(SkipReason::SubmitterGated)
                )
        });
    assert!(
        !gated,
        "an optional upstream failure must not gate snapcraft (continue-on-error)"
    );
    // Positive proof the gate is open, not merely that no gated row was
    // recorded (which the no-work path would also satisfy).
    assert!(
        !ctx.publish_report()
            .expect("report present")
            .submitter_gate_closed(),
        "an optional upstream failure must leave the submitter gate open"
    );
}

// ---------------------------------------------------------------
// Pre-submitter verify-release gate — the post-publish content check
// the in-dispatch Submitter loop also consults, run here too because
// snapcraft executes as its own pipeline stage outside that loop. A
// release configuring ONLY `snapcraft:` never puts a single publisher
// through the in-dispatch loop, so its lazy eval never fires without
// this stage running the check itself.
// ---------------------------------------------------------------

#[test]
#[serial_test::serial(path_env)]
fn verify_gate_records_skipped_verify_gate_blocked() {
    // No prior publish_report at all — the snapcraft-only-release
    // shape: no trait-dispatched Submitter publisher ever ran, so
    // this stage is the first and only place the gate can fire.
    let mut ctx = TestContextBuilder::new()
        .crates(vec![snap_crate("demo", None, Some("stable"))])
        .build();
    ctx.artifacts.add(snap_artifact("demo"));
    assert!(ctx.publish_report.is_none());
    ctx.verify_gate = Some(std::sync::Arc::new(|_ctx| Ok(false)));

    let stage = SnapcraftPublishStage;
    stage.run(&mut ctx).expect("gate path returns Ok");

    let report = ctx
        .publish_report()
        .expect("gate check initializes the report");
    assert!(report.verify_gate_evaluated);
    assert!(report.verify_gate_blocked);
    let snap = report
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry present");
    assert_eq!(
        snap.outcome,
        PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked)
    );
    assert!(snap.evidence.is_none(), "gated skip records no evidence");
}

#[test]
#[serial_test::serial(path_env)]
fn verify_gate_stays_open_when_gate_passes() {
    // A crate + matching artifact must be configured — the gate only
    // runs once there is real snapcraft work to do (the "no work, no
    // record" contract skips the gate entirely otherwise). The gate
    // passing means the stage proceeds into run_uploads and actually
    // spawns `snapcraft`, so it needs the same hermetic stub every other
    // upload-reaching test in this file uses.
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;

    let tools = FakeToolDir::new();
    tools.tool("snapcraft").exit(0).install();
    let _path = tools.activate();

    let mut ctx = TestContextBuilder::new()
        .crates(vec![snap_crate("demo", None, Some("stable"))])
        .build();
    ctx.artifacts.add(snap_artifact("demo"));
    ctx.verify_gate = Some(std::sync::Arc::new(|_ctx| Ok(true)));

    let stage = SnapcraftPublishStage;
    stage.run(&mut ctx).expect("ungated path returns Ok");

    let report = ctx
        .publish_report()
        .expect("gate check initializes the report");
    assert!(report.verify_gate_evaluated);
    assert!(!report.verify_gate_blocked);
}

#[test]
#[serial_test::serial(path_env)]
fn no_work_does_not_evaluate_the_verify_gate() {
    // Without a configured crate or a matching artifact, the stage
    // must take the "no work, no record" early-return before ever
    // reaching the verify-release gate — no live GH asset-verification
    // fetch, no phantom `Skipped(VerifyGateBlocked)` entry.
    let invoked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let invoked_clone = invoked.clone();
    let mut ctx = TestContextBuilder::new().build();
    ctx.verify_gate = Some(std::sync::Arc::new(move |_ctx| {
        invoked_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(false)
    }));

    let stage = SnapcraftPublishStage;
    stage.run(&mut ctx).expect("no-work path returns Ok");

    assert_eq!(
        invoked.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a release with no snapcraft work must never invoke the verify gate"
    );
    assert!(
        ctx.publish_report.is_none(),
        "no work attempted — publish_report must stay untouched"
    );
}

#[test]
#[serial_test::serial(path_env)]
fn verify_gate_evaluated_once_when_dispatch_already_ran_it() {
    // Simulates the shared, cross-crate coordination: the in-dispatch
    // Submitter loop already ran the live gate check (e.g. this
    // release also configures `cargo:`) and persisted its verdict.
    // The stage must trust that verdict rather than invoking the gate
    // closure a second time.
    let invoked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let invoked_clone = invoked.clone();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![snap_crate("demo", None, Some("stable"))])
        .build();
    ctx.artifacts.add(snap_artifact("demo"));
    ctx.verify_gate = Some(std::sync::Arc::new(move |_ctx| {
        invoked_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(false)
    }));
    ctx.publish_report = Some(PublishReport {
        verify_gate_evaluated: true,
        verify_gate_blocked: true,
        ..Default::default()
    });

    let stage = SnapcraftPublishStage;
    stage.run(&mut ctx).expect("gate path returns Ok");

    assert_eq!(
        invoked.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "an already-evaluated gate must not be invoked again"
    );
    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry present");
    assert_eq!(
        snap.outcome,
        PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked)
    );
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
        false,
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
        false,
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
        ..Default::default()
    }]);
    record_snapcraft_result(
        &mut ctx,
        Some(evidence.clone()),
        PublisherOutcome::Succeeded,
        false,
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
// derive_snapcraft_required — Finding 1: an unset/false snapcraft
// `required:` must still let verify-release surface a failed upload,
// but must NOT abort the pipeline; an opt-in `required: true` must.
// Exercised across all three config modes.
// ---------------------------------------------------------------

#[test]
fn derive_snapcraft_required_defaults_false_single_crate() {
    let crate_cfg = CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        snapcrafts: Some(vec![SnapcraftConfig {
            publish: Some(true),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![crate_cfg],
        ..Default::default()
    };
    let ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
    assert!(
        !derive_snapcraft_required(&ctx),
        "an unset `required:` must default to false"
    );
}

#[test]
fn derive_snapcraft_required_true_lockstep_workspace() {
    // Lockstep mode: multiple crates under one top-level `crates:` list
    // sharing one workspace version — any one opting in escalates the
    // aggregated stage-level bit.
    let quiet = CrateConfig {
        name: "quiet".to_string(),
        path: ".".to_string(),
        snapcrafts: Some(vec![SnapcraftConfig {
            publish: Some(true),
            required: Some(false),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let loud = CrateConfig {
        name: "loud".to_string(),
        path: ".".to_string(),
        snapcrafts: Some(vec![SnapcraftConfig {
            publish: Some(true),
            required: Some(true),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![quiet, loud],
        ..Default::default()
    };
    let ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
    assert!(
        derive_snapcraft_required(&ctx),
        "any crate's `required: true` must escalate the aggregated stage bit"
    );
}

#[test]
fn derive_snapcraft_required_sees_workspace_only_crate() {
    // Per-crate (workspace) mode: `required: true` on a workspace-only
    // crate must still escalate — a `config.crates`-only derivation
    // would silently miss it.
    let config = anodizer_core::config::Config {
        workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                name: "ws-only".to_string(),
                path: ".".to_string(),
                snapcrafts: Some(vec![SnapcraftConfig {
                    publish: Some(true),
                    required: Some(true),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
    assert!(
        ctx.config.crates.is_empty(),
        "fixture must be a pure-workspace config"
    );
    assert!(
        derive_snapcraft_required(&ctx),
        "workspace-only `required: true` must escalate the stage gate"
    );
}

#[test]
fn derive_snapcraft_required_respects_selected_crates_filter() {
    let quiet = CrateConfig {
        name: "quiet".to_string(),
        path: ".".to_string(),
        snapcrafts: Some(vec![SnapcraftConfig {
            publish: Some(true),
            required: Some(false),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let loud = CrateConfig {
        name: "loud".to_string(),
        path: ".".to_string(),
        snapcrafts: Some(vec![SnapcraftConfig {
            publish: Some(true),
            required: Some(true),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![quiet, loud],
        ..Default::default()
    };
    let options = anodizer_core::context::ContextOptions {
        selected_crates: vec!["quiet".to_string()],
        ..Default::default()
    };
    let ctx = Context::new(config, options);
    assert!(
        !derive_snapcraft_required(&ctx),
        "a `--crate quiet` selection must not see the deselected crate's `required: true`"
    );
}

#[test]
fn derive_snapcraft_required_ignores_build_only_config() {
    // A `publish: false` config's `required: true` names an upload that
    // will never be attempted — it must not escalate an unrelated
    // `publish: true` config in the same crate into required.
    let crate_cfg = CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        snapcrafts: Some(vec![
            SnapcraftConfig {
                publish: Some(false),
                required: Some(true),
                ..Default::default()
            },
            SnapcraftConfig {
                publish: Some(true),
                required: Some(false),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![crate_cfg],
        ..Default::default()
    };
    let ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
    assert!(
        !derive_snapcraft_required(&ctx),
        "a build-only config's `required: true` is inert; only \
             `publish: true` configs may escalate the aggregated bit"
    );
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
        tag_template: Some("v{{ .Version }}".to_string()),
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

/// A snap-configured crate whose `publish.skip` template references an
/// undefined var — reaching the pre-pass would hard-error. Used as a
/// non-invocation oracle: a correctly-firing deselect gate returns
/// `Ok(Deselected)` before the pre-pass, so the render error never
/// surfaces; a leaked gate hits the error.
fn render_error_snap_crate(name: &str) -> CrateConfig {
    use anodizer_core::config::StringOrBool;
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        snapcrafts: Some(vec![SnapcraftConfig {
            name: Some(name.to_string()),
            publish: Some(true),
            skip: Some(StringOrBool::String(
                "{{ undefined_var_that_will_not_render }}".to_string(),
            )),
            ..Default::default()
        }]),
        ..Default::default()
    }
}

fn assert_snapcraft_deselected_not_uploaded(
    crate_names: &[&str],
    opts: anodizer_core::context::ContextOptions,
) {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;
    use std::path::PathBuf;

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: crate_names
            .iter()
            .map(|n| render_error_snap_crate(n))
            .collect(),
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Version", "1.0.0");
    // Seed a snap artifact per crate so a leaked gate would reach the
    // pre-pass (the empty-snap_artifacts early-return must not mask the
    // proof).
    for n in crate_names {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Snap,
            name: String::new(),
            path: PathBuf::from(format!("/tmp/dist/{n}_1.0.0_amd64.snap")),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: n.to_string(),
            metadata: HashMap::new(),
            size: None,
        });
    }

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("deselected snapcraft must short-circuit to Ok before the upload pre-pass");

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    assert_eq!(
        snap.outcome,
        PublisherOutcome::Skipped(SkipReason::Deselected),
        "deselected snapcraft must record Skipped(Deselected)"
    );
}

#[test]
fn snapcraft_deselected_by_skip_not_uploaded_single_crate() {
    let opts = anodizer_core::context::ContextOptions {
        skip_stages: vec!["snapcraft-publish".to_string()],
        ..Default::default()
    };
    assert_snapcraft_deselected_not_uploaded(&["demo"], opts);
}

#[test]
fn snapcraft_deselected_by_allowlist_not_uploaded_single_crate() {
    let opts = anodizer_core::context::ContextOptions {
        publisher_allowlist: vec!["cargo".to_string()],
        ..Default::default()
    };
    assert_snapcraft_deselected_not_uploaded(&["demo"], opts);
}

#[test]
fn snapcraft_deselected_by_allowlist_not_uploaded_workspace_per_crate() {
    let opts = anodizer_core::context::ContextOptions {
        publisher_allowlist: vec!["cargo".to_string()],
        ..Default::default()
    };
    assert_snapcraft_deselected_not_uploaded(&["core", "cli"], opts);
}

#[test]
fn snapcraft_deselected_skip_wins_over_allowlist() {
    let opts = anodizer_core::context::ContextOptions {
        skip_stages: vec!["snapcraft-publish".to_string()],
        publisher_allowlist: vec!["snapcraft-publish".to_string()],
        ..Default::default()
    };
    assert_snapcraft_deselected_not_uploaded(&["demo"], opts);
}

#[test]
fn snapcraft_in_allowlist_is_not_deselected() {
    // `--publishers snapcraft-publish`: snapcraft IS selected, so the
    // deselect gate must NOT fire — the render-error config then surfaces
    // its error, proving the upload pre-pass WAS entered.
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;
    use std::path::PathBuf;

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![render_error_snap_crate("demo")],
        ..Default::default()
    };
    let opts = anodizer_core::context::ContextOptions {
        publisher_allowlist: vec!["snapcraft-publish".to_string()],
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
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
    let err = SnapcraftPublishStage
        .run(&mut ctx)
        .expect_err("selected snapcraft enters the pre-pass and hits the render error");
    assert!(
        format!("{err:#}").contains("render publish.skip template"),
        "{err}"
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
        tag_template: Some("v{{ .Version }}".to_string()),
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
    let planned = Vec::new();
    let outcome = run_uploads(&ctx, &crates, &[], &skip_decisions, &log, &planned);
    assert!(!outcome.attempted, "publish:false → no attempted upload");
    assert_eq!(
        outcome.skipped_already_published, 0,
        "no snaps → nothing skipped"
    );
    assert!(outcome.result.is_ok(), "no work done → Ok(())");
}

// Drives the real upload path against a stubbed `snapcraft` whose upload
// answers with the store's manual-review-hold wording: the run must stay
// green (a hold can still be approved) while the evidence snapshot and
// the outcome both carry the unresolved hold instead of a silent
// "uploaded".
// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn review_hold_is_recorded_on_evidence_not_reported_as_uploaded() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    let tools = FakeToolDir::new();
    tools
            .tool("snapcraft")
            .script(
                "if [ \"$1\" = \"upload\" ]; then\n\
                 echo \"A human will soon review your snap: (NEEDS REVIEW) confinement 'classic' not allowed\"\n\
                 exit 2\nfi\nexit 1\n",
            )
            .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                channel_templates: Some(vec!["stable".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("a review hold is non-fatal — the stage must return Ok");

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    assert_eq!(snap.outcome, PublisherOutcome::Succeeded);
    let targets = match &snap.evidence.as_ref().expect("evidence").extra {
        anodizer_core::PublishEvidenceExtra::Snapcraft(e) => &e.snapcraft_targets,
        other => panic!("wrong extra variant: {other:?}"),
    };
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].package_name, "demo");
    assert_eq!(targets[0].version.as_deref(), Some("1.0.0"));
    assert!(
        targets[0].held_for_review,
        "the review hold must be stamped on the evidence snapshot"
    );
}

// "Waiting for previous upload" is a transient store-processing conflict,
// not a manual-review hold — it must keep the non-fatal pending-warn
// treatment WITHOUT stamping held_for_review (no review queue exists for
// the operator to visit).
// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn previous_upload_conflict_is_not_stamped_as_review_hold() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(
            "if [ \"$1\" = \"upload\" ]; then\n\
                 echo \"Waiting for previous upload to complete\"\n\
                 exit 2\nfi\nexit 1\n",
        )
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("a pending-upload conflict is non-fatal");

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    let targets = match &snap.evidence.as_ref().expect("evidence").extra {
        anodizer_core::PublishEvidenceExtra::Snapcraft(e) => &e.snapcraft_targets,
        other => panic!("wrong extra variant: {other:?}"),
    };
    assert!(
        !targets[0].held_for_review,
        "a processing conflict is not a review hold"
    );
}

// A content-identical upload rejection (Snap Store binary_sha3_384
// dedup) is permanent for the given bytes — retrying resends the same
// .snap and gets rejected every time. The upload must fast-fail on the
// first attempt (no wasted retry budget) and the recorded outcome must
// carry diagnostic text explaining the rejection is content-based, not
// a transient store error.
// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn content_dedup_rejection_fails_fast_without_retry() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    let counter_dir = tempfile::TempDir::new().unwrap();
    let counter_file = counter_dir.path().join("upload_attempts");
    std::fs::write(&counter_file, "").unwrap();

    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(format!(
            "if [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {counter}\n\
                 echo \"A file with this exact same content has already been uploaded\"\n\
                 exit 2\nfi\nexit 1\n",
            counter = counter_file.display(),
        ))
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    // Per-target upload failures are reported via `PublisherResult`, not
    // the stage's own `Result` — the stage return stays `Ok(())` so
    // announce-gating and the Submitter gate still run (see the comment
    // at the end of `SnapcraftPublishStage::run`).
    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok even when a publisher fails");

    let attempts = std::fs::read_to_string(&counter_file).unwrap();
    assert_eq!(
        attempts.len(),
        1,
        "a content-dedup rejection must fail fast on the first attempt, \
             never retried — retrying resends identical bytes and gets \
             rejected every time (got {} attempts)",
        attempts.len()
    );

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    match &snap.outcome {
        PublisherOutcome::Failed(msg) => {
            assert!(
                msg.contains("content-identical"),
                "recorded failure must explain the content-dedup mechanism, got: {msg}"
            );
        }
        other => panic!("expected Failed outcome, got: {other:?}"),
    }
}

// -----------------------------------------------------------------
// Two co-occurring RETRIABLE markers — a `[503]` and the Store's
// uniqueness-check fault ("Error checking upload uniqueness.") in the
// same output — must retry. Neither is a confirmed content duplicate
// (both are transient store errors), so the second attempt gets a
// clean shot. This is NOT the 5xx-vs-dedup straddle (no definitive
// dedup marker is present); that ordering case is the next test.
// -----------------------------------------------------------------

// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn co_occurring_5xx_and_uniqueness_fault_still_retries() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{HumanDuration, RetryConfig};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    let counter_dir = tempfile::TempDir::new().unwrap();
    let counter_file = counter_dir.path().join("upload_attempts");
    std::fs::write(&counter_file, "").unwrap();

    // The FIRST upload attempt's combined output carries both a `[503]`
    // marker and the "Error checking upload uniqueness." marker — the
    // shape observed on the failed CI run this fix was diagnosed from.
    // Both now classify as retriable (the uniqueness-check fault is a
    // transient store error, NOT a confirmed content duplicate), so the
    // SECOND attempt gets a chance to succeed cleanly.
    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(format!(
            "if [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {counter}\n\
                 n=$(wc -c < {counter})\n\
                 if [ \"$n\" -eq 1 ]; then\n\
                 echo \"[503] Service Unavailable — Error checking upload uniqueness.\"\n\
                 exit 2\nfi\n\
                 exit 0\nfi\n\
                 exit 1\n",
            counter = counter_file.display(),
        ))
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        retry: Some(RetryConfig {
            attempts: 5,
            delay: HumanDuration(Duration::from_millis(1)),
            max_delay: HumanDuration(Duration::from_millis(1)),
            max_elapsed: None,
        }),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok");

    let attempts = std::fs::read_to_string(&counter_file).unwrap();
    assert_eq!(
        attempts.len(),
        2,
        "a co-occurring 5xx must still retry (2 attempts expected: the \
             failing one and the recovering one), got {} attempt(s)",
        attempts.len()
    );

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    assert!(
        matches!(snap.outcome, PublisherOutcome::Succeeded),
        "expected the retry to recover to Succeeded, got: {:?}",
        snap.outcome
    );
}

// -----------------------------------------------------------------
// Classifier ordering — `is_retriable_snap_push` is checked BEFORE
// `is_content_dedup_rejection`, so a `[503]` co-occurring with the
// DEFINITIVE content-dedup marker ("a file with this exact same
// content has already been uploaded") still retries: a transient 5xx
// whose response body also echoes the dedup wording must not be
// permanently fast-failed. `publish_stage.rs` returns
// `ControlFlow::Continue` from the retriable branch before the dedup
// block is reached. This is the true straddle of the two classifiers,
// distinct from the uniqueness-fault case above (which carries no
// dedup marker at all).
// -----------------------------------------------------------------

// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn co_occurring_5xx_and_definitive_dedup_marker_still_retries() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{HumanDuration, RetryConfig};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    let counter_dir = tempfile::TempDir::new().unwrap();
    let counter_file = counter_dir.path().join("upload_attempts");
    std::fs::write(&counter_file, "").unwrap();

    // The FIRST attempt returns a `[503]` whose body ALSO carries the
    // definitive content-dedup wording. Because the 5xx classifier is
    // checked first, this must be treated as transient and retried —
    // never fast-failed as a permanent duplicate. The SECOND attempt
    // succeeds cleanly.
    let tools = FakeToolDir::new();
    tools
            .tool("snapcraft")
            .script(format!(
                "if [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {counter}\n\
                 n=$(wc -c < {counter})\n\
                 if [ \"$n\" -eq 1 ]; then\n\
                 echo \"[503] Service Unavailable — A file with this exact same content has already been uploaded\"\n\
                 exit 2\nfi\n\
                 exit 0\nfi\n\
                 exit 1\n",
                counter = counter_file.display(),
            ))
            .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        retry: Some(RetryConfig {
            attempts: 5,
            delay: HumanDuration(Duration::from_millis(1)),
            max_delay: HumanDuration(Duration::from_millis(1)),
            max_elapsed: None,
        }),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok");

    let attempts = std::fs::read_to_string(&counter_file).unwrap();
    assert_eq!(
        attempts.len(),
        2,
        "a 5xx co-occurring with the definitive dedup marker must still \
             retry (5xx classified first): 2 attempts expected (failing + \
             recovering), got {} attempt(s)",
        attempts.len()
    );

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    assert!(
        matches!(snap.outcome, PublisherOutcome::Succeeded),
        "expected the retry to recover to Succeeded, got: {:?}",
        snap.outcome
    );
}

// -----------------------------------------------------------------
// Regression guard for the misclassified transient: `Error checking
// upload uniqueness.` with NO co-occurring 5xx must retry (it reports
// the store's uniqueness-check step faulting, a transient backend
// error — not a confirmed content duplicate). It previously fast-failed
// as a permanent dedup and emitted a false "contents must change"
// verdict, which sank the whole release. A freshly-versioned snap's
// bytes cannot collide with an older revision, so this marker is never a
// real dedup.
// -----------------------------------------------------------------

// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn uniqueness_check_fault_alone_retries_and_recovers() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{HumanDuration, RetryConfig};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    let counter_dir = tempfile::TempDir::new().unwrap();
    let counter_file = counter_dir.path().join("upload_attempts");
    std::fs::write(&counter_file, "").unwrap();

    // First attempt: uniqueness-check fault ONLY (no 5xx). Second
    // attempt: clean success. Pre-fix this fast-failed on attempt 1.
    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(format!(
            "if [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {counter}\n\
                 n=$(wc -c < {counter})\n\
                 if [ \"$n\" -eq 1 ]; then\n\
                 echo \"binary_sha3_384: Error checking upload uniqueness.\"\n\
                 exit 2\nfi\n\
                 exit 0\nfi\n\
                 exit 1\n",
            counter = counter_file.display(),
        ))
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        retry: Some(RetryConfig {
            attempts: 5,
            delay: HumanDuration(Duration::from_millis(1)),
            max_delay: HumanDuration(Duration::from_millis(1)),
            max_elapsed: None,
        }),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok");

    let attempts = std::fs::read_to_string(&counter_file).unwrap();
    assert_eq!(
        attempts.len(),
        2,
        "a uniqueness-check fault must be retried, not fast-failed as a \
             permanent dedup (2 attempts expected: the faulting one and the \
             recovering one), got {} attempt(s)",
        attempts.len()
    );

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    assert!(
        matches!(snap.outcome, PublisherOutcome::Succeeded),
        "expected the retry to recover to Succeeded, got: {:?}",
        snap.outcome
    );
}

// -----------------------------------------------------------------
// Dedup-rejection recovery — a matching-version colliding revision is
// promoted rather than reported as a permanent repack error.
// -----------------------------------------------------------------

// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn dedup_rejection_with_matching_revision_promotes_instead_of_failing() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    let upload_counter_dir = tempfile::TempDir::new().unwrap();
    let upload_counter = upload_counter_dir.path().join("upload_attempts");
    std::fs::write(&upload_counter, "").unwrap();
    let lr_counter_dir = tempfile::TempDir::new().unwrap();
    let lr_counter = lr_counter_dir.path().join("list_revisions_calls");
    std::fs::write(&lr_counter, "").unwrap();
    let release_dir = tempfile::TempDir::new().unwrap();
    let release_log = release_dir.path().join("release_calls");
    std::fs::write(&release_log, "").unwrap();

    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(format!(
            "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 printf 'x' >> {lr}\n\
                 n=$(wc -c < {lr})\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 if [ \"$n\" -gt 1 ]; then\n\
                 echo \"7    2024-06-01T00:00:00Z  amd64   1.0.0    -\"\n\
                 fi\n\
                 echo \"1    2024-01-01T00:00:00Z  amd64   0.9.0    stable\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {up}\n\
                 echo \"A file with this exact same content has already been uploaded\"\n\
                 exit 2\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"$2 $3 $4\" >> {rel}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
            lr = lr_counter.display(),
            up = upload_counter.display(),
            rel = release_log.display(),
        ))
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                channel_templates: Some(vec!["stable".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok");

    let upload_attempts = std::fs::read_to_string(&upload_counter).unwrap();
    assert_eq!(
        upload_attempts.len(),
        1,
        "recovery via promotion must not retry the upload — the bytes \
             already landed"
    );

    let release_calls = std::fs::read_to_string(&release_log).unwrap();
    assert_eq!(
        release_calls.trim(),
        "demo 7 stable",
        "expected exactly one `snapcraft release` promoting revision 7 \
             to the configured 'stable' channel, got: {release_calls:?}"
    );

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    assert!(
        matches!(snap.outcome, PublisherOutcome::Succeeded),
        "a recovered dedup rejection must record Succeeded, not Failed, \
             got: {:?}",
        snap.outcome
    );
}

// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn dedup_rejection_promotion_failure_is_reported_distinctly() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    // The FIRST `list-revisions` call is the pre-upload idempotency probe
    // (`revision_missing_channels`), which must NOT see a matching
    // revision yet or it would skip the upload before the dedup-rejection
    // / promotion-recovery path this test targets is ever reached. Only
    // the SECOND+ call (from `find_colliding_revision`, after the upload
    // is rejected as a duplicate) reports the matching revision.
    let lr_counter_dir = tempfile::TempDir::new().unwrap();
    let lr_counter = lr_counter_dir.path().join("list_revisions_calls");
    std::fs::write(&lr_counter, "").unwrap();

    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(format!(
            "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 printf 'x' >> {lr}\n\
                 n=$(wc -c < {lr})\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 if [ \"$n\" -gt 1 ]; then\n\
                 echo \"7    2024-06-01T00:00:00Z  amd64   1.0.0    -\"\n\
                 fi\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 echo \"A file with this exact same content has already been uploaded\"\n\
                 exit 2\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"snapcraft release: 403 Forbidden\"\n\
                 exit 1\n\
                 fi\n\
                 exit 1\n",
            lr = lr_counter.display(),
        ))
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                channel_templates: Some(vec!["stable".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok even when a publisher fails");

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    match &snap.outcome {
        PublisherOutcome::Failed(msg) => {
            assert!(
                msg.contains("revision 7") && msg.contains("promoting"),
                "a failed promotion must name the colliding revision and \
                     explain that promotion itself failed, got: {msg}"
            );
        }
        other => panic!("expected Failed outcome, got: {other:?}"),
    }
}

// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn preupload_probe_promotes_orphaned_revision_instead_of_skipping() {
    // A revision for this exact version already exists (an earlier run
    // uploaded it) but its Channels column is "-" — never released. The
    // pre-upload idempotency probe must not silently report
    // Skipped(AlreadyPublished) for content that was never actually
    // published to any channel; it must promote the existing revision.
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    let upload_counter_dir = tempfile::TempDir::new().unwrap();
    let upload_counter = upload_counter_dir.path().join("upload_attempts");
    std::fs::write(&upload_counter, "").unwrap();
    let release_dir = tempfile::TempDir::new().unwrap();
    let release_log = release_dir.path().join("release_calls");
    std::fs::write(&release_log, "").unwrap();

    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(format!(
            "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 echo \"7    2024-06-01T00:00:00Z  amd64   1.0.0    -\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {up}\n\
                 exit 1\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"$2 $3 $4\" >> {rel}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
            up = upload_counter.display(),
            rel = release_log.display(),
        ))
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                channel_templates: Some(vec!["stable".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok");

    let upload_attempts = std::fs::read_to_string(&upload_counter).unwrap();
    assert_eq!(
        upload_attempts.len(),
        0,
        "an orphaned-but-unreleased revision must be promoted, never re-uploaded \
             (re-upload would only hit the Store's content-dedup rejection)"
    );

    let release_calls = std::fs::read_to_string(&release_log).unwrap();
    assert_eq!(
        release_calls.trim(),
        "demo 7 stable",
        "expected exactly one `snapcraft release` promoting the orphaned revision 7 \
             to the configured 'stable' channel, got: {release_calls:?}"
    );

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    assert!(
        matches!(snap.outcome, PublisherOutcome::Succeeded),
        "recovering an orphaned revision must record Succeeded, not Skipped, got: {:?}",
        snap.outcome
    );
}

// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn preupload_probe_fully_released_revision_still_skips_cleanly() {
    // Regression guard: a revision that IS already released to every
    // configured channel is a true re-run at an already-published
    // version — must still skip cleanly, never upload or promote.
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    let upload_counter_dir = tempfile::TempDir::new().unwrap();
    let upload_counter = upload_counter_dir.path().join("upload_attempts");
    std::fs::write(&upload_counter, "").unwrap();
    let release_dir = tempfile::TempDir::new().unwrap();
    let release_log = release_dir.path().join("release_calls");
    std::fs::write(&release_log, "").unwrap();

    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(format!(
            "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 echo \"7    2024-06-01T00:00:00Z  amd64   1.0.0    stable\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 printf 'x' >> {up}\n\
                 exit 1\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"$2 $3 $4\" >> {rel}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
            up = upload_counter.display(),
            rel = release_log.display(),
        ))
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                channel_templates: Some(vec!["stable".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok");

    assert_eq!(
        std::fs::read_to_string(&upload_counter).unwrap().len(),
        0,
        "a fully-released revision must never be re-uploaded"
    );
    assert_eq!(
        std::fs::read_to_string(&release_log).unwrap().trim(),
        "",
        "a fully-released revision must never be re-promoted"
    );

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    assert!(
        matches!(
            snap.outcome,
            PublisherOutcome::Skipped(SkipReason::AlreadyPublished)
        ),
        "a revision already released everywhere configured must still skip cleanly, \
             got: {:?}",
        snap.outcome
    );
}

// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn preupload_promotion_failure_is_reported_as_failed() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(
            "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 echo \"7    2024-06-01T00:00:00Z  amd64   1.0.0    -\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 exit 1\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"snapcraft release: 403 Forbidden\"\n\
                 exit 1\n\
                 fi\n\
                 exit 1\n",
        )
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                channel_templates: Some(vec!["stable".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok even when a publisher fails");

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    match &snap.outcome {
        PublisherOutcome::Failed(msg) => {
            assert!(
                msg.contains("revision 7") && msg.contains("promoting"),
                "a failed promotion of an orphaned revision must name it and \
                     explain that promotion itself failed, got: {msg}"
            );
        }
        other => panic!("expected Failed outcome, got: {other:?}"),
    }
}

// -----------------------------------------------------------------
// Dual-arch isolation — a dual-arch snap config (`crates:` targeting
// both x86_64 and aarch64) mints one `list-revisions` row per arch per
// version; the amd64 and arm64 legs must be probed independently.
// -----------------------------------------------------------------

// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn dual_arch_arm64_not_skipped_when_only_amd64_published() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    let upload_log_dir = tempfile::TempDir::new().unwrap();
    let upload_log = upload_log_dir.path().join("upload_calls");
    std::fs::write(&upload_log, "").unwrap();

    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(format!(
            "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 echo \"5    2026-07-01T00:00:00Z  amd64   1.0.0    stable\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 echo \"$2\" >> {up}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
            up = upload_log.display(),
        ))
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                channel_templates: Some(vec!["stable".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Snap,
        name: String::new(),
        path: PathBuf::from("/tmp/dist/demo_1.0.0_arm64.snap"),
        target: Some("aarch64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok");

    let uploaded = std::fs::read_to_string(&upload_log).unwrap();
    assert_eq!(
        uploaded.trim(),
        "/tmp/dist/demo_1.0.0_arm64.snap",
        "matching on version alone would find amd64's already-released \
             revision 5 and wrongly skip arm64 too; arm64 has no revision of \
             its own yet and must be uploaded, while amd64 must NOT be \
             re-uploaded (it is already published), got: {uploaded:?}"
    );

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    assert!(
        matches!(snap.outcome, PublisherOutcome::Succeeded),
        "one arch skipped + one arch uploaded is still an overall \
             success, got: {:?}",
        snap.outcome
    );
}

// -----------------------------------------------------------------
// Per-arch revision recording — a fresh dual-arch upload must record ONE
// evidence entry per architecture, each carrying that arch's minted Snap
// Store revision, so a later `promote --from-run` can release every arch.
// Before the fix the evidence recorded `revision: None` and one entry per
// config, making `--from-run` a dead selector for multi-arch snaps.
// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
// -----------------------------------------------------------------
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn fresh_dual_arch_upload_records_a_revision_per_arch() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    // A minimal Snap Store simulation: `upload` mints an incrementing
    // revision for the arch in the snap path and appends it to a state file;
    // `list-revisions` prints that state. So the post-upload evidence probe
    // resolves each arch's own freshly-minted revision.
    let state_dir = tempfile::TempDir::new().unwrap();
    let state = state_dir.path().join("revs");
    let count = state_dir.path().join("count");
    std::fs::write(&state, "").unwrap();
    std::fs::write(&count, "0").unwrap();

    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(format!(
            "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 cat {state} 2>/dev/null\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 n=$(cat {count}); n=$((n+1)); echo \"$n\" > {count}\n\
                 case \"$2\" in\n\
                   *arm64*) a=arm64;;\n\
                   *) a=amd64;;\n\
                 esac\n\
                 echo \"$n  2026-07-01T00:00:00Z  $a  1.0.0  stable\" >> {state}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
            state = state.display(),
            count = count.display(),
        ))
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                channel_templates: Some(vec!["stable".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Snap,
        name: String::new(),
        path: PathBuf::from("/tmp/dist/demo_1.0.0_arm64.snap"),
        target: Some("aarch64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok");

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    assert_eq!(snap.outcome, PublisherOutcome::Succeeded);
    let targets: Vec<SnapcraftTarget> = match &snap.evidence.as_ref().expect("evidence").extra {
        anodizer_core::PublishEvidenceExtra::Snapcraft(e) => e.snapcraft_targets.clone(),
        other => panic!("wrong extra variant: {other:?}"),
    };
    assert_eq!(
        targets.len(),
        2,
        "one evidence entry per architecture: {targets:?}"
    );
    let amd = targets
        .iter()
        .find(|t| t.arch.as_deref() == Some("amd64"))
        .expect("amd64 entry");
    let arm = targets
        .iter()
        .find(|t| t.arch.as_deref() == Some("arm64"))
        .expect("arm64 entry");
    assert_eq!(
        amd.revision.as_deref(),
        Some("1"),
        "amd64's minted revision must be recorded: {amd:?}"
    );
    assert_eq!(
        arm.revision.as_deref(),
        Some("2"),
        "arm64's minted revision must be recorded: {arm:?}"
    );
}

// -----------------------------------------------------------------
// Dedup rejection against a DIFFERENT version's bytes — no revision
// exists at the current version, so there is nothing to promote and the
// upload must fail with a repack-required error rather than silently
// succeed or promote the wrong revision.
// -----------------------------------------------------------------

// The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn dedup_rejection_against_different_version_reports_repack_error() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::path::PathBuf;

    let release_dir = tempfile::TempDir::new().unwrap();
    let release_log = release_dir.path().join("release_calls");
    std::fs::write(&release_log, "").unwrap();

    let tools = FakeToolDir::new();
    tools
        .tool("snapcraft")
        .script(format!(
            "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Rev  Uploaded              Arches  Version  Channels\"\n\
                 echo \"3    2024-01-01T00:00:00Z  amd64   0.9.0    stable\"\n\
                 exit 0\n\
                 elif [ \"$1\" = \"upload\" ]; then\n\
                 echo \"A file with this exact same content has already been uploaded\"\n\
                 exit 2\n\
                 elif [ \"$1\" = \"release\" ]; then\n\
                 echo \"$2 $3 $4\" >> {rel}\n\
                 exit 0\n\
                 fi\n\
                 exit 1\n",
            rel = release_log.display(),
        ))
        .install();
    let _path = tools.activate();

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                channel_templates: Some(vec!["stable".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
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

    SnapcraftPublishStage
        .run(&mut ctx)
        .expect("stage return stays Ok even when a publisher fails");

    assert_eq!(
        std::fs::read_to_string(&release_log).unwrap().trim(),
        "",
        "no revision exists at the current version — there is nothing \
             to promote, so `snapcraft release` must never be called"
    );

    let snap = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "snapcraft")
        .expect("snapcraft entry recorded")
        .clone();
    match &snap.outcome {
        PublisherOutcome::Failed(msg) => {
            assert!(
                msg.contains("OTHER than") && msg.contains("contents must change"),
                "the collision is against a different version's bytes — \
                     `find_colliding_revision` found no revision at the \
                     current version, so the error must say so and direct \
                     the operator to repack rather than retry, got: {msg}"
            );
        }
        other => panic!("expected Failed outcome, got: {other:?}"),
    }
}

// -----------------------------------------------------------------
// Rendered-channel/grade preflight re-check — a template that only
// resolves to a forbidden channel at render time (or a `--publish-only`
// run, which never executes the build stage's raw preflight at all)
// must still be caught before the Snap Store ever sees the upload.
// -----------------------------------------------------------------

#[test]
fn rendered_channel_rejected_even_though_raw_template_is_not_literally_restricted() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;
    use std::path::PathBuf;

    let config = anodizer_core::config::Config {
        project_name: "demo".to_string(),
        crates: vec![CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            snapcrafts: Some(vec![anodizer_core::config::SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(true),
                confinement: Some("devmode".to_string()),
                // The raw string is "{{ .Channel }}" — it does not
                // literally equal a restricted risk word, so a check
                // against the unrendered template (the build stage's
                // preflight) would not catch it. Only after rendering
                // does it become "stable".
                channel_templates: Some(vec!["{{ .Channel }}".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, anodizer_core::context::ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Channel", "stable");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Snap,
        name: String::new(),
        path: PathBuf::from("/tmp/dist/demo_1.0.0_amd64.snap"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "demo".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let err = SnapcraftPublishStage
        .run(&mut ctx)
        .expect_err("a rendered channel the Store rejects for devmode snaps must abort");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("devmode confinement") && msg.contains("'stable'"),
        "expected the rendered-channel rejection to name devmode \
             confinement and the offending channel, got: {msg}"
    );
}
