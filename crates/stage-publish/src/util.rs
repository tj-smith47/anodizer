use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anyhow::{Context as _, Result};
use std::process::Command;

/// Run a command with the given program and arguments, failing with `context_msg`
/// on spawn failure or non-zero exit.
pub(crate) fn run_cmd(program: &str, args: &[&str], context_msg: &str) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("{}: spawn", context_msg))?;
    if !status.success() {
        anyhow::bail!("{}: exited with {}", context_msg, status);
    }
    Ok(())
}

/// Run a command in a specific working directory, failing with `context_msg`
/// on spawn failure or non-zero exit.
pub(crate) fn run_cmd_in(
    dir: &std::path::Path,
    program: &str,
    args: &[&str],
    context_msg: &str,
) -> Result<()> {
    let status = Command::new(program)
        .current_dir(dir)
        .args(args)
        .status()
        .with_context(|| format!("{}: spawn", context_msg))?;
    if !status.success() {
        anyhow::bail!("{}: exited with {}", context_msg, status);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// YAML quoting (shared by winget, krew, and any other YAML-producing publisher)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// OS / architecture inference from target triples
// ---------------------------------------------------------------------------
//
// The functions below provide a two-layer normalisation scheme:
//
// 1. **Generic inference** (`infer_os` / `infer_arch`):
//    Map a Rust-style target triple (e.g. `x86_64-unknown-linux-gnu`,
//    `aarch64-apple-darwin`) to a canonical short form used internally
//    by `OsArtifact` (`"linux"`, `"darwin"`, `"windows"`, `"amd64"`,
//    `"arm64"`).
//
// 2. **Publisher-specific mapping** (e.g. `krew_os`, `krew_arch` in krew.rs):
//    Translate the canonical form to whatever the target ecosystem expects.
//    For Krew the mapping is effectively a no-op today, but keeping a
//    separate layer means we can adjust for future drift without touching
//    the shared inference code.
//
// Both `find_artifacts_by_os` and `find_all_platform_artifacts` use these
// shared helpers so the inference logic lives in exactly one place.

/// Infer the canonical OS string from a target triple.
///
/// Returns one of `"linux"`, `"darwin"`, `"windows"`, or the provided
/// `fallback` when the triple does not match any known pattern.
pub(crate) fn infer_os<'a>(target: &str, fallback: &'a str) -> &'a str {
    if target.contains("linux") {
        // Returns a 'static str that trivially outlives 'a
        return known_os_str("linux");
    }
    if target.contains("darwin") || target.contains("apple") {
        return known_os_str("darwin");
    }
    if target.contains("windows") {
        return known_os_str("windows");
    }
    fallback
}

/// Infer the canonical architecture string from a target triple.
///
/// Returns one of `"arm64"`, `"amd64"`, or `"unknown"`.
pub(crate) fn infer_arch(target: &str) -> &'static str {
    if target.contains("aarch64") || target.contains("arm64") {
        "arm64"
    } else if target.contains("x86_64") || target.contains("amd64") {
        "amd64"
    } else {
        "unknown"
    }
}

/// Map a known OS name to a `&'static str` literal.
///
/// Only handles the fixed set `"linux"`, `"darwin"`, `"windows"`; anything
/// else maps to `"unknown"`.
fn known_os_str(s: &str) -> &'static str {
    match s {
        "linux" => "linux",
        "darwin" => "darwin",
        "windows" => "windows",
        _ => "unknown",
    }
}

/// Describes the OS + architecture of an artifact match.
pub(crate) struct OsArtifact {
    pub url: String,
    pub sha256: String,
    pub os: String,
    pub arch: String,
}

/// Convert a single `Artifact` reference into an `OsArtifact`, using the
/// shared `infer_os` / `infer_arch` helpers.
///
/// `os_fallback` is used when the OS cannot be determined from the target
/// triple (e.g. when calling from `find_artifacts_by_os` with a known needle).
fn artifact_to_os_artifact(a: &Artifact, os_fallback: &str) -> OsArtifact {
    let url = a
        .metadata
        .get("url")
        .cloned()
        .unwrap_or_else(|| a.path.to_string_lossy().into_owned());
    let sha256 = a.metadata.get("sha256").cloned().unwrap_or_default();
    let target = a.target.as_deref().unwrap_or("");
    OsArtifact {
        url,
        sha256,
        os: infer_os(target, os_fallback).to_string(),
        arch: infer_arch(target).to_string(),
    }
}

/// Find all Archive artifacts for the given crate whose target or path
/// matches `os_needle` (e.g. "linux", "darwin", "windows").
///
/// Returns a vec of `OsArtifact` with the URL, SHA256, and inferred
/// os/arch strings extracted from the target triple.
pub(crate) fn find_artifacts_by_os(
    ctx: &Context,
    crate_name: &str,
    os_needle: &str,
) -> Vec<OsArtifact> {
    ctx.artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name)
        .into_iter()
        .filter(|a| {
            a.target
                .as_deref()
                .map(|t| t.to_ascii_lowercase().contains(os_needle))
                .unwrap_or(false)
                || a.path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(os_needle)
        })
        .map(|a| artifact_to_os_artifact(a, os_needle))
        .collect()
}

/// Find all Archive artifacts for the given crate across all platforms.
///
/// Returns a vec of `OsArtifact` with the URL, SHA256, and inferred
/// os/arch strings extracted from the target triple.
pub(crate) fn find_all_platform_artifacts(ctx: &Context, crate_name: &str) -> Vec<OsArtifact> {
    ctx.artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name)
        .into_iter()
        .map(|a| artifact_to_os_artifact(a, "unknown"))
        .collect()
}

/// Find a Windows Archive artifact for the given crate and return `(url, sha256)`.
///
/// Returns `None` when no matching artifact exists.
pub(crate) fn find_windows_artifact(ctx: &Context, crate_name: &str) -> Option<(String, String)> {
    let artifact = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Archive, crate_name)
        .into_iter()
        .find(|a| {
            a.target
                .as_deref()
                .map(|t| t.contains("windows") || t.contains("pc-windows"))
                .unwrap_or(false)
                || a.path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("windows")
        })?;

    let url = artifact
        .metadata
        .get("url")
        .cloned()
        .unwrap_or_else(|| artifact.path.to_string_lossy().into_owned());
    let hash = artifact.metadata.get("sha256").cloned().unwrap_or_default();
    Some((url, hash))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodize_core::artifact::{Artifact, ArtifactKind};
    use anodize_core::config::{Config, CrateConfig};
    use anodize_core::context::{Context, ContextOptions};
    use std::collections::HashMap;
    use std::path::PathBuf;

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
                path: PathBuf::from(format!(
                    "dist/{}",
                    url.rsplit('/').next().unwrap_or("a.tar.gz")
                )),
                target: Some(target.to_string()),
                crate_name: crate_name.to_string(),
                metadata: meta,
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
        assert_eq!(infer_arch("wasm32-unknown-unknown"), "unknown");
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
}
