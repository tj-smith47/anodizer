//! Tests for the `util/` submodules. Externalised from the original
//! `util.rs`'s bottom `mod tests` block.

#![allow(clippy::field_reassign_with_default)]

use super::artifacts::{
    OsArtifact, filter_by_variant, find_all_platform_artifacts_with_variant,
    find_artifacts_by_os_with_variant, infer_arch, infer_os,
};
use super::config::{resolve_artifact_kind, resolve_repo_owner_name, should_skip_upload};
use super::template::{render_or_warn, render_url_template};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{Config, CrateConfig};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};
use std::collections::HashMap;
use std::path::PathBuf;

fn test_log() -> StageLogger {
    StageLogger::new("publish-test", Verbosity::Quiet)
}

/// Helper: build a Context with mock Archive artifacts for a given crate.
fn ctx_with_artifacts(crate_name: &str, artifacts: Vec<(&str, &str, &str)>) -> Context {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: crate_name.to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    for (target, url, sha256) in artifacts {
        let mut meta = HashMap::new();
        meta.insert("url".to_string(), url.to_string());
        meta.insert("sha256".to_string(), sha256.to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from(format!(
                "dist/{}",
                url.rsplit('/').next().unwrap_or("a.tar.gz")
            )),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
            size: None,
        });
    }
    ctx
}

// -----------------------------------------------------------------------
// infer_os / infer_arch unit tests
// -----------------------------------------------------------------------

#[test]
fn test_infer_os_linux() {
    assert_eq!(infer_os("x86_64-unknown-linux-gnu", "fallback"), "linux");
    assert_eq!(infer_os("aarch64-unknown-linux-musl", "fallback"), "linux");
}

#[test]
fn test_infer_os_darwin() {
    assert_eq!(infer_os("aarch64-apple-darwin", "fallback"), "darwin");
    assert_eq!(infer_os("x86_64-apple-darwin", "fallback"), "darwin");
}

#[test]
fn test_infer_os_windows() {
    assert_eq!(infer_os("x86_64-pc-windows-msvc", "fallback"), "windows");
}

#[test]
fn test_infer_os_unknown_uses_fallback() {
    assert_eq!(
        infer_os("wasm32-unknown-unknown", "myfallback"),
        "myfallback"
    );
}

#[test]
fn test_infer_arch_x86_64() {
    assert_eq!(infer_arch("x86_64-unknown-linux-gnu"), "amd64");
    assert_eq!(infer_arch("x86_64-pc-windows-msvc"), "amd64");
    assert_eq!(infer_arch("x86_64-apple-darwin"), "amd64");
}

#[test]
fn test_infer_arch_aarch64() {
    assert_eq!(infer_arch("aarch64-apple-darwin"), "arm64");
    assert_eq!(infer_arch("aarch64-unknown-linux-musl"), "arm64");
}

#[test]
fn test_infer_arch_unknown() {
    // map_target passes unrecognised arch prefixes through verbatim
    assert_eq!(infer_arch("wasm32-unknown-unknown"), "wasm32");
}

// -----------------------------------------------------------------------
// find_artifacts_by_os tests
// -----------------------------------------------------------------------

#[test]
fn test_find_artifacts_by_os_linux() {
    let ctx = ctx_with_artifacts(
        "mytool",
        vec![
            (
                "x86_64-unknown-linux-gnu",
                "https://example.com/mytool-linux-amd64.tar.gz",
                "hash_linux_amd64",
            ),
            (
                "aarch64-unknown-linux-musl",
                "https://example.com/mytool-linux-arm64.tar.gz",
                "hash_linux_arm64",
            ),
            (
                "aarch64-apple-darwin",
                "https://example.com/mytool-darwin-arm64.tar.gz",
                "hash_darwin_arm64",
            ),
            (
                "x86_64-pc-windows-msvc",
                "https://example.com/mytool-windows-amd64.zip",
                "hash_win_amd64",
            ),
        ],
    );

    let linux =
        find_artifacts_by_os_with_variant(&ctx, "mytool", "linux", None, None, None).unwrap();
    assert_eq!(linux.len(), 2);
    assert!(linux.iter().all(|a| a.os == "linux"));
    assert!(
        linux
            .iter()
            .any(|a| a.arch == "amd64" && a.sha256 == "hash_linux_amd64")
    );
    assert!(
        linux
            .iter()
            .any(|a| a.arch == "arm64" && a.sha256 == "hash_linux_arm64")
    );
}

#[test]
fn test_find_artifacts_by_os_darwin() {
    let ctx = ctx_with_artifacts(
        "mytool",
        vec![
            (
                "x86_64-unknown-linux-gnu",
                "https://example.com/mytool-linux-amd64.tar.gz",
                "h1",
            ),
            (
                "aarch64-apple-darwin",
                "https://example.com/mytool-darwin-arm64.tar.gz",
                "h2",
            ),
            (
                "x86_64-apple-darwin",
                "https://example.com/mytool-darwin-amd64.tar.gz",
                "h3",
            ),
        ],
    );

    let darwin =
        find_artifacts_by_os_with_variant(&ctx, "mytool", "darwin", None, None, None).unwrap();
    assert_eq!(darwin.len(), 2);
    assert!(darwin.iter().all(|a| a.os == "darwin"));
}

#[test]
fn test_find_artifacts_by_os_no_match() {
    let ctx = ctx_with_artifacts(
        "mytool",
        vec![(
            "x86_64-unknown-linux-gnu",
            "https://example.com/mytool-linux-amd64.tar.gz",
            "h1",
        )],
    );

    let windows =
        find_artifacts_by_os_with_variant(&ctx, "mytool", "windows", None, None, None).unwrap();
    assert!(windows.is_empty());
}

// -----------------------------------------------------------------------
// find_all_platform_artifacts tests
// -----------------------------------------------------------------------

#[test]
fn test_find_all_platform_artifacts() {
    let ctx = ctx_with_artifacts(
        "mytool",
        vec![
            (
                "x86_64-unknown-linux-gnu",
                "https://example.com/linux-amd64.tar.gz",
                "h1",
            ),
            (
                "aarch64-apple-darwin",
                "https://example.com/darwin-arm64.tar.gz",
                "h2",
            ),
            (
                "x86_64-pc-windows-msvc",
                "https://example.com/windows-amd64.zip",
                "h3",
            ),
        ],
    );

    let all = find_all_platform_artifacts_with_variant(&ctx, "mytool", None, None, None).unwrap();
    assert_eq!(all.len(), 3);
    assert!(all.iter().any(|a| a.os == "linux" && a.arch == "amd64"));
    assert!(all.iter().any(|a| a.os == "darwin" && a.arch == "arm64"));
    assert!(all.iter().any(|a| a.os == "windows" && a.arch == "amd64"));
}

#[test]
fn test_find_all_platform_artifacts_empty() {
    let ctx = ctx_with_artifacts("mytool", vec![]);
    let all = find_all_platform_artifacts_with_variant(&ctx, "mytool", None, None, None).unwrap();
    assert!(all.is_empty());
}

#[test]
fn test_find_all_platform_artifacts_wrong_crate() {
    let ctx = ctx_with_artifacts(
        "mytool",
        vec![(
            "x86_64-unknown-linux-gnu",
            "https://example.com/linux-amd64.tar.gz",
            "h1",
        )],
    );
    let all =
        find_all_platform_artifacts_with_variant(&ctx, "other_tool", None, None, None).unwrap();
    assert!(all.is_empty());
}

// -----------------------------------------------------------------------
// artifact_to_os_artifact sha256 bail tests
// -----------------------------------------------------------------------

#[test]
fn artifact_to_os_artifact_bails_on_empty_sha256() {
    let ctx = ctx_with_artifacts(
        "mytool",
        vec![(
            "x86_64-unknown-linux-gnu",
            "https://example.com/mytool-linux-amd64.tar.gz",
            "",
        )],
    );

    let err = find_artifacts_by_os_with_variant(&ctx, "mytool", "linux", None, None, None)
        .expect_err("empty sha256 must produce an error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing sha256 metadata"),
        "error must mention missing sha256: {msg}"
    );
    assert!(
        msg.contains("checksum stage"),
        "error must mention the checksum stage: {msg}"
    );
}

#[test]
fn artifact_to_os_artifact_bails_on_missing_sha256_key() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    // Add an artifact without any sha256 metadata key at all
    let mut meta = HashMap::new();
    meta.insert(
        "url".to_string(),
        "https://example.com/mytool.tar.gz".to_string(),
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/mytool.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "mytool".to_string(),
        metadata: meta,
        size: None,
    });

    let err = find_all_platform_artifacts_with_variant(&ctx, "mytool", None, None, None)
        .expect_err("missing sha256 key must produce an error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing sha256 metadata"),
        "error must mention missing sha256: {msg}"
    );
    assert!(
        msg.contains("checksum stage"),
        "error must mention the checksum stage: {msg}"
    );
}

// -----------------------------------------------------------------------
// resolve_artifact_kind tests
// -----------------------------------------------------------------------

#[test]
fn test_resolve_artifact_kind_none_defaults_to_archive() {
    assert!(matches!(resolve_artifact_kind(None), ArtifactKind::Archive));
}

#[test]
fn test_resolve_artifact_kind_archive() {
    assert!(matches!(
        resolve_artifact_kind(Some("archive")),
        ArtifactKind::Archive
    ));
}

#[test]
fn test_resolve_artifact_kind_msi() {
    assert!(matches!(
        resolve_artifact_kind(Some("msi")),
        ArtifactKind::Installer
    ));
}

#[test]
fn test_resolve_artifact_kind_nsis() {
    assert!(matches!(
        resolve_artifact_kind(Some("nsis")),
        ArtifactKind::Installer
    ));
}

#[test]
fn test_resolve_artifact_kind_unknown_defaults_to_archive() {
    assert!(matches!(
        resolve_artifact_kind(Some("unknown")),
        ArtifactKind::Archive
    ));
}

// -----------------------------------------------------------------------
// render_url_template tests
// -----------------------------------------------------------------------

#[test]
fn test_render_url_template_basic() {
    let url = render_url_template(
        "https://example.com/{{ name }}/{{ version }}/{{ arch }}-{{ os }}.zip",
        "mytool",
        "1.2.3",
        "amd64",
        "windows",
    );
    assert_eq!(url, "https://example.com/mytool/1.2.3/amd64-windows.zip");
}

#[test]
fn test_render_url_template_invalid_fallback() {
    let url = render_url_template(
        "https://example.com/{{ bad unclosed",
        "mytool",
        "1.0.0",
        "amd64",
        "linux",
    );
    assert_eq!(url, "https://example.com/{{ bad unclosed");
}

/// F1 — `render_url_template_with_ctx` exposes the full project template
/// surface (`Tag`, `ProjectName`, `Version`, `Major/Minor/Patch`,
/// `Commit`, `Branch`, `PreviousTag`, `Env.*`, `ArtifactName`, …) — not
/// just the lower-case 4-var subset. Dotted-variable configs that
/// reference `{{ .Tag }}` or `{{ .Env.X }}` in `url_template:` would
/// silently produce empty fields under the legacy renderer.
#[test]
fn test_render_url_template_with_ctx_full_surface() {
    use crate::util::render_url_template_with_ctx;
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());
    // Populate the full project template surface, then overlay per-artifact.
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.template_vars_mut().set("Version", "1.2.3");
    ctx.template_vars_mut().set("Major", "1");

    let url = render_url_template_with_ctx(
        &ctx,
        "https://github.com/{{ ProjectName }}/releases/download/{{ Tag }}/{{ name }}-{{ os }}-{{ arch }}.tar.gz",
        "myapp",
        "1.2.3",
        "amd64",
        "linux",
    );
    assert_eq!(
        url,
        "https://github.com/myapp/releases/download/v1.2.3/myapp-linux-amd64.tar.gz"
    );

    // Per-artifact `Os` / `Arch` overlays exposed alongside the lower-case
    // shorthand (the artifact-scoped render shape).
    let url2 = render_url_template_with_ctx(
        &ctx,
        "https://example.com/{{ ProjectName }}-{{ Os }}-{{ Arch }}.zip",
        "myapp",
        "1.2.3",
        "amd64",
        "windows",
    );
    assert_eq!(url2, "https://example.com/myapp-windows-amd64.zip");
}

// -----------------------------------------------------------------------
// filter_by_variant tests
// -----------------------------------------------------------------------

#[test]
fn test_filter_by_variant_no_filter_passes_all() {
    let artifacts = vec![
        OsArtifact {
            url: "u1".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "amd64".into(),
            id: None,
            amd64_variant: Some("v1".into()),
            arm_variant: None,
            binary: None,
        },
        OsArtifact {
            url: "u2".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "amd64".into(),
            id: None,
            amd64_variant: Some("v3".into()),
            arm_variant: None,
            binary: None,
        },
    ];
    let result = filter_by_variant(artifacts, None, None);
    assert_eq!(result.len(), 2);
}

#[test]
fn test_filter_by_variant_amd64_v1() {
    let artifacts = vec![
        OsArtifact {
            url: "v1".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "amd64".into(),
            id: None,
            amd64_variant: Some("v1".into()),
            arm_variant: None,
            binary: None,
        },
        OsArtifact {
            url: "v3".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "amd64".into(),
            id: None,
            amd64_variant: Some("v3".into()),
            arm_variant: None,
            binary: None,
        },
        OsArtifact {
            url: "arm64".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "arm64".into(),
            id: None,
            amd64_variant: None,
            arm_variant: None,
            binary: None,
        },
    ];
    let result = filter_by_variant(artifacts, Some("v1"), None);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].url, "v1");
    assert_eq!(result[1].url, "arm64"); // non-amd64 passes through
}

#[test]
fn test_filter_by_variant_amd64_no_metadata_passes() {
    // Artifacts without amd64_variant metadata pass through.
    let artifacts = vec![OsArtifact {
        url: "u1".into(),
        sha256: "s".into(),
        os: "linux".into(),
        arch: "amd64".into(),
        id: None,
        amd64_variant: None,
        arm_variant: None,
        binary: None,
    }];
    let result = filter_by_variant(artifacts, Some("v1"), None);
    assert_eq!(result.len(), 1);
}

#[test]
fn test_filter_by_variant_arm_filter() {
    let artifacts = vec![
        OsArtifact {
            url: "arm6".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "armv6".into(),
            id: None,
            amd64_variant: None,
            arm_variant: Some("6".into()),
            binary: None,
        },
        OsArtifact {
            url: "arm7".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "armv7".into(),
            id: None,
            amd64_variant: None,
            arm_variant: Some("7".into()),
            binary: None,
        },
    ];
    let result = filter_by_variant(artifacts, None, Some("7"));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].url, "arm7");
}

#[test]
fn test_filter_by_variant_combined() {
    let artifacts = vec![
        OsArtifact {
            url: "amd64-v1".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "amd64".into(),
            id: None,
            amd64_variant: Some("v1".into()),
            arm_variant: None,
            binary: None,
        },
        OsArtifact {
            url: "amd64-v3".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "amd64".into(),
            id: None,
            amd64_variant: Some("v3".into()),
            arm_variant: None,
            binary: None,
        },
        OsArtifact {
            url: "arm6".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "armv6".into(),
            id: None,
            amd64_variant: None,
            arm_variant: Some("6".into()),
            binary: None,
        },
        OsArtifact {
            url: "arm7".into(),
            sha256: "s".into(),
            os: "linux".into(),
            arch: "armv7".into(),
            id: None,
            amd64_variant: None,
            arm_variant: Some("7".into()),
            binary: None,
        },
    ];
    let result = filter_by_variant(artifacts, Some("v1"), Some("7"));
    assert_eq!(result.len(), 2);
    assert!(result.iter().any(|a| a.url == "amd64-v1"));
    assert!(result.iter().any(|a| a.url == "arm7"));
}

// -----------------------------------------------------------------------
// should_skip_upload tests
// -----------------------------------------------------------------------

#[test]
fn test_should_skip_upload_true_string() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let val = StringOrBool::String("true".to_string());
    assert!(should_skip_upload(Some(&val), &ctx, &test_log()).unwrap());
}

#[test]
fn test_should_skip_upload_true_bool() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let val = StringOrBool::Bool(true);
    assert!(should_skip_upload(Some(&val), &ctx, &test_log()).unwrap());
}

#[test]
fn test_should_skip_upload_false_when_none() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(!should_skip_upload(None, &ctx, &test_log()).unwrap());
}

#[test]
fn test_should_skip_upload_explicit_false_string() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let val = StringOrBool::String("false".to_string());
    assert!(!should_skip_upload(Some(&val), &ctx, &test_log()).unwrap());
}

#[test]
fn test_should_skip_upload_explicit_false_bool() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let val = StringOrBool::Bool(false);
    assert!(!should_skip_upload(Some(&val), &ctx, &test_log()).unwrap());
}

#[test]
fn test_should_skip_upload_auto_skips_prerelease() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set("Prerelease", "rc.1");
    let val = StringOrBool::String("auto".to_string());
    assert!(should_skip_upload(Some(&val), &ctx, &test_log()).unwrap());
}

#[test]
fn test_should_skip_upload_auto_does_not_skip_stable() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set("Prerelease", "");
    let val = StringOrBool::String("auto".to_string());
    assert!(!should_skip_upload(Some(&val), &ctx, &test_log()).unwrap());
}

#[test]
fn test_should_skip_upload_auto_does_not_skip_when_no_prerelease_var() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let val = StringOrBool::String("auto".to_string());
    assert!(!should_skip_upload(Some(&val), &ctx, &test_log()).unwrap());
}

#[test]
fn should_skip_publisher_with_if_honors_skip_upload_auto_on_prerelease() {
    // The shared gate routes skip_upload through should_skip_upload so the
    // `auto` value (skip on prerelease) is honored — a bare bool-eval would
    // treat `auto` as an unknown string and never skip, regressing the
    // winget/scoop callers that route through this helper.
    use super::config::should_skip_publisher_with_if;
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};

    let auto = StringOrBool::String("auto".to_string());

    let mut pre_ctx = Context::new(Config::default(), ContextOptions::default());
    pre_ctx.template_vars_mut().set("Prerelease", "rc.1");
    assert!(
        should_skip_publisher_with_if(&pre_ctx, None, Some(&auto), None, "x", &test_log()).unwrap(),
        "skip_upload: auto must skip when Prerelease is set"
    );

    let mut stable_ctx = Context::new(Config::default(), ContextOptions::default());
    stable_ctx.template_vars_mut().set("Prerelease", "");
    assert!(
        !should_skip_publisher_with_if(&stable_ctx, None, Some(&auto), None, "x", &test_log())
            .unwrap(),
        "skip_upload: auto must NOT skip a stable release"
    );
}

#[test]
fn test_should_skip_upload_template_rendered() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set_env("SKIP", "true");
    let val = StringOrBool::String("{{ .Env.SKIP }}".to_string());
    assert!(should_skip_upload(Some(&val), &ctx, &test_log()).unwrap());
}

#[test]
fn test_should_skip_upload_template_rendered_false() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set_env("SKIP", "false");
    let val = StringOrBool::String("{{ .Env.SKIP }}".to_string());
    assert!(!should_skip_upload(Some(&val), &ctx, &test_log()).unwrap());
}

// -----------------------------------------------------------------------
// resolve_repo_owner_name tests
// -----------------------------------------------------------------------

#[test]
fn test_resolve_repo_owner_name_modern_only() {
    use anodizer_core::config::RepositoryConfig;
    let repo = RepositoryConfig {
        owner: Some("a".into()),
        name: Some("b".into()),
        ..Default::default()
    };
    let got = resolve_repo_owner_name(Some(&repo));
    assert_eq!(got, Some(("a".to_string(), "b".to_string())));
}

#[test]
fn test_resolve_repo_owner_name_neither() {
    let got = resolve_repo_owner_name(None);
    assert_eq!(got, None);
}

#[test]
fn test_resolve_repo_owner_name_partial_returns_none() {
    use anodizer_core::config::RepositoryConfig;
    let repo = RepositoryConfig {
        branch: Some("main".into()),
        ..Default::default()
    };
    let got = resolve_repo_owner_name(Some(&repo));
    assert_eq!(got, None);
}

// -----------------------------------------------------------------------
// render_or_warn regression: malformed template must NOT propagate as
// Err; instead, the raw value is preserved and a warning is emitted.
// Pins the warn-and-fallback path against a future drift back to
// `unwrap_or_else(|_| raw.clone())` (silent swallow) or `with_context()`
// (hard-fail).
// -----------------------------------------------------------------------

/// Lenient (production dry-run / snapshot / nightly): a malformed Tera
/// template (`{{ unclosed`) feeding `render_or_warn` must NOT error — it
/// yields the raw value back (not Err, not empty) and warns on stderr. This
/// pins the warn-and-fallback path so a forgiving release stays forgiving.
#[test]
fn test_render_or_warn_falls_back_on_malformed_template() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let log = StageLogger::new("publish", Verbosity::Normal);

    let raw = "{{ unclosed";
    let out = render_or_warn(&ctx, &log, "aur.name", raw)
        .expect("lenient render must not error on a malformed template");
    assert_eq!(
        out, raw,
        "malformed template must fall back to raw value, got {out:?}"
    );
}

/// Strict (the pre-publish guard's render pass, or the user's global
/// `--strict`): the SAME malformed template must propagate `Err`, naming the
/// field, so a broken publisher template fails the release before any
/// irreversible publisher fires instead of being swallowed.
#[test]
fn test_render_or_warn_errors_on_malformed_template_when_strict() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.set_render_strict(true);
    let log = StageLogger::new("publish", Verbosity::Normal);

    let err = render_or_warn(&ctx, &log, "winget.description", "{{ unclosed")
        .expect_err("strict render must propagate the malformed-template error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("winget.description"),
        "strict error names the field: {msg}"
    );
}

/// Strict via the user's global `--strict` (`options.strict`) — distinct from
/// the guard's transient `render_strict` flag — also makes the same malformed
/// template error, proving `--strict` hardens renders everywhere, not just
/// under the guard.
#[test]
fn test_render_or_warn_errors_on_malformed_template_under_global_strict() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let opts = ContextOptions {
        strict: true,
        ..ContextOptions::default()
    };
    let ctx = Context::new(Config::default(), opts);
    let log = StageLogger::new("publish", Verbosity::Normal);

    let err = render_or_warn(&ctx, &log, "scoop.name", "{{ unclosed")
        .expect_err("global --strict must make a malformed template error");
    assert!(format!("{err:#}").contains("scoop.name"));
}

/// Well-formed templates render normally — pin the success path so a
/// future refactor that breaks the Ok branch trips this test.
#[test]
fn test_render_or_warn_renders_well_formed_template() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let mut config = Config::default();
    config.project_name = "myproj".to_string();
    let ctx = Context::new(config, ContextOptions::default());
    let log = StageLogger::new("publish", Verbosity::Normal);

    let out = render_or_warn(&ctx, &log, "aur.name", "{{ .ProjectName }}-bin").unwrap();
    assert_eq!(out, "myproj-bin");
}

// ---------------------------------------------------------------------------
// cmd / token-redaction tests
// ---------------------------------------------------------------------------
//
// `redact_output_token` and `replace_bytes` scrub tokens from
// `std::process::Output` before its bytes flow into `StageLogger::error`
// or an `anyhow::bail!` chain. Regression coverage for the C1 finding:
// "git clone failure with token-bearing URL leaks the token via stderr".

mod redact_output_token_tests {
    use super::super::cmd::{redact_output_token, replace_bytes};
    use std::process::Output;

    /// Build a synthetic `Output` for the redaction test cases.
    ///
    /// `redact_output_token` only reads `output.stderr` / `output.stdout`,
    /// so any concrete `ExitStatus` works here. We spawn `true` (Unix) or
    /// `cmd /c exit 0` (Windows) just to obtain a real `ExitStatus` value,
    /// since `ExitStatus` cannot be constructed directly in stable Rust.
    fn failing_output(stderr: &[u8], stdout: &[u8]) -> Output {
        let real = std::process::Command::new("true")
            .output()
            .or_else(|_| {
                std::process::Command::new("cmd")
                    .args(["/c", "exit", "0"])
                    .output()
            })
            .unwrap();
        Output {
            status: real.status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn redact_output_token_replaces_in_stderr_and_stdout() {
        let stderr = b"fatal: cannot access 'https://x-access-token:secret123@host/repo.git'";
        let stdout = b"see also secret123 here";
        let out = failing_output(stderr, stdout);
        let redacted = redact_output_token(out, Some("secret123"));

        let s_err = String::from_utf8_lossy(&redacted.stderr);
        let s_out = String::from_utf8_lossy(&redacted.stdout);

        assert!(
            !s_err.contains("secret123"),
            "stderr must not retain the token after redaction: {s_err}"
        );
        assert!(
            s_err.contains("<REDACTED_TOKEN>"),
            "stderr must contain the redaction marker: {s_err}"
        );
        assert!(
            !s_out.contains("secret123"),
            "stdout must not retain the token after redaction: {s_out}"
        );
        assert!(
            s_out.contains("<REDACTED_TOKEN>"),
            "stdout must contain the redaction marker: {s_out}"
        );
    }

    #[test]
    fn redact_output_token_no_token_passthrough() {
        let stderr = b"fatal: noise without a secret";
        let stdout = b"normal output";
        let out = failing_output(stderr, stdout);
        let redacted = redact_output_token(out, None);
        assert_eq!(redacted.stderr, stderr);
        assert_eq!(redacted.stdout, stdout);
    }

    #[test]
    fn redact_output_token_empty_token_passthrough() {
        // Empty secret must NOT replace every empty substring — that would
        // turn `abc` into `<REDACTED_TOKEN>a<REDACTED_TOKEN>...`.
        let stderr = b"abc";
        let stdout = b"def";
        let out = failing_output(stderr, stdout);
        let redacted = redact_output_token(out, Some(""));
        assert_eq!(redacted.stderr, stderr);
        assert_eq!(redacted.stdout, stdout);
    }

    #[test]
    fn replace_bytes_overlapping_collapses_to_non_overlapping() {
        // Pin the chosen semantics: needle `aa` in haystack `aaaa` produces
        // two replacements (after each match the cursor jumps past the
        // consumed needle), not three. Documented in `replace_bytes`'s
        // doc comment.
        let out = replace_bytes(b"aaaa", b"aa", b"X");
        assert_eq!(out, b"XX");
    }

    #[test]
    fn replace_bytes_empty_needle_passthrough() {
        let out = replace_bytes(b"abc", b"", b"X");
        assert_eq!(out, b"abc");
    }

    #[test]
    fn replace_bytes_empty_haystack_passthrough() {
        let out = replace_bytes(b"", b"abc", b"X");
        assert_eq!(out, b"");
    }

    #[test]
    fn replace_bytes_multiple_non_overlapping_matches() {
        let out = replace_bytes(b"foo bar foo bar", b"foo", b"X");
        assert_eq!(out, b"X bar X bar");
    }
}

// ===========================================================================
// Q-author1: resolve_commit_opts template-rendering tests
// ===========================================================================

#[cfg(test)]
mod commit_opts_tests {
    use super::super::commit::resolve_commit_opts;
    use anodizer_core::config::{CommitAuthorConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    /// Build a minimal Context where ProjectName, Version, and one Env var
    /// are set, so `{{ ProjectName }}` and `{{ Env.X }}` template expressions
    /// render predictably.
    fn ctx_for_template_tests() -> Context {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            ..Default::default()
        }];
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v1.2.3");
        ctx.template_vars_mut().set("Version", "1.2.3");
        ctx.template_vars_mut().set_env("BOT_NAME", "release-bot");
        ctx
    }

    /// Regression: commit-author resolution runs Tera templates
    /// over `name` and `email`. Anodizer must do the same so a config like
    ///
    ///     commit_author:
    ///       name: "{{ Env.BOT_NAME }}"
    ///       email: "{{ ProjectName }}-bot@example.com"
    ///
    /// produces the rendered values, not the raw template strings.
    #[test]
    fn test_resolve_commit_opts_renders_name_and_email() {
        let ctx = ctx_for_template_tests();
        let ca = CommitAuthorConfig {
            name: Some("{{ Env.BOT_NAME }}".to_string()),
            email: Some("{{ ProjectName }}-bot@example.com".to_string()),
            signing: None,
            use_github_app_token: false,
        };
        let opts = resolve_commit_opts(&ctx, Some(&ca), &super::test_log()).unwrap();
        assert_eq!(opts.author_name.as_deref(), Some("release-bot"));
        assert_eq!(opts.author_email.as_deref(), Some("myapp-bot@example.com"));
    }

    /// Lenient (no guard, no `--strict`): a malformed `name` template falls
    /// back to the literal string and warns, rather than failing the whole
    /// publish stage — keeping a forgiving dry-run / snapshot building.
    #[test]
    fn test_resolve_commit_opts_unrendered_template_falls_back_to_literal() {
        let ctx = ctx_for_template_tests();
        let ca = CommitAuthorConfig {
            name: Some("{{ unclosed".to_string()),
            email: None,
            signing: None,
            use_github_app_token: false,
        };
        let opts = resolve_commit_opts(&ctx, Some(&ca), &super::test_log()).unwrap();
        // The name is the literal template (because rendering failed lenient).
        assert_eq!(opts.author_name.as_deref(), Some("{{ unclosed"));
    }

    /// Strict (the pre-publish guard's render pass, or the user's global
    /// `--strict`): the SAME malformed `name` template propagates `Err`,
    /// naming the field, so a broken commit-author template fails the release
    /// before any irreversible PR-publisher pushes a commit.
    #[test]
    fn test_resolve_commit_opts_malformed_template_errors_when_strict() {
        let ctx = ctx_for_template_tests();
        ctx.set_render_strict(true);
        let ca = CommitAuthorConfig {
            name: Some("{{ unclosed".to_string()),
            email: None,
            signing: None,
            use_github_app_token: false,
        };
        let err = resolve_commit_opts(&ctx, Some(&ca), &super::test_log())
            .expect_err("strict render must propagate a malformed commit_author.name");
        assert!(format!("{err:#}").contains("commit_author.name"));
    }

    /// `use_github_app_token: true` propagates onto the resulting
    /// `CommitOptions`, so downstream `commit_and_push_with_opts` knows to
    /// skip the explicit `-c user.name=` / `-c user.email=` overrides.
    #[test]
    fn test_resolve_commit_opts_propagates_use_github_app_token() {
        let ctx = ctx_for_template_tests();
        let ca = CommitAuthorConfig {
            name: Some("override-name".to_string()),
            email: Some("override@example.com".to_string()),
            signing: None,
            use_github_app_token: true,
        };
        let opts = resolve_commit_opts(&ctx, Some(&ca), &super::test_log()).unwrap();
        assert!(
            opts.use_github_app_token,
            "use_github_app_token must propagate from config to CommitOptions"
        );
        // Resolved name/email are still present (in case the consumer ever
        // wants to surface them in logs); the consumer toggle is what
        // determines whether they are emitted on the wire.
        assert_eq!(opts.author_name.as_deref(), Some("override-name"));
        assert_eq!(opts.author_email.as_deref(), Some("override@example.com"));
    }

    /// When neither commit_author nor legacy fields are set and `git config
    /// user.{name,email}` has nothing useful, the built-in anodizer defaults
    /// are returned. Templates do not enter the picture in this path.
    #[test]
    fn test_resolve_commit_opts_no_config_uses_defaults() {
        let ctx = ctx_for_template_tests();
        let opts = resolve_commit_opts(&ctx, None, &super::test_log()).unwrap();
        // We can't assert the exact value because it depends on the local
        // git config of the test environment, but it must be Some(...).
        assert!(opts.author_name.is_some());
        assert!(opts.author_email.is_some());
        assert!(!opts.use_github_app_token);
    }
}
