//! `cargo package` invocation for the determinism harness.
//!
//! Allow-listed entry point for `Command::new("cargo")` calls that drive
//! the `.crate` packaging side of the determinism harness. The harness
//! itself lives in `crates/cli/` which is forbid-listed for direct
//! subprocess spawn (see `.claude/rules/module-boundaries.md`); this
//! module owns the call site so the security surface stays small.
//!
//! Why `cargo package` and not `cargo publish`: `package` is the
//! offline-equivalent of `publish` minus the registry upload. It writes
//! the same `.crate` tarball that would have been pushed, so probing it
//! for byte-stability surfaces packaging non-determinism (mtimes, tar
//! ordering, `.cargo_vcs_info.json` content) without any network reach.
//!
//! Known non-determinism the harness will detect when this stage runs:
//!
//! - **File mtimes inside the tar**: cargo canonicalizes these to
//!   `SOURCE_DATE_EPOCH` since cargo 1.74 — the harness exports it via
//!   its hermetic env block so the workaround takes effect.
//! - **`.cargo_vcs_info.json` contents**: cargo writes the current git
//!   sha + dirty flag + timestamp. The harness's per-run worktree is
//!   detached at the same commit, so the sha line is stable; the dirty
//!   flag depends on whether `Cargo.lock` was touched between checkout
//!   and `cargo package`. The harness shells `cargo package` from a
//!   fresh worktree so the dirty flag should be `false` on both runs.
//! - **tar member ordering**: cargo sorts entries since 1.74; no
//!   per-call workaround needed.
//!
//! When the harness detects drift after these workarounds, the failure
//! is a real regression in cargo's reproducibility story — surface the
//! drift via the report and don't silently pass.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

use crate::log::StageLogger;
use std::process::Command;

/// Run `cargo package --workspace --no-verify --allow-dirty
/// --no-metadata` against `manifest_dir` with the supplied isolated env.
///
/// Pinning args:
/// - `--workspace` — assemble every workspace member's `.crate` in one
///   invocation. For a non-workspace single-crate repo cargo accepts
///   this flag and packages the single crate (verified against cargo
///   1.95). Single-call avoids per-crate manifest enumeration in the
///   harness.
/// - `--no-verify` — skip the compile-then-test pass cargo would
///   otherwise run inside the unpacked `.crate`. The harness only
///   cares about the packaging stage's byte output, not whether the
///   packaged bytes themselves compile (the prior `build` stage
///   already covered that).
/// - `--allow-dirty` — the per-run worktree may carry a `Cargo.lock`
///   diff from the harness's lockfile generation, or the harness's
///   `--remap-path-prefix` `RUSTFLAGS` may surface as a worktree
///   modification; the dirty check would otherwise abort with a hard
///   error that has nothing to do with packaging determinism.
/// - `--no-metadata` — suppress the warning cargo emits when a
///   `Cargo.toml` is missing `description` / `license` / `repository`.
///   Minimal fixture crates the harness's integration tests
///   bootstrap may legitimately lack these; the harness only cares
///   about byte-stability of whatever cargo emits.
///
/// `cargo_target_dir` is set as `CARGO_TARGET_DIR` (already exported
/// via `env`) so the `.crate` lands at
/// `<cargo_target_dir>/package/<name>-<version>.crate` — the location
/// the harness's discover step walks to pick up the artifacts.
///
/// `env` carries the harness's hermetic env block —
/// `SOURCE_DATE_EPOCH`, `CARGO_HOME`, `HOME`, `RUSTFLAGS`, `PATH`,
/// etc. The function `env_clear`s the child first so host env vars
/// cannot perturb the packaging step.
///
/// `log` governs cargo's packaging chatter: at default verbosity the
/// `Packaging`/`Archiving` lines are captured silently (surfaced only on
/// failure), and at `-v` they stream live — the same `status` vs `verbose`
/// register every other subprocess obeys.
///
/// Returns `Ok(())` on cargo exit 0; bubbles a context-wrapped error
/// otherwise so the harness's per-run loop can attach the run number.
pub fn package_workspace(
    manifest_dir: &Path,
    env: &HashMap<String, String>,
    log: &StageLogger,
) -> Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.arg("package")
        .arg("--workspace")
        .arg("--no-verify")
        .arg("--allow-dirty")
        .arg("--no-metadata");
    cmd.current_dir(manifest_dir);
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    crate::run::run_checked(&mut cmd, log, "cargo package")
        .with_context(|| format!("`cargo package` failed in {}", manifest_dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_workspace_fails_when_manifest_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let env: HashMap<String, String> = HashMap::new();
        let (log, _cap) = StageLogger::with_capture("test", crate::log::Verbosity::Normal);
        let res = package_workspace(tmp.path(), &env, &log);
        assert!(
            res.is_err(),
            "cargo package against a directory without Cargo.toml should error"
        );
    }
}
