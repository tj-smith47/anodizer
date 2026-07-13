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
    // The crate-universe union must surface a snapcraft config declared only
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
