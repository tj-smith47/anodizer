use super::*;
use anodizer_core::config::{AurSourceConfig, CrateConfig, PublishConfig, StringOrBool};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

fn aur_source_crate(name: &str, git_url: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            aur_source: Some(AurSourceConfig {
                git_url: Some(git_url.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn aur_source_publisher_classification() {
    let p = AurSourcePublisher::new();
    assert_eq!(p.name(), "upstream-aur");
    assert_eq!(p.group(), PublisherGroup::Submitter);
    assert!(!p.required());
    assert_eq!(p.rollback_scope_needed(), Some("AUR_SSH_KEY write"));
}

/// `--crate x` selects only the skip:true entry; an active sibling `y`
/// outside the selection must not keep the publisher live.
#[test]
fn config_fully_inactive_true_when_selected_crate_is_skipped_sibling_active() {
    let mut skipped = aur_source_crate("x", "ssh://aur@aur.archlinux.org/x.git");
    skipped
        .publish
        .as_mut()
        .unwrap()
        .aur_source
        .as_mut()
        .unwrap()
        .skip = Some(StringOrBool::Bool(true));
    let active = aur_source_crate("y", "ssh://aur@aur.archlinux.org/y.git");
    let ctx = TestContextBuilder::new()
        .crates(vec![skipped, active])
        .selected_crates(vec!["x".to_string()])
        .build();

    assert!(
        AurSourcePublisher::new().config_fully_inactive(&ctx),
        "--crate x selects only the skip:true entry; active sibling y is out of \
             scope and must not keep the publisher live"
    );
}

/// Empty `--crate` selection means "all crates" — an active entry with
/// no `--crate` filter applied must keep the publisher live.
#[test]
fn config_fully_inactive_false_with_empty_selection_and_active_entry() {
    let ctx = TestContextBuilder::new()
        .crates(vec![aur_source_crate(
            "x",
            "ssh://aur@aur.archlinux.org/x.git",
        )])
        .build();

    assert!(
        !AurSourcePublisher::new().config_fully_inactive(&ctx),
        "empty selection means \"all crates\"; an active entry must keep the \
             publisher live"
    );
}

/// The AUR schema floor runs `bash -n` over the rendered source
/// PKGBUILD when the tool is present and warn+skips otherwise, so it is
/// ADVISORY: recommended to the auto-install layer, never a blocker.
#[test]
fn aur_source_advisory_requirements_emit_bash_when_active() {
    let ctx = TestContextBuilder::new()
        .crates(vec![aur_source_crate(
            "demo",
            "ssh://aur@aur.archlinux.org/demo.git",
        )])
        .build();
    let reqs = AurSourcePublisher::new().advisory_requirements(&ctx);
    assert!(
        reqs.iter().any(|r| matches!(
            r,
            anodizer_core::EnvRequirement::Tool { name } if name == "bash"
        )),
        "active aur_source entry must recommend bash: {reqs:?}"
    );
}

#[test]
fn aur_source_advisory_requirements_empty_when_all_entries_skipped() {
    let mut c = aur_source_crate("demo", "ssh://aur@aur.archlinux.org/demo.git");
    if let Some(a) = c.publish.as_mut().and_then(|p| p.aur_source.as_mut()) {
        a.skip = Some(anodizer_core::config::StringOrBool::Bool(true));
    }
    let ctx = TestContextBuilder::new().crates(vec![c]).build();
    let reqs = AurSourcePublisher::new().advisory_requirements(&ctx);
    assert!(
        reqs.is_empty(),
        "every entry skipped ⇒ no advisory recommendations: {reqs:?}"
    );
}

/// `git_url` unset → derives `ssh://aur@aur.archlinux.org/<pkg>.git`
/// (no `-bin` suffix for source packages); an explicit `git_url` is used
/// verbatim; an empty-string `git_url` is treated as unset.
#[test]
fn aur_source_push_git_url_derives_from_name() {
    use anodizer_core::config::AurSourceConfig;

    // Per-crate path: default name is the crate name (no -bin strip).
    let cfg = AurSourceConfig::default();
    let pkg = resolve_aur_source_package_name(&cfg, "mytool", false);
    assert_eq!(pkg, "mytool");
    assert_eq!(
        aur_source_push_git_url(&cfg, &pkg),
        "ssh://aur@aur.archlinux.org/mytool.git",
    );

    // Top-level path: default name is the project name with a trailing
    // `-bin` stripped, so a `foo-bin` project yields `foo`.
    let pkg_top = resolve_aur_source_package_name(&cfg, "foo-bin", true);
    assert_eq!(pkg_top, "foo");
    assert_eq!(
        aur_source_push_git_url(&cfg, &pkg_top),
        "ssh://aur@aur.archlinux.org/foo.git",
    );

    // Explicit `name:` override → url tracks the override.
    let cfg_name = AurSourceConfig {
        name: Some("widget".to_string()),
        ..Default::default()
    };
    let pkg_name = resolve_aur_source_package_name(&cfg_name, "mytool", false);
    assert_eq!(
        aur_source_push_git_url(&cfg_name, &pkg_name),
        "ssh://aur@aur.archlinux.org/widget.git",
    );

    // Empty-string git_url is treated as unset (still derives).
    let cfg_empty = AurSourceConfig {
        git_url: Some(String::new()),
        ..Default::default()
    };
    assert_eq!(
        aur_source_push_git_url(&cfg_empty, "mytool"),
        "ssh://aur@aur.archlinux.org/mytool.git",
    );

    // Explicit git_url is a verbatim override.
    let cfg_override = AurSourceConfig {
        git_url: Some("ssh://aur@aur.archlinux.org/custom.git".to_string()),
        name: Some("widget".to_string()),
        ..Default::default()
    };
    assert_eq!(
        aur_source_push_git_url(&cfg_override, "widget"),
        "ssh://aur@aur.archlinux.org/custom.git",
    );
}

#[test]
fn aur_source_preflight_defaults_to_pass() {
    let ctx = TestContextBuilder::new().build();
    let p = AurSourcePublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight ok"),
        PreflightCheck::Pass
    ));
}

/// The publisher emits the shared per-crate entry line — upstream-AUR
/// previously had NO run-entry scan line, so an operator grepping the
/// `starting … publish — scanning` family saw every per-crate publisher
/// except this one.
#[test]
fn aur_source_run_emits_shared_start_line() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    ctx.with_log_capture(capture.clone());
    let p = AurSourcePublisher::new();
    let _ = p.run(&mut ctx);
    let msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
    assert!(
        msgs.iter()
            .any(|m| m.contains("starting aur_source publish — scanning")),
        "run() must emit the shared entry line; got: {msgs:?}"
    );
}

#[test]
fn aur_source_rollback_warns_when_no_targets_recorded() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    ctx.with_log_capture(capture.clone());
    let evidence = PublishEvidence::new("upstream-aur");
    let p = AurSourcePublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());

    let warns = capture.warn_messages();
    assert!(
        warns.iter().any(|m| m.contains("upstream-aur")
            && m.contains("recorded force-pushes")
            && m.contains("verify")),
        "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
    );
}

#[test]
fn aur_source_rollback_warns_per_target_when_evidence_present() {
    let mut ctx = TestContextBuilder::new().build();
    let mut evidence = PublishEvidence::new("upstream-aur");
    evidence.extra = anodizer_core::PublishEvidenceExtra::AurSource(
        anodizer_core::publish_evidence::AurSourceExtra {
            aur_source_targets: vec![
                AurSourceTarget {
                    target: "aur_source: crate 'demo'".into(),
                    package: "demo".into(),
                    tag: "1.2.3".into(),
                    git_url: "ssh://aur@aur.archlinux.org/demo.git".into(),
                },
                AurSourceTarget {
                    target: "aur_sources[0]".into(),
                    package: "widget".into(),
                    tag: "1.2.3".into(),
                    git_url: "ssh://aur@aur.archlinux.org/widget.git".into(),
                },
            ],
        },
    );
    let p = AurSourcePublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());
    assert_eq!(decode_aur_source_targets(&evidence.extra).len(), 2);
}

#[test]
fn aur_source_target_extra_roundtrips() {
    let original = vec![AurSourceTarget {
        target: "aur_source: crate 'demo'".into(),
        package: "demo".into(),
        tag: "1.2.3".into(),
        git_url: "ssh://aur@aur.archlinux.org/demo.git".into(),
    }];
    let extra = anodizer_core::PublishEvidenceExtra::AurSource(
        anodizer_core::publish_evidence::AurSourceExtra {
            aur_source_targets: original.clone(),
        },
    );
    let decoded = decode_aur_source_targets(&extra);
    assert_eq!(decoded, original);
}

#[test]
fn aur_source_target_extra_carries_no_secret_material() {
    // Structural pin: build a typed-variant evidence and assert
    // (a) no credential-shaped keys appear AND (b) the
    // operator-public coordinates are preserved. The
    // `AurSourceTargetSnapshot` type has no field for
    // `private_key` / `git_ssh_command`, so the type system
    // rejects any future leak attempt at the encode boundary.
    let mut e = PublishEvidence::new("upstream-aur");
    e.extra = anodizer_core::PublishEvidenceExtra::AurSource(
        anodizer_core::publish_evidence::AurSourceExtra {
            aur_source_targets: vec![AurSourceTarget {
                target: "aur_source: crate 'demo'".into(),
                package: "demo".into(),
                tag: "1.2.3".into(),
                git_url: "ssh://aur@aur.archlinux.org/demo.git".into(),
            }],
        },
    );
    let s = serde_json::to_string(&e).expect("serialize");
    assert!(!s.contains("\"private_key\":"), "{s}");
    assert!(!s.contains("\"git_ssh_command\":"), "{s}");
    assert!(!s.contains("\"token\":"), "{s}");
    assert!(!s.contains("\"auth\":"), "{s}");
    assert!(!s.contains("\"password\":"), "{s}");
    assert!(!s.contains("\"secret\":"), "{s}");
    assert!(!s.contains("\"api_key\":"), "{s}");
    // Positive shape: operator-public coordinates serialize.
    assert!(s.contains("\"package\":\"demo\""), "{s}");
    assert!(s.contains("\"tag\":\"1.2.3\""), "{s}");
    assert!(
        s.contains("\"git_url\":\"ssh://aur@aur.archlinux.org/demo.git\""),
        "{s}"
    );
}

#[test]
fn aur_source_collect_per_crate_target_uses_default_name() {
    let ctx = TestContextBuilder::new()
        .crates(vec![aur_source_crate(
            "demo",
            "ssh://aur@aur.archlinux.org/demo.git",
        )])
        .build();
    let t = collect_aur_source_per_crate_target(&ctx, "demo").expect("target");
    assert_eq!(t.package, "demo");
    assert_eq!(t.git_url, "ssh://aur@aur.archlinux.org/demo.git");
}

/// A workspace-only crate (pure-workspace config) must snapshot a
/// rollback target: without the universe lookup the publish pushes the
/// AUR repo but records nothing, orphaning the push from rollback.
#[test]
fn aur_source_collect_per_crate_target_sees_workspace_only_crate() {
    let ctx = TestContextBuilder::new()
        .workspaces(vec![anodizer_core::config::WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![aur_source_crate(
                "ws-only",
                "ssh://aur@aur.archlinux.org/ws-only.git",
            )],
            ..Default::default()
        }])
        .build();
    assert!(
        ctx.config.crates.is_empty(),
        "fixture must be a pure-workspace config"
    );
    let t = collect_aur_source_per_crate_target(&ctx, "ws-only").expect("target snapshot");
    assert_eq!(t.package, "ws-only");
}

/// The `arch=()` array comes from the workspace-only crate's own
/// `builds[].targets` — a `config.crates`-only lookup missed the crate
/// and silently fell back to the default target set, advertising the
/// wrong architectures for the source package.
#[test]
fn aur_source_arches_uses_workspace_only_crate_builds() {
    let ws_crate = CrateConfig {
        name: "ws-only".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        builds: Some(vec![anodizer_core::config::BuildConfig {
            targets: Some(vec!["aarch64-unknown-linux-gnu".to_string()]),
            ..Default::default()
        }]),
        publish: Some(PublishConfig {
            aur_source: Some(AurSourceConfig::default()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ctx = TestContextBuilder::new()
        .workspaces(vec![anodizer_core::config::WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![ws_crate],
            ..Default::default()
        }])
        .build();
    let arches = aur_source_arches(&ctx, "ws-only").expect("arches");
    assert_eq!(
        arches,
        vec!["aarch64".to_string()],
        "arches must come from the crate's own builds, not the default target fallback"
    );
}

/// The per-crate "no aur_source config block" line routes through
/// `skip_line` (Debug by default) like the other per-crate publishers —
/// not an unconditional `status`, which would emit one line per
/// non-applicable crate at default verbosity under `--crate` selection.
#[test]
fn aur_source_no_config_skip_is_debug_level_by_default() {
    use anodizer_core::log::LogLevel;
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            aur_source_crate("configured", "ssh://aur@aur.archlinux.org/configured.git"),
            CrateConfig {
                name: "unconfigured".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                publish: Some(PublishConfig::default()),
                ..Default::default()
            },
        ])
        .selected_crates(vec!["unconfigured".to_string()])
        .show_skipped(false)
        .build();
    ctx.with_log_capture(capture.clone());
    let p = AurSourcePublisher::new();
    let _ = p.run(&mut ctx);
    let lines = capture.all_messages();
    let skip = lines
        .iter()
        .find(|(_, m)| m.contains("no aur_source config block"))
        .unwrap_or_else(|| panic!("skip line must be recorded: {lines:?}"));
    assert_eq!(
        skip.0,
        LogLevel::Debug,
        "no-config skip must not record at Status by default: {lines:?}"
    );
}
