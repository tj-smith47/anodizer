use super::*;
use anodizer_core::config::{
    CrateConfig, KrewConfig, PublishConfig, RepositoryConfig, StringOrBool,
};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

fn krew_crate(name: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            krew: Some(KrewConfig {
                repository: Some(RepositoryConfig {
                    owner: Some("acme".to_string()),
                    name: Some("krew-index-fork".to_string()),
                    ..Default::default()
                }),
                short_description: Some("a kubectl plugin".to_string()),
                description: Some("a kubectl plugin".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn krew_publisher_classification() {
    let p = KrewPublisher::new();
    assert_eq!(p.name(), "krew");
    assert_eq!(p.group(), PublisherGroup::Manager);
    assert!(!p.required());
    assert_eq!(
        p.rollback_scope_needed(),
        Some("GITHUB_TOKEN pull_request:write")
    );
}

/// `--crate x` selects only the skip:true entry; an active sibling `y`
/// outside the selection must not keep the publisher live.
#[test]
fn config_fully_inactive_true_when_selected_crate_is_skipped_sibling_active() {
    let mut skipped = krew_crate("x");
    skipped
        .publish
        .as_mut()
        .unwrap()
        .krew
        .as_mut()
        .unwrap()
        .skip = Some(StringOrBool::Bool(true));
    let ctx = TestContextBuilder::new()
        .crates(vec![skipped, krew_crate("y")])
        .selected_crates(vec!["x".to_string()])
        .build();

    assert!(
        KrewPublisher::new().config_fully_inactive(&ctx),
        "--crate x selects only the skip:true entry; active sibling y is out of \
             scope and must not keep the publisher live"
    );
}

/// Empty `--crate` selection means "all crates" — an active entry with
/// no `--crate` filter applied must keep the publisher live.
#[test]
fn config_fully_inactive_false_with_empty_selection_and_active_entry() {
    let ctx = TestContextBuilder::new()
        .crates(vec![krew_crate("x")])
        .build();

    assert!(
        !KrewPublisher::new().config_fully_inactive(&ctx),
        "empty selection means \"all crates\"; an active entry must keep the \
             publisher live"
    );
}

#[test]
fn krew_preflight_defaults_to_pass() {
    let ctx = TestContextBuilder::new().build();
    let p = KrewPublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight ok"),
        PreflightCheck::Pass
    ));
}

#[test]
fn krew_rollback_warns_when_no_targets_recorded() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    ctx.with_log_capture(capture.clone());
    let evidence = PublishEvidence::new("krew");
    let p = KrewPublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());

    let warns = capture.warn_messages();
    assert!(
        warns
            .iter()
            .any(|m| m.contains("krew") && m.contains("PR targets") && m.contains("verify")),
        "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
    );
}

/// Rollback with a recorded target but NO token resolvable from the
/// env: the per-target loop must warn (naming the target + the env
/// var it tried) and `continue` WITHOUT making any network call —
/// then complete `Ok(())`, emitting the all-zero summary. Pins the
/// no-token skip arm that protects against firing a credential-less
/// GitHub API request.
#[test]
fn krew_rollback_warns_and_skips_target_when_no_token_resolvable() {
    let capture = anodizer_core::log::LogCapture::new();
    // A sealed (closed, empty) env source carries NONE of
    // KREW_INDEX_TOKEN / ANODIZER_GITHUB_TOKEN / GITHUB_TOKEN, so
    // resolve_token yields None and the target is skipped before any
    // api.github.com request.
    let mut ctx = TestContextBuilder::new().sealed_env().build();
    ctx.with_log_capture(capture.clone());
    let mut evidence = PublishEvidence::new("krew");
    evidence.extra =
        anodizer_core::PublishEvidenceExtra::Krew(anodizer_core::publish_evidence::KrewExtra {
            krew_targets: vec![KrewPrTarget {
                target: "demo".into(),
                upstream_owner: "kubernetes-sigs".into(),
                upstream_repo: "krew-index".into(),
                fork_owner: "acme".into(),
                branch: "demo-v1.2.3".into(),
                token_env_var: Some("KREW_INDEX_TOKEN".into()),
            }],
        });
    let p = KrewPublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());

    let warns = capture.warn_messages();
    assert!(
        warns.iter().any(|m| m.contains("no krew token resolvable")
            && m.contains("demo")
            && m.contains("KREW_INDEX_TOKEN")),
        "expected a no-token warn naming the target + env var; got: {warns:?}"
    );
    // The final summary reports zero work — no PR was queried or closed.
    let all = capture.all_messages();
    assert!(
        all.iter().any(|(_, m)| m.contains("closed 0")
            && m.contains("already-closed 0")
            && m.contains("failed 0")),
        "no-token skip must leave all counters at zero; got: {all:?}"
    );
}

#[test]
fn krew_target_extra_roundtrips() {
    let original = vec![KrewPrTarget {
        target: "demo".into(),
        upstream_owner: "kubernetes-sigs".into(),
        upstream_repo: "krew-index".into(),
        fork_owner: "acme".into(),
        branch: "demo-v1.2.3".into(),
        token_env_var: Some("KREW_INDEX_TOKEN".into()),
    }];
    let extra =
        anodizer_core::PublishEvidenceExtra::Krew(anodizer_core::publish_evidence::KrewExtra {
            krew_targets: original.clone(),
        });
    let decoded = decode_krew_targets(&extra);
    assert_eq!(decoded, original);
}

#[test]
fn krew_target_extra_carries_no_secret_material() {
    // Structural pin: build a typed-variant evidence and assert
    // (a) no credential-shaped keys appear AND (b) the
    // operator-public PR coordinates are preserved.
    let mut e = anodizer_core::PublishEvidence::new("krew");
    e.extra =
        anodizer_core::PublishEvidenceExtra::Krew(anodizer_core::publish_evidence::KrewExtra {
            krew_targets: vec![KrewPrTarget {
                target: "demo".into(),
                upstream_owner: "kubernetes-sigs".into(),
                upstream_repo: "krew-index".into(),
                fork_owner: "acme".into(),
                branch: "demo-v1.2.3".into(),
                token_env_var: Some("KREW_INDEX_TOKEN".into()),
            }],
        });
    let s = serde_json::to_string(&e).expect("serialize");
    assert!(!s.contains("\"token\":"), "{s}");
    assert!(!s.contains("\"password\":"), "{s}");
    assert!(!s.contains("\"pat\":"), "{s}");
    assert!(!s.contains("\"private_key\":"), "{s}");
    assert!(!s.contains("\"secret\":"), "{s}");
    assert!(!s.contains("\"api_key\":"), "{s}");
    assert!(s.contains("KREW_INDEX_TOKEN"), "{s}");
    assert!(s.contains("\"upstream_owner\":\"kubernetes-sigs\""), "{s}");
    assert!(s.contains("\"upstream_repo\":\"krew-index\""), "{s}");
    assert!(s.contains("\"fork_owner\":\"acme\""), "{s}");
}

#[test]
fn krew_effective_publish_crates_implicit_all_when_selection_empty() {
    // Regression pin for the `selected_crates = Vec::new()` failure
    // mode: the run path used to iterate the empty Vec and silently
    // skip every configured krew plugin. The helper now resolves to
    // implicit-all over `publish.krew`-carrying crates.
    let ctx = TestContextBuilder::new()
        .crates(vec![
            krew_crate("alpha"),
            krew_crate("beta"),
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
        crate::publisher_helpers::effective_publish_crates(&ctx, is_krew_per_crate_configured);
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
}

#[test]
fn krew_effective_publish_crates_honors_non_empty_selection() {
    let ctx = TestContextBuilder::new()
        .crates(vec![krew_crate("alpha"), krew_crate("beta")])
        .selected_crates(vec!["beta".to_string()])
        .build();
    let names =
        crate::publisher_helpers::effective_publish_crates(&ctx, is_krew_per_crate_configured);
    assert_eq!(names, vec!["beta".to_string()]);
}

#[test]
fn krew_collect_run_targets_uses_default_upstream() {
    let ctx = TestContextBuilder::new()
        .crates(vec![krew_crate("demo")])
        .build();
    let target = collect_krew_target(&ctx, "demo", &ctx.logger("publish"))
        .expect("render ok")
        .expect("target");
    assert_eq!(target.target, "demo");
    assert_eq!(target.upstream_owner, "kubernetes-sigs");
    assert_eq!(target.upstream_repo, "krew-index");
    assert_eq!(target.fork_owner, "acme");
    assert!(
        target.branch.starts_with("demo-v"),
        "branch: {}",
        target.branch
    );
}

#[test]
fn krew_collect_run_targets_honors_pull_request_base_override() {
    use anodizer_core::config::{PullRequestBaseConfig, PullRequestConfig};
    let mut c = krew_crate("demo");
    if let Some(p) = c.publish.as_mut()
        && let Some(k) = p.krew.as_mut()
        && let Some(r) = k.repository.as_mut()
    {
        r.pull_request = Some(PullRequestConfig {
            enabled: Some(true),
            base: Some(PullRequestBaseConfig {
                owner: Some("custom-org".to_string()),
                name: Some("custom-index".to_string()),
                branch: None,
            }),
            draft: None,
            body: None,
        });
    }
    let ctx = TestContextBuilder::new().crates(vec![c]).build();
    let target = collect_krew_target(&ctx, "demo", &ctx.logger("publish"))
        .expect("render ok")
        .expect("target");
    assert_eq!(target.upstream_owner, "custom-org");
    assert_eq!(target.upstream_repo, "custom-index");
}

// -----------------------------------------------------------------------
// Log-message helpers — the operator-facing log strings the publisher
// emits at each boundary.

#[test]
fn run_per_crate_start_message_names_crate() {
    let msg = run_per_crate_start_message("demo");
    assert!(msg.starts_with("starting per-crate krew publish"), "{msg}");
    assert!(msg.contains("'demo'"), "{msg}");
}

#[test]
fn run_done_message_reports_processed_count() {
    let msg = run_done_message(2);
    assert!(msg.starts_with("finished krew publish"), "{msg}");
    assert!(msg.contains("2 configured crate(s) processed"), "{msg}");
}

#[test]
fn run_no_eligible_crates_warning_names_remediation() {
    let msg = run_no_eligible_crates_warning(5);
    assert!(msg.starts_with("krew publisher registered"), "{msg}");
    assert!(msg.contains("0 of 5 effective"), "{msg}");
    assert!(msg.contains("nothing pushed"), "{msg}");
    assert!(msg.contains("--crate"), "{msg}");
    assert!(msg.contains("--all"), "{msg}");
}

/// The no-eligible-crates warning must fire only when the iteration
/// loop's configured-predicate filtered every selected crate out — not
/// when the publish path was reached successfully.
#[test]
fn should_warn_no_eligible_only_fires_when_predicate_filtered_everything() {
    // One configured crate reached the publish path → no warning.
    assert!(!should_warn_no_eligible(1, 1));
    // True positive: none configured.
    assert!(should_warn_no_eligible(0, 3));
    // Empty selection → no warning.
    assert!(!should_warn_no_eligible(0, 0));
    // Partial-skip → no warning.
    assert!(!should_warn_no_eligible(1, 3));
}

/// Run the publisher end-to-end in dry-run mode against a context that
/// selects a krew-configured crate. Verifies the run path is wired
/// (returns Ok). The log lines are written to stderr and asserted
/// indirectly via the helper-string tests above.
#[test]
fn krew_publisher_run_dry_run_returns_ok() {
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![krew_crate("demo")])
        .selected_crates(vec!["demo".to_string()])
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = KrewPublisher::new();
    let evidence = p.run(&mut ctx).expect("dry-run publisher.run");
    // dry-run publish_to_krew short-circuits before branch push; no actual
    // push occurred so evidence.extra must be empty (no phantom targets).
    let targets = decode_krew_targets(&evidence.extra);
    assert!(
        targets.is_empty(),
        "dry-run must not record rollback targets: {targets:?}"
    );
}

/// When the publisher is registered (a crate has a krew block) but the
/// selected-crates filter excludes every krew-configured crate, the run
/// path must still return Ok and the processed count is zero.
#[test]
fn krew_publisher_run_no_eligible_crates_returns_ok() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            krew_crate("demo"),
            CrateConfig {
                name: "other".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig::default()),
                ..Default::default()
            },
        ])
        // Select only the non-krew crate — publisher registered but
        // run path will iterate zero krew-configured crates.
        .selected_crates(vec!["other".to_string()])
        .dry_run(true)
        .build();
    let p = KrewPublisher::new();
    // Must return Ok even when no krew-configured crate is selected.
    p.run(&mut ctx).expect("publisher.run ok");
}

/// Implicit-all selection (empty `selected_crates`) + dry-run must
/// produce empty evidence. The implicit-all path resolves through
/// `effective_publish_crates` to every krew-configured crate, so this
/// pins the gate where phantom rollback targets used to leak.
#[test]
fn test_publish_to_krew_dry_run_implicit_all_produces_empty_evidence() {
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![krew_crate("demo"), krew_crate("other")])
        // No selected_crates → implicit-all resolves to both krew crates.
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = KrewPublisher::new();
    let evidence = p.run(&mut ctx).expect("dry-run implicit-all publisher.run");
    let targets = decode_krew_targets(&evidence.extra);
    assert!(
        targets.is_empty(),
        "dry-run + implicit-all must not record rollback targets: {targets:?}"
    );
}

/// skip_upload path must produce empty evidence — no branch push occurred.
#[test]
fn krew_publisher_run_skip_upload_produces_empty_evidence() {
    let mut crate_with_skip = krew_crate("demo");
    if let Some(ref mut publish) = crate_with_skip.publish
        && let Some(ref mut krew) = publish.krew
    {
        krew.skip_upload = Some(StringOrBool::Bool(true));
    }
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![crate_with_skip])
        .selected_crates(vec!["demo".to_string()])
        .project_root(repo.path().to_path_buf())
        .build();
    let p = KrewPublisher::new();
    let evidence = p.run(&mut ctx).expect("skip_upload publisher.run");
    let targets = decode_krew_targets(&evidence.extra);
    assert!(
        targets.is_empty(),
        "skip_upload must not record rollback targets: {targets:?}"
    );
}

#[test]
fn krew_publisher_visible_work_contract() {
    use crate::testing::assert_publisher_visible_work_contract;
    let repo = crate::testing::hermetic_tagged_repo();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![krew_crate("demo")])
        .selected_crates(vec!["demo".to_string()])
        .dry_run(true)
        .project_root(repo.path().to_path_buf())
        .build();
    let p = KrewPublisher::new();
    assert_publisher_visible_work_contract(&p, &mut ctx);
}

/// Building a krew plugin manifest for an artifact whose `sha256`
/// metadata is empty must bail with an actionable error. Defaulting
/// to `""` would embed an empty `sha256:` field in the rendered
/// manifest, which krew's `addURIAndSha` validator rejects at
/// install time. The bail message must name the publisher, the
/// field, the offending artifact context, and a next-step hint.
#[test]
fn krew_sha256_empty_metadata_bails_with_actionable_error() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{Config, CrateConfig, KrewConfig, PublishConfig, RepositoryConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("krew-index-fork".to_string()),
                        ..Default::default()
                    }),
                    short_description: Some("a kubectl plugin".to_string()),
                    description: Some("a kubectl plugin".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from("/tmp/mytool-linux-amd64.tar.gz"),
        name: "mytool-linux-amd64.tar.gz".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "mytool".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "url".to_string(),
                "https://example.com/mytool-linux-amd64.tar.gz".to_string(),
            );
            m.insert("extra_binaries".to_string(), "mytool".to_string());
            m
        },
        size: None,
    });
    let log = StageLogger::new("publish", Verbosity::Quiet);
    let err = publish_to_krew(&mut ctx, "mytool", &log).expect_err("missing sha256 must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing sha256 metadata"),
        "error must mention missing sha256; got: {msg}"
    );
    assert!(
        msg.contains("mytool"),
        "error must name the offending crate; got: {msg}"
    );
    assert!(
        msg.contains("checksum stage"),
        "error must mention the checksum stage; got: {msg}"
    );
}

/// The krew `short_description` gate intentionally bails only when BOTH
/// `short_description` AND the effective description (including the
/// Cargo.toml-derived one) are empty. A crate with no `short_description`
/// but a Cargo.toml `package.description` must get PAST the gate (and the
/// short_description fall back to that description), failing later on the
/// missing artifact/sha256 — never on "short_description is not set".
#[test]
fn krew_short_description_falls_back_to_cargo_toml_description() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("mytool");
    std::fs::create_dir_all(&crate_dir).unwrap();
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"mytool\"\ndescription = \"a derived kubectl plugin\"\n",
    )
    .unwrap();

    let mut config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: "mytool".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                krew: Some(KrewConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("acme".to_string()),
                        name: Some("krew-index-fork".to_string()),
                        ..Default::default()
                    }),
                    // No short_description AND no description here — both
                    // must come from the crate's Cargo.toml.
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    config.populate_derived_metadata(tmp.path());

    let mut ctx = Context::new(config, ContextOptions::default());
    let log = StageLogger::new("publish", Verbosity::Quiet);
    // No artifacts registered → fails downstream, but NOT on the gate.
    let err = publish_to_krew(&mut ctx, "mytool", &log)
        .expect_err("no artifacts → must still fail downstream");
    let msg = format!("{err:#}");
    assert!(
        !msg.contains("short_description is not set"),
        "short_description must fall back to Cargo.toml description, not gate-bail; got: {msg}"
    );
    assert!(
        !msg.contains("description is not set"),
        "description must resolve from Cargo.toml; got: {msg}"
    );
}
