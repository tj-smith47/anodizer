use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::path::Path;
use std::process::Command;

/// Run a command in a specific working directory, failing with `label`
/// on spawn failure or non-zero exit.  Captures stdout/stderr so that
/// diagnostics are included in the error message.
pub(crate) fn run_cmd_in(dir: &Path, program: &str, args: &[&str], label: &str) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("{}: failed to run {} {}", label, program, args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        anyhow::bail!(
            "{}: {} {} failed (exit {})\nstderr: {}\nstdout: {}",
            label,
            program,
            args.join(" "),
            output.status.code().unwrap_or(-1),
            stderr,
            stdout
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Publisher config lookup
// ---------------------------------------------------------------------------

use anodize_core::config::{CrateConfig, PublishConfig};

/// Look up a crate's config and its `publish` section by name, returning a
/// descriptive error when either is missing.
pub(crate) fn get_publish_config<'a>(
    ctx: &'a Context,
    crate_name: &str,
    label: &str,
) -> Result<(&'a CrateConfig, &'a PublishConfig)> {
    let crate_cfg = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("{label}: crate '{crate_name}' not found in config"))?;

    let publish = crate_cfg
        .publish
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{label}: no publish config for '{crate_name}'"))?;

    Ok((crate_cfg, publish))
}

// ---------------------------------------------------------------------------
// Token resolution
// ---------------------------------------------------------------------------

/// Resolve an auth token from the context, then a publisher-specific env var,
/// then the generic `GITHUB_TOKEN` env var.
pub(crate) fn resolve_token(ctx: &Context, env_var: Option<&str>) -> Option<String> {
    ctx.options
        .token
        .clone()
        .or_else(|| env_var.and_then(|v| std::env::var(v).ok()))
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
}

// ---------------------------------------------------------------------------
// Git repo helpers  (clone, configure auth, commit, push)
// ---------------------------------------------------------------------------

/// Clone a git repo into `tmp_dir` using `http.extraheader` for auth (avoids
/// leaking tokens in URLs).  Also configures auth on the clone for subsequent
/// push operations.
pub(crate) fn clone_repo_with_auth(
    repo_url: &str,
    token: Option<&str>,
    tmp_dir: &Path,
    label: &str,
    log: &StageLogger,
) -> Result<()> {
    let auth_header;
    let mut clone_args: Vec<&str> = vec!["clone", "--depth=1"];
    if let Some(tok) = token {
        auth_header = format!("http.extraheader=Authorization: bearer {}", tok);
        clone_args.extend_from_slice(&["-c", &auth_header]);
    }
    clone_args.push(repo_url);
    let repo_path_str = tmp_dir.to_string_lossy();
    clone_args.push(&repo_path_str);

    let output = Command::new("git")
        .args(&clone_args)
        .output()
        .with_context(|| format!("{label}: git clone: spawn"))?;
    log.check_output(output, &format!("{label}: git clone"))?;

    // Configure auth for subsequent push operations in this repo clone.
    if let Some(tok) = token {
        run_cmd_in(
            tmp_dir,
            "git",
            &[
                "config",
                "http.extraheader",
                &format!("Authorization: bearer {}", tok),
            ],
            &format!("{label}: git config auth"),
        )?;
    }

    Ok(())
}

/// Stage files, commit, and push. Optionally creates a new branch first.
pub(crate) fn commit_and_push(
    repo_path: &Path,
    files: &[&str],
    message: &str,
    branch: Option<&str>,
    label: &str,
) -> Result<()> {
    if let Some(branch_name) = branch {
        run_cmd_in(
            repo_path,
            "git",
            &["checkout", "-b", branch_name],
            &format!("{label}: git checkout"),
        )?;
    }

    for file in files {
        run_cmd_in(
            repo_path,
            "git",
            &["add", file],
            &format!("{label}: git add"),
        )?;
    }

    run_cmd_in(
        repo_path,
        "git",
        &["commit", "-m", message],
        &format!("{label}: git commit"),
    )?;

    let push_args: Vec<&str> = if let Some(branch_name) = branch {
        vec!["push", "-u", "origin", branch_name]
    } else {
        vec!["push"]
    };

    run_cmd_in(repo_path, "git", &push_args, &format!("{label}: git push"))
}

// ---------------------------------------------------------------------------
// PR submission via `gh` CLI
// ---------------------------------------------------------------------------

/// Submit a pull request via the GitHub CLI. Logs a warning instead of failing
/// if `gh` is not available or the command exits non-zero.
pub(crate) fn submit_pr_via_gh(
    repo_path: &Path,
    upstream_repo: &str,
    head: &str,
    title: &str,
    body: &str,
    label: &str,
    log: &StageLogger,
) {
    let pr_result = Command::new("gh")
        .current_dir(repo_path)
        .args([
            "pr",
            "create",
            "--repo",
            upstream_repo,
            "--title",
            title,
            "--body",
            body,
            "--head",
            head,
        ])
        .output();

    match pr_result {
        Ok(output) if output.status.success() => {
            log.status(&format!("{label}: PR submitted"));
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            log.warn(&format!(
                "{label}: gh pr create exited with {} — you may need to create the PR manually{}",
                output.status,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", stderr)
                }
            ));
        }
        Err(e) => {
            log.warn(&format!(
                "{label}: could not run gh to create PR: {} — you may need to create the PR manually",
                e
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Windows artifact helper
// ---------------------------------------------------------------------------

/// Find a Windows Archive artifact and return `(url, sha256)`, or bail with a
/// descriptive error.
pub(crate) fn require_windows_artifact(
    ctx: &Context,
    crate_name: &str,
    label: &str,
) -> Result<(String, String)> {
    find_windows_artifact(ctx, crate_name).ok_or_else(|| {
        anyhow::anyhow!(
            "{}: no Windows archive artifact found for crate '{}'",
            label,
            crate_name
        )
    })
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
/// Delegates to [`anodize_core::target::map_target`] for the actual parsing.
/// Returns the mapped OS, or `fallback` when the OS is `"unknown"`.
pub(crate) fn infer_os(target: &str, fallback: &str) -> String {
    let (os, _) = anodize_core::target::map_target(target);
    if os == "unknown" {
        fallback.to_string()
    } else {
        os
    }
}

/// Infer the canonical architecture string from a target triple.
///
/// Delegates to [`anodize_core::target::map_target`] for the actual parsing.
pub(crate) fn infer_arch(target: &str) -> String {
    let (_, arch) = anodize_core::target::map_target(target);
    arch
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
        os: infer_os(target, os_fallback),
        arch: infer_arch(target),
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
    let a = find_artifacts_by_os(ctx, crate_name, "windows")
        .into_iter()
        .next()?;
    Some((a.url, a.sha256))
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
}
