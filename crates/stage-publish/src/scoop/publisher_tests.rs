use super::*;
use anodizer_core::config::{
    CrateConfig, PublishConfig, RepositoryConfig, ScoopConfig, StringOrBool,
};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

fn scoop_crate(name: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            scoop: Some(ScoopConfig {
                repository: Some(RepositoryConfig {
                    owner: Some("acme".to_string()),
                    name: Some("scoop-bucket".to_string()),
                    branch: Some("main".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn scoop_publisher_classification() {
    let p = ScoopPublisher::new();
    assert_eq!(p.name(), "scoop");
    assert_eq!(p.group(), PublisherGroup::Manager);
    assert!(!p.required());
    assert_eq!(
        p.rollback_scope_needed(),
        Some("GITHUB_TOKEN contents:write")
    );
}

/// `--crate x` selects only the skip_upload:true entry; an active
/// sibling `y` outside the selection must not keep the publisher live.
#[test]
fn config_fully_inactive_true_when_selected_crate_is_skipped_sibling_active() {
    let mut skipped = scoop_crate("x");
    skipped
        .publish
        .as_mut()
        .unwrap()
        .scoop
        .as_mut()
        .unwrap()
        .skip_upload = Some(StringOrBool::Bool(true));
    let ctx = TestContextBuilder::new()
        .crates(vec![skipped, scoop_crate("y")])
        .selected_crates(vec!["x".to_string()])
        .build();

    assert!(
        ScoopPublisher::new().config_fully_inactive(&ctx),
        "--crate x selects only the skip_upload:true entry; active sibling y is \
         out of scope and must not keep the publisher live"
    );
}

/// Empty `--crate` selection means "all crates" — an active entry with
/// no `--crate` filter applied must keep the publisher live.
#[test]
fn config_fully_inactive_false_with_empty_selection_and_active_entry() {
    let ctx = TestContextBuilder::new()
        .crates(vec![scoop_crate("x")])
        .build();

    assert!(
        !ScoopPublisher::new().config_fully_inactive(&ctx),
        "empty selection means \"all crates\"; an active entry must keep the \
         publisher live"
    );
}

#[test]
fn scoop_preflight_defaults_to_pass() {
    let ctx = TestContextBuilder::new().build();
    let p = ScoopPublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight ok"),
        PreflightCheck::Pass
    ));
}

#[test]
fn scoop_rollback_warns_when_no_targets_recorded() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    ctx.with_log_capture(capture.clone());
    let evidence = PublishEvidence::new("scoop");
    let p = ScoopPublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());

    let warns = capture.warn_messages();
    assert!(
        warns.iter().any(|m| m.contains("scoop")
            && m.contains("bucket clone targets")
            && m.contains("verify")),
        "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
    );
}

#[test]
fn scoop_target_extra_carries_no_secret_material() {
    // Structural pin: build evidence with a populated variant and
    // assert (a) no credential-shaped keys appear AND (b) the
    // operator-public shape is preserved. The type system pins
    // the negative half — the snapshot struct has no token field
    // to land in.
    let mut e = PublishEvidence::new("scoop");
    e.extra =
        anodizer_core::PublishEvidenceExtra::Scoop(anodizer_core::publish_evidence::ScoopExtra {
            scoop_targets: vec![ScoopTarget {
                target: "demo".into(),
                repo_url: "https://github.com/acme/scoop-bucket.git".into(),
                branch: Some("main".into()),
                token_env_var: Some("SCOOP_BUCKET_TOKEN".into()),
            }],
        });
    let s = serde_json::to_string(&e).expect("serialize");
    assert!(!s.contains("\"token\":"), "{s}");
    assert!(!s.contains("\"password\":"), "{s}");
    assert!(!s.contains("\"pat\":"), "{s}");
    assert!(!s.contains("\"private_key\":"), "{s}");
    assert!(!s.contains("\"secret\":"), "{s}");
    assert!(!s.contains("\"api_key\":"), "{s}");
    assert!(s.contains("SCOOP_BUCKET_TOKEN"), "{s}");
    assert!(s.contains("\"target\":\"demo\""), "{s}");
    assert!(s.contains("\"branch\":\"main\""), "{s}");
}

#[test]
fn commit_outcome_is_pushed() {
    assert!(util::CommitOutcome::Pushed.is_pushed());
    assert!(!util::CommitOutcome::NoChanges.is_pushed());
}

#[test]
fn scoop_target_extra_roundtrips() {
    let original = vec![ScoopTarget {
        target: "demo".into(),
        repo_url: "https://github.com/acme/scoop-bucket.git".into(),
        branch: Some("main".into()),
        token_env_var: Some("SCOOP_BUCKET_TOKEN".into()),
    }];
    let extra =
        anodizer_core::PublishEvidenceExtra::Scoop(anodizer_core::publish_evidence::ScoopExtra {
            scoop_targets: original.clone(),
        });
    let decoded = decode_scoop_targets(&extra);
    assert_eq!(decoded, original);
}

#[test]
fn scoop_collect_run_targets_walks_per_crate_config() {
    let ctx = TestContextBuilder::new()
        .crates(vec![scoop_crate("demo")])
        .build();
    let targets = collect_scoop_run_targets(&ctx);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].target, "demo");
    assert_eq!(targets[0].branch.as_deref(), Some("main"));
}

/// A pure-workspace config (empty top-level `crates:`, the cfgd shape)
/// must still record an evidence/rollback target: the run loop
/// dispatches the workspace crate and pushes the bucket commit, so an
/// empty target list here means a push with no rollback evidence
/// ("no targets recorded").
#[test]
fn scoop_collect_run_targets_sees_workspace_only_crate() {
    let ctx = TestContextBuilder::new()
        .workspaces(vec![anodizer_core::config::WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![scoop_crate("ws-only")],
            ..Default::default()
        }])
        .build();
    assert!(
        ctx.config.crates.is_empty(),
        "fixture must be a pure-workspace config"
    );
    let targets = collect_scoop_run_targets(&ctx);
    assert_eq!(targets.len(), 1, "{targets:?}");
    assert_eq!(targets[0].target, "ws-only");
}

#[test]
fn scoop_effective_publish_crates_implicit_all_when_selection_empty() {
    // Regression pin for the `selected_crates = Vec::new()` failure
    // mode: the run path used to iterate the empty Vec and silently
    // skip every configured bucket. The helper now resolves to
    // implicit-all over `publish.scoop`-carrying crates.
    let ctx = TestContextBuilder::new()
        .crates(vec![
            scoop_crate("alpha"),
            scoop_crate("beta"),
            CrateConfig {
                name: "gamma".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig::default()),
                ..Default::default()
            },
        ])
        .build();
    let names =
        crate::publisher_helpers::effective_publish_crates(&ctx, is_scoop_per_crate_configured);
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
}

#[test]
fn scoop_effective_publish_crates_honors_non_empty_selection() {
    let ctx = TestContextBuilder::new()
        .crates(vec![scoop_crate("alpha"), scoop_crate("beta")])
        .selected_crates(vec!["beta".to_string()])
        .build();
    let names =
        crate::publisher_helpers::effective_publish_crates(&ctx, is_scoop_per_crate_configured);
    assert_eq!(names, vec!["beta".to_string()]);
}

#[test]
fn scoop_rollback_dedups_shared_bucket() {
    // A single bucket can be configured for multiple crates;
    // dedup so the second `git revert HEAD` doesn't undo the
    // first. Mirror of homebrew_rollback_dedups_shared_tap.
    let targets = vec![
        ScoopTarget {
            target: "alpha".into(),
            repo_url: "https://github.com/acme/scoop-bucket.git".into(),
            branch: Some("main".into()),
            token_env_var: Some("SCOOP_BUCKET_TOKEN".into()),
        },
        ScoopTarget {
            target: "beta".into(),
            repo_url: "https://github.com/acme/scoop-bucket.git".into(),
            branch: Some("main".into()),
            token_env_var: Some("SCOOP_BUCKET_TOKEN".into()),
        },
    ];
    let unique = dedup_scoop_targets(&targets);
    assert_eq!(unique.len(), 1);
    assert_eq!(unique[0].target, "alpha");
}

// -----------------------------------------------------------------------
// Log-message helpers — the operator-facing log strings the publisher
// emits at each boundary.

#[test]
fn run_per_crate_start_message_names_crate() {
    let msg = run_per_crate_start_message("demo");
    assert!(msg.starts_with("starting per-crate scoop publish"), "{msg}");
    assert!(msg.contains("'demo'"), "{msg}");
}

#[test]
fn run_done_message_reports_processed_count() {
    let msg = run_done_message(2);
    assert!(msg.starts_with("finished scoop publish"), "{msg}");
    assert!(msg.contains("2 configured crate(s) processed"), "{msg}");
}

#[test]
fn run_no_eligible_crates_warning_names_remediation() {
    let msg = run_no_eligible_crates_warning(5);
    assert!(msg.starts_with("scoop publisher registered"), "{msg}");
    assert!(msg.contains("0 of 5 effective"), "{msg}");
    assert!(msg.contains("nothing pushed"), "{msg}");
    assert!(msg.contains("--crate"), "{msg}");
    assert!(msg.contains("--all"), "{msg}");
}

/// The no-eligible-crates warning must fire only when the iteration
/// loop's configured-predicate filtered every selected crate out — NOT
/// when `publish_to_scoop` returned `Ok(false)` because of dry-run /
/// skip_upload short-circuits.
#[test]
fn should_warn_no_eligible_only_fires_when_predicate_filtered_everything() {
    // Dry-run with one configured crate: `processed` increments on
    // crate-entry (1), so warning must not fire.
    assert!(!should_warn_no_eligible(1, 1));
    // True positive: none configured.
    assert!(should_warn_no_eligible(0, 3));
    // Empty selection → no warning.
    assert!(!should_warn_no_eligible(0, 0));
    // Partial-skip → no warning.
    assert!(!should_warn_no_eligible(1, 3));
}

/// Run the publisher end-to-end in dry-run mode against a context that
/// selects a scoop-configured crate. Verifies the run path is wired
/// (returns Ok). The bug-1 regression is anchored by
/// `should_warn_no_eligible_only_fires_when_predicate_filtered_everything`.
#[test]
fn scoop_publisher_run_dry_run_returns_ok() {
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![scoop_crate("demo")])
        .selected_crates(vec!["demo".to_string()])
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = ScoopPublisher::new();
    let evidence = p.run(&mut ctx).expect("dry-run publisher.run");
    // dry-run publish_to_scoop returns false (no actual push), so
    // evidence.extra will be empty — the run path must not error.
    let _ = decode_scoop_targets(&evidence.extra);
}

/// When the publisher is registered (a crate has a scoop block) but the
/// selected-crates filter excludes every scoop-configured crate, the run
/// path must still return Ok and record no targets.
#[test]
fn scoop_publisher_run_no_eligible_crates_returns_empty_evidence() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            scoop_crate("demo"),
            CrateConfig {
                name: "other".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig::default()),
                ..Default::default()
            },
        ])
        // Select only the non-scoop crate — publisher registered but
        // run path will iterate zero scoop-configured crates.
        .selected_crates(vec!["other".to_string()])
        .dry_run(true)
        .build();
    let p = ScoopPublisher::new();
    let evidence = p.run(&mut ctx).expect("publisher.run ok");
    assert!(
        evidence.primary_ref.is_none(),
        "no scoop-eligible crate selected, primary_ref must be unset"
    );
    let targets = decode_scoop_targets(&evidence.extra);
    assert!(
        targets.is_empty(),
        "no scoop-eligible crate selected, targets must be empty"
    );
}

#[test]
fn scoop_publisher_visible_work_contract() {
    use crate::testing::assert_publisher_visible_work_contract;
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![scoop_crate("demo")])
        .selected_crates(vec!["demo".to_string()])
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = ScoopPublisher::new();
    assert_publisher_visible_work_contract(&p, &mut ctx);
}

/// Building a scoop bucket manifest for a Windows artifact whose `sha256`
/// metadata is empty must bail with an actionable error. Defaulting to
/// `""` would emit a manifest with `architecture.hash: ""`, which
/// `scoop install` rejects (the verify step fails before the download
/// even begins). The bail message must name the publisher, the field,
/// and the offending artifact.
#[test]
fn scoop_sha256_empty_metadata_bails_with_actionable_error() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::log::{StageLogger, Verbosity};
    let mut ctx = TestContextBuilder::new()
        .crates(vec![scoop_crate("demo")])
        .build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from("/tmp/demo-windows-amd64.zip"),
        name: "demo-windows-amd64.zip".to_string(),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "demo".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("url".to_string(), "https://example.com/x.zip".to_string());
            // sha256 deliberately missing.
            m
        },
        size: None,
    });
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let err =
        super::publish_to_scoop(&mut ctx, "demo", &log).expect_err("missing sha256 must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("scoop:") && msg.contains("sha256"),
        "error must name publisher + field; got: {msg}"
    );
    assert!(
        msg.contains("demo-windows-amd64.zip"),
        "error must name the offending artifact; got: {msg}"
    );
    assert!(
        msg.contains("dist/artifacts.json") || msg.contains("re-run"),
        "error must include a next-step hint; got: {msg}"
    );
}
