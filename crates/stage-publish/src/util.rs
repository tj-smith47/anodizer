use anodize_core::artifact::ArtifactKind;
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
    let hash = artifact
        .metadata
        .get("sha256")
        .cloned()
        .unwrap_or_default();
    Some((url, hash))
}
