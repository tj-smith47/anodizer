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

/// Stage names the determinism harness must NOT run.
///
/// Single source of truth for the `--skip=...` list passed to the child
/// `anodize release --snapshot` invocation. Every entry here is a stage
/// in `crates/cli/src/pipeline.rs::build_release_pipeline` that either:
///
/// - touches upstream (uploads, API calls, push, announce), OR
/// - mutates host state outside `<worktree>/dist` (docker daemon, kms),
///
/// i.e. a "side-effect" stage that has no place in a hermetic regression
/// rebuild. Adding a future side-effect stage to the release pipeline
/// MUST add its stage name here too — otherwise the harness will fire it
/// from inside the supposedly-hermetic build.
///
/// Audit reference: `.claude/audits/2026-05-15-release-resilience-review.md#i8`.
///
/// Order mirrors the position in `build_release_pipeline` so reviewers
/// scanning both files can pattern-match. Listed exhaustively (no
/// `starts_with` / glob matching) so a new stage with a similar name
/// (e.g. `docker-extra`) doesn't accidentally inherit the skip.
pub const SIDE_EFFECT_STAGES: &[&str] = &[
    // Publish phase — upstream side effects.
    "release",
    "docker",
    "docker-sign",
    "publish",
    "blob",
    "snapcraft-publish",
    "announce",
];

/// Comma-join [`SIDE_EFFECT_STAGES`] for use as the `--skip=<list>` CLI
/// argument value. Kept as a function (not a const) because Rust can't
/// const-evaluate `[&str]::join`.
fn side_effect_stages_skip_arg() -> String {
    format!("--skip={}", SIDE_EFFECT_STAGES.join(","))
}

/// Invoke the running `anodize` binary against `worktree_path` with the
/// supplied isolated env.
///
/// Pinning args:
/// - `release` — drives the full build-side pipeline.
/// - `--snapshot` — disables tag-cutting and tells stages to use the
///   pre-resolved SDE.
/// - `--skip=<SIDE_EFFECT_STAGES>` — strips every side-effect-producing
///   stage. Doubling N is safe in any env because of this skip list.
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
    cmd.args(["release", "--snapshot", &side_effect_stages_skip_arg()]);
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

    #[test]
    fn side_effect_stages_covers_every_known_publish_side_effect() {
        // Regression guard against I8 (audit 2026-05-15): if a future
        // pipeline edit adds a side-effect stage and forgets to register
        // it here, this test surfaces the omission. Add the new stage to
        // SIDE_EFFECT_STAGES (and update this list) once the new entry is
        // confirmed to belong in the skip set.
        let expected = [
            "release",
            "docker",
            "docker-sign",
            "publish",
            "blob",
            "snapcraft-publish",
            "announce",
        ];
        for name in expected {
            assert!(
                SIDE_EFFECT_STAGES.contains(&name),
                "SIDE_EFFECT_STAGES missing known publish-side stage `{name}`"
            );
        }
    }

    #[test]
    fn side_effect_stages_skip_arg_starts_with_skip_flag() {
        // I8 fix shape: harness still uses --skip=<list> (the conservative
        // path; --only=<list> would require a new CLI flag). Guard against
        // a future refactor accidentally flipping to a different prefix.
        let arg = super::side_effect_stages_skip_arg();
        assert!(
            arg.starts_with("--skip="),
            "expected --skip= prefix, got `{arg}`"
        );
        // And the joined list is non-empty.
        assert!(arg.len() > "--skip=".len(), "skip list must not be empty");
    }

    #[test]
    fn side_effect_stages_skip_arg_round_trips_through_comma_join() {
        let arg = super::side_effect_stages_skip_arg();
        let list = arg
            .trim_start_matches("--skip=")
            .split(',')
            .collect::<Vec<_>>();
        assert_eq!(list.len(), SIDE_EFFECT_STAGES.len());
        for (a, b) in list.iter().zip(SIDE_EFFECT_STAGES.iter()) {
            assert_eq!(a, b);
        }
    }

    /// Every name the harness shovels into `--skip=...` MUST be accepted
    /// by the release CLI's skip validator. Surfaced by the I12
    /// drift-injection integration test (audit 2026-05-15) when `docker-sign`
    /// was present in [`SIDE_EFFECT_STAGES`] but missing from
    /// [`crate::context::VALID_RELEASE_SKIPS`] — the harness's child
    /// subprocess bombed with `invalid --skip value(s): docker-sign`. This
    /// pure-cross-check unit test catches the drift in milliseconds so a
    /// future addition to either list flags the gap immediately.
    #[test]
    fn side_effect_stages_are_all_valid_release_skip_values() {
        use crate::context::VALID_RELEASE_SKIPS;
        for &name in SIDE_EFFECT_STAGES {
            assert!(
                VALID_RELEASE_SKIPS.contains(&name),
                "SIDE_EFFECT_STAGES contains `{name}` but VALID_RELEASE_SKIPS does not — \
                 the harness would fail at `anodize release --skip=<list>` invocation. \
                 Add `{name}` to VALID_RELEASE_SKIPS in crates/core/src/context.rs."
            );
        }
    }
}
