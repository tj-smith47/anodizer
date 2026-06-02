use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::context::Context;
use anodizer_core::{EnvSource, ProcessEnvSource};

// ---------------------------------------------------------------------------
// check_workspace_package — validate --package flag for workspace crates
// ---------------------------------------------------------------------------

/// If the Cargo.toml at `crate_path` has a `[workspace]` section with `members`,
/// verify that the build flags contain `--package` or `-p`. Returns an error
/// if the workspace is detected but no package flag is present.
pub(crate) fn check_workspace_package(crate_path: &str, flags: &[String]) -> Result<()> {
    let cargo_toml_path = Path::new(crate_path).join("Cargo.toml");
    let content = match std::fs::read_to_string(&cargo_toml_path) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let doc = match content.parse::<toml_edit::DocumentMut>() {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };

    // Check for [workspace] with members
    if let Some(ws) = doc.get("workspace")
        && ws.get("members").is_some()
    {
        // Check if flags contain --package or -p
        let has_package = flags.iter().any(|t| {
            t == "-p"
                || t.starts_with("--package")
                || t.starts_with("-p=")
                || t.starts_with("--package=")
        });
        if !has_package {
            anyhow::bail!(
                "you need to specify which workspace package to build, \
                     please add '--package=<name>' to your build flags"
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// find_workspace_root — walk up from crate path to find workspace Cargo.toml
// ---------------------------------------------------------------------------

/// Walk up from `crate_path` looking for a `Cargo.toml` that contains a
/// `[workspace]` section.  Returns the directory containing the workspace
/// root `Cargo.toml`, or `None` if no workspace root is found.
pub(crate) fn find_workspace_root(crate_path: &str) -> Option<PathBuf> {
    let mut dir = std::fs::canonicalize(crate_path).ok()?;
    // Walk upward from the crate directory
    loop {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists()
            && let Ok(content) = std::fs::read_to_string(&cargo_toml)
            && let Ok(doc) = content.parse::<toml_edit::DocumentMut>()
            && doc.get("workspace").is_some()
        {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// resolve_binary_path — check both relative target/ and workspace root target/
// ---------------------------------------------------------------------------

/// Resolve the actual binary path after a build.
///
/// Cargo places build artifacts in the workspace root's `target/` directory,
/// not in per-crate `target/` directories.  When the expected relative path
/// does not exist, this function tries the workspace root's target directory.
pub(crate) fn resolve_binary_path(expected: &Path, crate_path: &str) -> PathBuf {
    if expected.exists() {
        return expected.to_path_buf();
    }
    // Try workspace root target directory
    if let Some(ws_root) = find_workspace_root(crate_path) {
        let ws_path = ws_root.join(expected);
        if ws_path.exists() {
            return ws_path;
        }
    }
    // Return the original path — the caller will handle the error.
    expected.to_path_buf()
}

// ---------------------------------------------------------------------------
// cargo_target_dir_with_env — respect CARGO_TARGET_DIR / CARGO_BUILD_TARGET_DIR
// ---------------------------------------------------------------------------

/// Return the Cargo target directory, honoring per-build env config
/// (`build.env`) first and then falling back to `CARGO_TARGET_DIR` /
/// `CARGO_BUILD_TARGET_DIR` from the injected env source, finally
/// defaulting to `target`. Production wires up
/// [`anodizer_core::ProcessEnvSource`] via
/// [`anodizer_core::Context::env_source`]; tests inject a
/// [`anodizer_core::MapEnvSource`] so the fallback branches can be
/// exercised without mutating the process env.
///
/// The `build_env` parameter carries the per-target env map from
/// config, which is passed to the cargo Command but also needs to be
/// reflected here so that the predicted binary path matches where
/// cargo actually writes it.
pub(crate) fn cargo_target_dir_with_env<E: EnvSource + ?Sized>(
    build_env: Option<&HashMap<String, String>>,
    env: &E,
) -> PathBuf {
    // Check per-build env vars first — these override process env
    if let Some(map) = build_env {
        if let Some(dir) = map.get("CARGO_TARGET_DIR")
            && !dir.is_empty()
        {
            return PathBuf::from(dir);
        }
        if let Some(dir) = map.get("CARGO_BUILD_TARGET_DIR")
            && !dir.is_empty()
        {
            return PathBuf::from(dir);
        }
    }
    // Fall back to the injected env source
    if let Some(dir) = env.var("CARGO_TARGET_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = env.var("CARGO_BUILD_TARGET_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    PathBuf::from("target")
}

// ---------------------------------------------------------------------------
// resolve_reproducible_epoch — parse SOURCE_DATE_EPOCH with commit_timestamp fallback
// ---------------------------------------------------------------------------

pub fn resolve_reproducible_epoch(commit_timestamp: &str) -> Option<i64> {
    resolve_reproducible_epoch_with_env(commit_timestamp, &ProcessEnvSource)
}

/// Env-injectable form of [`resolve_reproducible_epoch`]. Production
/// wires up [`ProcessEnvSource`]; tests inject a
/// [`anodizer_core::MapEnvSource`] so the `SOURCE_DATE_EPOCH` override
/// can be driven without mutating the process env.
pub fn resolve_reproducible_epoch_with_env<E: EnvSource + ?Sized>(
    commit_timestamp: &str,
    env: &E,
) -> Option<i64> {
    let epoch = env
        .var("SOURCE_DATE_EPOCH")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| commit_timestamp.parse::<i64>().unwrap_or(0));
    if epoch > 0 { Some(epoch) } else { None }
}

// ---------------------------------------------------------------------------
// copy_from resolution helper
// ---------------------------------------------------------------------------

/// Resolve a copy_from job: look up the source binary from registered artifacts
/// (filtering by target **and** crate_name to avoid cross-crate collisions),
/// copy it to the destination, and return Ok.
pub(crate) fn resolve_copy_from(
    ctx: &Context,
    src: &Path,
    dst: &Path,
    target: &str,
    crate_name: &str,
) -> Result<()> {
    let resolved_src = ctx
        .artifacts
        .by_kind(ArtifactKind::Binary)
        .into_iter()
        .find(|a| {
            a.target.as_deref() == Some(target) && a.crate_name == crate_name && a.path == *src
        })
        .map(|a| a.path.clone())
        .unwrap_or_else(|| src.to_path_buf());

    std::fs::copy(&resolved_src, dst).with_context(|| {
        format!(
            "copy_from: failed to copy {} -> {}",
            resolved_src.display(),
            dst.display()
        )
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ensure_targets_installed — run `rustup target add` for cross-compilation targets
// ---------------------------------------------------------------------------

/// For each unique non-host target, run `rustup target add` to ensure the
/// target toolchain is installed. If `rustup` is not available (e.g. when
/// using cargo-cross or a pre-configured environment), this is silently skipped.
pub(crate) fn ensure_targets_installed(
    ctx: &Context,
    targets: &[String],
    log: &anodizer_core::log::StageLogger,
    dry_run: bool,
) -> Result<()> {
    let host = anodizer_core::partial::detect_host_target().unwrap_or_default();
    for target in targets {
        if target == &host {
            continue;
        }
        if dry_run {
            log.status(&format!("(dry-run) would run: rustup target add {target}"));
            continue;
        }
        let output = Command::new("rustup")
            .args(["target", "add", target])
            .output();
        match output {
            Ok(o) if o.status.success() => {
                log.verbose(&format!("ensured target installed: {target}"));
            }
            Ok(o) => {
                // GoReleaser parity: `rustup target add` failure is a hard
                // error (rust/build.go:60-62 returns
                // `fmt.Errorf("could not add target %s: %w: %s", ...)`).
                // The previous warn-and-continue let the subsequent
                // `cargo build --target=...` fail with a less-clear
                // "no such target" error.
                anyhow::bail!(
                    "rustup target add {target} failed: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(_) => {
                ctx.strict_guard(log, "rustup not found, skipping target installation")?;
                return Ok(()); // If rustup isn't available, skip all
            }
        }
    }
    Ok(())
}
