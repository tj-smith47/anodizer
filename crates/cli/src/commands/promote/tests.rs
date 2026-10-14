use super::*;
use anodizer_core::config::{Config, CrateConfig, NpmConfig, SnapcraftConfig, WorkspaceConfig};
use anodizer_core::context::ContextOptions;
use anodizer_core::promote::{PromoteStatus, dispatch_promotions};

/// A crate carrying a publishable snapcraft config.
fn snap_crate(name: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        snapcrafts: Some(vec![SnapcraftConfig {
            name: Some(name.to_string()),
            publish: Some(true),
            ..Default::default()
        }]),
        ..Default::default()
    }
}

/// Single-crate mode: one top-level crate with a snapcraft config.
fn ctx_single_crate() -> Context {
    let config = Config {
        project_name: "solo".to_string(),
        crates: vec![snap_crate("solo")],
        ..Default::default()
    };
    Context::new(config, ContextOptions::default())
}

/// Workspace mode (lockstep / per-crate share this shape): snapcraft config on a
/// crate declared only under `workspaces[].crates`, reached via the crate
/// universe.
fn ctx_workspace() -> Context {
    let config = Config {
        project_name: "ws".to_string(),
        workspaces: Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![snap_crate("member-a"), snap_crate("member-b")],
            ..Default::default()
        }]),
        ..Default::default()
    };
    Context::new(config, ContextOptions::default())
}

/// No snapcraft anywhere — a promotion-capable publisher is not configured.
fn ctx_no_snap() -> Context {
    let config = Config {
        project_name: "bare".to_string(),
        crates: vec![CrateConfig {
            name: "bare".to_string(),
            path: ".".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    Context::new(config, ContextOptions::default())
}

#[test]
fn default_selection_includes_configured_snapcraft_single_crate() {
    let ctx = ctx_single_crate();
    let selected = select_publishers(&ctx, &[]).expect("selection");
    let names: Vec<&str> = selected.iter().map(|p| p.name()).collect();
    assert_eq!(names, vec!["snapcraft"]);
}

#[test]
fn default_selection_includes_configured_snapcraft_workspace_mode() {
    // The crate-universe union must include a snapcraft config declared only
    // under workspaces[].crates — promotion works in per-crate / lockstep modes.
    let ctx = ctx_workspace();
    let selected = select_publishers(&ctx, &[]).expect("selection");
    let names: Vec<&str> = selected.iter().map(|p| p.name()).collect();
    assert_eq!(names, vec!["snapcraft"]);
}

#[test]
fn default_selection_empty_when_no_promotable_configured() {
    let ctx = ctx_no_snap();
    let selected = select_publishers(&ctx, &[]).expect("selection");
    assert!(selected.is_empty());
}

#[test]
fn explicit_configured_capable_publisher_is_selected() {
    let ctx = ctx_single_crate();
    let selected = select_publishers(&ctx, &["snapcraft".to_string()]).expect("selection");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name(), "snapcraft");
}

#[test]
fn explicit_not_capable_publisher_errors() {
    let ctx = ctx_single_crate();
    let err = select_publishers(&ctx, &["cargo".to_string()])
        .err()
        .expect("expected selection error")
        .to_string();
    assert!(
        err.contains("does not support promotion"),
        "expected capability error, got: {err}"
    );
}

#[test]
fn npm_is_capable_but_unconfigured_here_errors_as_not_configured() {
    // npm is promotion-capable, but this project configures only snapcraft, so
    // naming npm is the "capable but not configured" error, not "incapable".
    let ctx = ctx_single_crate();
    let err = select_publishers(&ctx, &["npm".to_string()])
        .err()
        .expect("expected selection error")
        .to_string();
    assert!(err.contains("not configured"), "got: {err}");
}

#[test]
fn npm_workspace_level_block_contributes_promoter() {
    // `npms:` is a workspace-level block; a configured npm publisher must yield
    // an npm promoter in every config mode.
    let config = Config {
        project_name: "solo".to_string(),
        crates: vec![snap_crate("solo")],
        npms: Some(vec![NpmConfig {
            name: Some("solo".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let ctx = Context::new(config, ContextOptions::default());
    let selected = select_publishers(&ctx, &["npm".to_string()]).expect("selection");
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name(), "npm");
}

#[test]
fn docker_and_github_per_crate_blocks_contribute_promoters() {
    use anodizer_core::config::{DockerV2Config, ReleaseConfig, ScmRepoConfig};

    // Per-crate mode: docker + github release configured on a workspace member.
    let config = Config {
        project_name: "ws".to_string(),
        workspaces: Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                name: "member".to_string(),
                path: ".".to_string(),
                dockers_v2: Some(vec![DockerV2Config {
                    dockerfile: "Dockerfile".to_string(),
                    images: vec!["ghcr.io/o/member".to_string()],
                    tags: vec!["latest".to_string()],
                    ..Default::default()
                }]),
                release: Some(ReleaseConfig {
                    github: Some(ScmRepoConfig {
                        owner: "o".to_string(),
                        name: "member".to_string(),
                        token: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let ctx = Context::new(config, ContextOptions::default());
    let selected = select_publishers(&ctx, &[]).expect("selection");
    let mut names: Vec<&str> = selected.iter().map(|p| p.name()).collect();
    names.sort();
    assert_eq!(names, vec!["docker", "github"]);
}

#[test]
fn explicit_capable_but_unconfigured_publisher_errors() {
    let ctx = ctx_no_snap();
    let err = select_publishers(&ctx, &["snapcraft".to_string()])
        .err()
        .expect("expected selection error")
        .to_string();
    assert!(
        err.contains("not configured"),
        "expected not-configured error, got: {err}"
    );
}

#[test]
fn dry_run_dispatch_emits_would_promote_and_spawns_nothing() {
    // `snapcraft` is not installed on the test box; a dry-run that returned a
    // DryRun outcome (rather than erroring) proves no external command ran.
    let config = Config {
        project_name: "solo".to_string(),
        crates: vec![snap_crate("solo")],
        ..Default::default()
    };
    let ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );

    let selected = select_publishers(&ctx, &[]).expect("selection");
    let report = dispatch_promotions(
        &selected,
        "prerelease",
        "stable",
        &PromoteSelector::Newest,
        &ctx,
    );
    assert_eq!(report.results.len(), 1);
    let outcome = &report.results[0];
    assert!(matches!(outcome.status, PromoteStatus::DryRun));
    // `prerelease` resolved to snapcraft's native `candidate`.
    assert_eq!(outcome.from, "candidate");
    assert_eq!(outcome.to, "stable");
    assert!(!report.any_failure());
    assert_eq!(
        outcome.summary_line(),
        "snapcraft: candidate→stable (dry-run)"
    );
}

/// The promote page quotes the abort a failed promotion prints. The same
/// report shape the page's example produces — one snapcraft target rejected on
/// its channel name — is built here and the message compared with the page.
#[test]
fn the_abort_quoted_in_the_promote_docs_is_what_a_failed_run_produces() {
    let page = anodizer_core::test_helpers::docs_pages::read_docs_page("publish/promote.md");
    let quoted: Vec<String> = page
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("Error "))
        .filter(|line| !line.contains('\u{2026}'))
        .map(str::to_string)
        .collect();

    let report = anodizer_core::promote::PromoteReport {
        results: vec![anodizer_core::promote::PromoteOutcome::failed(
            "snapcraft",
            "candidate",
            "lastest",
            "promote --to: invalid snapcraft channel 'lastest'",
        )],
    };

    assert_eq!(quoted.len(), 1, "the aborts the page quotes");
    assert_eq!(quoted[0], promote_failure_message(&report));
}

/// `anodizer promote` runs the same inference the release pipeline runs: a
/// `release.github` block naming only a token takes its owner and name from
/// the `origin` remote, so the flip targets the real repository and not
/// `/repos///releases`.
#[cfg(unix)]
#[test]
#[serial_test::serial(cwd)]
fn promote_fills_a_token_only_release_block_from_the_origin_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widget.git",
        ][..],
    ] {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(args).current_dir(dir);
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} must succeed");
    }
    let config_path = dir.join(".anodizer.yaml");
    std::fs::write(
        &config_path,
        "project_name: app\ncrates:\n  - name: app\n    release:\n      github:\n        token: \"{{ .Env.RELEASE_TOKEN }}\"\n",
    )
    .unwrap();
    let _cwd = anodizer_core::test_helpers::CwdGuard::new(dir).unwrap();
    let opts = PromoteOpts {
        to: "stable".to_string(),
        from: None,
        publishers: vec!["github".to_string()],
        version: Some("v1.0.0".to_string()),
        from_run: None,
        dry_run: true,
        config_override: Some(config_path),
        verbose: false,
        debug: false,
        quiet: true,
    };
    let ctx = load_context(&opts, &StageLogger::new("promote", Verbosity::Quiet)).unwrap();
    let gh = ctx.config.crates[0]
        .release
        .as_ref()
        .unwrap()
        .github
        .as_ref()
        .unwrap();
    assert_eq!(
        (gh.owner.as_str(), gh.name.as_str(), gh.token.as_deref()),
        ("acme", "widget", Some("{{ .Env.RELEASE_TOKEN }}"))
    );
    let repo = anodizer_core::download_url::resolve_release_repo(
        ctx.config.crates[0].release.as_ref().unwrap(),
        anodizer_core::scm::ScmTokenType::GitHub,
        &ctx,
    )
    .unwrap()
    .expect("the filled block resolves");
    assert_eq!(
        (repo.owner.as_str(), repo.name.as_str()),
        ("acme", "widget")
    );
}
