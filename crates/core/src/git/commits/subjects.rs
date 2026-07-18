use anyhow::{Context as _, Result, bail};
use std::path::Path;
use std::process::Command;

/// Subject prefix anodizer stamps on its own release-machinery commits
/// (version-sync bumps, rollback reverts). The matchers that must recognise
/// those commits — rollback's idempotency check, the changelog stage's
/// version-sync exclusion — compose their patterns from this same constant
/// so a reworded writer can never silently break a matcher.
pub const RELEASE_COMMIT_PREFIX: &str = "chore(release): ";

/// `chore(release): bump ` — the subject prefix shared by every version-sync
/// bump commit (see [`release_bump_subject`]).
pub fn release_bump_subject_prefix() -> String {
    format!("{RELEASE_COMMIT_PREFIX}bump ")
}

/// Build a version-sync bump commit subject:
/// `chore(release): bump <summary><suffix>`. `suffix` carries the optional
/// ` [skip ci]` marker (empty when none applies).
pub fn release_bump_subject(summary: &str, suffix: &str) -> String {
    format!("{}{summary}{suffix}", release_bump_subject_prefix())
}

/// Prefix of the changelog-provenance marker lines the `tag` and
/// `bump --commit` commands record in the version-bump commit body when their
/// `--changelog` refresh regenerates on-disk `CHANGELOG.md` files.
///
/// The publish stage's already-published content guard matches these markers
/// (via [`changelog_regenerated_recorded_in`]) to decide whether a crate-root
/// `CHANGELOG.md` difference against an already-published version is the
/// tool's own re-cut artifact (forgivable) or operator-authored drift (a hard
/// divergence). Writer and matcher compose from this one constant so a
/// reworded marker can never silently break the guard.
pub const CHANGELOG_PROVENANCE_PREFIX: &str = "changelog regenerated for ";

/// Marker line recording that the tool regenerated the changelog file owned
/// by `crate_name` at `version`: `changelog regenerated for <crate>@<version>`.
/// Always crate-scoped, so one crate's regeneration can never vouch for a
/// same-numbered version of a different crate, and a root-only aggregate that
/// touched no packaged crate's own `CHANGELOG.md` mints no marker at all.
pub fn changelog_regenerated_marker(crate_name: &str, version: &str) -> String {
    format!("{CHANGELOG_PROVENANCE_PREFIX}{crate_name}@{version}")
}

/// Whether the LAST commit that touched `changelog_rel_path` (repo-relative,
/// `/`-separated) in `workspace_root` records that the tool regenerated the
/// changelog for `crate_name` at `version`.
///
/// Anchoring on the file's last toucher — not any marker anywhere in history —
/// scopes the provenance to the file's CURRENT content: an operator hand-edit
/// committed after the tool's regeneration makes the operator's commit the
/// last toucher, whose message carries no marker, so the guard reverts to
/// byte-strict instead of forgiving drift the tool did not author.
///
/// The marker match is an exact trimmed-line comparison, ruling out substring
/// false positives (a `0.12.0` marker never matches a `0.12.01` probe).
/// Returns `Ok(false)` when the file has never been committed or the last
/// toucher carries no matching marker; `Err` only when git itself fails
/// (callers making forgiveness decisions treat that as "no provenance").
pub fn changelog_regenerated_recorded_in(
    workspace_root: &Path,
    crate_name: &str,
    version: &str,
    changelog_rel_path: &str,
) -> Result<bool> {
    let marker = changelog_regenerated_marker(crate_name, version);
    let out = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args([
            "-c",
            "log.showSignature=false",
            "log",
            "-1",
            "--pretty=format:%B",
            "--",
            changelog_rel_path,
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .output()
        .context("failed to invoke git log for the changelog provenance marker")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // A repo with no commits yet trivially records no marker.
        if stderr.contains("does not have any commits yet") {
            return Ok(false);
        }
        let raw = format!("git log failed: {}", stderr.trim());
        bail!("{}", crate::redact::redact_process_env(&raw));
    }
    let body = String::from_utf8_lossy(&out.stdout);
    Ok(body.lines().map(str::trim).any(|line| line == marker))
}
