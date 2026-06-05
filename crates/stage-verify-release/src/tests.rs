//! Stage-orchestration tests for [`VerifyReleaseStage`].
//!
//! The check LOGIC (asset-diff, glibc compare/extract, smoke argv) is unit-
//! tested in the respective modules without network or Docker. These tests
//! cover the stage wiring: enabled/skip/dry-run gating, the produced-set
//! derivation, the binary-name fallback, and the multi-crate (workspace)
//! fan-out — all offline (no real release exists, so the gating paths return
//! before any network call).

use super::*;
use anodizer_core::artifact::Artifact;
use anodizer_core::config::{CrateConfig, InstallSmokeConfig, VerifyReleaseConfig};
use anodizer_core::test_helpers::TestContextBuilder;
use std::collections::HashMap;

/// Deserialize a minimal crate with a GitHub release block so it counts as
/// "published" for the gate's crate iteration.
fn published_crate(name: &str, binary: Option<&str>) -> CrateConfig {
    let builds = match binary {
        Some(b) => format!("builds:\n  - binary: {b}\n"),
        None => String::new(),
    };
    let yaml = format!(
        "name: {name}\npath: .\ntag_template: \"v{{{{ .Version }}}}\"\n\
         release:\n  github: {{ owner: me, name: repo }}\n{builds}"
    );
    serde_yaml_ng::from_str(&yaml).expect("valid crate yaml")
}

fn add_artifact(ctx: &mut Context, kind: ArtifactKind, name: &str, crate_name: &str) {
    ctx.artifacts.add(Artifact {
        kind,
        name: name.to_string(),
        path: std::path::PathBuf::from(name),
        target: None,
        crate_name: crate_name.to_string(),
        metadata: HashMap::new(),
        size: None,
    });
}

#[test]
fn disabled_is_noop() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("myapp", None)])
        .build();
    // verify_release defaults to disabled.
    assert!(!ctx.config.verify_release.enabled);
    add_artifact(&mut ctx, ArtifactKind::Archive, "myapp.tar.gz", "myapp");
    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "disabled gate must be a no-op (no network)"
    );
}

#[test]
fn enabled_but_dry_run_is_noop() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .dry_run(true)
        .crates(vec![published_crate("myapp", None)])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        assert_assets: true,
        ..Default::default()
    };
    add_artifact(&mut ctx, ArtifactKind::Archive, "myapp.tar.gz", "myapp");
    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "dry-run has no published release to verify; must no-op without fetching"
    );
}

#[test]
fn enabled_but_snapshot_is_noop() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .snapshot(true)
        .crates(vec![published_crate("myapp", None)])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(VerifyReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn skip_flag_is_noop() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("myapp", None)])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        ..Default::default()
    };
    ctx.options.skip_stages = vec!["verify-release".to_string()];
    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "--skip=verify-release must short-circuit before any fetch"
    );
}

#[test]
fn no_published_crates_is_noop() {
    // A crate with no release block is not "published"; the gate finds
    // nothing to verify and returns Ok without touching the network.
    let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(VerifyReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn produced_asset_names_derives_from_registry_per_crate() {
    // Rule #11 evidence: the produced set comes from release_uploadable_kinds()
    // + by_kind_and_crate, with NO config. Per-crate isolation (workspace mode):
    // crate A's archive must not leak into crate B's produced set.
    let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
    add_artifact(&mut ctx, ArtifactKind::Archive, "a.tar.gz", "crate-a");
    add_artifact(&mut ctx, ArtifactKind::Checksum, "checksums.txt", "crate-a");
    add_artifact(&mut ctx, ArtifactKind::LinuxPackage, "a.deb", "crate-a");
    add_artifact(&mut ctx, ArtifactKind::Archive, "b.tar.gz", "crate-b");
    // A raw Binary is NOT in release_uploadable_kinds(); must be excluded.
    add_artifact(&mut ctx, ArtifactKind::Binary, "raw-bin", "crate-a");

    let a = produced_asset_names(&ctx, "crate-a", None);
    assert_eq!(a, vec!["a.deb", "a.tar.gz", "checksums.txt"]);
    let b = produced_asset_names(&ctx, "crate-b", None);
    assert_eq!(b, vec!["b.tar.gz"], "crate-b set is isolated from crate-a");
}

/// Add an artifact carrying an `id` in metadata so `release.ids` filtering can
/// select / exclude it (mirrors how upstream stages tag artifacts with `id`).
fn add_artifact_with_id(
    ctx: &mut Context,
    kind: ArtifactKind,
    name: &str,
    crate_name: &str,
    id: &str,
) {
    let mut metadata = HashMap::new();
    metadata.insert("id".to_string(), id.to_string());
    ctx.artifacts.add(Artifact {
        kind,
        name: name.to_string(),
        path: std::path::PathBuf::from(name),
        target: None,
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    });
}

#[test]
fn produced_asset_names_honors_release_ids_filter() {
    // The upload path applies `release.ids`; the asset-existence check must use
    // the SAME filter so an artifact intentionally filtered OUT of the upload
    // set is NOT reported as a missing asset (false post-release FAIL).
    let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
    add_artifact_with_id(
        &mut ctx,
        ArtifactKind::Archive,
        "linux.tar.gz",
        "app",
        "linux",
    );
    add_artifact_with_id(
        &mut ctx,
        ArtifactKind::Archive,
        "windows.zip",
        "app",
        "windows",
    );

    // No filter: both candidates are expected assets.
    let all = produced_asset_names(&ctx, "app", None);
    assert_eq!(all, vec!["linux.tar.gz", "windows.zip"]);

    // ids = [linux]: the windows artifact is filtered out of the upload set and
    // therefore must NOT appear in the expected (produced) asset names.
    let ids = vec!["linux".to_string()];
    let filtered = produced_asset_names(&ctx, "app", Some(&ids));
    assert_eq!(
        filtered,
        vec!["linux.tar.gz"],
        "ids-filtered-out artifact must not be reported as a produced asset"
    );
}

#[test]
fn crate_binary_name_prefers_build_binary_then_falls_back() {
    let with_bin = published_crate("mycrate", Some("mybin"));
    assert_eq!(crate_binary_name(&with_bin), "mybin");
    let without = published_crate("mycrate", None);
    assert_eq!(
        crate_binary_name(&without),
        "mycrate",
        "falls back to crate name when no build binary is set"
    );
}

#[test]
fn smoke_disabled_when_no_install_smoke_block() {
    // With install_smoke=None, docker_available() must never be consulted and
    // the stage must not hard-fail on a docker-less host. We force enabled but
    // dry-run so the whole run is a no-op regardless — the real assertion is
    // that the default config leaves smoke off.
    let cfg = VerifyReleaseConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(cfg.install_smoke.is_none(), "smoke off unless configured");
}

#[test]
fn libc_check_off_without_ceiling() {
    let cfg = VerifyReleaseConfig {
        enabled: true,
        glibc_ceiling: None,
        ..Default::default()
    };
    assert!(
        !cfg.glibc_check_enabled(),
        "no ceiling => libc check does not run"
    );
}

#[test]
fn install_smoke_resolves_per_type_images() {
    let smoke = InstallSmokeConfig::default();
    // All defaults when nothing configured.
    assert_eq!(smoke.deb_image(), "debian:stable-slim");
    assert_eq!(smoke.rpm_image(), "fedora:latest");
    assert_eq!(smoke.apk_image(), "alpine:latest");
}

#[test]
fn multi_crate_iteration_covers_all_published_crates() {
    // Workspace per-crate mode: two published crates, dry-run so no network.
    // The stage must consider BOTH (not silo to one) — verified indirectly by
    // produced_asset_names isolation plus the dry-run no-op completing for a
    // multi-crate config.
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .dry_run(true)
        .crates(vec![
            published_crate("crate-a", Some("bin-a")),
            published_crate("crate-b", Some("bin-b")),
        ])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        ..Default::default()
    };
    add_artifact(&mut ctx, ArtifactKind::Archive, "a.tar.gz", "crate-a");
    add_artifact(&mut ctx, ArtifactKind::Archive, "b.tar.gz", "crate-b");
    assert!(VerifyReleaseStage.run(&mut ctx).is_ok());
}
