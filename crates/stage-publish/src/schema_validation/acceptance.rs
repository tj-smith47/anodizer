//! Cross-publisher acceptance tests for the offline artifact-validation pass.
//!
//! The per-publisher modules each prove their own validator in isolation. This
//! module proves the whole pass end-to-end: one crate carrying *several*
//! publishers at once, with the artifacts they all need, must
//! [`validate_publisher_schemas`] clean as a unit — and, when one option is
//! malformed, must fail loud with a message that names the offending publisher
//! and field. A second test guards the suite's completeness: every publisher
//! the suite is meant to cover must have a registered validator, so adding an
//! eleventh publisher (or dropping one) without a matching validator fails here
//! rather than silently leaving an artifact unchecked at release time.

use std::collections::HashMap;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    AurConfig, CrateConfig, HomebrewConfig, NfpmConfig, NixConfig, PublishConfig, ReleaseConfig,
    RepositoryConfig, ScmRepoConfig, ScoopConfig, WingetConfig,
};
use anodizer_core::context::Context;
use anodizer_core::test_helpers::TestContextBuilder;

use super::{validate_publisher_schemas, validators};

const VERSION: &str = "1.0.0";

/// A `WingetConfig` whose schema-constrained fields all carry registry-valid
/// values. `publisher_url` is the seam the malformed case rewrites to a
/// non-URL, which the locale manifest's `PublisherUrl` URL pattern rejects.
fn winget_cfg(publisher_url: &str) -> WingetConfig {
    WingetConfig {
        name: Some("Widget".to_string()),
        package_name: Some("Widget Tool".to_string()),
        package_identifier: Some("AcmeCo.Widget".to_string()),
        publisher: Some("Acme Co".to_string()),
        publisher_url: Some(publisher_url.to_string()),
        license: Some("MIT".to_string()),
        short_description: Some("A widget management tool".to_string()),
        homepage: Some("https://acme.example/widget".to_string()),
        repository: Some(RepositoryConfig {
            owner: Some("acme".to_string()),
            name: Some("winget-pkgs-fork".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn scoop_cfg() -> ScoopConfig {
    ScoopConfig {
        repository: Some(RepositoryConfig {
            owner: Some("acme".to_string()),
            name: Some("scoop-bucket".to_string()),
            ..Default::default()
        }),
        name: Some("widget".to_string()),
        description: Some("A widget management tool".to_string()),
        license: Some("MIT".to_string()),
        homepage: Some("https://acme.example/widget".to_string()),
        ..Default::default()
    }
}

fn homebrew_cfg() -> HomebrewConfig {
    HomebrewConfig {
        name: Some("widget".to_string()),
        repository: Some(RepositoryConfig {
            owner: Some("acme".to_string()),
            name: Some("homebrew-tap".to_string()),
            branch: Some("main".to_string()),
            ..Default::default()
        }),
        description: Some("A widget management tool".to_string()),
        homepage: Some("https://acme.example/widget".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    }
}

fn aur_cfg() -> AurConfig {
    AurConfig {
        name: Some("widget-bin".to_string()),
        description: Some("A widget management tool".to_string()),
        homepage: Some("https://acme.example/widget".to_string()),
        license: Some("MIT".to_string()),
        maintainers: Some(vec!["Acme Corp <dev@acme.example>".to_string()]),
        depends: Some(vec!["glibc".to_string()]),
        git_url: Some("ssh://aur@aur.archlinux.org/widget-bin.git".to_string()),
        ..Default::default()
    }
}

fn nix_cfg() -> NixConfig {
    NixConfig {
        name: Some("widget".to_string()),
        repository: Some(RepositoryConfig {
            owner: Some("acme".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            branch: Some("main".to_string()),
            ..Default::default()
        }),
        description: Some("A widget management tool".to_string()),
        homepage: Some("https://acme.example/widget".to_string()),
        license: Some("MIT".to_string()),
        main_program: Some("widget".to_string()),
        ..Default::default()
    }
}

fn nfpm_cfg() -> NfpmConfig {
    NfpmConfig {
        package_name: Some("widget".to_string()),
        formats: vec!["deb".to_string(), "rpm".to_string()],
        maintainer: Some("Acme <ops@acme.example>".to_string()),
        description: Some("A widget management tool".to_string()),
        license: Some("MIT".to_string()),
        homepage: Some("https://acme.example/widget".to_string()),
        ..Default::default()
    }
}

/// One crate carrying winget + scoop + homebrew + aur + nix (under `publish`)
/// and an nfpm config, spanning the JSON-schema validators (winget, scoop,
/// nfpm) and the structural/gated validators (homebrew Ruby, aur PKGBUILD, nix
/// derivation). `winget_publisher_url` is threaded so the malformed case can
/// poison exactly one field.
fn multi_publisher_crate(winget_publisher_url: &str) -> CrateConfig {
    CrateConfig {
        name: "widget".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        release: Some(ReleaseConfig {
            github: Some(ScmRepoConfig {
                owner: "acme".to_string(),
                name: "widget".to_string(),
            }),
            ..Default::default()
        }),
        publish: Some(PublishConfig {
            winget: Some(winget_cfg(winget_publisher_url)),
            scoop: Some(scoop_cfg()),
            homebrew: Some(homebrew_cfg()),
            aur: Some(aur_cfg()),
            nix: Some(nix_cfg()),
            ..Default::default()
        }),
        nfpms: Some(vec![nfpm_cfg()]),
        ..Default::default()
    }
}

/// Add a Windows zip archive + binary (drives winget + scoop), a macOS archive
/// (drives homebrew), a Linux archive with os/arch metadata (drives aur + nix),
/// and a Linux binary (drives nfpm) — the full artifact set every configured
/// publisher needs to render its manifest.
fn add_all_artifacts(ctx: &mut Context) {
    add_windows_zip(ctx);
    add_macos_archive(ctx);
    add_linux_archive(ctx);
    add_linux_binary(ctx);
}

fn add_windows_zip(ctx: &mut Context) {
    let target = "x86_64-pc-windows-msvc";
    let mut archive_meta = HashMap::new();
    archive_meta.insert(
        "url".to_string(),
        format!("https://github.com/acme/widget/releases/download/v{VERSION}/widget-{target}.zip"),
    );
    archive_meta.insert("sha256".to_string(), "a".repeat(64));
    archive_meta.insert("format".to_string(), "zip".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/dist/widget-{target}.zip")),
        name: format!("widget-{target}.zip"),
        target: Some(target.to_string()),
        crate_name: "widget".to_string(),
        metadata: archive_meta,
        size: None,
    });

    let mut bin_meta = HashMap::new();
    bin_meta.insert("binary".to_string(), "widget".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        path: std::path::PathBuf::from("/dist/widget.exe"),
        name: "widget.exe".to_string(),
        target: Some(target.to_string()),
        crate_name: "widget".to_string(),
        metadata: bin_meta,
        size: None,
    });
}

fn add_macos_archive(ctx: &mut Context) {
    let target = "x86_64-apple-darwin";
    let mut meta = HashMap::new();
    meta.insert(
        "url".to_string(),
        format!(
            "https://github.com/acme/widget/releases/download/v{VERSION}/widget-{target}.tar.gz"
        ),
    );
    meta.insert("sha256".to_string(), "a".repeat(64));
    meta.insert("format".to_string(), "tar.gz".to_string());
    meta.insert("os".to_string(), "darwin".to_string());
    meta.insert("arch".to_string(), "amd64".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/dist/widget-{target}.tar.gz")),
        name: format!("widget-{target}.tar.gz"),
        target: Some(target.to_string()),
        crate_name: "widget".to_string(),
        metadata: meta,
        size: None,
    });
}

fn add_linux_archive(ctx: &mut Context) {
    let target = "x86_64-unknown-linux-gnu";
    let mut meta = HashMap::new();
    meta.insert(
        "url".to_string(),
        format!(
            "https://github.com/acme/widget/releases/download/v{VERSION}/widget-{target}.tar.gz"
        ),
    );
    meta.insert("sha256".to_string(), "a".repeat(64));
    meta.insert("format".to_string(), "tar.gz".to_string());
    meta.insert("os".to_string(), "linux".to_string());
    meta.insert("arch".to_string(), "amd64".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("/dist/widget-{target}.tar.gz")),
        name: format!("widget-{target}.tar.gz"),
        target: Some(target.to_string()),
        crate_name: "widget".to_string(),
        metadata: meta,
        size: None,
    });
}

fn add_linux_binary(ctx: &mut Context) {
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        path: std::path::PathBuf::from("/dist/widget"),
        name: "widget".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "widget".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
}

fn scope_version(ctx: &mut Context, version: &str) {
    ctx.template_vars_mut().set("Version", version);
    ctx.template_vars_mut().set("RawVersion", version);
    ctx.template_vars_mut().set("Tag", &format!("v{version}"));
}

/// Build the multi-publisher ctx the acceptance cases share. `winget_publisher_url`
/// is valid for the positive case and a non-URL for the malformed case.
fn multi_publisher_ctx(winget_publisher_url: &str) -> Context {
    let mut ctx = TestContextBuilder::new()
        .snapshot(true)
        .project_name("widget")
        .crates(vec![multi_publisher_crate(winget_publisher_url)])
        .build();
    scope_version(&mut ctx, VERSION);
    add_all_artifacts(&mut ctx);
    ctx
}

/// Positive: a single crate configured with six publishers at once — spanning
/// the JSON-schema kind (winget, scoop, nfpm) and the structural/gated kind
/// (homebrew, aur, nix) — and carrying every artifact they need, validates
/// clean as a unit. The whole pass returns `Ok(())`.
#[test]
fn all_publishers_with_valid_config_validate_clean_together() {
    let mut ctx = multi_publisher_ctx("https://acme.example");
    let log = ctx.logger("publish");

    // Fixed-tag resolver: single crate at the pre-scoped VERSION, so per-crate
    // scoping resolves to the same version without a git fixture.
    let resolver = |_: &Context, _: &CrateConfig| Some(VERSION.to_string());
    validate_publisher_schemas(&mut ctx, &log, &resolver)
        .expect("every configured publisher's artifact validates clean together");
}

/// Negative: take the same ctx and poison exactly one option — a winget
/// `publisher_url` that is not an `https?://` URL. The locale manifest's
/// `PublisherUrl` URL-pattern rejects it, so the whole pass fails, and the
/// aggregated message names the offending publisher (`winget`) and the
/// `PublisherUrl` field. This proves the pass fails loud and points at the
/// defect rather than passing a release that would later be rejected.
#[test]
fn one_malformed_option_fails_loud_naming_publisher_and_field() {
    let mut ctx = multi_publisher_ctx("not-a-url");
    let log = ctx.logger("publish");

    let resolver = |_: &Context, _: &CrateConfig| Some(VERSION.to_string());
    let err = validate_publisher_schemas(&mut ctx, &log, &resolver)
        .expect_err("a malformed winget publisher_url must fail the pass");
    let message = format!("{err:#}");

    assert!(
        message.contains("winget"),
        "the aggregated error names the offending publisher, got: {message}"
    );
    assert!(
        message.contains("PublisherUrl"),
        "the aggregated error names the offending field, got: {message}"
    );
}

/// Wiring-regression guard: every publisher the suite is meant to cover must
/// have a registered validator. If a future publisher is added without one (or
/// a validator is dropped), this fails until the expected set is updated
/// deliberately — keeping the offline artifact-validation pass complete so no
/// publisher's manifest ships unchecked.
#[test]
fn validators_cover_exactly_the_expected_publisher_set() {
    let expected: std::collections::BTreeSet<&str> = [
        "winget",
        "scoop",
        "krew",
        "mcp",
        "chocolatey",
        "snapcraft",
        "homebrew",
        "nfpm",
        "aur",
        "nix",
    ]
    .into_iter()
    .collect();

    let registered: std::collections::BTreeSet<&str> =
        validators().iter().map(|v| v.publisher()).collect();

    assert_eq!(
        registered, expected,
        "the registered validator set must equal the expected publisher set; a new \
         publisher needs a validator (and this list updated), and a dropped one must \
         be removed here deliberately"
    );
}
