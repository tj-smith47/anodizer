use super::*;
use anodizer_core::EnvRequirement;
use anodizer_core::log::Verbosity;
use anodizer_core::test_helpers::TestContextBuilder;

fn crate_from_yaml(yaml: &str) -> anodizer_core::config::CrateConfig {
    serde_yaml_ng::from_str(yaml).expect("crate config yaml")
}

/// Per-crate workspace union: a publisher configured ONLY on a
/// workspace crate (invisible to `configured_publishers`' top-level
/// predicates until the per-crate overlay flattens it) must still
/// contribute requirements when collection runs once, up front.
#[test]
fn collect_requirements_unions_workspace_crates() {
    let top = crate_from_yaml(
        r#"
name: top
publish:
  scoop:
    repository: { owner: o, name: bucket }
"#,
    );
    let ws_crate = crate_from_yaml(
        r#"
name: wscrate
publish:
  aur:
    private_key: "{{ .Env.PF_TEST_AUR_KEY }}"
"#,
    );
    let ws = anodizer_core::config::WorkspaceConfig {
        name: "ws".to_string(),
        crates: vec![ws_crate],
        ..Default::default()
    };
    let ctx = TestContextBuilder::new()
        .crates(vec![top])
        .workspaces(vec![ws])
        .build();

    let reqs = collect_requirements(&ctx, PreflightScope::Full);
    assert!(
        reqs.iter().any(|r| r.source == "publish:scoop"),
        "top-level crate's scoop requirements missing: {reqs:?}"
    );
    let aur_key = reqs.iter().any(|r| {
        r.source == "publish:aur"
            && matches!(
                &r.requirement,
                EnvRequirement::KeyEnv { var, .. } if var == "PF_TEST_AUR_KEY"
            )
    });
    assert!(
        aur_key,
        "workspace crate's aur key requirement missing: {reqs:?}"
    );
}

/// Publisher ADVISORY requirements ride the same deselection predicate
/// as the hard set: a `--publishers` allowlist that excludes homebrew
/// must drop its `ruby` recommendation, while an unfiltered run carries
/// it (advisory, sourced to the publisher).
#[test]
fn advisory_requirements_respect_publisher_deselection() {
    let yaml = r#"
name: top
publish:
  homebrew:
    repository: { owner: acme, name: homebrew-tap }
"#;
    let ruby_advisory = |reqs: &[SourcedRequirement]| {
        reqs.iter().any(|r| {
            r.advisory
                && r.source == "publish:homebrew"
                && matches!(
                    &r.requirement,
                    EnvRequirement::Tool { name } if name == "ruby"
                )
        })
    };
    let ctx = TestContextBuilder::new()
        .crates(vec![crate_from_yaml(yaml)])
        .build();
    assert!(
        ruby_advisory(&collect_requirements(&ctx, PreflightScope::Full)),
        "unfiltered run must carry homebrew's advisory ruby"
    );
    let deselected = TestContextBuilder::new()
        .crates(vec![crate_from_yaml(yaml)])
        .publisher_allowlist(vec!["npm".to_string()])
        .build();
    assert!(
        !ruby_advisory(&collect_requirements(&deselected, PreflightScope::Full)),
        "a --publishers allowlist excluding homebrew must drop its advisory ruby"
    );
}

/// `--skip=publish` must drop every publisher-sourced requirement
/// while stage-sourced ones survive.
#[test]
fn skip_publish_drops_publisher_requirements() {
    let top = crate_from_yaml(
        r#"
name: top
publish:
  scoop:
    repository: { owner: o, name: bucket }
"#,
    );
    let ctx = TestContextBuilder::new()
        .crates(vec![top])
        .skip_stages(vec!["publish".to_string()])
        .build();
    let reqs = collect_requirements(&ctx, PreflightScope::Full);
    assert!(
        !reqs.iter().any(|r| r.source.starts_with("publish:")),
        "publisher requirements survived --skip=publish: {reqs:?}"
    );
    assert!(
        reqs.iter().any(|r| r.source == "stage:build"),
        "stage requirements must survive a publish skip: {reqs:?}"
    );
}

/// An empty config must not demand publisher credentials: the
/// ungated `all_publishers` walk relies on every `requirements`
/// impl self-gating on its own configuration. That includes
/// github-release — the release stage only releases crates carrying a
/// `release:` block (real configs get one injected by defaults
/// merging), so a crate-less config demands no token.
#[test]
fn empty_config_yields_no_publisher_requirements() {
    let ctx = TestContextBuilder::new().build();
    let reqs = collect_requirements(&ctx, PreflightScope::Full);
    assert!(
        !reqs.iter().any(|r| r.source.starts_with("publish:")),
        "unconfigured publishers contributed requirements: {reqs:?}"
    );

    // And the inverse: a crate WITH a release block demands the ladder.
    let top = crate_from_yaml("name: top\nrelease: { github: { owner: o, name: r } }");
    let ctx = TestContextBuilder::new().crates(vec![top]).build();
    let reqs = collect_requirements(&ctx, PreflightScope::Full);
    assert!(
        reqs.iter().any(|r| r.source == "publish:github-release"),
        "a configured release block must require the token ladder: {reqs:?}"
    );
}

/// Bundler stages contribute their tools only when the configured
/// build targets include the platform they package: a Windows target
/// matrix demands makensis + the WiX toolchain, a Linux-only matrix
/// demands neither — and dmg's detection ladder surfaces as a
/// tool-any-of, not three hard requirements.
#[test]
fn bundler_requirements_follow_configured_targets() {
    let installer_yaml = |targets: &str| {
        crate_from_yaml(&format!(
            r#"
name: app
builds:
  - binary: app
    targets: [{targets}]
msis:
  - wxs: app.wxs
    version: v4
nsis:
  - script: app.nsi
dmgs:
  - {{}}
flatpaks:
  - app_id: org.example.App
"#
        ))
    };

    let ctx = TestContextBuilder::new()
        .crates(vec![installer_yaml(
            "x86_64-pc-windows-msvc, aarch64-apple-darwin",
        )])
        .build();
    let reqs = collect_requirements(&ctx, PreflightScope::Full);
    let tool = |reqs: &[SourcedRequirement], source: &str, name: &str| {
        reqs.iter().any(|r| {
            r.source == source
                && matches!(&r.requirement, EnvRequirement::Tool { name: n } if n == name)
        })
    };
    assert!(
        tool(&reqs, "stage:msi", "wix"),
        "windows target must demand the configured WiX v4 toolchain: {reqs:?}"
    );
    assert!(
        tool(&reqs, "stage:nsis", "makensis"),
        "windows target must demand makensis: {reqs:?}"
    );
    let dmg_ladder = reqs.iter().any(|r| {
        r.source == "stage:dmg"
            && matches!(
                &r.requirement,
                EnvRequirement::ToolAnyOf { names } if names.contains(&"hdiutil".to_string())
            )
    });
    assert!(
        dmg_ladder,
        "darwin target must demand the dmg tool ladder: {reqs:?}"
    );
    assert!(
        !reqs.iter().any(|r| r.source == "stage:flatpak"),
        "no linux target configured — flatpak must contribute nothing: {reqs:?}"
    );

    let ctx = TestContextBuilder::new()
        .crates(vec![installer_yaml("x86_64-unknown-linux-gnu")])
        .build();
    let reqs = collect_requirements(&ctx, PreflightScope::Full);
    for absent in ["stage:msi", "stage:nsis", "stage:dmg", "stage:pkg"] {
        assert!(
            !reqs.iter().any(|r| r.source == absent),
            "linux-only matrix must not demand {absent} tools: {reqs:?}"
        );
    }
    assert!(
        tool(&reqs, "stage:flatpak", "flatpak-builder"),
        "linux target must demand flatpak-builder: {reqs:?}"
    );
}

/// An active `notarize.macos` entry demands rcodesign plus the env
/// refs of its templated secret fields; the templated values
/// themselves never appear in the requirements.
#[test]
fn notarize_requirements_follow_active_entries() {
    use anodizer_core::config::{
        MacOSNotarizeApiConfig, MacOSSignConfig, MacOSSignNotarizeConfig, NotarizeConfig,
    };
    let mut ctx = TestContextBuilder::new().build();
    ctx.config.notarize = Some(NotarizeConfig {
        macos: Some(vec![MacOSSignNotarizeConfig {
            sign: Some(MacOSSignConfig {
                certificate: Some("{{ .Env.PF_P12_B64 }}".to_string()),
                password: Some("{{ .Env.PF_P12_PASSWORD }}".to_string()),
                ..Default::default()
            }),
            notarize: Some(MacOSNotarizeApiConfig {
                key: Some("{{ .Env.PF_ASC_KEY }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    });
    let reqs = collect_requirements(&ctx, PreflightScope::Full);
    assert!(
        reqs.iter().any(|r| {
            r.source == "stage:notarize"
                && matches!(&r.requirement, EnvRequirement::Tool { name } if name == "rcodesign")
        }),
        "active macos entry must demand rcodesign: {reqs:?}"
    );
    for var in ["PF_P12_B64", "PF_P12_PASSWORD", "PF_ASC_KEY"] {
        assert!(
            reqs.iter().any(|r| {
                r.source == "stage:notarize"
                    && matches!(
                        &r.requirement,
                        EnvRequirement::EnvAllOf { vars } if vars.contains(&var.to_string())
                    )
            }),
            "templated notarize field must demand {var}: {reqs:?}"
        );
    }
}

/// Announce credentials derive from the per-announcer env resolution:
/// SMTP_PASSWORD for an enabled email announcer, the SLACK_WEBHOOK
/// fallback for slack without a configured webhook_url, the env ref of
/// a templated webhook_url instead when one is set — and announce is
/// part of the publish-only scope (its pipeline runs the stage last).
#[test]
fn announce_requirements_derive_from_announcer_config() {
    use anodizer_core::config::{
        AnnounceConfig, EmailAnnounce, SlackAnnounce, StringOrBool, TelegramAnnounce,
    };
    let mut ctx = TestContextBuilder::new().build();
    ctx.config.announce = Some(AnnounceConfig {
        email: Some(EmailAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            host: Some("smtp.example.com".to_string()),
            username: Some("releases@example.com".to_string()),
            ..Default::default()
        }),
        slack: Some(SlackAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }),
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            bot_token: Some("{{ .Env.PF_TG_TOKEN }}".to_string()),
            ..Default::default()
        }),
        // Present but not enabled: must contribute nothing.
        reddit: Some(Default::default()),
        ..Default::default()
    });

    for scope in [PreflightScope::Full, PreflightScope::PublishOnly] {
        let reqs = collect_requirements(&ctx, scope);
        let announce_env = |var: &str| {
            reqs.iter().any(|r| {
                r.source == "stage:announce"
                    && matches!(
                        &r.requirement,
                        EnvRequirement::EnvAllOf { vars } if vars.contains(&var.to_string())
                    )
            })
        };
        assert!(
            announce_env("SMTP_PASSWORD"),
            "{scope:?}: enabled email announcer must demand SMTP_PASSWORD: {reqs:?}"
        );
        assert!(
            announce_env("SLACK_WEBHOOK"),
            "{scope:?}: slack without webhook_url must demand the fallback: {reqs:?}"
        );
        assert!(
            announce_env("PF_TG_TOKEN"),
            "{scope:?}: templated bot_token must demand its env ref: {reqs:?}"
        );
        assert!(
            !announce_env("TELEGRAM_TOKEN"),
            "{scope:?}: configured bot_token must not demand the fallback: {reqs:?}"
        );
        assert!(
            !announce_env("REDDIT_SECRET"),
            "{scope:?}: a present-but-disabled announcer must contribute nothing: {reqs:?}"
        );
    }

    // `--skip=announce` drops the whole surface.
    let mut skipped = TestContextBuilder::new()
        .skip_stages(vec!["announce".to_string()])
        .build();
    skipped.config.announce = ctx.config.announce.clone();
    let reqs = collect_requirements(&skipped, PreflightScope::Full);
    assert!(
        !reqs.iter().any(|r| r.source == "stage:announce"),
        "--skip=announce must drop announce requirements: {reqs:?}"
    );
}

/// `--publishers npm` must restrict the publish-only preflight to npm's
/// requirements alone: the github-hosted npm-provenance job carries only
/// NPM_TOKEN, so demanding cargo / chocolatey / etc. credentials —
/// publishers the allowlist deselected — falsely aborts the run.
#[test]
fn publishers_allowlist_restricts_publisher_requirements() {
    let top = crate_from_yaml(
        r#"
name: top
publish:
  cargo: {}
  scoop:
    repository: { owner: o, name: bucket }
"#,
    );
    let mut ctx = TestContextBuilder::new()
        .crates(vec![top])
        .publisher_allowlist(vec!["npm".to_string()])
        .build();
    ctx.config.npms = Some(vec![anodizer_core::config::NpmConfig::default()]);
    let reqs = collect_requirements(&ctx, PreflightScope::PublishOnly);

    let publisher_sources: Vec<&str> = reqs
        .iter()
        .map(|r| r.source.as_str())
        .filter(|s| s.starts_with("publish:"))
        .collect();
    assert!(
        !publisher_sources.is_empty(),
        "the npm publisher must still contribute its own requirements: {reqs:?}"
    );
    assert!(
        publisher_sources.iter().all(|s| *s == "publish:npm"),
        "--publishers npm must yield only npm publisher requirements: {publisher_sources:?}"
    );
    for absent in ["publish:cargo", "publish:scoop", "publish:github-release"] {
        assert!(
            !reqs.iter().any(|r| r.source == absent),
            "deselected publisher {absent} must contribute no requirements: {reqs:?}"
        );
    }
}

/// The github-hosted npm-provenance job runs
/// `release --publish-only --publishers npm` with NO `--skip`: the
/// `--publishers npm` allowlist alone must deselect every non-npm surface.
/// That is every OTHER publisher PLUS the stages that self-skip at runtime
/// on publisher deselection —
///
/// - publisher-named stages — blob / snapcraft-publish / docker /
///   docker-sign / announce / **release** (github-release is a real
///   publisher, so the release stage self-skips when it is deselected);
/// - BOTH slices of the `sign` stage — their only consumers are
///   github-release / blob / artifactory / uploads, ALL deselected here, so
///   the stage skips both signature loops and demands no cosign/GPG.
#[test]
fn publishers_allowlist_deselects_self_skipping_stages() {
    let top = crate_from_yaml(
        r#"
name: top
release: { github: { owner: o, name: r } }
blobs:
  - provider: s3
    bucket: releases
    endpoint: "https://minio.example.com"
snapcrafts:
  - publish: true
publish:
  cargo: {}
"#,
    );
    let mut ctx = TestContextBuilder::new()
        .crates(vec![top])
        .signs(vec![anodizer_core::config::SignConfig {
            artifacts: Some("all".to_string()),
            cmd: Some("cosign".to_string()),
            ..Default::default()
        }])
        // binary_signs hangs off the same consumer set as signs, so the
        // npm-only allowlist deselects it too.
        .binary_signs(vec![anodizer_core::config::SignConfig {
            artifacts: Some("all".to_string()),
            cmd: Some("cosign".to_string()),
            ..Default::default()
        }])
        .publisher_allowlist(vec!["npm".to_string()])
        .build();
    ctx.config.npms = Some(vec![anodizer_core::config::NpmConfig::default()]);
    ctx.config.announce = Some({
        use anodizer_core::config::{AnnounceConfig, SlackAnnounce, StringOrBool};
        AnnounceConfig {
            slack: Some(SlackAnnounce {
                enabled: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }),
            ..Default::default()
        }
    });

    let reqs = collect_requirements(&ctx, PreflightScope::PublishOnly);
    let has = |source: &str| reqs.iter().any(|r| r.source == source);
    // Count the `stage:sign` cosign tool demands. Both slices use cosign,
    // so the COUNT distinguishes "neither slice ran" (0), "one slice ran"
    // (1), and "both ran" (2).
    let cosign_sign_reqs = reqs
        .iter()
        .filter(|r| {
            r.source == "stage:sign"
                && matches!(&r.requirement, EnvRequirement::Tool { name } if name == "cosign")
        })
        .count();

    assert!(
        has("publish:npm"),
        "npm is selected — it must still contribute its requirements: {reqs:?}"
    );
    for absent in [
        "stage:blob",
        "stage:snapcraft-publish",
        "stage:docker",
        "stage:docker-sign",
        "stage:announce",
        "stage:release",
    ] {
        assert!(
            !has(absent),
            "allowlist-deselected stage {absent} must contribute no requirements: {reqs:?}"
        );
    }
    // PublishOnly scope: every signature consumer is deselected by the
    // npm-only allowlist, so BOTH slices self-skip and ZERO cosign demands
    // survive — this is the npm-job's clean surface.
    assert_eq!(
        cosign_sign_reqs, 0,
        "under --publish-only --publishers npm BOTH sign slices must self-skip \
         (every signature consumer is deselected): {reqs:?}"
    );

    // Selecting any ONE signature consumer (here: github-release) revives
    // both slices — a binary signature uploads to that release too — so
    // TWO cosign demands return.
    let mut keep_one = TestContextBuilder::new()
        .crates(ctx.config.crates.clone())
        .signs(vec![anodizer_core::config::SignConfig {
            artifacts: Some("all".to_string()),
            cmd: Some("cosign".to_string()),
            ..Default::default()
        }])
        .binary_signs(vec![anodizer_core::config::SignConfig {
            artifacts: Some("all".to_string()),
            cmd: Some("cosign".to_string()),
            ..Default::default()
        }])
        .publisher_allowlist(vec!["npm".to_string(), "github-release".to_string()])
        .build();
    keep_one.config.npms = ctx.config.npms.clone();
    let reqs = collect_requirements(&keep_one, PreflightScope::PublishOnly);
    assert!(
        reqs.iter().any(|r| r.source == "stage:release"),
        "selecting github-release must keep the release requirement: {reqs:?}"
    );
    let cosign_sign_reqs = reqs
        .iter()
        .filter(|r| {
            r.source == "stage:sign"
                && matches!(&r.requirement, EnvRequirement::Tool { name } if name == "cosign")
        })
        .count();
    assert_eq!(
        cosign_sign_reqs, 2,
        "selecting a signature consumer must revive BOTH sign slices, \
         so two cosign demands: {reqs:?}"
    );
}

/// FULL scope (the main release job's shape, empty allowlist) keeps BOTH
/// sign slices, so the binaries that ship are still signed. Pins the
/// main-job binary-signing invariant alongside the deselection case above.
#[test]
fn full_scope_keeps_both_sign_slices() {
    let top = crate_from_yaml(
        r#"
name: top
release: { github: { owner: o, name: r } }
publish:
  cargo: {}
"#,
    );
    let ctx = TestContextBuilder::new()
        .crates(vec![top])
        .signs(vec![anodizer_core::config::SignConfig {
            artifacts: Some("all".to_string()),
            cmd: Some("cosign".to_string()),
            ..Default::default()
        }])
        .binary_signs(vec![anodizer_core::config::SignConfig {
            artifacts: Some("all".to_string()),
            cmd: Some("cosign".to_string()),
            ..Default::default()
        }])
        .build();
    let reqs = collect_requirements(&ctx, PreflightScope::Full);
    let cosign_sign_reqs = reqs
        .iter()
        .filter(|r| {
            r.source == "stage:sign"
                && matches!(&r.requirement, EnvRequirement::Tool { name } if name == "cosign")
        })
        .count();
    assert_eq!(
        cosign_sign_reqs, 2,
        "full scope must keep BOTH sign slices (signs: + binary_signs:): {reqs:?}"
    );
}

/// The main release job runs with an EMPTY allowlist + `--skip=npm`, so
/// neither the release stage nor the `signs:` slice may be deselected:
/// `publisher_deselected` short-circuits to the denylist alone, which names
/// only `npm`. This pins the non-regression of the main job alongside the
/// npm-job deselection above.
#[test]
fn empty_allowlist_keeps_release_and_signs_for_main_job() {
    let top = crate_from_yaml(
        r#"
name: top
release: { github: { owner: o, name: r } }
publish:
  cargo: {}
"#,
    );
    let mut ctx = TestContextBuilder::new()
        .crates(vec![top])
        .signs(vec![anodizer_core::config::SignConfig {
            artifacts: Some("all".to_string()),
            cmd: Some("cosign".to_string()),
            ..Default::default()
        }])
        .skip_stages(vec!["npm".to_string()])
        .build();
    ctx.config.npms = Some(vec![anodizer_core::config::NpmConfig::default()]);
    let reqs = collect_requirements(&ctx, PreflightScope::PublishOnly);
    let has = |source: &str| reqs.iter().any(|r| r.source == source);

    assert!(
        has("stage:release"),
        "empty allowlist must keep the release requirement (main job): {reqs:?}"
    );
    assert!(
        reqs.iter().any(|r| {
            r.source == "stage:sign"
                && matches!(&r.requirement, EnvRequirement::Tool { name } if name == "cosign")
        }),
        "empty allowlist must keep the signs cosign demand (main job): {reqs:?}"
    );
    assert!(
        !reqs.iter().any(|r| r.source == "publish:npm"),
        "--skip=npm must still drop npm: {reqs:?}"
    );
}

/// The symmetric denylist case: `--skip=npm` drops npm's requirements
/// while every other configured publisher keeps contributing.
#[test]
fn skip_publisher_drops_only_that_publisher() {
    let top = crate_from_yaml(
        r#"
name: top
publish:
  scoop:
    repository: { owner: o, name: bucket }
"#,
    );
    let mut ctx = TestContextBuilder::new()
        .crates(vec![top])
        .skip_stages(vec!["npm".to_string()])
        .build();
    ctx.config.npms = Some(vec![anodizer_core::config::NpmConfig::default()]);
    let reqs = collect_requirements(&ctx, PreflightScope::PublishOnly);

    assert!(
        !reqs.iter().any(|r| r.source == "publish:npm"),
        "--skip=npm must drop npm requirements: {reqs:?}"
    );
    assert!(
        reqs.iter().any(|r| r.source == "publish:scoop"),
        "--skip=npm must keep other publishers' requirements: {reqs:?}"
    );
}

/// The announce-only scope collects the announce surface and nothing
/// else, even when builds and publishers are configured: announcers
/// are the only side effects `--announce-only` can produce, so their
/// secrets are the only thing its preflight may demand.
#[test]
fn announce_only_scope_collects_announce_requirements_alone() {
    use anodizer_core::config::{AnnounceConfig, StringOrBool, TelegramAnnounce};
    let top = crate_from_yaml(
        r#"
name: top
publish:
  scoop:
    repository: { owner: o, name: bucket }
"#,
    );
    let mut ctx = TestContextBuilder::new().crates(vec![top]).build();
    ctx.config.announce = Some(AnnounceConfig {
        telegram: Some(TelegramAnnounce {
            enabled: Some(StringOrBool::Bool(true)),
            ..Default::default()
        }),
        ..Default::default()
    });

    let reqs = collect_requirements(&ctx, PreflightScope::AnnounceOnly);
    assert!(
        reqs.iter().all(|r| r.source == "stage:announce"),
        "announce-only scope must collect only announce requirements: {reqs:?}"
    );
    assert!(
        reqs.iter().any(|r| matches!(
            &r.requirement,
            EnvRequirement::EnvAllOf { vars } if vars.contains(&"TELEGRAM_TOKEN".to_string())
        )),
        "enabled telegram announcer must demand its token: {reqs:?}"
    );
}

/// The build block reports the resolved cross toolchain (not just
/// `cargo`): a crate cross-compiling to a non-host glibc-Linux target under
/// the default `auto` strategy must surface `cargo-zigbuild` + `zig` so
/// `anodizer tools` tells the runner what to install. Skipped on non
/// x86_64-linux-gnu hosts, where Auto routing differs.
#[test]
fn build_block_reports_cross_toolchain_for_cross_target() {
    if anodizer_core::partial::detect_host_target()
        .as_deref()
        .unwrap_or_default()
        != "x86_64-unknown-linux-gnu"
    {
        return;
    }
    let krate = crate_from_yaml(
        r#"
name: app
builds:
  - binary: app
    targets: [aarch64-unknown-linux-gnu]
"#,
    );
    let ctx = TestContextBuilder::new().crates(vec![krate]).build();
    let full = collect_requirements(&ctx, PreflightScope::Full);
    let build_tools: Vec<&str> = full
        .iter()
        .filter(|r| r.source == "stage:build")
        .filter_map(|r| match &r.requirement {
            EnvRequirement::Tool { name } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    // cargo stays first; the cross toolchain is appended.
    assert_eq!(build_tools.first(), Some(&"cargo"));
    assert!(
        build_tools.contains(&"cargo-zigbuild") && build_tools.contains(&"zig"),
        "build block must report cargo-zigbuild + zig for a cross target: {build_tools:?}"
    );

    // cargo is HARD; the cross toolchain is ADVISORY (the build degrades
    // gracefully without it). A missing zig must warn, never block a
    // release — the regression that aborted preflight on any box without it.
    let advisory_of = |name: &str| -> bool {
        full.iter()
            .find(|r| {
                r.source == "stage:build"
                    && matches!(&r.requirement, EnvRequirement::Tool { name: n } if n == name)
            })
            .map(|r| r.advisory)
            .unwrap_or_else(|| panic!("missing build tool {name}"))
    };
    assert!(!advisory_of("cargo"), "cargo must stay hard-required");
    assert!(advisory_of("zig"), "zig must be advisory");
    assert!(
        advisory_of("cargo-zigbuild"),
        "cargo-zigbuild must be advisory"
    );

    // Gate behaviour: cargo present, cross toolchain absent ⇒ the report is
    // OK (no failures) with the cross tools demoted to warnings.
    let report = anodizer_core::env_preflight::evaluate(
        &full,
        &|_| None,
        &anodizer_core::env_preflight::EnvProbes {
            tool: &|name| name == "cargo",
            endpoint: &|_| Ok(()),
            docker: &|| true,
        },
    );
    assert!(
        report.ok(),
        "release must not be blocked by a missing cross toolchain: {report}"
    );
    assert!(
        report.warnings.iter().any(|w| w.message.contains("zig")),
        "missing zig must surface as a warning: {:?}",
        report.warnings
    );
}

/// A `signs:` cosign config keyed off `env://COSIGN_KEY` must surface as a
/// distinct `env://COSIGN_KEY` ref for the offline load verification, and
/// duplicate refs (cosign declared on two crates) collapse to one.
#[test]
fn cosign_key_refs_reconstructs_env_scheme_and_dedups() {
    let reqs = vec![
        SourcedRequirement::new(
            "stage:sign",
            EnvRequirement::KeyEnv {
                kind: anodizer_core::KeyKind::Cosign,
                var: "COSIGN_KEY".to_string(),
            },
        ),
        SourcedRequirement::new(
            "stage:sign",
            EnvRequirement::KeyEnv {
                kind: anodizer_core::KeyKind::Cosign,
                var: "COSIGN_KEY".to_string(),
            },
        ),
        // A non-cosign key kind must NOT be picked up by the cosign verify.
        SourcedRequirement::new(
            "publish:aur",
            EnvRequirement::KeyEnv {
                kind: anodizer_core::KeyKind::SshPrivate,
                var: "AUR_SSH_KEY".to_string(),
            },
        ),
    ];
    assert_eq!(cosign_key_refs(&reqs), vec!["env://COSIGN_KEY".to_string()]);
}

/// No cosign key in the requirement set ⇒ nothing to verify ⇒ no WARN, no
/// error, returns `true` (the gate stays clean for configs without cosign).
#[test]
fn verify_cosign_keys_load_noop_without_cosign_config() {
    let reqs = vec![SourcedRequirement::new(
        "stage:build",
        EnvRequirement::Tool {
            name: "cargo".to_string(),
        },
    )];
    let (log, capture) = StageLogger::with_capture("preflight", Verbosity::Normal);
    assert!(verify_cosign_keys_load(&reqs, &log));
    assert!(
        capture.all_messages().is_empty(),
        "no cosign requirement must emit nothing: {:?}",
        capture.all_messages()
    );
}

/// When cosign is NOT on PATH, an active cosign-key config must WARN (load
/// verification deferred to sign time) and NOT hard-fail. The absent outcome
/// is injected via the load-resolver hook so the WARN branch runs on every
/// shard — including CI shards that DO carry cosign — rather than self-skipping.
#[test]
fn verify_cosign_keys_load_warns_when_cosign_absent() {
    use anodizer_core::log::LogLevel;
    let reqs = vec![SourcedRequirement::new(
        "stage:sign",
        EnvRequirement::KeyEnv {
            kind: anodizer_core::KeyKind::Cosign,
            var: "COSIGN_KEY".to_string(),
        },
    )];
    let (log, capture) = StageLogger::with_capture("preflight", Verbosity::Normal);
    // Force the cosign-absent outcome deterministically, independent of PATH.
    let all_loaded = verify_cosign_keys_load_with(&reqs, &log, |_| {
        anodizer_stage_sign::CosignKeyLoad::CosignUnavailable
    });
    // cosign-absent is NOT a failure.
    assert!(all_loaded, "cosign-absent must not fail the gate");
    let msgs = capture.all_messages();
    assert!(
        msgs.iter()
            .any(|(lvl, m)| *lvl == LogLevel::Warn && m.contains("cosign not installed")),
        "cosign-absent must WARN: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|(lvl, _)| *lvl == LogLevel::Error),
        "cosign-absent must NOT emit an error: {msgs:?}"
    );
}

/// A genuinely bad key (cosign installed, load fails) must FAIL the gate
/// and emit an Error. Injected via the hook so it runs deterministically
/// regardless of PATH.
#[test]
fn verify_cosign_keys_load_fails_on_bad_key() {
    use anodizer_core::log::LogLevel;
    let reqs = vec![SourcedRequirement::new(
        "stage:sign",
        EnvRequirement::KeyEnv {
            kind: anodizer_core::KeyKind::Cosign,
            var: "COSIGN_KEY".to_string(),
        },
    )];
    let (log, capture) = StageLogger::with_capture("preflight", Verbosity::Normal);
    let all_loaded = verify_cosign_keys_load_with(&reqs, &log, |_| {
        anodizer_stage_sign::CosignKeyLoad::Failed("wrong COSIGN_PASSWORD".to_string())
    });
    assert!(!all_loaded, "a failing key load must fail the gate");
    let msgs = capture.all_messages();
    assert!(
        msgs.iter()
            .any(|(lvl, m)| *lvl == LogLevel::Error && m.contains("failed to load")),
        "a bad key must emit an Error: {msgs:?}"
    );
}

/// gpg cmd extraction: only sign/docker-sign-sourced `Tool` requirements
/// that classify as gpg, deduped across configs sharing one cmd; cosign
/// and non-sign sources never contribute.
#[test]
fn gpg_sign_cmds_filters_and_dedupes() {
    let tool = |source: &str, name: &str| {
        SourcedRequirement::new(
            source,
            EnvRequirement::Tool {
                name: name.to_string(),
            },
        )
    };
    let reqs = vec![
        tool("stage:sign", "gpg"),
        tool("stage:sign", "gpg"),
        tool("stage:sign", "cosign"),
        tool("stage:docker-sign", "/usr/bin/gpg2"),
        tool("stage:build", "gpg"),
    ];
    assert_eq!(
        gpg_sign_cmds(&reqs),
        vec!["gpg".to_string(), "/usr/bin/gpg2".to_string()]
    );
}

/// No gpg sign cmd in the requirement set ⇒ nothing to probe ⇒ no
/// output, returns `true`.
#[test]
fn verify_gpg_faked_system_time_noop_without_gpg_config() {
    let reqs = vec![SourcedRequirement::new(
        "stage:sign",
        EnvRequirement::Tool {
            name: "cosign".to_string(),
        },
    )];
    let (log, capture) = StageLogger::with_capture("preflight", Verbosity::Normal);
    assert!(verify_gpg_faked_system_time_with(&reqs, true, &log, |_| {
        panic!("no gpg cmd must mean no probe")
    }));
    assert!(
        capture.all_messages().is_empty(),
        "no gpg requirement must emit nothing: {:?}",
        capture.all_messages()
    );
}

/// A failing probe with SOURCE_DATE_EPOCH UNSET must WARN (latent
/// incompatibility) and NOT fail the gate.
#[test]
fn verify_gpg_faked_system_time_warns_without_sde() {
    use anodizer_core::log::LogLevel;
    let reqs = vec![SourcedRequirement::new(
        "stage:sign",
        EnvRequirement::Tool {
            name: "gpg".to_string(),
        },
    )];
    let (log, capture) = StageLogger::with_capture("preflight", Verbosity::Normal);
    let ok = verify_gpg_faked_system_time_with(&reqs, false, &log, |_| false);
    assert!(ok, "flag-unsupported without SDE must not fail the gate");
    let msgs = capture.all_messages();
    assert!(
        msgs.iter().any(|(lvl, m)| *lvl == LogLevel::Warn
            && m.contains("--faked-system-time")
            && m.contains("SOURCE_DATE_EPOCH")),
        "must WARN naming the flag and the consequence: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|(lvl, _)| *lvl == LogLevel::Error),
        "must NOT emit an error without SDE: {msgs:?}"
    );
}

/// A failing probe with SOURCE_DATE_EPOCH SET must FAIL the gate and
/// emit an Error — the sign stage WILL inject the flag this run.
#[test]
fn verify_gpg_faked_system_time_blocks_with_sde() {
    use anodizer_core::log::LogLevel;
    let reqs = vec![SourcedRequirement::new(
        "stage:sign",
        EnvRequirement::Tool {
            name: "gpg2".to_string(),
        },
    )];
    let (log, capture) = StageLogger::with_capture("preflight", Verbosity::Normal);
    let ok = verify_gpg_faked_system_time_with(&reqs, true, &log, |_| false);
    assert!(!ok, "flag-unsupported with SDE set must fail the gate");
    let msgs = capture.all_messages();
    assert!(
        msgs.iter()
            .any(|(lvl, m)| *lvl == LogLevel::Error
                && m.contains("gpg2 rejects --faked-system-time")),
        "must emit an Error naming the cmd: {msgs:?}"
    );
}

/// A passing probe emits the per-cmd status line and keeps the gate
/// clean — for a bare `gpg` and an absolute-path cmd alike.
#[test]
fn verify_gpg_faked_system_time_passes_on_supported_gpg() {
    use anodizer_core::log::LogLevel;
    let reqs = vec![SourcedRequirement::new(
        "stage:sign",
        EnvRequirement::Tool {
            name: "/usr/bin/gpg".to_string(),
        },
    )];
    let (log, capture) = StageLogger::with_capture("preflight", Verbosity::Normal);
    assert!(verify_gpg_faked_system_time_with(&reqs, true, &log, |_| {
        true
    }));
    let msgs = capture.all_messages();
    assert!(
        msgs.iter().any(|(lvl, m)| *lvl == LogLevel::Status
            && m.contains("/usr/bin/gpg accepts --faked-system-time")),
        "supported gpg must emit the status result: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|(lvl, _)| matches!(*lvl, LogLevel::Warn | LogLevel::Error)),
        "supported gpg must not warn or error: {msgs:?}"
    );
}

/// Reduced-scope membership is IN LOCKSTEP with the pipeline builders:
/// the publish-only scope admits exactly the stages
/// `build_publish_only_pipeline` assembles (so a stage added to that
/// pipeline is preflighted automatically) and never the from-scratch
/// artifact producers; announce-only mirrors `build_announce_pipeline`.
#[test]
fn reduced_scope_membership_matches_pipeline_builders() {
    let to_set = |p: crate::pipeline::Pipeline| -> std::collections::BTreeSet<String> {
        p.stage_names().iter().map(|s| s.to_string()).collect()
    };

    let publish_only = PreflightScope::PublishOnly
        .included_stage_set()
        .expect("publish-only is a reduced scope");
    assert_eq!(
        publish_only,
        to_set(crate::pipeline::build_publish_only_pipeline()),
        "publish-only scope drifted from build_publish_only_pipeline"
    );
    for producer in [
        "build",
        "nfpm",
        "srpm",
        "sbom",
        "makeself",
        "install-script",
        "upx",
        "appimage",
    ] {
        assert!(
            !publish_only.contains(producer),
            "publish-only scope must not preflight the from-scratch producer '{producer}'"
        );
    }

    let announce_only = PreflightScope::AnnounceOnly
        .included_stage_set()
        .expect("announce-only is a reduced scope");
    assert_eq!(
        announce_only,
        to_set(crate::pipeline::build_announce_pipeline()),
        "announce-only scope drifted from build_announce_pipeline"
    );

    assert!(
        PreflightScope::Full.included_stage_set().is_none(),
        "full scope must admit every stage"
    );
}

/// Every stage name `collect_requirements` gates on must be a real stage
/// registered in the full release pipeline — a typo'd or renamed stage
/// name would silently drop that stage's requirements from every scope.
/// Iterates the PRODUCTION `GATED_STAGES` table (the same list the
/// collection loop walks), so a stage added to one is covered by the
/// other by construction.
#[test]
fn requirement_gated_stage_names_exist_in_release_pipeline() {
    let release = crate::pipeline::build_release_pipeline();
    let names = release.stage_names();
    for gated in GATED_STAGES {
        assert!(
            names.contains(&gated.stage),
            "collect_requirements gates on '{}' but the release pipeline has no such stage: {names:?}",
            gated.stage
        );
    }
    // The table must stay one row per stage name (aside from sources a
    // row emits itself) — a duplicate row would double-collect.
    let mut seen = std::collections::BTreeSet::new();
    for gated in GATED_STAGES {
        assert!(
            seen.insert(gated.stage),
            "duplicate GATED_STAGES row for '{}'",
            gated.stage
        );
    }
}

/// A repo whose history is `1 + commits_after` commits with `tag` on the
/// FIRST one. `commits_after == 0` leaves the tag at HEAD. Returns `None`
/// when git is unusable on this host.
fn repo_with_tag(tag: &str, commits_after: usize) -> Option<tempfile::TempDir> {
    use anodizer_core::test_helpers::{git_test_ok, git_test_output};
    let tmp = tempfile::tempdir().ok()?;
    let dir = tmp.path();
    if !git_test_output(dir, &["init", "-q"]).status.success() {
        return None;
    }
    git_test_ok(dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
    git_test_ok(dir, &["tag", tag]);
    for i in 0..commits_after {
        git_test_ok(
            dir,
            &["commit", "-q", "--allow-empty", "-m", &format!("after-{i}")],
        );
    }
    Some(tmp)
}

fn sweep_for(tag: &str, repo: &std::path::Path) -> ReconcileSweep {
    sweep_for_source(tag, repo, TagSource::Inferred)
}

fn sweep_for_source(tag: &str, repo: &std::path::Path, source: TagSource) -> ReconcileSweep {
    let ctx = anodizer_core::test_helpers::TestContextBuilder::new()
        .tag(tag)
        .tag_source(source)
        .project_root(repo.to_path_buf())
        .build();
    reconcile_sweep(&ctx, anodizer_core::test_helpers::test_logger())
}

/// Between two releases the resolved tag is the LAST released one, so
/// every publisher probe describes a version nobody is about to publish.
/// The sweep must not run at all — and the report that replaces it must
/// clear the exit gate, since a required `Diverged` on the previous
/// version is true and irrelevant to the version this tree will cut.
#[test]
fn reconcile_sweep_skips_a_released_tag_head_has_advanced_past() {
    let Some(repo) = repo_with_tag("v0.22.2", 1) else {
        eprintln!("skipping: git unusable on this host");
        return;
    };
    let sweep = sweep_for("v0.22.2", repo.path());
    let ReconcileSweep::Stale { reason } = sweep else {
        panic!("expected the sweep to be skipped, got {sweep:?}");
    };
    assert!(
        reason.contains("v0.22.2") && reason.contains("advanced past it"),
        "the skip must name the version and the reason: {reason}"
    );
    // The exit gate reads `blocking().len()`; a skipped sweep contributes
    // nothing to it, so the command exits 0 on the publisher axis.
    assert!(ReconcileReport::skipped(reason).blocking().is_empty());
}

/// The resume / backfill case: HEAD sits exactly on the tag, so the
/// version resolved IS the version this run would publish and the sweep
/// must run rather than be skipped as stale.
#[test]
fn reconcile_sweep_applies_when_head_is_at_the_tag() {
    let Some(repo) = repo_with_tag("v0.22.2", 0) else {
        eprintln!("skipping: git unusable on this host");
        return;
    };
    assert_eq!(sweep_for("v0.22.2", repo.path()), ReconcileSweep::Applies);
}

/// An operator-declared tag names the version this run targets, so its
/// position relative to HEAD carries no information about staleness. A
/// backfill canary — `ANODIZER_CURRENT_TAG` set to an already-released
/// version, run from a tree many commits ahead of it — is exactly the tree
/// shape the staleness inference skips, and exactly the run whose whole
/// purpose is to probe that version.
#[test]
fn reconcile_sweep_applies_to_a_declared_tag_head_has_advanced_past() {
    let Some(repo) = repo_with_tag("v0.20.0", 3) else {
        eprintln!("skipping: git unusable on this host");
        return;
    };
    assert!(
        matches!(
            sweep_for("v0.20.0", repo.path()),
            ReconcileSweep::Stale { .. }
        ),
        "the same tree must read as stale when the tag was INFERRED"
    );
    assert_eq!(
        sweep_for_source("v0.20.0", repo.path(), TagSource::Declared),
        ReconcileSweep::Applies,
        "a declared tag must never be inferred stale"
    );
}

/// A released tag that HEAD is not on the history of — an older checkout,
/// a divergent branch — is just as unpublishable from here, so the sweep
/// is skipped; the reason must not claim HEAD advanced past it.
#[test]
fn reconcile_sweep_skips_a_tag_off_heads_history_without_claiming_advancement() {
    let Some(repo) = repo_with_tag("v0.22.2", 1) else {
        eprintln!("skipping: git unusable on this host");
        return;
    };
    anodizer_core::test_helpers::git_test_ok(repo.path(), &["tag", "v0.23.0"]);
    anodizer_core::test_helpers::git_test_ok(repo.path(), &["reset", "--hard", "-q", "HEAD~1"]);

    let sweep = sweep_for("v0.23.0", repo.path());
    let ReconcileSweep::Stale { reason } = sweep else {
        panic!("expected the sweep to be skipped, got {sweep:?}");
    };
    assert!(
        reason.contains("HEAD is not on its history"),
        "the reason must describe the real position: {reason}"
    );
    assert!(
        !reason.contains("advanced past"),
        "must not claim HEAD advanced past a tag it is behind: {reason}"
    );
}

/// A tag that does not exist yet is the ordinary fresh-version case: the
/// probe runs, and every publisher answers `absent — will publish`.
#[test]
fn reconcile_sweep_applies_when_the_tag_does_not_exist() {
    let Some(repo) = repo_with_tag("v0.22.2", 1) else {
        eprintln!("skipping: git unusable on this host");
        return;
    };
    assert_eq!(sweep_for("v0.23.0", repo.path()), ReconcileSweep::Applies);
}
/// The preflight is one engine, run from exactly two places. Every
/// production function calling either half (`run_env_preflight(` or
/// `run_preflight(` in any spelling of its path) is `run_engine`,
/// and every production caller of `run_engine(` is the standalone
/// `preflight::run` or `release::run::run`. A third caller, or a command
/// that runs one half on its own, fails here until it goes through the
/// engine. Rule: `.claude/rules/preflight-one-engine.md`.
#[test]
fn every_preflight_half_runs_inside_the_one_engine() {
    use anodizer_core::test_helpers::test_sources::{
        function_bodies, production_half, workspace_production_sources,
    };

    /// `run_preflight` as a whole word, however the path is spelled: a
    /// direct call, an `as` alias or a `use …::run_preflight;` import all
    /// count. `should_run_preflight(` is a different word, and stage-blob's
    /// `crate::preflight::run_preflight(` is that crate's own per-publisher
    /// probe, called from its `Publisher::preflight`.
    fn calls_publisher_half(rest: &str) -> bool {
        rest.match_indices("run_preflight").any(|(at, m)| {
            let before = rest[..at].chars().next_back();
            let after = &rest[at + m.len()..];
            let word_char = |c: char| c.is_alphanumeric() || c == '_';
            let whole = !before.is_some_and(word_char)
                && (after.starts_with('(')
                    || after.starts_with(" as ")
                    || after.starts_with(';')
                    || after.starts_with(',')
                    || after.starts_with('}'));
            whole && !rest[..at].ends_with("crate::preflight::")
        })
    }

    fn fn_name(body: &str) -> &str {
        let header = body.lines().next().unwrap_or_default();
        let after = &header[header.find("fn ").map(|i| i + 3).unwrap_or(0)..];
        after
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .next()
            .unwrap_or_default()
    }

    let mut half_callers: Vec<(String, String)> = Vec::new();
    let mut engine_callers: Vec<(String, String)> = Vec::new();
    for source in workspace_production_sources() {
        let text = std::fs::read_to_string(&source).expect("read source");
        let file = source.to_string_lossy().replace('\\', "/");
        for body in function_bodies(production_half(&text)) {
            let name = fn_name(&body).to_string();
            // The header line only defines the function.
            let rest: String = body.lines().skip(1).collect::<Vec<_>>().join("\n");
            if rest.contains("run_env_preflight(") || calls_publisher_half(&rest) {
                half_callers.push((file.clone(), name.clone()));
            }
            if rest.contains("run_engine(") {
                engine_callers.push((file.clone(), name));
            }
        }
    }

    let engine_only: Vec<&(String, String)> = half_callers
        .iter()
        .filter(|(_, name)| name != "run_engine")
        .collect();
    assert!(
        engine_only.is_empty(),
        "a preflight half is run outside run_engine: {engine_only:?}"
    );
    assert_eq!(
        half_callers.len(),
        1,
        "exactly one function runs the halves: {half_callers:?}"
    );

    let mut sites: Vec<String> = engine_callers
        .iter()
        .map(|(file, name)| {
            let tail = file.rsplit("/src/").next().unwrap_or(file);
            format!("{tail}::{name}")
        })
        .collect();
    sites.sort();
    assert_eq!(
        sites,
        vec![
            "commands/preflight/standalone.rs::run".to_string(),
            "commands/release/run.rs::run".to_string(),
        ],
        "run_engine has exactly two callers, the standalone command and release"
    );
}
