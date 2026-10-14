//! Snapshot-mode SOURCE_DATE_EPOCH resolver.
//!
//! Resolves SDE in this priority order:
//!   1. `SOURCE_DATE_EPOCH` env var (seconds since epoch), the standard
//!      reproducibility contract a CI system or packager sets.
//!   2. `ANODIZE_SOURCE_DATE_EPOCH` env var, the older anodizer-specific
//!      spelling of the same override.
//!   3. HEAD commit timestamp, whether or not the working tree is dirty.
//!
//! The value is stamped into artifacts as a real date (archive entry
//! mtimes, the OCI `created` label, SBOM timestamps), so every source is a
//! date the operator or the commit chose. A dirty tree keeps the HEAD
//! timestamp: successive snapshot runs against the same tree still produce
//! byte-identical SDE.

use anyhow::{Context as _, Result};
use std::path::Path;

use crate::EnvSource;

/// Resolves the SOURCE_DATE_EPOCH for a snapshot-mode release run.
///
/// Returns seconds-since-epoch. See module docs for the resolution order.
pub fn resolve_snapshot_sde(repo: &Path) -> Result<i64> {
    resolve_snapshot_sde_with_env(repo, &crate::ProcessEnvSource)
}

/// Env-injectable form of [`resolve_snapshot_sde`]. Production wires up
/// `ProcessEnvSource`; tests inject a [`MapEnvSource`](crate::MapEnvSource)
/// to drive the env-var branches without mutating the process env.
pub fn resolve_snapshot_sde_with_env<E: EnvSource + ?Sized>(repo: &Path, env: &E) -> Result<i64> {
    for key in ["SOURCE_DATE_EPOCH", "ANODIZE_SOURCE_DATE_EPOCH"] {
        if let Some(v) = env.var(key) {
            return v
                .parse::<i64>()
                .with_context(|| format!("{key} is set but not a valid i64: {v}"));
        }
    }
    head_commit_timestamp(repo)
}

fn head_commit_timestamp(repo: &Path) -> Result<i64> {
    let out = crate::git::git_command()
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%ct"])
        .output()
        .context("failed to invoke git log -1 --format=%ct")?;
    if !out.status.success() {
        anyhow::bail!(
            "git log -1 --format=%ct failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let text = String::from_utf8(out.stdout)
        .context("git log %ct produced non-utf8 output")?
        .trim()
        .to_string();
    text.parse::<i64>()
        .with_context(|| format!("git log %ct returned non-i64 timestamp: {}", text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.arg("-C").arg(dir.path()).arg("init");
                cmd
            },
            "git",
        );
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.arg("-C")
                    .arg(dir.path())
                    .args(["config", "user.email", "test@example.com"]);
                cmd
            },
            "git",
        );
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.arg("-C")
                    .arg(dir.path())
                    .args(["config", "user.name", "test"]);
                cmd
            },
            "git",
        );
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.arg("-C").arg(dir.path()).args(["add", "a.txt"]);
                cmd
            },
            "git",
        );
        // Pin commit timestamp deterministically so test assertions are stable.
        anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.arg("-C")
                    .arg(dir.path())
                    .env("GIT_AUTHOR_DATE", "1715000000 +0000")
                    .env("GIT_COMMITTER_DATE", "1715000000 +0000")
                    .args(["commit", "-m", "init"]);
                cmd
            },
            "git",
        );
        dir
    }

    #[test]
    fn snapshot_sde_uses_source_date_epoch_when_set() {
        // 1767225600 = 2026-01-01T00:00:00Z; the fixture commit is 2024-05-06.
        let dir = init_repo();
        let env = crate::MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1767225600");
        let sde = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap();
        assert_eq!(sde, 1_767_225_600);
    }

    #[test]
    fn source_date_epoch_wins_over_the_legacy_spelling() {
        let dir = init_repo();
        let env = crate::MapEnvSource::new()
            .with("SOURCE_DATE_EPOCH", "1767225600")
            .with("ANODIZE_SOURCE_DATE_EPOCH", "999999999");
        let sde = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap();
        assert_eq!(sde, 1_767_225_600);
    }

    #[test]
    fn snapshot_sde_uses_the_legacy_spelling_when_it_is_the_only_one_set() {
        let dir = init_repo();
        let env = crate::MapEnvSource::new().with("ANODIZE_SOURCE_DATE_EPOCH", "999999999");
        let sde = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap();
        assert_eq!(sde, 999_999_999);
    }

    #[test]
    fn a_malformed_source_date_epoch_is_an_error() {
        let dir = init_repo();
        let env = crate::MapEnvSource::new().with("SOURCE_DATE_EPOCH", "soon");
        let err = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap_err();
        assert!(
            err.to_string()
                .contains("SOURCE_DATE_EPOCH is set but not a valid i64: soon"),
            "{err}"
        );
    }

    #[test]
    fn snapshot_sde_uses_head_when_tree_clean() {
        // Empty env: no override, so the HEAD commit time is used.
        let env = crate::MapEnvSource::new();
        let dir = init_repo();
        let sde = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap();
        assert_eq!(sde, 1_715_000_000);
    }

    #[test]
    fn a_dirty_tree_keeps_the_head_commit_time() {
        // The value becomes archive mtimes and the OCI created label, so an
        // untracked or modified file must not move it off the commit date.
        let env = crate::MapEnvSource::new();
        let dir = init_repo();
        fs::write(dir.path().join("b.txt"), "dirty").unwrap();
        fs::write(dir.path().join("a.txt"), "changed").unwrap();
        let sde = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap();
        assert_eq!(sde, 1_715_000_000);
    }
}
