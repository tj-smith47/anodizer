use super::*;

/// Inputs to [`merge_into_changelog`].
///
/// Bundles the file location plus everything the Keep-a-Changelog roll needs
/// (the previous release ref, the new version, and the generated commit body
/// used to fill an empty `## [Unreleased]` section).
pub(crate) struct MergeArgs<'a> {
    /// Absolute path of the crate's `CHANGELOG.md` (may not yet exist).
    pub(crate) file_path: &'a std::path::Path,
    /// H1 used ONLY to synthesize a title for an absent file (or a file with no
    /// H1). An existing H1 is always preserved. The caller passes
    /// [`crate_h1`] for a per-crate file or [`project_h1`] for the shared root,
    /// so the title never keys off the crate name in the root case.
    pub(crate) h1: &'a str,
    /// Fully-rendered `## [<version>] - <date>\n\n<body>\n` section used by
    /// the non-KAC splice path.
    pub(crate) new_section: &'a str,
    /// The generated commit body (no heading), used to fill a KAC
    /// `## [Unreleased]` section that the user left empty.
    pub(crate) generated_body: &'a str,
    /// Previous release tag (e.g. `v0.5.0` or `crate-v0.5.0`), or `None` for
    /// a first release. Used to derive the tag prefix when no footer link
    /// exists.
    pub(crate) from_tag: Option<&'a str>,
    /// Version being released (e.g. `0.6.0`).
    pub(crate) to_version: &'a str,
    /// Repository root, used to resolve the `origin` remote when a KAC file
    /// has a `## [Unreleased]` heading but no `[Unreleased]:` footer link.
    pub(crate) workspace_root: &'a std::path::Path,
}

/// Case-insensitive match for a `## [Unreleased]` heading line (allowing
/// trailing whitespace), the marker for a Keep-a-Changelog-shaped file.
pub(crate) fn is_unreleased_heading(line: &str) -> bool {
    let trimmed = line.trim_end();
    let Some(rest) = trimmed.strip_prefix("##") else {
        return false;
    };
    let rest = rest.trim_start();
    rest.eq_ignore_ascii_case("[unreleased]")
}

/// Whether a line opens a new top-level changelog section (`## ...`).
pub(crate) fn is_section_heading(line: &str) -> bool {
    line.starts_with("## ")
}

/// Whether `line` is a `## [<version>]` heading for the exact `version`
/// (allowing an optional ` - <date>` suffix and trailing whitespace). Used to
/// detect a same-version section already present so a second roll is a no-op.
pub(crate) fn is_version_heading(line: &str, version: &str) -> bool {
    let Some(rest) = line.trim_end().strip_prefix("##") else {
        return false;
    };
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix('[') else {
        return false;
    };
    let Some(close) = rest.find(']') else {
        return false;
    };
    &rest[..close] == version
}

/// Merge a freshly-rendered release into the crate's `CHANGELOG.md`.
///
/// Detects the Keep-a-Changelog shape (a `## [Unreleased]` heading) and, in
/// that mode, performs the standard release roll; otherwise falls back to
/// splicing `new_section` directly after the leading H1.
pub(crate) fn merge_into_changelog(args: MergeArgs<'_>) -> Result<String> {
    let MergeArgs {
        file_path,
        h1,
        new_section,
        generated_body,
        from_tag,
        to_version,
        workspace_root,
    } = args;

    let header = format!("{}\n\n", h1);
    if !file_path.is_file() {
        return Ok(format!("{}{}", header, new_section));
    }
    let existing = std::fs::read_to_string(file_path)
        .with_context(|| format!("failed to read {}", file_path.display()))?;

    if existing.lines().any(is_unreleased_heading) {
        return roll_keep_a_changelog(KacRollArgs {
            existing: &existing,
            generated_body,
            from_tag,
            to_version,
            workspace_root,
        });
    }

    splice_after_h1(&existing, new_section, &header)
}

/// Splice `new_section` after the leading H1, preserving any prelude.
/// Synthesizes `header` + section when the file has no H1.
pub(crate) fn splice_after_h1(existing: &str, new_section: &str, header: &str) -> Result<String> {
    let mut head = String::new();
    let mut tail = String::new();
    let mut consumed_h1 = false;
    let mut blank_after_h1_seen = false;
    for line in existing.lines() {
        if !consumed_h1 {
            head.push_str(line);
            head.push('\n');
            if line.starts_with("# ") {
                consumed_h1 = true;
            }
            continue;
        }
        if !blank_after_h1_seen {
            // Consume one blank line right after the H1 to keep formatting.
            if line.trim().is_empty() {
                head.push('\n');
                blank_after_h1_seen = true;
                continue;
            }
            blank_after_h1_seen = true;
        }
        tail.push_str(line);
        tail.push('\n');
    }
    if !consumed_h1 {
        // No H1 found — synthesize one and place existing content after our
        // new section.
        return Ok(format!("{}{}\n{}", header, new_section, existing));
    }
    Ok(format!("{}{}\n{}", head, new_section, tail))
}

/// Inputs to [`roll_keep_a_changelog`].
pub(crate) struct KacRollArgs<'a> {
    existing: &'a str,
    generated_body: &'a str,
    from_tag: Option<&'a str>,
    to_version: &'a str,
    workspace_root: &'a std::path::Path,
}

/// The tag/anchor prefix that precedes its version (`v0.5.0` → `v`,
/// `anodizer-v0.5.0` → `anodizer-v`, `2024.01.0` → `""`). Used to cluster a
/// `Tag`-chronology changelog by track.
///
/// Strips the trailing version group (`v?<digits>.<digits>[...]`, optionally
/// after a `-`/`_`/`/` separator, or `v`-led / bare-digit at the start) and
/// returns what remains. This tolerates a digit *inside* the prefix
/// (`py3-v1.2.3` → `py3-v`, where a first-digit scan would wrongly yield `py`)
/// and a leading-digit calendar version (`2024.01.0` → `""`).
pub(crate) fn tag_prefix(anchor: &str) -> String {
    // The trailing version core (`\d+\.\d+[...]`) is captured so the prefix is
    // simply everything before it. Mirrors `git::parse_semver_tag`'s version
    // recognition so prefix clustering and version parsing agree on where the
    // version begins. A `v` immediately before the digits stays in the prefix
    // (`anodizer-v1.2.3` → `anodizer-v`).
    static VERSION_CORE_RE: LazyLock<Regex> =
        LazyLock::new(|| anodizer_core::util::static_regex(r"\d+\.\d+(?:\.\d+)?(?:[-+].*)?$"));
    match VERSION_CORE_RE.find(anchor) {
        Some(m) => anchor[..m.start()].to_string(),
        // No recognizable trailing version: fall back to the whole anchor.
        None => anchor.to_string(),
    }
}

/// Perform the Keep-a-Changelog release roll on `existing`:
///   1. promote `## [Unreleased]` to `## [<version>] - <date>`,
///   2. preserve a curated body verbatim (else fill from generated commits),
///   3. insert a fresh empty `## [Unreleased]` above it,
///   4. roll the `[Unreleased]` / `[<version>]` compare-link footer.
pub(crate) fn roll_keep_a_changelog(args: KacRollArgs<'_>) -> Result<String> {
    let KacRollArgs {
        existing,
        generated_body,
        from_tag,
        to_version,
        workspace_root,
    } = args;

    let lines: Vec<&str> = existing.lines().collect();

    // Idempotence: if a `## [<to_version>]` section already exists, the roll
    // has already happened for this version. Promoting again would emit a
    // duplicate same-version section, so return the file unchanged.
    if lines.iter().any(|l| is_version_heading(l, to_version)) {
        return Ok(existing.to_string());
    }

    // Locate the `## [Unreleased]` heading and the start of the next section
    // (next `## ` heading) which bounds the Unreleased body. The footer link
    // block (if any) lives at or after the last section and is handled
    // separately, so the body scan also stops at the first footer-link line.
    let Some(unreleased_idx) = lines.iter().position(|l| is_unreleased_heading(l)) else {
        // Caller only invokes this when an Unreleased heading exists.
        return Ok(existing.to_string());
    };

    let mut body_end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(unreleased_idx + 1) {
        if is_section_heading(line) || parse_unreleased_footer(line).is_some() {
            body_end = i;
            break;
        }
    }

    let curated_body: Vec<&str> = lines[unreleased_idx + 1..body_end].to_vec();
    // First/last non-blank line bounds of the curated block. `Some` exactly
    // when the user left curated content under `## [Unreleased]`; `None` means
    // the section was empty and the body is filled from generated commits.
    let curated_bounds = curated_body
        .iter()
        .position(|l| !l.trim().is_empty())
        .map(|start| {
            // `rposition` is `Some` whenever `position` was, so the closing
            // bound is taken on the same guaranteed-non-empty slice.
            let end = curated_body
                .iter()
                .rposition(|l| !l.trim().is_empty())
                .map_or(start + 1, |i| i + 1);
            (start, end)
        });

    let date = today_yyyy_mm_dd();
    let promoted_heading = format!("## [{}] - {}", to_version, date);

    let mut out_lines: Vec<String> = Vec::new();
    // Everything before the Unreleased heading stays byte-identical.
    out_lines.extend(lines[..unreleased_idx].iter().map(|s| s.to_string()));

    // Fresh empty Unreleased section above the promoted release.
    out_lines.push("## [Unreleased]".to_string());
    out_lines.push(String::new());

    // Promoted release heading + its body (curated verbatim, else generated).
    out_lines.push(promoted_heading);
    out_lines.push(String::new());
    if let Some((start, end)) = curated_bounds {
        // Curated block with leading/trailing blanks trimmed, interior verbatim.
        out_lines.extend(curated_body[start..end].iter().map(|s| s.to_string()));
    } else {
        out_lines.extend(generated_body.lines().map(|s| s.to_string()));
    }
    out_lines.push(String::new());

    // Everything from the next section onward, with the footer rolled.
    let tail = &lines[body_end..];
    roll_footer(&mut out_lines, tail, from_tag, to_version, workspace_root)?;

    let mut result = out_lines.join("\n");
    if existing.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
}

/// Parse `[Unreleased]: <url>` (case-insensitive on the label, allowing
/// trailing whitespace) and return the URL.
pub(crate) fn parse_unreleased_footer(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('[')?;
    let close = rest.find(']')?;
    let (label, after) = rest.split_at(close);
    if !label.eq_ignore_ascii_case("unreleased") {
        return None;
    }
    let after = after.strip_prefix("]:")?;
    Some(after.trim())
}

/// Split a `<base>/compare/<anchor>...HEAD` compare URL into
/// `(base_including_compare, old_anchor)`.
pub(crate) fn parse_compare_url(url: &str) -> Option<(&str, &str)> {
    let (base, rest) = url.split_once("/compare/")?;
    let anchor = rest.strip_suffix("...HEAD")?;
    if anchor.is_empty() {
        return None;
    }
    Some((base, anchor))
}

/// Append the tail (next section onward) to `out_lines`, rolling the
/// `[Unreleased]:` compare-link footer when one is present.
pub(crate) fn roll_footer(
    out_lines: &mut Vec<String>,
    tail: &[&str],
    from_tag: Option<&str>,
    to_version: &str,
    workspace_root: &std::path::Path,
) -> Result<()> {
    // Locate an existing `[Unreleased]:` footer link in the tail.
    let footer_idx = tail
        .iter()
        .position(|l| parse_unreleased_footer(l).is_some());

    let Some(footer_idx) = footer_idx else {
        // No footer link. Synthesize one only when a remote compare base can
        // be resolved cheaply; otherwise pass the tail through unchanged.
        out_lines.extend(tail.iter().map(|s| s.to_string()));
        synthesize_footer(out_lines, from_tag, to_version, workspace_root);
        return Ok(());
    };

    let url = parse_unreleased_footer(tail[footer_idx]).unwrap_or("");
    let Some((base, old_anchor)) = parse_compare_url(url) else {
        // Footer present but not a recognized compare URL — leave as-is.
        out_lines.extend(tail.iter().map(|s| s.to_string()));
        return Ok(());
    };

    let prefix = tag_prefix(old_anchor);
    let new_tag = format!("{}{}", prefix, to_version);

    // Emit tail lines up to (not including) the footer link unchanged.
    out_lines.extend(tail[..footer_idx].iter().map(|s| s.to_string()));
    // Rolled `[Unreleased]:` link + the new `[<version>]:` link.
    push_compare_footer(out_lines, base, old_anchor, &new_tag, to_version);
    // Remaining footer lines (prior `[x.y.z]:` links) unchanged.
    out_lines.extend(tail[footer_idx + 1..].iter().map(|s| s.to_string()));

    Ok(())
}

/// Push the two compare-link footer lines that close a Keep-a-Changelog roll:
///
/// ```text
/// [Unreleased]: <base>/compare/<new_tag>...HEAD
/// [<to_version>]: <base>/compare/<old_anchor>...<new_tag>
/// ```
///
/// Single-sources the compare-URL shape so the roll path and the
/// synthesize path can never drift into producing a different link layout.
pub(crate) fn push_compare_footer(
    out_lines: &mut Vec<String>,
    base: &str,
    old_anchor: &str,
    new_tag: &str,
    to_version: &str,
) {
    out_lines.push(format!("[Unreleased]: {}/compare/{}...HEAD", base, new_tag));
    out_lines.push(format!(
        "[{}]: {}/compare/{}...{}",
        to_version, base, old_anchor, new_tag
    ));
}

/// Synthesize a `[Unreleased]:` / `[<version>]:` footer from the `origin`
/// remote when the KAC file lacks one.
///
/// The compare base is derived from the actual `origin` URL host, so a
/// self-hosted GitLab/Gitea KAC file gets a host-correct link rather than a
/// hardcoded `github.com` one (mirroring how the roll path preserves whatever
/// base an existing footer used). Skips gracefully (no footer appended) when
/// the previous tag or the remote cannot be resolved — a missing remote must
/// never fail the render.
pub(crate) fn synthesize_footer(
    out_lines: &mut Vec<String>,
    from_tag: Option<&str>,
    to_version: &str,
    workspace_root: &std::path::Path,
) {
    let Some(old_anchor) = from_tag else {
        return;
    };
    let Ok(base) = anodizer_core::git::detect_remote_web_base_in(workspace_root) else {
        return;
    };
    let prefix = tag_prefix(old_anchor);
    let new_tag = format!("{}{}", prefix, to_version);

    // Ensure a blank line separates the body from a freshly-added footer block.
    if out_lines.last().is_some_and(|l| !l.is_empty()) {
        out_lines.push(String::new());
    }
    push_compare_footer(out_lines, &base, old_anchor, &new_tag, to_version);
}
