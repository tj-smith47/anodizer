//! Tests for the Nix publisher's open-PR reconcile probe.

use anodizer_core::config::{
    CrateConfig, NixConfig, PublishConfig, PullRequestBaseConfig, PullRequestConfig,
    RepositoryConfig,
};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::test_helpers::TestContextBuilder;

use super::publisher::build_nix_reconcile_target;

fn nix_crate(name: &str, repo: Option<RepositoryConfig>, overlay: Option<&str>) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        publish: Some(PublishConfig {
            nix: Some(NixConfig {
                repository: repo,
                name: overlay.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn fork_repo() -> RepositoryConfig {
    RepositoryConfig {
        owner: Some("acme".to_string()),
        name: Some("nur-packages".to_string()),
        ..Default::default()
    }
}

fn nix_target(
    ctx: &anodizer_core::context::Context,
    crate_name: &str,
) -> Option<crate::util::PrReconcileTarget> {
    let log = StageLogger::new("nix", Verbosity::Quiet);
    build_nix_reconcile_target(ctx, crate_name, &log).expect("builder ok")
}

#[test]
fn build_nix_reconcile_target_probes_the_pull_request_base_not_the_fork() {
    let mut repo = fork_repo();
    repo.pull_request = Some(PullRequestConfig {
        enabled: Some(true),
        base: Some(PullRequestBaseConfig {
            owner: Some("nixos-org".to_string()),
            name: Some("nixpkgs".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let ctx = TestContextBuilder::new()
        .crates(vec![nix_crate("x", Some(repo), None)])
        .build();
    let t = nix_target(&ctx, "x").expect("target built");
    // run() submits base-else-fork; probing the bare fork searches a repo the
    // PR never lands in, so reconcile would re-open a duplicate.
    assert_eq!(t.upstream_owner, "nixos-org");
    assert_eq!(t.upstream_repo, "nixpkgs");
}

#[test]
fn build_nix_reconcile_target_falls_back_to_the_fork_when_no_base_is_set() {
    let ctx = TestContextBuilder::new()
        .crates(vec![nix_crate("x", Some(fork_repo()), None)])
        .build();
    let t = nix_target(&ctx, "x").expect("target built");
    assert_eq!(t.upstream_owner, "acme");
    assert_eq!(t.upstream_repo, "nur-packages");
}

#[test]
fn build_nix_reconcile_target_title_is_sourced_from_the_submitter() {
    let ctx = TestContextBuilder::new()
        .crates(vec![nix_crate("x", Some(fork_repo()), None)])
        .build();
    let t = nix_target(&ctx, "x").expect("target built");
    // A probe that re-derives the title drifts silently from the submitter and
    // reports Complete for a PR that was never opened.
    assert_eq!(
        t.title,
        crate::nix::publish::build::pr_title(&t.package, &t.version)
    );
}

#[test]
fn build_nix_reconcile_target_defaults_the_overlay_name_to_the_crate_name() {
    let ctx = TestContextBuilder::new()
        .crates(vec![nix_crate("mytool", Some(fork_repo()), None)])
        .build();
    assert_eq!(
        nix_target(&ctx, "mytool").expect("target").package,
        "mytool"
    );
}

#[test]
fn build_nix_reconcile_target_renders_a_templated_overlay_name() {
    let ctx = TestContextBuilder::new()
        .crates(vec![nix_crate(
            "x",
            Some(fork_repo()),
            Some("{{ .ProjectName }}-unstable"),
        )])
        .build();
    let t = nix_target(&ctx, "x").expect("target built");
    assert!(!t.package.contains("{{"), "unrendered name: {}", t.package);
    assert!(t.package.ends_with("-unstable"), "got {}", t.package);
}

#[test]
fn build_nix_reconcile_target_without_a_repository_is_none() {
    let ctx = TestContextBuilder::new()
        .crates(vec![nix_crate("x", None, None)])
        .build();
    assert!(
        nix_target(&ctx, "x").is_none(),
        "no owner/name means no probe coordinates; run() owns the diagnostics"
    );
}
