use super::*;

/// Render a `## [<to_version>]` section for the given crate's changelog and
/// merge it into the crate's `CHANGELOG.md`.
///
/// Produces a single [`ChangelogUpdate`] carrying the full merged file content
/// so a version bump can bundle the changelog edit into one staged commit.
///
/// Returns `Ok(None)` when:
///   - no anodizer config exists under `workspace_root` (well-known
///     candidate names), or it has no `changelog:` section
///   - there are no qualifying commits since `from_tag` (or `HEAD` history,
///     when `from_tag` is `None`) touching `crate_path`
///
/// On success, the returned [`ChangelogUpdate`] always carries the FULL final
/// file content with [`InsertionMode::Replace`]: the function reads any
/// existing `CHANGELOG.md`, prepends the new section after the leading H1
/// header (creating one when missing), and returns the merged text.
pub fn render_crate_section(
    workspace_root: &std::path::Path,
    crate_name: &str,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
    to_version: &str,
) -> Result<Option<ChangelogUpdate>> {
    // Per-crate context for templated group fields. `Tag` is left empty here:
    // the per-crate file carries only the bare version, and this entry point
    // takes no full tag (a group title referencing `{{ .Version }}` /
    // `{{ .ProjectName }}` resolves correctly).
    let section_vars = SectionVars {
        crate_name,
        version: to_version,
        tag: "",
    };
    let Some((_cfg, body)) =
        render_section_body(workspace_root, crate_path, from_tag, None, section_vars)?
    else {
        return Ok(None);
    };

    let section_heading = format!(
        "## [{ver}] - {date}",
        ver = to_version,
        date = today_yyyy_mm_dd()
    );
    let new_section = format!("{}\n\n{}\n", section_heading, body.trim_end());

    let file_path = crate_path.join("CHANGELOG.md");
    // Per-crate file: a synthesized H1 is crate-named, matching the per-crate
    // refresh path.
    let merged = merge_into_changelog(MergeArgs {
        file_path: &file_path,
        h1: &crate_h1(crate_name),
        new_section: &new_section,
        generated_body: body.trim_end(),
        from_tag,
        to_version,
        workspace_root,
    })?;

    Ok(Some(ChangelogUpdate {
        file_path,
        rendered_text: merged,
        insertion_mode: InsertionMode::Replace,
    }))
}

/// Render a release section for `crate_name` and promote it into the SHARED
/// root `<workspace_root>/CHANGELOG.md` (NOT the per-crate file).
///
/// A multi-track workspace keeps one root `CHANGELOG.md` whose
/// `## [Unreleased]` holds a `### <crate>` subsection per crate. Tagging a
/// track promotes ONLY that track's subsection into a released
/// `## [<tag>] - <date>` section — re-leveled to `### <GroupTitle>` headings
/// and regrouped per the configured `groups:` — and leaves every other crate's
/// subsection in place. A single-track root (no `### <crate>` subsections)
/// falls through to the flat Keep-a-Changelog roll, byte-identical to
/// [`render_crate_section`]'s behaviour.
///
/// `tag` is the FULL new tag for this release (e.g. `v0.7.0` or
/// `core-v0.5.1`); the promoted heading and the rolled compare-link footer
/// both derive from it and from this track's own `from_tag`, so multi-track
/// compare ranges stay correct even when the shared `[Unreleased]:` anchor
/// belongs to a different track. `chronology` slots the new section among the
/// existing released sections (`Date`: newest-on-top; `Tag`: clustered by
/// tag-prefix, semver-descending within a cluster).
///
/// `multitrack` is the caller's TOPOLOGY signal (the root aggregates more than
/// one independent crate track). It REPLACES text inference for the flat-vs-
/// subsection decision: a `false` value forces the flat roll regardless of the
/// existing file's shape (Single / Lockstep / FlatAggregate), and a `true` value
/// drives the subsection-promote path even on a fresh root. When `multitrack` is
/// `false` but the existing file already carries a known-crate subsection (a
/// `--crate`-filtered single-target run on a PerCrate repo), crate-name-aware
/// detection over `crate_names` still routes to the subsection-promote path.
///
/// `crate_names` is the set of known crate names routed to the root; it drives
/// crate-name-aware subsection classification so a curated/foreign `### Added`
/// is never mistaken for a crate subsection.
///
/// Returns `Ok(None)` when there is nothing to release for this track: no
/// `changelog:` config / no commits AND no curated `### <crate>` subsection.
// Flat parameter list (root paths, range, version + tag, ordering, the
// multitrack signal, the crate-name set) with no natural grouping; a params
// struct would only relocate the breadth.
#[allow(clippy::too_many_arguments)]
pub fn render_root_section(
    workspace_root: &std::path::Path,
    crate_name: &str,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
    to_version: &str,
    tag: &str,
    chronology: anodizer_core::config::Chronology,
    multitrack: bool,
    crate_names: &[String],
) -> Result<Option<ChangelogUpdate>> {
    // Per-crate context for templated group fields (`groups[].title` etc.).
    let section_vars = SectionVars {
        crate_name,
        version: to_version,
        tag,
    };
    let rendered = render_section_body(workspace_root, crate_path, from_tag, None, section_vars)?;
    // Reuse the config already parsed by `render_section_body` (its group titles
    // are already template-rendered). Only re-load on the `None` arm (no
    // `changelog:` block, or a curated subsection with no qualifying commits) so
    // a curated promote still buckets under the configured group headings
    // without a second disk read; the reloaded titles are rendered here so the
    // curated-bucketing path agrees with the commit-driven one.
    let groups = match &rendered {
        Some((cfg, _)) => cfg.groups.clone().unwrap_or_default(),
        None => {
            let mut groups = load_changelog_config(workspace_root)?
                .and_then(|c| c.groups)
                .unwrap_or_default();
            let template_vars = build_section_template_vars(section_vars);
            let log = StageLogger::new("bump-changelog", Verbosity::default());
            render_group_titles_in_place(&mut groups, &template_vars, &log);
            groups
        }
    };
    let generated_body = rendered
        .as_ref()
        .map(|(_, body)| body.clone())
        .unwrap_or_default();

    let file_path = workspace_root.join("CHANGELOG.md");
    let existing: Option<String> = if file_path.is_file() {
        Some(
            std::fs::read_to_string(&file_path)
                .with_context(|| format!("failed to read {}", file_path.display()))?,
        )
    } else {
        None
    };

    // Multi-track decision. `multitrack` is the caller's repo TOPOLOGY (the root
    // aggregates more than one independent crate track). The existing-subsection
    // fallback additionally rescues a `--crate`-filtered single-target run on a
    // PerCrate repo (topology count alone can't say "multi"). When neither
    // holds, the root is flat (Single / Lockstep / FlatAggregate).
    let use_multitrack = multitrack
        || existing
            .as_deref()
            .is_some_and(|e| has_crate_subsections(e, crate_names));

    if use_multitrack {
        // Tag-prefixed, accumulating, chronology-slotted dated section built
        // from this track's commits (or a consumed `### <crate>` subsection).
        // The footer base prefers an existing compare link, then the `origin`
        // remote, so a self-hosted host stays correct.
        let base = resolve_compare_base(existing.as_deref().unwrap_or(""), workspace_root);
        let Some(merged) = promote_multitrack_section(MultitrackPromoteArgs {
            existing: existing.as_deref(),
            h1: &project_h1(workspace_root),
            crate_name,
            tag,
            from_tag,
            chronology,
            groups: &groups,
            generated_body: generated_body.trim_end(),
            base: base.as_deref(),
        })?
        else {
            return Ok(None);
        };
        return Ok(Some(ChangelogUpdate {
            file_path,
            rendered_text: merged,
            insertion_mode: InsertionMode::Replace,
        }));
    }

    // Flat root (single-track aggregate): the whole release is one flat
    // `## [<version>]` section, byte-identical to `render_crate_section`.
    let Some((_cfg, body)) = rendered else {
        return Ok(None);
    };
    let section_heading = format!(
        "## [{ver}] - {date}",
        ver = to_version,
        date = today_yyyy_mm_dd()
    );
    let new_section = format!("{}\n\n{}\n", section_heading, body.trim_end());
    // Root file: a synthesized H1 is the project header, never a crate name (a
    // fresh lockstep/flat root must not be titled with the last-bumped crate).
    let merged = merge_into_changelog(MergeArgs {
        file_path: &file_path,
        h1: &project_h1(workspace_root),
        new_section: &new_section,
        generated_body: body.trim_end(),
        from_tag,
        to_version,
        workspace_root,
    })?;
    Ok(Some(ChangelogUpdate {
        file_path,
        rendered_text: merged,
        insertion_mode: InsertionMode::Replace,
    }))
}

/// Resolve the `<base>/compare` URL prefix for footer links: prefer the base
/// embedded in an existing `[Unreleased]:` compare link, else synthesize one
/// from the `origin` remote. Returns `None` when neither is available (the
/// footer roll then leaves links absent rather than emitting a 404).
pub(crate) fn resolve_compare_base(
    existing: &str,
    workspace_root: &std::path::Path,
) -> Option<String> {
    if let Some(url) = existing.lines().find_map(parse_unreleased_footer)
        && let Some((base, _anchor)) = parse_compare_url(url)
    {
        return Some(base.to_string());
    }
    anodizer_core::git::detect_remote_web_base_in(workspace_root).ok()
}

/// Whether `existing` has at least one `### <crate>` subsection under its
/// `## [Unreleased]` heading (the marker of a multi-track root).
///
/// Classification is crate-name-aware: a `### <name>` is a crate subsection IFF
/// `name` is a known crate name in `crate_names`. Foreign curated headings
/// (`### Added`, `### CI/CD`) — and the configured `### <GroupTitle>` headings of
/// a flat curated body — are therefore NEVER mistaken for a crate subsection. A
/// flat `[Unreleased]` with only foreign/group headings, no `### ` lines, or no
/// `[Unreleased]` at all returns `false` so the caller takes the flat roll.
pub(crate) fn has_crate_subsections(existing: &str, crate_names: &[String]) -> bool {
    let lines: Vec<&str> = existing.lines().collect();
    let Some(unreleased_idx) = lines.iter().position(|l| is_unreleased_heading(l)) else {
        return false;
    };
    for line in lines.iter().skip(unreleased_idx + 1) {
        if is_section_heading(line) || parse_unreleased_footer(line).is_some() {
            return false;
        }
        if let Some(name) = is_subsection_heading(line)
            && crate_names.iter().any(|c| c == name)
        {
            return true;
        }
    }
    false
}

/// Whether `line` is an H3 `### <name>` subsection heading, returning the
/// trimmed `<name>`. Matches exactly three leading hashes (so a deeper `####`
/// is not mistaken for a crate subsection).
pub(crate) fn is_subsection_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_end();
    let rest = trimmed.strip_prefix("### ")?;
    if rest.starts_with('#') {
        return None;
    }
    let name = rest.trim();
    if name.is_empty() { None } else { Some(name) }
}
