//! Pure text-rewrite logic for bumping an existing Homebrew formula.
//!
//! The formula file is never parsed as Ruby — the bump rewrites the small,
//! rigidly-formatted stanzas Homebrew's own audit tooling enforces:
//! `url "..."` (optionally with `tag:`/`revision:` for git-based formulae),
//! the standalone source `sha256 "..."`, and the explicit `version "..."`
//! when present. Bottle-block `sha256` lines (`sha256 cellar: ...`,
//! `sha256 arm64_sonoma: "..."`) are structurally different (they carry a
//! key before the digest) and are deliberately left untouched — Homebrew's
//! CI rebuilds bottles after a version bump.

use anyhow::{Result, bail};

/// The replacement values one bump writes into the formula text.
#[derive(Debug, Clone, Default)]
pub(crate) struct FormulaRewrite {
    /// New value for the `url "..."` stanza.
    pub url: String,
    /// New source-archive digest for the standalone `sha256 "..."` stanza.
    /// Ignored when the formula uses the `tag:`/`revision:` form (git-based
    /// formulae carry no source sha256).
    pub sha256: Option<String>,
    /// New value for the explicit `version "..."` stanza, when the formula
    /// has one.
    pub version: String,
    /// New value for the `tag:` field of a git-based `url ... tag: ...,
    /// revision: ...` stanza.
    pub tag: Option<String>,
    /// New value for the `revision:` field paired with `tag:`.
    pub revision: Option<String>,
}

/// What [`rewrite_formula`] actually changed — consumed by the caller's
/// status line and by the already-current idempotency check.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RewriteSummary {
    pub url_rewritten: bool,
    pub sha256_rewritten: bool,
    pub version_rewritten: bool,
    pub tag_rewritten: bool,
    pub revision_rewritten: bool,
}

/// The sharded homebrew-core layout path for a formula:
/// `Formula/<first-char>/<name>.rb` (shard = first character, lowercased —
/// digit-named formulae shard under their digit, e.g. `Formula/7/7zip.rb`).
pub(crate) fn sharded_formula_path(name: &str) -> String {
    let shard = name
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase())
        .unwrap_or('_');
    format!("Formula/{}/{}.rb", shard, name)
}

/// The flat layout path (`Formula/<name>.rb`) most personal taps use.
pub(crate) fn flat_formula_path(name: &str) -> String {
    format!("Formula/{}.rb", name)
}

/// Replace the double-quoted string on `line`, returning the new line.
/// Returns `None` when the line does not carry a quoted string.
fn replace_quoted(line: &str, new_value: &str) -> Option<String> {
    let open = line.find('"')?;
    let close_rel = line[open + 1..].find('"')?;
    let mut out = String::with_capacity(line.len() + new_value.len());
    out.push_str(&line[..open + 1]);
    out.push_str(new_value);
    out.push_str(&line[open + 1 + close_rel..]);
    Some(out)
}

/// True when this is the standalone source-archive `sha256 "..."` stanza —
/// i.e. `sha256` followed directly by a quoted digest, NOT a bottle-block
/// line carrying a key (`sha256 cellar: ...`, `sha256 arm64_sonoma: "..."`).
fn is_source_sha256_line(line: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix("sha256") else {
        return false;
    };
    rest.trim_start().starts_with('"')
}

/// True when this line opens the `url "..."` stanza.
fn is_url_line(line: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix("url") else {
        return false;
    };
    rest.trim_start().starts_with('"')
}

/// True for the explicit `version "..."` stanza.
fn is_version_line(line: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix("version") else {
        return false;
    };
    rest.trim_start().starts_with('"')
}

/// Rewrite the value of a `key: "..."` field on a line, if present.
fn replace_keyed_quoted(line: &str, key: &str, new_value: &str) -> Option<String> {
    let key_pat = format!("{}:", key);
    let key_pos = line.find(&key_pat)?;
    let after = &line[key_pos + key_pat.len()..];
    let open_rel = after.find('"')?;
    let open = key_pos + key_pat.len() + open_rel;
    let close_rel = line[open + 1..].find('"')?;
    let mut out = String::with_capacity(line.len() + new_value.len());
    out.push_str(&line[..open + 1]);
    out.push_str(new_value);
    out.push_str(&line[open + 1 + close_rel..]);
    Some(out)
}

/// Rewrite the formula text for the new release.
///
/// * The first `url "..."` stanza is rewritten to `rw.url`.
/// * When the url stanza (or its continuation lines) carries `tag:` /
///   `revision:` fields, those are rewritten to `rw.tag` / `rw.revision`
///   (git-based formula form) and the sha256 rewrite is skipped.
/// * Otherwise the first standalone `sha256 "..."` stanza is rewritten to
///   `rw.sha256` — required for the archive form.
/// * The first `version "..."` stanza, when present, becomes `rw.version`.
///
/// Errors when the text has no `url` stanza (not a formula) or when the
/// archive form needs a sha256 the caller did not supply.
pub(crate) fn rewrite_formula(text: &str, rw: &FormulaRewrite) -> Result<(String, RewriteSummary)> {
    let mut summary = RewriteSummary::default();
    // Track the url stanza's continuation lines: a git-based url spreads
    // `tag:`/`revision:` over the lines following `url "...",`.
    let mut in_url_continuation = false;
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut line = line.to_string();
        if !summary.url_rewritten && is_url_line(&line) {
            match replace_quoted(&line, &rw.url) {
                Some(new_line) => {
                    line = new_line;
                    summary.url_rewritten = true;
                    // A trailing comma means the stanza continues (git form).
                    in_url_continuation = line.trim_end().ends_with(',');
                }
                None => bail!("formula url stanza has no quoted value: {line}"),
            }
        }
        // tag:/revision: may sit on the url line itself or a continuation.
        if summary.url_rewritten
            && (in_url_continuation || summary.tag_rewritten || is_url_line(&line))
        {
            if let Some(tag) = rw.tag.as_deref()
                && !summary.tag_rewritten
                && let Some(new_line) = replace_keyed_quoted(&line, "tag", tag)
            {
                line = new_line;
                summary.tag_rewritten = true;
            }
            if let Some(rev) = rw.revision.as_deref()
                && !summary.revision_rewritten
                && let Some(new_line) = replace_keyed_quoted(&line, "revision", rev)
            {
                line = new_line;
                summary.revision_rewritten = true;
            }
            if in_url_continuation && !line.trim_end().ends_with(',') {
                in_url_continuation = false;
            }
        }
        if !summary.sha256_rewritten && !summary.tag_rewritten && is_source_sha256_line(&line) {
            let Some(sha) = rw.sha256.as_deref() else {
                bail!(
                    "formula has an archive `sha256` stanza but no new digest was \
                     computed — supply `sha256:` or a downloadable `download_url`"
                );
            };
            match replace_quoted(&line, sha) {
                Some(new_line) => {
                    line = new_line;
                    summary.sha256_rewritten = true;
                }
                None => bail!("formula sha256 stanza has no quoted value: {line}"),
            }
        }
        if !summary.version_rewritten && is_version_line(&line) {
            if let Some(new_line) = replace_quoted(&line, &rw.version) {
                line = new_line;
                summary.version_rewritten = true;
            }
        }
        lines.push(line);
    }
    if !summary.url_rewritten {
        bail!("no `url \"...\"` stanza found — is this a Homebrew formula?");
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    Ok((out, summary))
}

/// True when the formula text already references this release — either the
/// exact new url, a `tag:` matching the new tag, or an explicit `version`
/// stanza carrying the new version. Drives the idempotent already-bumped
/// skip.
pub(crate) fn formula_is_current(text: &str, url: &str, tag: Option<&str>, version: &str) -> bool {
    if text.contains(&format!("\"{}\"", url)) {
        return true;
    }
    if let Some(tag) = tag
        && text.contains(&format!("tag: \"{}\"", tag))
    {
        return true;
    }
    text.lines()
        .any(|l| is_version_line(l) && l.contains(&format!("\"{}\"", version)))
}
