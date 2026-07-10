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
#[derive(Clone)]
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
    /// The shared root holds one flat whole-release `[Unreleased]` block rather
    /// than N `### <crate>` subsections. Set when the selected crates all share
    /// one tag prefix and route to one root file (a lockstep aggregate), so the
    /// root renderer takes the flat path regardless of the existing file's
    /// `### <Heading>` shape.
    pub single_track: bool,
    /// The shared root aggregates more than one independent crate track (a
    /// PerCrate workspace), so each crate owns a `### <crate>` subsection under
    /// `## [Unreleased]`. The TOPOLOGY signal threaded to the root renderer,
    /// replacing text inference for the flat-vs-subsection decision; derived from
    /// the routed-crate count, not from the existing file's shape.
    pub multitrack: bool,
    /// Every crate name routed to the shared root (regardless of an active
    /// `--crate` filter). Drives crate-name-aware subsection classification so a
    /// foreign `### Added` is never mistaken for a crate subsection, and rescues
    /// the `--crate`-filtered single-target case (topology count == 1).
    pub root_crate_names: Vec<String>,
}

impl<'a> ChangelogRouting<'a> {
    /// Build a routing descriptor from a resolved `changelog:` config.
    ///
    /// `single_track` defaults to `false`; callers that have resolved a
    /// same-prefix shared-root aggregate set it via [`Self::single_track`].
    pub fn from_config(cfg: &'a ChangelogConfig) -> Self {
        let dest = cfg.resolved_destination();
        Self {
            root_enabled: dest.root_enabled,
            per_crate: dest.per_crate,
            chronology: cfg.resolved_chronology(),
            root_crates: cfg.root_crates_filter(),
            single_track: false,
            multitrack: false,
            root_crate_names: Vec::new(),
        }
    }
}

/// Whether a crate contributes a section to the shared root changelog.
///
/// `None` (no filter) includes every crate; `Some(list)` includes only the
/// named crates (an empty list includes none).
pub(crate) fn crate_in_root(crate_name: &str, filter: Option<&[String]>) -> bool {
    filter.is_none_or(|names| names.iter().any(|n| n == crate_name))
}

/// Every crate name in the crate universe (flat `crates:` and every
/// `workspaces[].crates`, deduplicated), narrowed to the `root_crates`
/// allow-list. Drives the
/// root renderer's crate-name-aware subsection classification on a `--crate`-
/// filtered single-target run, where the target list alone can't supply the full
/// set. Returns an empty list for a config-less / single-crate-at-root project.
pub(crate) fn config_root_crate_names(
    config: &Config,
    root_crates: Option<&[String]>,
) -> Vec<String> {
    let mut names: Vec<String> = config
        .crate_universe()
        .into_iter()
        .map(|c| c.name.clone())
        .collect();
    names.retain(|n| crate_in_root(n, root_crates));
    names
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
                routing.multitrack,
                &routing.root_crate_names,
            )
            .with_context(|| format!("failed to render root changelog for {}", t.crate_name))?;
            persist_update(workspace_root, t, update, dry_run, log, &mut written)?;
        }
    }
    Ok(written)
}

/// Changelog-provenance marker lines for a version-bump commit body: one
/// `changelog regenerated for <crate>@<version>` line per packaged crate whose
/// OWN crate-root `CHANGELOG.md` is among the `written` repo-relative paths
/// returned by [`render_and_stage_changelogs`].
///
/// Deriving from the actually-written paths (never from "the changelog ran")
/// is what keeps the marker honest: a root-only aggregate config writes the
/// workspace-root `CHANGELOG.md` and no member files, so no member earns a
/// marker — unless a crate's directory IS the workspace root, in which case
/// the root file is that crate's own changelog and its marker is warranted.
///
/// `crates` is the set of REAL packaged crates as `(name, crate_dir, version)`
/// — synthetic aggregate targets (whose `crate_name` is a project label, not a
/// publishable crate) must not be passed in.
pub(crate) fn changelog_provenance_markers(
    workspace_root: &Path,
    crates: &[(String, PathBuf, String)],
    written: &[String],
) -> Vec<String> {
    let mut markers: Vec<String> = Vec::new();
    for (name, dir, version) in crates {
        let rel = rel_display(workspace_root, &dir.join("CHANGELOG.md"));
        if written.iter().any(|w| w == &rel) {
            let marker = anodizer_core::git::changelog_regenerated_marker(name, version);
            if !markers.contains(&marker) {
                markers.push(marker);
            }
        }
    }
    markers
}

/// Append changelog-provenance marker lines to a version-bump commit message.
/// With no markers the message is returned unchanged, byte-identical to the
/// pre-provenance form.
pub(crate) fn commit_message_with_markers(message: String, markers: &[String]) -> String {
    if markers.is_empty() {
        message
    } else {
        format!("{message}\n\n{}", markers.join("\n"))
    }
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
                    "(dry-run) would write changelog section for {} → {} in {}",
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
    let rel = rel_display(workspace_root, &update.file_path);
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
/// change are skipped.
///
/// Multiple crates routed to ONE shared root (genuine multi-track) refresh
/// sequentially against the accumulated text — each updates its own
/// `### <crate>` subsection, the running result feeding the next — so the final
/// output carries every crate's refreshed subsection, not only the first. A
/// single crate routed to both its per-crate file and the root still emits two
/// distinct files (different paths); a degenerate per-crate==root path (the
/// crate dir IS the workspace root) collapses to one output.
pub(crate) fn refresh_changelogs(
    workspace_root: &Path,
    targets: &[RefreshTarget],
    routing: &ChangelogRouting<'_>,
    write: bool,
    log: &StageLogger,
) -> Result<Vec<RefreshOutput>> {
    let mut outputs: Vec<RefreshOutput> = Vec::new();
    let root_file = workspace_root.join("CHANGELOG.md");
    // The running root-file text so successive same-root targets refresh against
    // the prior result rather than the stale on-disk copy.
    let mut root_working: Option<String> = None;
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
                routing.multitrack,
                &routing.root_crate_names,
                root_working.as_deref(),
            )
            .with_context(|| format!("failed to refresh root changelog for {}", t.crate_name))?;
            if let Some(ref u) = update
                && u.file_path == root_file
            {
                root_working = Some(u.rendered_text.clone());
            }
            collect_refresh(workspace_root, update, write, log, &mut outputs)?;
        }
    }
    Ok(outputs)
}

/// Write (when `write`) and/or record one regenerated section. When a later
/// target updates a file already recorded (successive crates sharing one root),
/// the existing output's text is replaced in place so the final output reflects
/// every crate's refresh rather than only the first.
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
    let rel = rel_display(workspace_root, &update.file_path);
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
    if let Some(existing) = outputs.iter_mut().find(|o| o.file_path == update.file_path) {
        existing.rendered_text = update.rendered_text;
        return Ok(());
    }
    outputs.push(RefreshOutput {
        file_path: update.file_path,
        rel_path: rel,
        rendered_text: update.rendered_text,
    });
    Ok(())
}

/// Repo-relative path of `file_path` under `workspace_root`, forward-slashed.
/// The value is shown to the user (preview `--- <path> ---` separators, the
/// "refreshed" status line) and compared in tests, so it must read identically
/// on every host rather than carrying the Windows `\` separator — matching the
/// `/`-normalized repo-relative paths anodizer already emits in context.json
/// and artifact rows.
fn rel_display(workspace_root: &Path, file_path: &Path) -> String {
    file_path
        .strip_prefix(workspace_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .replace('\\', "/")
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
    use anodizer_core::config::{ChangelogFilesConfig, Chronology, RootChangelogConfig};

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
            files: Some(ChangelogFilesConfig {
                per_crate: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let routing = ChangelogRouting::from_config(&cfg);
        assert!(!routing.root_enabled, "per_crate: true drops the root file");
        assert!(routing.per_crate);
    }

    #[test]
    fn routing_both_with_filter_and_chronology() {
        let cfg = ChangelogConfig {
            files: Some(ChangelogFilesConfig {
                per_crate: Some(true),
                root: Some(RootChangelogConfig {
                    chronology: Some(Chronology::Tag),
                    crates: Some(vec!["core".to_string()]),
                }),
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

    #[test]
    fn provenance_markers_only_for_crates_whose_own_changelog_was_written() {
        let ws = Path::new("/ws");
        let crates = vec![
            (
                "core".to_string(),
                PathBuf::from("/ws/crates/core"),
                "0.2.0".to_string(),
            ),
            (
                "cli".to_string(),
                PathBuf::from("/ws/crates/cli"),
                "0.3.0".to_string(),
            ),
        ];
        // Only core's own file was written (plus the shared root, which is not
        // any crate's changelog here).
        let written = vec![
            "crates/core/CHANGELOG.md".to_string(),
            "CHANGELOG.md".to_string(),
        ];
        assert_eq!(
            changelog_provenance_markers(ws, &crates, &written),
            vec!["changelog regenerated for core@0.2.0".to_string()]
        );
    }

    #[test]
    fn provenance_markers_root_only_aggregate_mints_none() {
        let ws = Path::new("/ws");
        let crates = vec![
            (
                "core".to_string(),
                PathBuf::from("/ws/crates/core"),
                "1.0.0".to_string(),
            ),
            (
                "cli".to_string(),
                PathBuf::from("/ws/crates/cli"),
                "1.0.0".to_string(),
            ),
        ];
        // Root-only config: the tool wrote the workspace-root aggregate and no
        // member's own file — no member earns forgiveness.
        let written = vec!["CHANGELOG.md".to_string()];
        assert!(changelog_provenance_markers(ws, &crates, &written).is_empty());
    }

    #[test]
    fn provenance_markers_crate_at_workspace_root_owns_the_root_file() {
        let ws = Path::new("/ws");
        let crates = vec![(
            "solo".to_string(),
            PathBuf::from("/ws"),
            "1.2.3".to_string(),
        )];
        let written = vec!["CHANGELOG.md".to_string()];
        assert_eq!(
            changelog_provenance_markers(ws, &crates, &written),
            vec!["changelog regenerated for solo@1.2.3".to_string()]
        );
    }

    #[test]
    fn provenance_markers_deduplicate() {
        let ws = Path::new("/ws");
        let crates = vec![
            (
                "core".to_string(),
                PathBuf::from("/ws/crates/core"),
                "0.2.0".to_string(),
            ),
            (
                "core".to_string(),
                PathBuf::from("/ws/crates/core"),
                "0.2.0".to_string(),
            ),
        ];
        let written = vec!["crates/core/CHANGELOG.md".to_string()];
        assert_eq!(changelog_provenance_markers(ws, &crates, &written).len(), 1);
    }

    #[test]
    fn commit_message_with_markers_without_markers_is_unchanged() {
        assert_eq!(
            commit_message_with_markers("chore(release): bump crates/x → 1.0.0".to_string(), &[]),
            "chore(release): bump crates/x → 1.0.0"
        );
    }

    #[test]
    fn commit_message_with_markers_appends_body_lines() {
        let msg = commit_message_with_markers(
            "chore(release): bump core→0.2.0, cli→0.3.0".to_string(),
            &[
                anodizer_core::git::changelog_regenerated_marker("core", "0.2.0"),
                anodizer_core::git::changelog_regenerated_marker("cli", "0.3.0"),
            ],
        );
        assert_eq!(
            msg,
            "chore(release): bump core→0.2.0, cli→0.3.0\n\n\
             changelog regenerated for core@0.2.0\n\
             changelog regenerated for cli@0.3.0"
        );
    }
}
