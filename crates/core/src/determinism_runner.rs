//! Subprocess runner for the determinism harness.
//!
//! Allow-listed entry-point for `Command::new` in core (see
//! `.claude/rules/module-boundaries.md`). The determinism harness in
//! `crates/cli/src/determinism_harness.rs` is forbidden from spawning
//! processes directly per the same rule, so this module owns the
//! `anodize release --snapshot --skip=...` invocation that drives each
//! from-clean rebuild.
//!
//! Why a separate module: `Command::new` is an authorization boundary
//! (write-to-disk, network, env exfiltration); concentrating the
//! harness's one call site here keeps the security-relevant surface
//! reviewable.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Invoke the running `anodize` binary against `worktree_path` with the
/// supplied isolated env.
///
/// Pinning args:
/// - `release` — drives the full build-side pipeline.
/// - `--snapshot` — disables tag-cutting and tells stages to use the
///   pre-resolved SDE.
/// - `--skip=release,publish,blob,snapcraft-publish,announce` — strips
///   every side-effect-producing stage. Doubling N is safe in any env
///   because of this skip list.
///
/// The child env is fully replaced (`env_clear` then re-populate) so
/// host env vars cannot leak through and perturb the build. Caller
/// (the harness) constructs the env map.
pub fn run_build_pipeline_subprocess(
    anodize_binary: &Path,
    worktree_path: &Path,
    env: &HashMap<String, String>,
) -> Result<()> {
    let mut cmd = Command::new(anodize_binary);
    cmd.args([
        "release",
        "--snapshot",
        "--skip=release,publish,blob,snapcraft-publish,announce",
    ]);
    cmd.current_dir(worktree_path);
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    let status = cmd
        .status()
        .context("spawning anodize release for determinism harness")?;
    anyhow::ensure!(
        status.success(),
        "harness build pipeline failed in worktree {} (exit {:?})",
        worktree_path.display(),
        status.code()
    );
    Ok(())
}

/// Resolve the path of the currently-running `anodize` binary. Thin
/// wrapper over [`std::env::current_exe`] kept here so the harness side
/// doesn't have to touch `std::env` for binary resolution.
pub fn current_anodize_binary() -> Result<PathBuf> {
    std::env::current_exe().context("locating the currently-running anodize binary")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_binary_resolves_to_a_real_file() {
        // In test context, `current_exe` returns the test runner; the
        // path is just expected to be readable.
        let p = current_anodize_binary().unwrap();
        assert!(p.exists(), "current_exe should point at a real file");
    }

    #[test]
    fn run_build_pipeline_subprocess_fails_when_binary_missing() {
        let env = HashMap::new();
        let worktree = std::env::temp_dir();
        let bogus = PathBuf::from("/nonexistent/anodize-binary-for-tests");
        let res = run_build_pipeline_subprocess(&bogus, &worktree, &env);
        assert!(
            res.is_err(),
            "missing binary should surface as an error, not a panic"
        );
    }
}
