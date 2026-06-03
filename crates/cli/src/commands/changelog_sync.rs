//! Shared render-and-persist of per-crate `CHANGELOG.md` sections.
//!
//! A single source of truth for the changelog loop run at bump time (folded
//! into the bump commit) and at tag time. Both paths render each crate's
//! section for its new version via the native changelog engine
//! (`anodizer_stage_changelog::render_crate_section`) and write it to disk;
//! keeping one copy prevents the two paths from drifting apart. The helper
//! writes files and returns the repo-relative paths so the caller can fold
//! them into its own `git add` set.

use anodizer_core::config::Config;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

/// Resolve whether a version-bump commit should refresh `CHANGELOG.md`.
///
/// Shared by `bump --commit` and `tag` so the two commands gate the changelog
/// render loop identically. Enabled when the `changelog:` config block is
/// present and not skipped, and `no_changelog` is not set (only `tag` exposes a
/// `--no-changelog` flag; `bump` always passes `false`).
///
/// A plain `skip: true` boolean disables; a templated `skip:` (e.g.
/// `"{{ if IsSnapshot }}true{{ endif }}"`) is treated as enabled because neither
/// command has a release context to render the template against — the
/// per-pipeline changelog stage evaluates such templates at release time, so
/// suppressing here on an unrenderable template would be a false negative.
pub(crate) fn resolve_changelog_enabled(config: Option<&Config>, no_changelog: bool) -> bool {
    if no_changelog {
        return false;
    }
    let Some(cl) = config.and_then(|c| c.changelog.as_ref()) else {
        return false;
    };
    match cl.skip.as_ref() {
        Some(skip) if !skip.is_template() => !skip.as_bool(),
        _ => true,
    }
}

/// One crate whose `CHANGELOG.md` should be rendered for `to_version`.
pub(crate) struct ChangelogTarget {
    /// Crate name as it appears in `Cargo.toml` (`package.name`).
    pub crate_name: String,
    /// Directory containing the crate's manifest (where `CHANGELOG.md` lives).
    pub crate_dir: PathBuf,
    /// The previous release tag for the crate, if any, bounding the commit
    /// range the section is rendered from.
    pub from_tag: Option<String>,
    /// The version the new section documents.
    pub to_version: String,
}

/// Render and (unless `dry_run`) write each target's `CHANGELOG.md` via the
/// native changelog engine, returning the repo-relative paths that were written
/// for the caller to fold into its `git add` set.
///
/// In `dry_run` nothing is written and a preview line is logged per target that
/// would change. Targets whose render yields no update are skipped. Returned
/// paths are deduplicated.
pub(crate) fn render_and_stage_changelogs(
    workspace_root: &Path,
    targets: &[ChangelogTarget],
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<String>> {
    let mut written: Vec<String> = Vec::new();
    for t in targets {
        let update = anodizer_stage_changelog::render_crate_section(
            workspace_root,
            &t.crate_name,
            &t.crate_dir,
            t.from_tag.as_deref(),
            &t.to_version,
        )
        .with_context(|| format!("failed to render changelog for {}", t.crate_name))?;
        let Some(update) = update else { continue };
        match update.insertion_mode {
            anodizer_stage_changelog::InsertionMode::Replace => {
                if dry_run {
                    log.status(&format!(
                        "(dry-run) changelog: would write section for {} → {} in {}",
                        t.crate_name,
                        t.to_version,
                        update.file_path.display()
                    ));
                    continue;
                }
                if let Some(parent) = update.file_path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                std::fs::write(&update.file_path, &update.rendered_text).with_context(|| {
                    format!(
                        "failed to write changelog at {}",
                        update.file_path.display()
                    )
                })?;
            }
        }
        log.verbose(&format!(
            "bundled changelog section for {} → {}",
            t.crate_name, t.to_version
        ));
        let rel = update
            .file_path
            .strip_prefix(workspace_root)
            .unwrap_or(update.file_path.as_path())
            .to_string_lossy()
            .into_owned();
        if !written.contains(&rel) {
            written.push(rel);
        }
    }
    Ok(written)
}
