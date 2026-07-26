//! Tests for the Homebrew publisher's open-PR reconcile probes.

use anodizer_core::config::{
    CrateConfig, HomebrewCaskConfig, HomebrewConfig, PublishConfig, PullRequestBaseConfig,
    PullRequestConfig, RepositoryConfig,
};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::test_helpers::TestContextBuilder;

use super::publisher::{
    build_homebrew_crate_reconcile_target, build_homebrew_top_cask_reconcile_target,
};

fn tap(pr_base: Option<(&str, &str)>) -> RepositoryConfig {
    RepositoryConfig {
        owner: Some("acme".to_string()),
        name: Some("homebrew-tap".to_string()),
        pull_request: Some(PullRequestConfig {
            enabled: Some(true),
            base: pr_base.map(|(o, n)| PullRequestBaseConfig {
                owner: Some(o.to_string()),
                name: Some(n.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn brew_crate(name: &str, repo: Option<RepositoryConfig>, formula: Option<&str>) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            homebrew: Some(HomebrewConfig {
                repository: repo,
                name: formula.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn log() -> StageLogger {
    StageLogger::new("homebrew", Verbosity::Quiet)
}

fn crate_target(
    ctx: &anodizer_core::context::Context,
    crate_name: &str,
) -> Option<crate::util::PrReconcileTarget> {
    build_homebrew_crate_reconcile_target(ctx, crate_name, &log()).expect("builder ok")
}

#[test]
fn build_homebrew_crate_reconcile_target_probes_the_pull_request_base_not_the_fork() {
    let ctx = TestContextBuilder::new()
        .crates(vec![brew_crate(
            "x",
            Some(tap(Some(("upstream-org", "homebrew-core-fork")))),
            None,
        )])
        .build();
    let t = crate_target(&ctx, "x").expect("target built");
    assert_eq!(t.upstream_owner, "upstream-org");
    assert_eq!(t.upstream_repo, "homebrew-core-fork");
}

#[test]
fn build_homebrew_crate_reconcile_target_falls_back_to_the_fork_when_no_base_is_set() {
    let ctx = TestContextBuilder::new()
        .crates(vec![brew_crate("x", Some(tap(None)), None)])
        .build();
    let t = crate_target(&ctx, "x").expect("target built");
    assert_eq!(t.upstream_owner, "acme");
    assert_eq!(t.upstream_repo, "homebrew-tap");
}

#[test]
fn build_homebrew_crate_reconcile_target_title_omits_the_cask_when_none_is_configured() {
    let ctx = TestContextBuilder::new()
        .crates(vec![brew_crate("x", Some(tap(None)), None)])
        .build();
    let t = crate_target(&ctx, "x").expect("target built");
    // Naming a cask the submitter never puts in the title strands the probe at
    // Absent forever, so the no-cask title must match the formula-only form.
    assert_eq!(
        t.title,
        super::publish_formula::publish::pr_title(&t.package, None, &t.version)
    );
    assert!(!t.title.contains("cask"), "got {}", t.title);
}

#[test]
fn build_homebrew_crate_reconcile_target_renders_a_templated_formula_name() {
    let ctx = TestContextBuilder::new()
        .crates(vec![brew_crate(
            "x",
            Some(tap(None)),
            Some("{{ .ProjectName }}-formula"),
        )])
        .build();
    let t = crate_target(&ctx, "x").expect("target built");
    assert!(!t.package.contains("{{"), "unrendered name: {}", t.package);
    assert!(t.package.ends_with("-formula"), "got {}", t.package);
}

#[test]
fn build_homebrew_crate_reconcile_target_without_a_repository_is_none() {
    let ctx = TestContextBuilder::new()
        .crates(vec![brew_crate("x", None, None)])
        .build();
    assert!(crate_target(&ctx, "x").is_none());
}

#[test]
fn build_homebrew_top_cask_reconcile_target_requires_pull_request_mode() {
    let cask = HomebrewCaskConfig {
        name: Some("mycask".to_string()),
        repository: Some(RepositoryConfig {
            owner: Some("acme".to_string()),
            name: Some("homebrew-tap".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ctx = TestContextBuilder::new().build();
    // A direct tap push is idempotent; only a PR-mode entry can be Complete.
    assert!(
        build_homebrew_top_cask_reconcile_target(&ctx, &cask, &log())
            .expect("builder ok")
            .is_none()
    );
}

#[test]
fn build_homebrew_top_cask_reconcile_target_probes_the_pull_request_base() {
    let cask = HomebrewCaskConfig {
        name: Some("mycask".to_string()),
        repository: Some(tap(Some(("upstream-org", "upstream-tap")))),
        ..Default::default()
    };
    let ctx = TestContextBuilder::new().build();
    let t = build_homebrew_top_cask_reconcile_target(&ctx, &cask, &log())
        .expect("builder ok")
        .expect("target built");
    assert_eq!(t.upstream_owner, "upstream-org");
    assert_eq!(t.upstream_repo, "upstream-tap");
    assert_eq!(t.package, "mycask");
    assert_eq!(
        t.title,
        super::publish_top::cask_pr_title(&t.package, &t.version)
    );
}

#[test]
fn build_homebrew_top_cask_reconcile_target_defaults_the_name_to_the_project() {
    let cask = HomebrewCaskConfig {
        repository: Some(tap(None)),
        ..Default::default()
    };
    let ctx = TestContextBuilder::new().project_name("myproj").build();
    let t = build_homebrew_top_cask_reconcile_target(&ctx, &cask, &log())
        .expect("builder ok")
        .expect("target built");
    assert_eq!(t.package, "myproj");
}
