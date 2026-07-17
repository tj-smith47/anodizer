use super::*;

/// Render the grouped commit body for `from_tag..to_ref` as a flat Markdown
/// block (no `## <version>` heading), independent of any existing file.
///
/// Returns `(cfg_present, body)`: `cfg_present` is `false` only when there is
/// no `changelog:` config at all (the sole "do nothing" signal the refresh
/// path honors); `body` is the rendered grouped block, empty when no commits
/// qualify in range.
pub(crate) fn refresh_body(
    workspace_root: &std::path::Path,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
    to_ref: Option<&str>,
    section_vars: SectionVars<'_>,
) -> Result<(bool, String)> {
    if load_changelog_config(workspace_root)?.is_none() {
        return Ok((false, String::new()));
    }
    let body =
        match render_section_body(workspace_root, crate_path, from_tag, to_ref, section_vars)? {
            Some((_cfg, body)) => body.trim_end().to_string(),
            // No qualifying commits in range: the regenerated body is empty.
            None => String::new(),
        };
    Ok((true, body))
}

/// Regenerate the `## [Unreleased]` body of a crate's `CHANGELOG.md` (or a flat
/// root) in place from the commits in `from_tag..to_ref`, WITHOUT promoting to
/// a dated release section.
///
/// Replaces everything between the `## [Unreleased]` heading and the next
/// `## ` section (or the compare-link footer), exclusive, with the freshly
/// grouped body. The H1, every released `## [x.y.z]` section, and the entire
/// compare-link footer are preserved verbatim. When the file is absent or has
/// no `## [Unreleased]` heading, a minimal Keep-a-Changelog skeleton is
/// created (or the section is inserted directly after the H1 of a non-KAC
/// file).
///
/// Returns `Ok(None)` when there is no `changelog:` config, or when the
/// regenerated content would be byte-identical to the existing file (an empty
/// range whose `[Unreleased]` body is already empty). Otherwise returns a
/// [`ChangelogUpdate`] with [`InsertionMode::Replace`] carrying the full file.
///
/// Running twice with the same commits yields byte-identical output.
pub fn refresh_crate_unreleased(
    workspace_root: &std::path::Path,
    crate_name: &str,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
    to_ref: Option<&str>,
) -> Result<Option<ChangelogUpdate>> {
    // The `[Unreleased]` refresh has no release version/tag in hand; only
    // `ProjectName`/`Name` carry meaning for a templated group title here.
    let section_vars = SectionVars {
        crate_name,
        version: "",
        tag: "",
    };
    let (cfg_present, body) =
        refresh_body(workspace_root, crate_path, from_tag, to_ref, section_vars)?;
    if !cfg_present {
        return Ok(None);
    }

    let file_path = crate_path.join("CHANGELOG.md");
    let existing = read_existing(&file_path)?;
    // Per-crate file: a synthesized H1 is crate-named (`# Changelog — <crate>`),
    // matching the per-crate promote path.
    let merged = replace_unreleased_body(existing.as_deref(), &crate_h1(crate_name), &body);

    if existing.as_deref() == Some(merged.as_str()) {
        return Ok(None);
    }

    Ok(Some(ChangelogUpdate {
        file_path,
        rendered_text: merged,
        insertion_mode: InsertionMode::Replace,
    }))
}

/// Regenerate the `## [Unreleased]` content for `crate_name` in the SHARED root
/// `<workspace_root>/CHANGELOG.md` from `from_tag..to_ref`, WITHOUT promoting
/// to a dated release section.
///
/// Mirrors [`render_root_section`]'s flat-vs-multitrack branching:
/// - MULTI-TRACK root (a `### <crate>` subsection lives under
///   `## [Unreleased]`): regenerate only THIS crate's `### <crate>`
///   subsection, leaving every other crate's subsection, all released
///   sections, and the footer verbatim.
/// - FLAT root (no crate subsections): behave exactly like
///   [`refresh_crate_unreleased`].
///
/// `single_track` forces the FLAT path regardless of the existing file's shape:
/// when the caller has resolved that N crates share one tag prefix and route to
/// one shared root (a lockstep aggregate), the whole `[Unreleased]` is one flat
/// body. Without this, a curated body whose `### <Heading>` titles don't match
/// the configured `groups:` would be misread as multi-track and grafted with a
/// spurious `### <crate>` subsection.
///
/// `multitrack` is the caller's TOPOLOGY signal: the root aggregates more than
/// one independent crate track (a PerCrate workspace), so each crate owns a
/// `### <crate>` subsection rather than sharing one flat `[Unreleased]` body.
/// It REPLACES text inference for the decision — a fresh/empty root no longer
/// silently collapses to the flat last-writer-wins path. When `multitrack` is
/// `false` but the existing file already carries a known-crate subsection (a
/// `--crate`-filtered single-target run on a PerCrate repo, where the topology
/// count is 1), the function still targets that subsection via crate-name-aware
/// detection over `crate_names`.
///
/// `crate_names` is the set of known crate names routed to the root; it drives
/// crate-name-aware subsection classification (foreign `### Added` is never
/// mistaken for a crate subsection).
///
/// `existing_override` supplies the current file text instead of reading disk.
/// Multiple crates whose `### <crate>` subsections live in ONE shared root must
/// refresh sequentially: each reads the PREVIOUS crate's result, not the stale
/// on-disk copy, so every subsection is updated rather than only the first.
/// `None` reads `<workspace_root>/CHANGELOG.md` as before.
///
/// Returns `Ok(None)` under the same "nothing to do" conditions as
/// [`refresh_crate_unreleased`]. Idempotent for a fixed commit set.
// The render/refresh contract pairs a long but flat parameter list (root paths,
// commit range, ordering, the multitrack signal, the crate-name set, the
// override) with no natural grouping; a params struct here would only relocate
// the breadth.
#[allow(clippy::too_many_arguments)]
pub fn refresh_root_unreleased(
    workspace_root: &std::path::Path,
    crate_name: &str,
    crate_path: &std::path::Path,
    from_tag: Option<&str>,
    to_ref: Option<&str>,
    chronology: anodizer_core::config::Chronology,
    multitrack: bool,
    crate_names: &[String],
    existing_override: Option<&str>,
) -> Result<Option<ChangelogUpdate>> {
    // `chronology` is accepted for signature symmetry with
    // `render_root_section`; refreshing the `[Unreleased]` block never slots a
    // dated section, so it has no ordering effect here.
    let _ = chronology;

    // The `[Unreleased]` refresh has no release version/tag in hand; only
    // `ProjectName`/`Name` carry meaning for a templated group title here.
    let section_vars = SectionVars {
        crate_name,
        version: "",
        tag: "",
    };
    let (cfg_present, body) =
        refresh_body(workspace_root, crate_path, from_tag, to_ref, section_vars)?;
    if !cfg_present {
        return Ok(None);
    }

    let file_path = workspace_root.join("CHANGELOG.md");
    let existing = match existing_override {
        Some(text) => Some(text.to_string()),
        None => read_existing(&file_path)?,
    };

    // Topology drives the decision; the existing-subsection fallback only rescues
    // the `--crate`-filtered single-target case where the count can't say "multi".
    let use_multitrack = multitrack
        || existing
            .as_deref()
            .is_some_and(|e| has_crate_subsections(e, crate_names));

    // Root file: a synthesized H1 is the project header, never a crate name.
    let h1 = project_h1(workspace_root);
    let merged = if use_multitrack {
        replace_crate_subsection_body(existing.as_deref(), &h1, crate_name, &body)
    } else {
        replace_unreleased_body(existing.as_deref(), &h1, &body)
    };

    if existing.as_deref() == Some(merged.as_str()) {
        return Ok(None);
    }

    Ok(Some(ChangelogUpdate {
        file_path,
        rendered_text: merged,
        insertion_mode: InsertionMode::Replace,
    }))
}

/// Read `file_path` to a string, returning `Ok(None)` when it does not exist.
pub(crate) fn read_existing(file_path: &std::path::Path) -> Result<Option<String>> {
    if !file_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(file_path)
        .with_context(|| format!("failed to read {}", file_path.display()))?;
    Ok(Some(text))
}

/// The descriptive H1 for a PER-CRATE `CHANGELOG.md`: `# Changelog — <crate>`.
/// Used identically by the per-crate refresh and per-crate promote paths so the
/// two never disagree on the synthesized title.
pub(crate) fn crate_h1(crate_name: &str) -> String {
    format!("# Changelog — {}", crate_name)
}

/// The H1 for the SHARED ROOT `CHANGELOG.md`: the project header. Renders
/// `changelog.header` from the discovered anodizer config when it is an
/// inline string (resolving `{{ ProjectName }}` against `project_name`),
/// else `# Changelog`.
///
/// A root H1 must NEVER carry a crate name. `from_file` / `from_url` header
/// sources need a full release [`Context`] to resolve and are handled by the
/// release-pipeline header path; this absent-file synthesis falls back to the
/// plain `# Changelog` default (an EXISTING root H1 is always preserved, so the
/// default only applies on first creation).
pub(crate) fn project_h1(workspace_root: &std::path::Path) -> String {
    const DEFAULT: &str = "# Changelog";
    let Some(cfg_path) = anodizer_core::config::find_config_candidate_in(workspace_root) else {
        return DEFAULT.to_string();
    };
    let Ok(raw) = anodizer_core::config::load_raw_config_value(&cfg_path) else {
        return DEFAULT.to_string();
    };

    let header = raw
        .get("changelog")
        .and_then(|c| c.get("header"))
        .and_then(|h| h.as_str());
    let Some(header) = header else {
        return DEFAULT.to_string();
    };

    let project_name = raw
        .get("project_name")
        .and_then(|n| n.as_str())
        .unwrap_or_default();
    let mut vars = TemplateVars::new();
    vars.set("ProjectName", project_name);
    let rendered = template::render(header, &vars)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if rendered.is_empty() {
        DEFAULT.to_string()
    } else {
        rendered
    }
}

/// Build a fresh Keep-a-Changelog skeleton holding only an `## [Unreleased]`
/// section with `body` (empty body collapses to a blank section), titled with
/// `h1` (the per-crate or project H1, WITHOUT a trailing newline). The single
/// owner of the skeleton shape.
pub(crate) fn kac_skeleton(h1: &str, body: &str) -> String {
    if body.is_empty() {
        format!("{}\n\n## [Unreleased]\n", h1)
    } else {
        format!("{}\n\n## [Unreleased]\n\n{}\n", h1, body)
    }
}

/// Wrap an already-demoted (`#### <Group>`) crate body under its `### <crate>`
/// heading, or return the empty string for an empty body (no heading is emitted
/// for a crate with no commits). The single source of the multi-track
/// subsection shape (`### <crate>` + blank + body).
pub(crate) fn wrap_subsection(crate_name: &str, body: &str) -> String {
    if body.is_empty() {
        String::new()
    } else {
        format!("### {}\n\n{}", crate_name, body)
    }
}

/// Replace the `## [Unreleased]` body of a flat changelog with `body`,
/// preserving the H1, every released section, and the footer verbatim.
///
/// `existing == None` (absent file) yields a fresh KAC skeleton titled with
/// `h1`. An existing file with an H1 but no `## [Unreleased]` heading gets the
/// section inserted directly after the H1, preserving the rest (its existing H1
/// is kept; `h1` only seeds an absent file).
pub(crate) fn replace_unreleased_body(existing: Option<&str>, h1: &str, body: &str) -> String {
    let Some(existing) = existing else {
        return kac_skeleton(h1, body);
    };

    let lines: Vec<&str> = existing.lines().collect();
    let trailing_newline = existing.ends_with('\n');

    let Some(unreleased_idx) = lines.iter().position(|l| is_unreleased_heading(l)) else {
        return insert_unreleased_after_h1(&lines, h1, body, trailing_newline);
    };

    // Bound the existing `[Unreleased]` body: from after the heading to the
    // first following `## ` section heading or compare-link footer line.
    let mut body_end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(unreleased_idx + 1) {
        if is_section_heading(line) || parse_unreleased_footer(line).is_some() {
            body_end = i;
            break;
        }
    }

    let mut out: Vec<String> = Vec::new();
    // Head: everything up to and including the `## [Unreleased]` heading.
    out.extend(lines[..=unreleased_idx].iter().map(|s| s.to_string()));
    // Fresh body, fenced by single blank lines, only when non-empty.
    out.push(String::new());
    if !body.is_empty() {
        out.extend(body.lines().map(|s| s.to_string()));
        out.push(String::new());
    }
    // Tail: the next section / footer onward, verbatim.
    out.extend(lines[body_end..].iter().map(|s| s.to_string()));

    finish(out, trailing_newline)
}

/// Insert a `## [Unreleased]` section (carrying the already-shaped `body`)
/// immediately after the leading H1 of a non-KAC file, preserving the rest.
/// Synthesizes a skeleton when no H1 is present.
///
/// `body` is inserted verbatim under the `## [Unreleased]` heading, so callers
/// pass either a flat group body (`### <Group>` …) or a wrapped crate
/// subsection (`### <crate>` …) — the spine is identical either way. `h1` titles
/// a synthesized skeleton when the file has no H1 to anchor to; an existing H1 is
/// preserved.
pub(crate) fn insert_unreleased_after_h1(
    lines: &[&str],
    h1: &str,
    body: &str,
    trailing_newline: bool,
) -> String {
    let Some(h1_idx) = lines.iter().position(|l| l.starts_with("# ")) else {
        // No H1 to anchor to: fall back to a fresh skeleton, then append the
        // prior content so nothing is lost.
        let skeleton = kac_skeleton(h1, body);
        if lines.is_empty() {
            return skeleton;
        }
        let rest = lines.join("\n");
        return finish(
            vec![skeleton.trim_end().to_string(), String::new(), rest],
            trailing_newline,
        );
    };

    let mut out: Vec<String> = Vec::new();
    out.extend(lines[..=h1_idx].iter().map(|s| s.to_string()));
    out.push(String::new());
    out.push("## [Unreleased]".to_string());
    if !body.is_empty() {
        out.push(String::new());
        out.extend(body.lines().map(|s| s.to_string()));
    }

    // Re-attach the post-H1 remainder (skipping a single blank line right after
    // the H1 so we don't double it).
    let mut tail_start = h1_idx + 1;
    if lines.get(tail_start).is_some_and(|l| l.trim().is_empty()) {
        tail_start += 1;
    }
    if tail_start < lines.len() {
        out.push(String::new());
        out.extend(lines[tail_start..].iter().map(|s| s.to_string()));
    }

    finish(out, trailing_newline)
}

/// Demote a generated grouped body's `### <Group>` headings one level deeper to
/// `#### <Group>` so they nest correctly UNDER a `### <crate>` subsection
/// (`### <crate>` is the crate heading; its groups must sit at `####`). Only
/// lines that are exactly an H3 heading are re-leveled; bullets, blank lines,
/// and any already-deeper heading are passed through verbatim.
pub(crate) fn demote_group_headings(body: &str) -> String {
    body.lines()
        .map(|line| match line.strip_prefix("### ") {
            Some(rest) if !rest.starts_with('#') => format!("#### {}", rest),
            _ => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace only `crate_name`'s `### <crate>` subsection body under
/// `## [Unreleased]` in a multi-track root, preserving sibling subsections,
/// released sections, and the footer verbatim.
///
/// The crate body's group headings are demoted to `#### <Group>` so they nest
/// under the `### <crate>` heading. Creation is data-loss-safe and omits empty
/// crates:
/// - `existing == None` (absent file) → a fresh KAC skeleton carrying this
///   crate's subsection (only when `body` is non-empty).
/// - `[Unreleased]` present without this crate's subsection → APPEND the
///   subsection at the end of the block (only when `body` is non-empty),
///   preserving every sibling subsection, released history, and the footer.
/// - File with an H1 but no `[Unreleased]` → insert an `[Unreleased]` after the
///   H1 carrying the subsection.
///
/// A crate with an empty `body` emits NO heading: when its subsection is absent
/// nothing is added, and when it already exists the prior content is left
/// untouched (R4 — an empty range never blanks a curated subsection). A
/// non-empty `body` regenerates an existing subsection in place (idempotent for
/// a fixed commit set).
pub(crate) fn replace_crate_subsection_body(
    existing: Option<&str>,
    h1: &str,
    crate_name: &str,
    body: &str,
) -> String {
    // `nested_body`: the demoted (`#### <Group>`) crate body, unwrapped — spliced
    // under an EXISTING `### <crate>` heading. `subsection`: the same body WRAPPED
    // under a fresh `### <crate>` heading (empty body → empty, so a crate with no
    // commits is omitted) — used when a subsection must be CREATED.
    let nested_body = demote_group_headings(body);
    let subsection = wrap_subsection(crate_name, &nested_body);

    let Some(existing) = existing else {
        // Absent file: bootstrap a skeleton (titled with the project `h1`)
        // carrying this crate's subsection (or a bare `[Unreleased]` when the
        // crate has no commits, so a later crate's refresh has an anchor to
        // append to).
        return kac_skeleton(h1, &subsection);
    };

    let lines: Vec<&str> = existing.lines().collect();
    let trailing_newline = existing.ends_with('\n');

    let Some(unreleased_idx) = lines.iter().position(|l| is_unreleased_heading(l)) else {
        // No `[Unreleased]` heading: insert one after the H1 carrying the
        // subsection, preserving the rest of the file.
        return insert_unreleased_after_h1(&lines, h1, &subsection, trailing_newline);
    };

    // Bound the `[Unreleased]` block: up to the first `## ` heading or footer.
    let mut unreleased_end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(unreleased_idx + 1) {
        if is_section_heading(line) || parse_unreleased_footer(line).is_some() {
            unreleased_end = i;
            break;
        }
    }

    // Locate this crate's `### <crate>` subsection within the block.
    let mut sub_start: Option<usize> = None;
    for (i, line) in lines
        .iter()
        .enumerate()
        .take(unreleased_end)
        .skip(unreleased_idx + 1)
    {
        if is_subsection_heading(line).is_some_and(|name| name == crate_name) {
            sub_start = Some(i);
            break;
        }
    }

    let mut out: Vec<String> = Vec::new();

    match sub_start {
        Some(_) if nested_body.is_empty() => {
            // Existing subsection, no new commits in range: leave it untouched
            // (R4). Returning the input unchanged makes the no-op detectable
            // (merged == existing).
            return existing.to_string();
        }
        Some(start) => {
            // Bound the existing subsection body: to the next `### `/`## `/footer.
            let mut sub_end = unreleased_end;
            for (i, line) in lines.iter().enumerate().skip(start + 1) {
                if is_subsection_heading(line).is_some()
                    || is_section_heading(line)
                    || parse_unreleased_footer(line).is_some()
                {
                    sub_end = i;
                    break;
                }
            }
            // Head through the subsection heading, then a blank line before the
            // body — matching the flat path and the append branch. `nested_body`
            // is non-empty here (the empty case returned above).
            out.extend(lines[..=start].iter().map(|s| s.to_string()));
            out.push(String::new());
            out.extend(nested_body.lines().map(|s| s.to_string()));
            out.push(String::new());
            // Tail from the next subsection / section / footer.
            out.extend(lines[sub_end..].iter().map(|s| s.to_string()));
        }
        None if nested_body.is_empty() => {
            // No subsection and no commits: emit nothing for this crate.
            // Returning the input unchanged makes the no-op detectable
            // (merged == existing).
            return existing.to_string();
        }
        None => {
            // Append a fresh subsection at the end of the `[Unreleased]` block.
            let mut block_end = unreleased_end;
            while block_end > unreleased_idx + 1 && lines[block_end - 1].trim().is_empty() {
                block_end -= 1;
            }
            out.extend(lines[..block_end].iter().map(|s| s.to_string()));
            out.push(String::new());
            out.extend(subsection.lines().map(|s| s.to_string()));
            out.push(String::new());
            out.extend(lines[unreleased_end..].iter().map(|s| s.to_string()));
        }
    }

    finish(out, trailing_newline)
}

/// Whether `line` toggles a fenced code block: a ```` ``` ```` or `~~~` fence
/// (allowing indentation and a trailing info string). Used to keep blank-line
/// collapsing from touching the interior of a preserved code fence.
pub(crate) fn is_code_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// Join rebuilt lines, collapsing runs of 2+ consecutive blank lines to a
/// single blank and restoring the file's original trailing-newline state.
/// Keeps refresh output idempotent regardless of how many blank lines the
/// splice produced.
///
/// Blank-collapsing is FENCE-AWARE: blank lines inside a fenced code block
/// (```` ``` ````/`~~~`) are passed through untouched, so intentional spacing
/// inside a preserved released section's code fence survives a refresh
/// byte-for-byte.
pub(crate) fn finish(lines: Vec<String>, trailing_newline: bool) -> String {
    let mut collapsed: Vec<String> = Vec::with_capacity(lines.len());
    let mut blanks = 0usize;
    let mut in_fence = false;
    for line in lines {
        if is_code_fence(&line) {
            in_fence = !in_fence;
            blanks = 0;
            collapsed.push(line);
            continue;
        }
        if !in_fence && line.trim().is_empty() {
            blanks += 1;
            if blanks >= 2 {
                continue;
            }
            collapsed.push(String::new());
        } else {
            // Interior fence content (including blank lines) is preserved
            // verbatim; only out-of-fence blanks reset/advance the run counter.
            if !line.trim().is_empty() {
                blanks = 0;
            }
            collapsed.push(line);
        }
    }
    // Drop any trailing blank lines; the newline state is applied below. A
    // trailing blank inside an unterminated fence is preserved (the fence
    // interior is verbatim), so only collapse when not mid-fence.
    if !in_fence {
        while collapsed.last().is_some_and(|l| l.trim().is_empty()) {
            collapsed.pop();
        }
    }
    let mut result = collapsed.join("\n");
    if trailing_newline {
        result.push('\n');
    }
    result
}
