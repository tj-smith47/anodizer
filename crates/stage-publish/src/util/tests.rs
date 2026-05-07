//! Tests for the `util/` submodules. Externalised from the original
//! `util.rs`'s bottom `mod tests` block.

#![allow(clippy::field_reassign_with_default)]

use super::artifacts::{
    OsArtifact, filter_by_variant, filter_os_artifacts_by_ids, find_all_platform_artifacts,
    find_artifacts_by_os, infer_arch, infer_os,
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

    let linux = find_artifacts_by_os(&ctx, "mytool", "linux");
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

    let darwin = find_artifacts_by_os(&ctx, "mytool", "darwin");
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

    let windows = find_artifacts_by_os(&ctx, "mytool", "windows");
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

    let all = find_all_platform_artifacts(&ctx, "mytool");
    assert_eq!(all.len(), 3);
    assert!(all.iter().any(|a| a.os == "linux" && a.arch == "amd64"));
    assert!(all.iter().any(|a| a.os == "darwin" && a.arch == "arm64"));
    assert!(all.iter().any(|a| a.os == "windows" && a.arch == "amd64"));
}

#[test]
fn test_find_all_platform_artifacts_empty() {
    let ctx = ctx_with_artifacts("mytool", vec![]);
    let all = find_all_platform_artifacts(&ctx, "mytool");
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
    let all = find_all_platform_artifacts(&ctx, "other_tool");
    assert!(all.is_empty());
}

// -----------------------------------------------------------------------
// OsArtifact id field tests
// -----------------------------------------------------------------------

#[test]
fn test_os_artifact_has_id_from_metadata() {
    let mut config = Config::default();
    config.crates = vec![CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    }];
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut meta = HashMap::new();
    meta.insert(
        "url".to_string(),
        "https://example.com/a.tar.gz".to_string(),
    );
    meta.insert("sha256".to_string(), "abc".to_string());
    meta.insert("id".to_string(), "my-archive".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/a.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "mytool".to_string(),
        metadata: meta,
        size: None,
    });

    let all = find_all_platform_artifacts(&ctx, "mytool");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id.as_deref(), Some("my-archive"));
}

#[test]
fn test_os_artifact_id_is_none_when_not_in_metadata() {
    let ctx = ctx_with_artifacts(
        "mytool",
        vec![(
            "x86_64-unknown-linux-gnu",
            "https://example.com/a.tar.gz",
            "abc",
        )],
    );
    let all = find_all_platform_artifacts(&ctx, "mytool");
    assert_eq!(all.len(), 1);
    assert!(all[0].id.is_none());
}

// -----------------------------------------------------------------------
// filter_os_artifacts_by_ids tests
// -----------------------------------------------------------------------

#[test]
fn test_filter_os_artifacts_by_ids_none_passes_all() {
    let artifacts = vec![
        OsArtifact {
            url: "u1".to_string(),
            sha256: "s1".to_string(),
            os: "linux".to_string(),
            arch: "amd64".to_string(),
            id: Some("a".to_string()),
            amd64_variant: None,
            arm_variant: None,
            binary: None,
        },
        OsArtifact {
            url: "u2".to_string(),
            sha256: "s2".to_string(),
            os: "darwin".to_string(),
            arch: "arm64".to_string(),
            id: Some("b".to_string()),
            amd64_variant: None,
            arm_variant: None,
            binary: None,
        },
    ];
    let result = filter_os_artifacts_by_ids(artifacts, None);
    assert_eq!(result.len(), 2);
}

#[test]
fn test_filter_os_artifacts_by_ids_filters_matching() {
    let artifacts = vec![
        OsArtifact {
            url: "u1".to_string(),
            sha256: "s1".to_string(),
            os: "linux".to_string(),
            arch: "amd64".to_string(),
            id: Some("keep-me".to_string()),
            amd64_variant: None,
            arm_variant: None,
            binary: None,
        },
        OsArtifact {
            url: "u2".to_string(),
            sha256: "s2".to_string(),
            os: "darwin".to_string(),
            arch: "arm64".to_string(),
            id: Some("drop-me".to_string()),
            amd64_variant: None,
            arm_variant: None,
            binary: None,
        },
        OsArtifact {
            url: "u3".to_string(),
            sha256: "s3".to_string(),
            os: "windows".to_string(),
            arch: "amd64".to_string(),
            id: None,
            amd64_variant: None,
            arm_variant: None,
            binary: None,
        },
    ];
    let ids = vec!["keep-me".to_string()];
    let result = filter_os_artifacts_by_ids(artifacts, Some(&ids));
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].url, "u1");
}

#[test]
fn test_filter_os_artifacts_by_ids_empty_ids_returns_nothing() {
    let artifacts = vec![OsArtifact {
        url: "u1".to_string(),
        sha256: "s1".to_string(),
        os: "linux".to_string(),
        arch: "amd64".to_string(),
        id: Some("a".to_string()),
        amd64_variant: None,
        arm_variant: None,
        binary: None,
    }];
    let ids: Vec<String> = vec![];
    let result = filter_os_artifacts_by_ids(artifacts, Some(&ids));
    assert!(result.is_empty());
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
    assert!(should_skip_upload(Some(&val), &ctx, &test_log()));
}

#[test]
fn test_should_skip_upload_true_bool() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let val = StringOrBool::Bool(true);
    assert!(should_skip_upload(Some(&val), &ctx, &test_log()));
}

#[test]
fn test_should_skip_upload_false_when_none() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(!should_skip_upload(None, &ctx, &test_log()));
}

#[test]
fn test_should_skip_upload_explicit_false_string() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let val = StringOrBool::String("false".to_string());
    assert!(!should_skip_upload(Some(&val), &ctx, &test_log()));
}

#[test]
fn test_should_skip_upload_explicit_false_bool() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let val = StringOrBool::Bool(false);
    assert!(!should_skip_upload(Some(&val), &ctx, &test_log()));
}

#[test]
fn test_should_skip_upload_auto_skips_prerelease() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set("Prerelease", "rc.1");
    let val = StringOrBool::String("auto".to_string());
    assert!(should_skip_upload(Some(&val), &ctx, &test_log()));
}

#[test]
fn test_should_skip_upload_auto_does_not_skip_stable() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set("Prerelease", "");
    let val = StringOrBool::String("auto".to_string());
    assert!(!should_skip_upload(Some(&val), &ctx, &test_log()));
}

#[test]
fn test_should_skip_upload_auto_does_not_skip_when_no_prerelease_var() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let ctx = Context::new(Config::default(), ContextOptions::default());
    let val = StringOrBool::String("auto".to_string());
    assert!(!should_skip_upload(Some(&val), &ctx, &test_log()));
}

#[test]
fn test_should_skip_upload_template_rendered() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set_env("SKIP", "true");
    let val = StringOrBool::String("{{ .Env.SKIP }}".to_string());
    assert!(should_skip_upload(Some(&val), &ctx, &test_log()));
}

#[test]
fn test_should_skip_upload_template_rendered_false() {
    use anodizer_core::config::{Config, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    ctx.template_vars_mut().set_env("SKIP", "false");
    let val = StringOrBool::String("{{ .Env.SKIP }}".to_string());
    assert!(!should_skip_upload(Some(&val), &ctx, &test_log()));
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
// (hard-fail). Source: Session C / Group E review deferral 2026-04-28.
// -----------------------------------------------------------------------

/// A malformed Tera template (`{{ unclosed`) feeding `render_or_warn`
/// must yield the raw value back (not Err, not empty). The warning
/// surfaces on stderr — the unit assertion focuses on the fallback
/// value which is the load-bearing wire-shape contract.
#[test]
fn test_render_or_warn_falls_back_on_malformed_template() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let log = StageLogger::new("publish", Verbosity::Normal);

    let raw = "{{ unclosed";
    let out = render_or_warn(&ctx, &log, "aur.name", raw);
    assert_eq!(
        out, raw,
        "malformed template must fall back to raw value, got {out:?}"
    );
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

    let out = render_or_warn(&ctx, &log, "aur.name", "{{ .ProjectName }}-bin");
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
