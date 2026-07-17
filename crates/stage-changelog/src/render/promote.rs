use super::*;

/// Inputs to [`promote_subsection`], the pure root-CHANGELOG transform.
pub(crate) struct PromoteArgs<'a> {
    /// Current root `CHANGELOG.md` contents.
    pub(crate) existing: &'a str,
    /// Crate whose `### <crate>` subsection is being promoted.
    pub(crate) crate_name: &'a str,
    /// FULL new tag for this release (e.g. `v0.7.0`, `core-v0.5.1`).
    pub(crate) tag: &'a str,
    /// This track's previous tag, or `None` for its first release.
    pub(crate) from_tag: Option<&'a str>,
    /// Section ordering for slotting the promoted section.
    pub(crate) chronology: anodizer_core::config::Chronology,
    /// Configured commit groups, used to bucket curated bullets.
    pub(crate) groups: &'a [ChangelogGroup],
    /// Generated grouped body (already `### <GroupTitle>`-grouped), used when
    /// the crate has commits but no curated subsection.
    pub(crate) generated_body: &'a str,
    /// `<base>/compare` URL prefix for footer links, or `None` to omit them.
    pub(crate) base: Option<&'a str>,
}

/// Inputs to [`promote_multitrack_section`].
///
/// Kept distinct from [`PromoteArgs`] rather than folded: this path runs at tag
/// time before any refresh, so `existing` is `Option` (the root may not exist
/// yet) — `PromoteArgs` requires a present file with a `## [Unreleased]` heading.
pub(crate) struct MultitrackPromoteArgs<'a> {
    /// Current root `CHANGELOG.md` contents, or `None` when the file is absent.
    pub(crate) existing: Option<&'a str>,
    /// Project H1 ([`project_h1`]) used to title an absent root; an existing H1
    /// is always preserved. Never a crate name.
    pub(crate) h1: &'a str,
    /// Crate being released; its `### <crate>` subsection is consumed when one
    /// exists under `[Unreleased]`, else the section is built from commits.
    pub(crate) crate_name: &'a str,
    /// FULL new tag for this release (e.g. `aaa-v0.3.0`, `v0.2.0`).
    pub(crate) tag: &'a str,
    /// This track's previous tag, or `None` for its first release.
    pub(crate) from_tag: Option<&'a str>,
    /// Section ordering for slotting the promoted section.
    pub(crate) chronology: anodizer_core::config::Chronology,
    /// Configured commit groups, used to bucket a consumed curated subsection.
    pub(crate) groups: &'a [ChangelogGroup],
    /// This crate's generated grouped body (already `### <GroupTitle>`-grouped),
    /// used when no curated subsection is present.
    pub(crate) generated_body: &'a str,
    /// `<base>/compare` URL prefix for footer links, or `None` to omit them.
    pub(crate) base: Option<&'a str>,
}

/// Build the promoted dated section block: `## [<tag>] - <date>` followed by the
/// section `body` lines and a trailing blank. The body opens with its own
/// `### <GroupTitle>` (or a bare bullet) directly under the heading — no blank
/// line between — matching the Keep-a-Changelog shape of existing released
/// sections.
pub(crate) fn build_promoted_section(tag: &str, body: &str) -> Vec<String> {
    let mut promoted: Vec<String> = Vec::with_capacity(body.lines().count() + 2);
    promoted.push(format!("## [{}] - {}", tag, today_yyyy_mm_dd()));
    promoted.extend(body.lines().map(|s| s.to_string()));
    promoted.push(String::new());
    promoted
}

/// Slot a promoted section among the existing released sections of `tail` and
/// roll the per-track compare-link footer, appending the result onto `out`
/// (whose head — the rebuilt `[Unreleased]` block or the project H1 — the caller
/// has already pushed). `tail` is the existing-sections-plus-footer slice; it is
/// split at the first footer-link line so `slot_sections` only reorders dated
/// sections and the footer is rolled by `push_root_footer`. Single source of the
/// shared promote tail for both root-promote paths.
pub(crate) fn append_slotted_section(
    out: &mut Vec<String>,
    tail: &[&str],
    promoted: &[String],
    tag: &str,
    from_tag: Option<&str>,
    chronology: anodizer_core::config::Chronology,
    base: Option<&str>,
) {
    let footer_idx = tail
        .iter()
        .position(|l| parse_unreleased_footer(l).is_some());
    let (sections, footer): (&[&str], &[&str]) = match footer_idx {
        Some(fi) => (&tail[..fi], &tail[fi..]),
        None => (tail, &[]),
    };
    out.extend(slot_sections(sections, promoted, tag, chronology));
    push_root_footer(out, footer, tag, from_tag, base);
}

/// Promote one track's release into the shared multi-track root, emitting a
/// tag-prefixed `## [<tag>] - <date>` section built from THIS crate's commits,
/// accumulated among the existing dated sections and slotted per `chronology`.
///
/// Unlike [`promote_subsection`] this does NOT require a pre-existing
/// `### <crate>` subsection under `## [Unreleased]`: a release runs at tag time,
/// before any `changelog --write` refresh has built subsections. Three input
/// states all reach the same dated-section result:
/// - file absent / has no `## [Unreleased]` heading → synthesize a `# Changelog`
///   H1 (or preserve the file's existing H1) and slot the new section among any
///   already-promoted `## [<tag>]` sections;
/// - `## [Unreleased]` present with this crate's `### <crate>` subsection →
///   delegate to [`promote_subsection`], which consumes the subsection.
///
/// Returns `Ok(None)` when there is nothing to release (no commits and no
/// curated subsection). Idempotent: a `## [<tag>]` section already present is
/// returned unchanged.
pub(crate) fn promote_multitrack_section(
    args: MultitrackPromoteArgs<'_>,
) -> Result<Option<String>> {
    let MultitrackPromoteArgs {
        existing,
        h1,
        crate_name,
        tag,
        from_tag,
        chronology,
        groups,
        generated_body,
        base,
    } = args;

    // When the root already carries a `## [Unreleased]` heading, reuse the
    // subsection-consuming promote (the refresh-then-tag workflow) so a curated
    // `### <crate>` body is honored verbatim.
    if let Some(existing) = existing
        && existing.lines().any(is_unreleased_heading)
    {
        return promote_subsection(PromoteArgs {
            existing,
            crate_name,
            tag,
            from_tag,
            chronology,
            groups,
            generated_body,
            base,
        });
    }

    // No `[Unreleased]` to consume: build the dated section straight from this
    // crate's commits. Nothing to release when the crate produced no body.
    if generated_body.is_empty() {
        return Ok(None);
    }

    let existing = existing.unwrap_or("");
    let lines: Vec<&str> = existing.lines().collect();

    // Idempotence: this track's section already promoted.
    if lines.iter().any(|l| is_version_heading(l, tag)) {
        return Ok(Some(existing.to_string()));
    }

    // Preserve the file's existing H1, else synthesize the project header `h1`
    // (a project title, never a crate name).
    let h1_idx = lines.iter().position(|l| l.starts_with("# "));
    let head: Vec<String> = match h1_idx {
        Some(idx) => lines[..=idx].iter().map(|s| s.to_string()).collect(),
        None => vec![h1.to_string()],
    };

    // Existing released sections + footer live after the H1 (or are the whole
    // file when no H1). Drop blank padding that bounds them; `slot_sections`
    // re-fences with single blanks and the footer push restores spacing.
    let rest: &[&str] = match h1_idx {
        Some(idx) => &lines[idx + 1..],
        None => &lines,
    };
    let tail: Vec<&str> = rest
        .iter()
        .copied()
        .skip_while(|l| l.trim().is_empty())
        .collect();

    let promoted = build_promoted_section(tag, generated_body);

    let mut out: Vec<String> = head;
    out.push(String::new());
    append_slotted_section(&mut out, &tail, &promoted, tag, from_tag, chronology, base);

    // `finish` collapses the blank-line padding the slot/footer assembly may
    // butt together and restores the trailing-newline state (an absent file
    // gets one).
    let trailing = existing.is_empty() || existing.ends_with('\n');
    Ok(Some(finish(out, trailing)))
}

/// Pure transform: promote `crate_name`'s `### <crate>` subsection out of
/// `## [Unreleased]` into a released `## [<tag>] - <date>` section, regroup its
/// bullets under `### <GroupTitle>` headings, slot it by `chronology`, and roll
/// the per-track compare-link footer. Returns `Ok(None)` when the crate has
/// neither a curated subsection nor generated commits (nothing to release).
pub(crate) fn promote_subsection(args: PromoteArgs<'_>) -> Result<Option<String>> {
    let PromoteArgs {
        existing,
        crate_name,
        tag,
        from_tag,
        chronology,
        groups,
        generated_body,
        base,
    } = args;

    let lines: Vec<&str> = existing.lines().collect();

    // Idempotence: a `## [<tag>]` section already present means this track's
    // roll already happened — return the file unchanged.
    if lines.iter().any(|l| is_version_heading(l, tag)) {
        return Ok(Some(existing.to_string()));
    }

    let Some(unreleased_idx) = lines.iter().position(|l| is_unreleased_heading(l)) else {
        return Ok(Some(existing.to_string()));
    };

    // Bound the `[Unreleased]` block: up to the first `## ` section heading or
    // footer-link line.
    let mut unreleased_end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(unreleased_idx + 1) {
        if is_section_heading(line) || parse_unreleased_footer(line).is_some() {
            unreleased_end = i;
            break;
        }
    }

    // Locate this crate's `### <crate>` subsection within `[Unreleased]`.
    let mut sub_start: Option<usize> = None;
    let mut idx = unreleased_idx + 1;
    while idx < unreleased_end {
        if let Some(name) = is_subsection_heading(lines[idx])
            && name == crate_name
        {
            sub_start = Some(idx);
            break;
        }
        idx += 1;
    }

    // Curated bullets (verbatim) when the subsection exists; the bounds run
    // from after its heading to the next `### `/`## `/footer line.
    let curated: Vec<&str> = match sub_start {
        Some(start) => {
            let mut end = unreleased_end;
            for (i, line) in lines.iter().enumerate().skip(start + 1) {
                if is_subsection_heading(line).is_some()
                    || is_section_heading(line)
                    || parse_unreleased_footer(line).is_some()
                {
                    end = i;
                    break;
                }
            }
            lines[start + 1..end]
                .iter()
                .copied()
                .filter(|l| !l.trim().is_empty())
                .collect()
        }
        None => Vec::new(),
    };

    // Build the promoted section body. Curated bullets are bucketed verbatim;
    // an absent / empty subsection falls back to the generated grouped body.
    let body = if !curated.is_empty() {
        bucket_curated_bullets(&curated, groups)?
    } else if !generated_body.is_empty() {
        generated_body.to_string()
    } else {
        // No curated subsection and no commits: nothing to release.
        return Ok(None);
    };

    let promoted = build_promoted_section(tag, &body);

    // Rebuild the `[Unreleased]` block with this crate's subsection removed and
    // every other subsection byte-identical, then slot the promoted section among
    // the existing dated sections and roll the per-track footer (shared tail).
    let mut out: Vec<String> =
        rebuild_unreleased(&lines, unreleased_idx, unreleased_end, sub_start);
    let tail = &lines[unreleased_end..];
    append_slotted_section(&mut out, tail, &promoted, tag, from_tag, chronology, base);

    // Manual newline finish (not `finish`): the rebuilt `[Unreleased]` block is
    // already byte-stable, so collapsing here could perturb its spacing.
    let mut result = out.join("\n");
    if existing.ends_with('\n') {
        result.push('\n');
    }
    Ok(Some(result))
}

/// Rebuild the `[Unreleased]` block (heading + remaining subsections) with the
/// subsection at `sub_start` removed. Every other line — including other
/// crates' subsections — is preserved byte-identically. A single trailing blank
/// line is kept after the block.
pub(crate) fn rebuild_unreleased(
    lines: &[&str],
    unreleased_idx: usize,
    unreleased_end: usize,
    sub_start: Option<usize>,
) -> Vec<String> {
    let mut block: Vec<String> = Vec::new();
    // Pre-`[Unreleased]` content (H1, prelude) stays verbatim.
    block.extend(lines[..unreleased_idx].iter().map(|s| s.to_string()));
    block.push(lines[unreleased_idx].to_string());

    // Bound of the removed subsection (if present).
    let removed_end = sub_start.map(|start| {
        let mut end = unreleased_end;
        for (i, line) in lines.iter().enumerate().skip(start + 1) {
            if is_subsection_heading(line).is_some()
                || is_section_heading(line)
                || parse_unreleased_footer(line).is_some()
            {
                end = i;
                break;
            }
        }
        end
    });

    let mut i = unreleased_idx + 1;
    while i < unreleased_end {
        if let (Some(start), Some(end)) = (sub_start, removed_end)
            && i >= start
            && i < end
        {
            i = end;
            continue;
        }
        block.push(lines[i].to_string());
        i += 1;
    }

    // Normalize to exactly one trailing blank line after the block.
    while block.last().is_some_and(|l| l.trim().is_empty()) {
        block.pop();
    }
    block.push(String::new());
    block
}

/// Bucket curated bullet lines under `### <GroupTitle>` headings, matching each
/// bullet's leading conventional-commit type against `groups` (first-match-wins,
/// a group with empty/absent `regexp` is the catch-all). Bullets are kept
/// VERBATIM — never re-rendered through the commit template. A bullet matching
/// no group and no catch-all is appended at the end under no heading so curated
/// content is never silently dropped. With no groups configured, bullets are
/// emitted flat in their original order.
pub(crate) fn bucket_curated_bullets(
    curated: &[&str],
    groups: &[ChangelogGroup],
) -> Result<String> {
    if groups.is_empty() {
        return Ok(curated.join("\n"));
    }

    // Compile group regexes in config order; an empty/absent regexp marks the
    // catch-all. Shares `compile_group_regexes` with `group_commits` so the
    // two paths agree on first-match-wins, the empty≡absent catch-all sentinel,
    // and the hard-fail on an invalid pattern — otherwise a typo'd regexp would
    // silently become a catch-all and shadow the real one.
    let compiled = compile_group_regexes(groups)?;
    let catch_all_idx = compiled.iter().position(|(re, _)| re.is_none());

    let mut buckets: Vec<Vec<&str>> = vec![Vec::new(); compiled.len()];
    let mut unmatched: Vec<&str> = Vec::new();
    // Where the previous bullet landed, so a wrapped continuation line (no list
    // marker) follows its parent bullet instead of being re-classified.
    let mut last: Option<usize> = None;

    'bullet: for &line in curated {
        let Some(payload) = strip_list_marker(line) else {
            // Continuation of the previous bullet (indented / no `-`/`*`
            // marker): keep it with the parent's bucket, never re-classify.
            match last {
                Some(idx) => buckets[idx].push(line),
                None => unmatched.push(line),
            }
            continue 'bullet;
        };
        // Re-derive the conventional-commit subject so the group regexes match
        // against the same `raw_message` shape `group_commits` sees.
        let info = parse_commit_message(payload);
        let raw = &info.raw_message;
        for (idx, (re, _)) in compiled.iter().enumerate() {
            if catch_all_idx == Some(idx) {
                break;
            }
            if let Some(re) = re
                && re.is_match(raw)
            {
                buckets[idx].push(line);
                last = Some(idx);
                continue 'bullet;
            }
        }
        if let Some(ci) = catch_all_idx {
            buckets[ci].push(line);
            last = Some(ci);
        } else {
            unmatched.push(line);
            last = None;
        }
    }

    // Emit non-empty groups in `order` (config order for equal/absent order).
    let mut indexed: Vec<usize> = (0..compiled.len()).collect();
    indexed.sort_by_key(|&i| compiled[i].1.order.unwrap_or(i32::MAX));

    let mut out: Vec<String> = Vec::new();
    for &i in &indexed {
        if buckets[i].is_empty() {
            continue;
        }
        out.push(format!("### {}", compiled[i].1.title));
        out.extend(buckets[i].iter().map(|s| s.to_string()));
    }
    // Curated bullets that matched no group and had no catch-all to absorb them
    // are preserved at the end under no heading rather than dropped.
    out.extend(unmatched.iter().map(|s| s.to_string()));
    Ok(out.join("\n"))
}

/// Strip a leading Markdown list marker (`- ` or `* `, with optional
/// indentation) from a bullet line, returning the bare payload.
///
/// Returns `None` when the line carries no list marker — a wrapped
/// continuation of the preceding bullet — so the caller can attach it to that
/// bullet rather than re-classifying it as a fresh entry.
pub(crate) fn strip_list_marker(line: &str) -> Option<&str> {
    let t = line.trim_start();
    t.strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .map(str::trim_start)
}

/// Insert `promoted` (the new release section) among the existing released
/// `## [<...>]` sections per `chronology`, returning the full section list.
/// Existing section bodies are never re-sorted or re-emitted — this is
/// insert-only.
pub(crate) fn slot_sections(
    sections: &[&str],
    promoted: &[String],
    tag: &str,
    chronology: anodizer_core::config::Chronology,
) -> Vec<String> {
    use anodizer_core::config::Chronology;

    // Index where each existing `## [<...>]` section heading begins.
    let heading_idxs: Vec<usize> = sections
        .iter()
        .enumerate()
        .filter(|(_, l)| is_section_heading(l))
        .map(|(i, _)| i)
        .collect();

    let insert_at = match chronology {
        // Date: today's section is newest — insert before the first existing
        // released section.
        Chronology::Date => heading_idxs.first().copied().unwrap_or(sections.len()),
        Chronology::Tag => tag_insert_index(sections, &heading_idxs, tag),
    };

    let mut out: Vec<String> = Vec::new();
    out.extend(sections[..insert_at].iter().map(|s| s.to_string()));
    out.extend(promoted.iter().cloned());
    out.extend(sections[insert_at..].iter().map(|s| s.to_string()));
    out
}

/// Compute the insert index (into `sections`) that keeps the `Tag` ordering
/// invariant: clusters ascend lexically by tag-prefix, and within the new tag's
/// prefix cluster versions descend by semver.
pub(crate) fn tag_insert_index(sections: &[&str], heading_idxs: &[usize], tag: &str) -> usize {
    let new_prefix = tag_prefix(tag);
    let new_ver = anodizer_core::git::parse_semver_tag(tag).ok();

    for &hi in heading_idxs {
        let Some(existing_tag) = section_heading_tag(sections[hi]) else {
            continue;
        };
        let existing_prefix = tag_prefix(existing_tag);
        match new_prefix.cmp(&existing_prefix) {
            std::cmp::Ordering::Less => return hi,
            std::cmp::Ordering::Greater => continue,
            std::cmp::Ordering::Equal => {
                // Same cluster: insert before the first same-prefix section whose
                // semver is strictly less than the new version (semver-descending).
                let existing_ver = anodizer_core::git::parse_semver_tag(existing_tag).ok();
                match (&new_ver, &existing_ver) {
                    (Some(nv), Some(ev)) if nv > ev => return hi,
                    (Some(_), None) => return hi,
                    // Non-semver same-prefix tags fall back to lexical descending.
                    (None, _) if tag > existing_tag => return hi,
                    _ => continue,
                }
            }
        }
    }
    sections.len()
}

/// Extract the `<tag>` from a `## [<tag>] - <date>` (or `## [<tag>]`) heading.
pub(crate) fn section_heading_tag(line: &str) -> Option<&str> {
    let rest = line.trim_end().strip_prefix("##")?.trim_start();
    let rest = rest.strip_prefix('[')?;
    let close = rest.find(']')?;
    Some(&rest[..close])
}

/// Roll the compare-link footer for the root subsection-promote path. The new
/// `[<tag>]:` lower bound and the `[Unreleased]:` upper anchor both derive from
/// THIS track's `tag` / `from_tag` — never from a sibling track's existing
/// `[Unreleased]:` anchor. All other `[<x>]:` footer links are preserved.
pub(crate) fn push_root_footer(
    out: &mut Vec<String>,
    footer: &[&str],
    tag: &str,
    from_tag: Option<&str>,
    base: Option<&str>,
) {
    let Some(base) = base else {
        // No resolvable base — keep any existing footer verbatim, add nothing.
        out.extend(footer.iter().map(|s| s.to_string()));
        return;
    };

    // Ensure a blank line separates the body from the footer block.
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.push(String::new());

    if let Some(from) = from_tag {
        // Same compare-link layout as the flat roll, single-sourced through
        // `push_compare_footer`: `[Unreleased]: .../compare/<tag>...HEAD` and
        // `[<tag>]: .../compare/<from>...<tag>`. The `[<tag>]:` label is the
        // full tag because both the version-label and new-tag arguments are
        // `tag` for this track.
        push_compare_footer(out, base, from, tag, tag);
    } else {
        // First release of a track: roll the `[Unreleased]:` anchor by hand
        // and point `[<tag>]:` at the release page rather than a 404 compare
        // range (no prior tag to compare against).
        out.push(format!("[Unreleased]: {}/compare/{}...HEAD", base, tag));
        out.push(format!("[{}]: {}/releases/tag/{}", tag, base, tag));
    }
    // Preserve every prior `[<x>]:` link (skip the old `[Unreleased]:`).
    for &line in footer {
        if parse_unreleased_footer(line).is_some() {
            continue;
        }
        out.push(line.to_string());
    }
}

pub(crate) fn today_yyyy_mm_dd() -> String {
    let secs = anodizer_core::sde::resolve_now().timestamp();
    // Days since the Unix epoch, then convert to a (y,m,d) triple via the
    // Howard Hinnant date algorithm (`days_from_civil` inverse). Avoids a
    // chrono dep purely for date formatting in changelog headings.
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}
