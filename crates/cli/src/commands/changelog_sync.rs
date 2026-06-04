//! Shared render-and-persist of `CHANGELOG.md` sections.
//!
//! A single source of truth for the changelog loop run at bump time (folded
//! into the bump commit) and at tag time. Both paths render each target's
//! section for its new version via the native changelog engine and write it to
//! disk; keeping one copy prevents the two paths from drifting apart. The
//! helper writes files and returns the repo-relative paths so the caller can
//! fold them into its own `git add` set.
//!
//! Each target is routed to a per-crate `crates/<name>/CHANGELOG.md`
//! (`render_crate_section`) and/or the shared root `<root>/CHANGELOG.md`
//! (`render_root_section`) per the resolved [`ChangelogRouting`], honoring the
//! root crates filter and section chronology.

use anodizer_core::config::{ChangelogConfig, Config};
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

/// Resolve whether a `tag` / `bump --commit` invocation should refresh
/// `CHANGELOG.md`.
///
/// Opt-in: the refresh runs only when the caller passes `--changelog`
/// (`opt_in == true`) AND a `changelog:` config block is present and not
/// skipped. Shared by `bump --commit` and `tag` so the two commands gate the
/// changelog render loop identically.
///
/// A plain `skip: true` boolean disables; a templated `skip:` (e.g.
/// `"{{ if IsSnapshot }}true{{ endif }}"`) is treated as enabled because neither
/// command has a release context to render the template against — the
/// per-pipeline changelog stage evaluates such templates at release time, so
/// suppressing here on an unrenderable template would be a false negative.
pub(crate) fn resolve_changelog_enabled(config: Option<&Config>, opt_in: bool) -> bool {
    if !opt_in {
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
    /// Directory containing the crate's manifest (where the per-crate
    /// `CHANGELOG.md` lives). For a lockstep root-aggregate target this is the
    /// workspace root so the section aggregates the whole release.
    pub crate_dir: PathBuf,
    /// The previous release tag for the crate, if any, bounding the commit
    /// range the section is rendered from.
    pub from_tag: Option<String>,
    /// The version the new section documents.
    pub to_version: String,
    /// The full new tag for this release (e.g. `v0.7.0`, `core-v0.5.1`). Used
    /// by the root renderer to promote / slot the section heading.
    pub full_tag: String,
}

/// How each [`ChangelogTarget`] is routed to the per-crate and/or shared root
/// `CHANGELOG.md`, derived from the resolved `changelog:` config.
pub(crate) struct ChangelogRouting<'a> {
    /// Write the shared root `<workspace_root>/CHANGELOG.md`.
    pub root_enabled: bool,
    /// Write per-crate `crate_dir/CHANGELOG.md` files.
    pub per_crate: bool,
    /// Section ordering for the root changelog.
    pub chronology: anodizer_core::config::Chronology,
    /// Crates that contribute a root section: `None` = every crate.
    pub root_crates: Option<&'a [String]>,
}

impl<'a> ChangelogRouting<'a> {
    /// Build a routing descriptor from a resolved `changelog:` config.
    pub fn from_config(cfg: &'a ChangelogConfig) -> Self {
        let dest = cfg.resolved_destination();
        Self {
            root_enabled: dest.root_enabled,
            per_crate: dest.per_crate,
            chronology: cfg.resolved_chronology(),
            root_crates: cfg.root_crates_filter(),
        }
    }
}

/// Whether a crate contributes a section to the shared root changelog.
///
/// `None` (no filter) includes every crate; `Some(list)` includes only the
/// named crates (an empty list includes none).
fn crate_in_root(crate_name: &str, filter: Option<&[String]>) -> bool {
    filter.is_none_or(|names| names.iter().any(|n| n == crate_name))
}

/// Render and (unless `dry_run`) write each target's `CHANGELOG.md` via the
/// native changelog engine, routing per `routing` to the per-crate file and/or
/// the shared root file. Returns the repo-relative paths that were written for
/// the caller to fold into its `git add` set.
///
/// In `dry_run` nothing is written and a preview line is logged per file that
/// would change. Targets/destinations whose render yields no update are
/// skipped. Returned paths are deduplicated.
pub(crate) fn render_and_stage_changelogs(
    workspace_root: &Path,
    targets: &[ChangelogTarget],
    routing: &ChangelogRouting<'_>,
    dry_run: bool,
    log: &StageLogger,
) -> Result<Vec<String>> {
    let mut written: Vec<String> = Vec::new();
    for t in targets {
        if routing.per_crate {
            let update = anodizer_stage_changelog::render_crate_section(
                workspace_root,
                &t.crate_name,
                &t.crate_dir,
                t.from_tag.as_deref(),
                &t.to_version,
            )
            .with_context(|| format!("failed to render changelog for {}", t.crate_name))?;
            persist_update(workspace_root, t, update, dry_run, log, &mut written)?;
        }
        if routing.root_enabled && crate_in_root(&t.crate_name, routing.root_crates) {
            let update = anodizer_stage_changelog::render_root_section(
                workspace_root,
                &t.crate_name,
                &t.crate_dir,
                t.from_tag.as_deref(),
                &t.to_version,
                &t.full_tag,
                routing.chronology,
            )
            .with_context(|| format!("failed to render root changelog for {}", t.crate_name))?;
            persist_update(workspace_root, t, update, dry_run, log, &mut written)?;
        }
    }
    Ok(written)
}

/// Write (or, under `dry_run`, preview) one rendered changelog update and fold
/// its repo-relative path into `written` (deduplicated).
fn persist_update(
    workspace_root: &Path,
    t: &ChangelogTarget,
    update: Option<anodizer_stage_changelog::ChangelogUpdate>,
    dry_run: bool,
    log: &StageLogger,
    written: &mut Vec<String>,
) -> Result<()> {
    let Some(update) = update else {
        return Ok(());
    };
    match update.insertion_mode {
        anodizer_stage_changelog::InsertionMode::Replace => {
            if dry_run {
                log.status(&format!(
                    "(dry-run) changelog: would write section for {} → {} in {}",
                    t.crate_name,
                    t.to_version,
                    update.file_path.display()
                ));
                return Ok(());
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
    Ok(())
}

/// One crate whose pending `## [Unreleased]` section should be regenerated in
/// place (no version promotion). Sibling of [`ChangelogTarget`] for the
/// `anodizer changelog` refresh path, which never bumps a version.
pub(crate) struct RefreshTarget {
    /// Crate name as it appears in `Cargo.toml` (`package.name`).
    pub crate_name: String,
    /// Directory containing the crate's manifest (where the per-crate
    /// `CHANGELOG.md` lives). For a lockstep root-aggregate target this is the
    /// workspace root so the section aggregates the whole release.
    pub crate_dir: PathBuf,
    /// Lower bound of the commit range: the crate's last matching tag, or
    /// `None` to walk full history.
    pub from_tag: Option<String>,
    /// Upper bound of the commit range, or `None` for `HEAD`.
    pub to_ref: Option<String>,
}

/// One regenerated `## [Unreleased]` section produced by [`refresh_changelogs`].
pub(crate) struct RefreshOutput {
    /// Absolute path of the `CHANGELOG.md` the section belongs to.
    pub file_path: PathBuf,
    /// Repo-relative form of `file_path` (for attributable preview headers and
    /// write logging).
    pub rel_path: String,
    /// The full regenerated file text (engine output), ready to write.
    pub rendered_text: String,
}

/// Regenerate each target's pending `## [Unreleased]` section via the native
/// changelog engine, routing per `routing` to the per-crate file and/or the
/// shared root file. Unlike [`render_and_stage_changelogs`] this NEVER promotes
/// a dated release section — it refreshes the in-progress `[Unreleased]` block
/// only.
///
/// When `write` is set, each rendered section is written to its `file_path`
/// (parent dirs created) and the repo-relative path is logged. When `write` is
/// `false` nothing touches disk; the caller renders the returned
/// [`RefreshOutput`]s as a preview. Targets/destinations whose render yields no
/// change are skipped. Outputs are deduplicated by file path so a crate routed
/// to both per-crate and root files never double-emits the same file.
pub(crate) fn refresh_changelogs(
    workspace_root: &Path,
    targets: &[RefreshTarget],
    routing: &ChangelogRouting<'_>,
    write: bool,
    log: &StageLogger,
) -> Result<Vec<RefreshOutput>> {
    let mut outputs: Vec<RefreshOutput> = Vec::new();
    for t in targets {
        if routing.per_crate {
            let update = anodizer_stage_changelog::refresh_crate_unreleased(
                workspace_root,
                &t.crate_name,
                &t.crate_dir,
                t.from_tag.as_deref(),
                t.to_ref.as_deref(),
            )
            .with_context(|| format!("failed to refresh changelog for {}", t.crate_name))?;
            collect_refresh(workspace_root, update, write, log, &mut outputs)?;
        }
        if routing.root_enabled && crate_in_root(&t.crate_name, routing.root_crates) {
            let update = anodizer_stage_changelog::refresh_root_unreleased(
                workspace_root,
                &t.crate_name,
                &t.crate_dir,
                t.from_tag.as_deref(),
                t.to_ref.as_deref(),
                routing.chronology,
            )
            .with_context(|| format!("failed to refresh root changelog for {}", t.crate_name))?;
            collect_refresh(workspace_root, update, write, log, &mut outputs)?;
        }
    }
    Ok(outputs)
}

/// Write (when `write`) and/or record one regenerated section, deduplicating by
/// file path so the same file is never emitted twice.
fn collect_refresh(
    workspace_root: &Path,
    update: Option<anodizer_stage_changelog::ChangelogUpdate>,
    write: bool,
    log: &StageLogger,
    outputs: &mut Vec<RefreshOutput>,
) -> Result<()> {
    let Some(update) = update else {
        return Ok(());
    };
    if outputs.iter().any(|o| o.file_path == update.file_path) {
        return Ok(());
    }
    let rel = update
        .file_path
        .strip_prefix(workspace_root)
        .unwrap_or(update.file_path.as_path())
        .to_string_lossy()
        .into_owned();
    if write {
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
        log.status(&format!("refreshed {}", rel));
    }
    outputs.push(RefreshOutput {
        file_path: update.file_path,
        rel_path: rel,
        rendered_text: update.rendered_text,
    });
    Ok(())
}

/// Extract the `## [Unreleased]` section (heading through the line before the
/// next `## ` heading or the compare-link footer) from a full Keep-a-Changelog
/// file. Returns the whole input when no `[Unreleased]` heading is present so a
/// preview never silently drops content.
pub(crate) fn extract_unreleased_section(rendered: &str) -> String {
    let mut lines = rendered.lines();
    let mut section: Vec<&str> = Vec::new();
    // Skip to the Unreleased heading.
    let mut found = false;
    for line in lines.by_ref() {
        if line.starts_with("## ") && line.contains("Unreleased") {
            section.push(line);
            found = true;
            break;
        }
    }
    if !found {
        return rendered.trim_end().to_string();
    }
    // Collect until the next `## ` heading or a footer reference-definition
    // (`[label]: url`). Match the link-definition shape — `[` + a
    // space-free, non-empty label + `]:` — rather than any `[`-leading line,
    // so a body bullet that renders to a markdown link (`- [#42](...) fix`) or
    // a bracketed commit subject (`* [ci] bump`) is NOT mistaken for the
    // footer and silently truncated.
    for line in lines {
        let is_ref_def = line.starts_with('[')
            && line
                .split_once("]:")
                .is_some_and(|(label, _)| !label.is_empty() && !label.contains(' '));
        if line.starts_with("## ") || is_ref_def {
            break;
        }
        section.push(line);
    }
    let mut out = section.join("\n");
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{Chronology, RootChangelogConfig};

    #[test]
    fn extract_unreleased_grabs_only_the_section() {
        let file = "# Changelog\n\n## [Unreleased]\n\n### Features\n\n- a thing\n\n## [0.1.0] - 2026-01-01\n\n- old\n\n[Unreleased]: http://x/compare\n";
        let section = extract_unreleased_section(file);
        assert!(section.starts_with("## [Unreleased]"), "{section}");
        assert!(section.contains("a thing"));
        assert!(
            !section.contains("0.1.0"),
            "released history leaked: {section}"
        );
        assert!(!section.contains("compare"), "footer leaked: {section}");
    }

    #[test]
    fn extract_unreleased_no_heading_returns_input() {
        let file = "# Changelog\n\nsome free text\n";
        let section = extract_unreleased_section(file);
        assert_eq!(section, "# Changelog\n\nsome free text");
    }

    /// A body bullet whose rendered text starts with `[` (a markdown-link
    /// bullet) must NOT be mistaken for the compare-link footer and truncate
    /// the preview early. Only a `[label]: url` reference-definition (or the
    /// next `## ` heading) ends the section.
    #[test]
    fn extract_unreleased_keeps_bracketed_bullets() {
        let file = "# Changelog\n\n## [Unreleased]\n\n### Fixes\n\n- [#42](https://x/42) fix the thing\n- later bullet\n\n[Unreleased]: http://x/compare\n";
        let section = extract_unreleased_section(file);
        assert!(
            section.contains("#42"),
            "bracketed-link bullet dropped: {section}"
        );
        assert!(
            section.contains("later bullet"),
            "entries after the bracketed bullet dropped: {section}"
        );
        assert!(!section.contains("compare"), "footer leaked: {section}");
    }

    #[test]
    fn crate_in_root_no_filter_includes_all() {
        assert!(crate_in_root("core", None));
        assert!(crate_in_root("anything", None));
    }

    #[test]
    fn crate_in_root_filter_includes_named() {
        let filter = vec!["core".to_string(), "cli".to_string()];
        assert!(crate_in_root("core", Some(&filter)));
        assert!(crate_in_root("cli", Some(&filter)));
    }

    #[test]
    fn crate_in_root_filter_excludes_unnamed() {
        let filter = vec!["core".to_string()];
        assert!(!crate_in_root("cli", Some(&filter)));
    }

    #[test]
    fn crate_in_root_empty_filter_excludes_all() {
        let filter: Vec<String> = Vec::new();
        assert!(!crate_in_root("core", Some(&filter)));
    }

    #[test]
    fn routing_bare_config_is_root_only() {
        let cfg = ChangelogConfig::default();
        let routing = ChangelogRouting::from_config(&cfg);
        assert!(
            routing.root_enabled,
            "bare changelog routes to the root file"
        );
        assert!(!routing.per_crate);
        assert_eq!(routing.chronology, Chronology::Date);
        assert!(routing.root_crates.is_none());
    }

    #[test]
    fn routing_per_crate_only() {
        let cfg = ChangelogConfig {
            per_crate: Some(true),
            ..Default::default()
        };
        let routing = ChangelogRouting::from_config(&cfg);
        assert!(!routing.root_enabled, "per_crate: true drops the root file");
        assert!(routing.per_crate);
    }

    #[test]
    fn routing_both_with_filter_and_chronology() {
        let cfg = ChangelogConfig {
            per_crate: Some(true),
            root: Some(RootChangelogConfig {
                chronology: Some(Chronology::Tag),
                crates: Some(vec!["core".to_string()]),
            }),
            ..Default::default()
        };
        let routing = ChangelogRouting::from_config(&cfg);
        assert!(routing.root_enabled);
        assert!(routing.per_crate);
        assert_eq!(routing.chronology, Chronology::Tag);
        assert_eq!(routing.root_crates, Some(["core".to_string()].as_slice()));
    }

    #[test]
    fn target_carries_full_tag() {
        let t = ChangelogTarget {
            crate_name: "core".to_string(),
            crate_dir: PathBuf::from("/ws/crates/core"),
            from_tag: Some("core-v0.1.0".to_string()),
            to_version: "0.2.0".to_string(),
            full_tag: "core-v0.2.0".to_string(),
        };
        assert_eq!(t.full_tag, "core-v0.2.0");
    }
}
