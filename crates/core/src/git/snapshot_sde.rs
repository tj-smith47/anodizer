//! Snapshot-mode SOURCE_DATE_EPOCH resolver.
//!
//! Resolves SDE in this priority order:
//!   1. `ANODIZE_SOURCE_DATE_EPOCH` env var (literal seconds since epoch).
//!   2. HEAD commit timestamp when the working tree is clean.
//!   3. HEAD commit timestamp PLUS a deterministic 32-bit hash of
//!      `git status --porcelain=v2 -z` output, when the tree is dirty.
//!
//! Determinism: option 3 is stable for unchanged dirty-tree state without
//! requiring a writable index (read-only worktrees produce the same
//! value), so successive snapshot runs against the same dirty tree
//! produce byte-identical SDE.

use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;

use crate::EnvSource;

/// Resolves the SOURCE_DATE_EPOCH for a snapshot-mode release run.
///
/// Returns seconds-since-epoch. See module docs for the resolution order.
pub fn resolve_snapshot_sde(repo: &Path) -> Result<i64> {
    resolve_snapshot_sde_with_env(repo, &crate::ProcessEnvSource)
}

/// Env-injectable form of [`resolve_snapshot_sde`]. Production wires up
/// [`ProcessEnvSource`]; tests inject a [`MapEnvSource`](crate::MapEnvSource)
/// to drive the `ANODIZE_SOURCE_DATE_EPOCH` branch without mutating the
/// process env.
pub fn resolve_snapshot_sde_with_env<E: EnvSource + ?Sized>(repo: &Path, env: &E) -> Result<i64> {
    if let Some(v) = env.var("ANODIZE_SOURCE_DATE_EPOCH") {
        let parsed = v.parse::<i64>().with_context(|| {
            format!(
                "ANODIZE_SOURCE_DATE_EPOCH is set but not a valid i64: {}",
                v
            )
        })?;
        return Ok(parsed);
    }

    let head_ts = head_commit_timestamp(repo)?;
    let porcelain = git_status_porcelain_v2(repo)?;
    if porcelain.is_empty() {
        return Ok(head_ts);
    }

    // Dirty tree: SHA256 the porcelain output, take the low 32 bits as a
    // deterministic per-tree offset. Does not require a writable index,
    // so read-only worktrees produce the same value across successive
    // runs.
    let mut hasher = Sha256::new();
    hasher.update(&porcelain);
    let digest = hasher.finalize();
    let dirty_offset = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]) as i64;
    Ok(head_ts + dirty_offset)
}

fn head_commit_timestamp(repo: &Path) -> Result<i64> {
    let out = Command::new("git")
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

fn git_status_porcelain_v2(repo: &Path) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain=v2", "-z"])
        .output()
        .context("failed to invoke git status --porcelain=v2 -z")?;
    if !out.status.success() {
        anyhow::bail!(
            "git status --porcelain=v2 -z failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn init_repo() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .arg("init")
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["config", "user.email", "test@example.com"])
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["config", "user.name", "test"])
            .output()
            .unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "a.txt"])
            .output()
            .unwrap();
        // Pin commit timestamp deterministically so test assertions are stable.
        Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .env("GIT_AUTHOR_DATE", "1715000000 +0000")
            .env("GIT_COMMITTER_DATE", "1715000000 +0000")
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();
        dir
    }

    #[test]
    fn snapshot_sde_uses_env_var_when_set() {
        let dir = init_repo();
        // The env-var branch is driven through an injected source, not process env.
        let env = crate::MapEnvSource::new().with("ANODIZE_SOURCE_DATE_EPOCH", "999999999");
        let sde = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap();
        assert_eq!(sde, 999_999_999);
    }

    #[test]
    fn snapshot_sde_uses_head_when_tree_clean() {
        // Empty env → no ANODIZE_SOURCE_DATE_EPOCH → falls through to HEAD time.
        let env = crate::MapEnvSource::new();
        let dir = init_repo();
        let sde = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap();
        assert_eq!(sde, 1_715_000_000);
    }

    #[test]
    fn snapshot_sde_uses_dirty_tree_hash_when_tree_dirty() {
        let env = crate::MapEnvSource::new();
        let dir = init_repo();
        fs::write(dir.path().join("b.txt"), "dirty").unwrap();
        let sde = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap();
        assert!(sde > 1_715_000_000);
        // The hash offset is bounded by u32::MAX (about 4.3e9). Verify it
        // sits in that range so the offset addition is bounded.
        assert!(sde - 1_715_000_000 <= u32::MAX as i64);
    }

    #[test]
    fn snapshot_sde_is_stable_for_unchanged_dirty_tree() {
        let env = crate::MapEnvSource::new();
        let dir = init_repo();
        fs::write(dir.path().join("b.txt"), "dirty").unwrap();
        let sde1 = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap();
        let sde2 = resolve_snapshot_sde_with_env(dir.path(), &env).unwrap();
        assert_eq!(sde1, sde2);
    }
}
