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
    /// New value for the `url "..."` stanza. `None` leaves the url stanza
    /// untouched — the git-form bump where the caller did not explicitly set
    /// `download_url` (a git formula's url is a `.git` clone URL that a
    /// tarball url would corrupt; only `tag:`/`revision:` move).
    pub url: Option<String>,
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

/// Extract the value of a `key: "..."` field on a line, if present.
fn keyed_quoted_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let key_pat = format!("{}:", key);
    let key_pos = line.find(&key_pat)?;
    let after = &line[key_pos + key_pat.len()..];
    let open_rel = after.find('"')?;
    let rest = &after[open_rel + 1..];
    let close_rel = rest.find('"')?;
    Some(&rest[..close_rel])
}

/// Return `line` with any trailing Ruby line-comment removed. A `#` only
/// starts a comment when it sits outside a quoted string, so `#`
/// interpolation (`url "…/v#{version}.tar.gz"`) and a literal `#` inside a
/// quoted value are preserved.
fn strip_trailing_comment(line: &str) -> &str {
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_double && !in_single => return &line[..i],
            _ => {}
        }
    }
    line
}

/// True when `line` continues the `url` stanza — its code portion (ignoring a
/// trailing comment) ends with a comma. `url "…git", # upstream` continues.
fn stanza_line_continues(line: &str) -> bool {
    strip_trailing_comment(line).trim_end().ends_with(',')
}

/// Structurally detect the git-based formula form: does the FIRST `url`
/// stanza (its opening line plus any comma-continued lines) itself carry a
/// `tag:` or `revision:` field? A git formula reads
/// `url "…git", tag: "…", revision: "…"`; the archive form is a bare
/// `url "…tar.gz"`. Only the url stanza is inspected, so a `tag:` sitting in
/// a `resource` block or a comment elsewhere in the file cannot flip the
/// verdict (the substring `content.contains("tag:")` false-positive this
/// replaces).
pub(crate) fn detect_git_form(text: &str) -> bool {
    let mut in_url = false;
    for line in text.lines() {
        if !in_url {
            if is_url_line(line) {
                in_url = true;
            } else {
                continue;
            }
        }
        // Probe the code portion so a `#`-commented `tag:` on the url line
        // cannot flip the verdict, and rewrite/detect agree on where the
        // stanza ends.
        let code = strip_trailing_comment(line);
        if keyed_quoted_value(code, "tag").is_some()
            || keyed_quoted_value(code, "revision").is_some()
        {
            return true;
        }
        // The stanza ends at the first line that does not continue with a
        // trailing comma; the archive form's single `url "…"` line stops here.
        if !code.trim_end().ends_with(',') {
            return false;
        }
    }
    false
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
/// The formula form is detected STRUCTURALLY (see [`detect_git_form`]):
///
/// * **Git form** (the first `url` stanza carries `tag:`/`revision:`): the
///   `tag:` and `revision:` fields — confined to the url stanza's own line
///   and its comma-continued lines — are rewritten to `rw.tag` / `rw.revision`
///   and the source `sha256` is left alone (git formulae carry no source
///   digest). The `url` stanza's quoted value is rewritten ONLY when `rw.url`
///   is `Some` (the caller passes it only when the user explicitly set
///   `download_url`); otherwise the `.git` clone URL is preserved verbatim.
/// * **Archive form**: the first `url "..."` and the first standalone
///   `sha256 "..."` stanzas are rewritten (`rw.url` is `Some` here); the
///   sha256 is required and its absence is a hard error.
/// * The first explicit `version "..."` stanza, when present, becomes
///   `rw.version` in either form.
///
/// Errors when the text has no `url` stanza (not a formula) or when the
/// archive form needs a sha256 the caller did not supply.
pub(crate) fn rewrite_formula(text: &str, rw: &FormulaRewrite) -> Result<(String, RewriteSummary)> {
    let git_form = detect_git_form(text);
    let mut summary = RewriteSummary::default();
    // `url_seen` latches once the first url stanza is entered (for the
    // not-a-formula error); `in_url_stanza` is live only WHILE inside that
    // first stanza — it gates tag/revision rewriting so a later unrelated
    // `revision:` (e.g. a git `resource` block) can never be clobbered.
    let mut url_seen = false;
    let mut url_stanza_done = false;
    let mut in_url_stanza = false;
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let mut line = line.to_string();
        if !url_stanza_done && !in_url_stanza && is_url_line(&line) {
            url_seen = true;
            in_url_stanza = true;
            // Rewrite the url's quoted value only when a new url is supplied;
            // a git-form bump without an explicit download_url leaves it.
            if let Some(new_url) = rw.url.as_deref() {
                match replace_quoted(&line, new_url) {
                    Some(new_line) => {
                        line = new_line;
                        summary.url_rewritten = true;
                    }
                    None => bail!("formula url stanza has no quoted value: {line}"),
                }
            }
        }
        // tag:/revision: are rewritten ONLY within the first url stanza.
        if in_url_stanza {
            // Guard on the code portion so a `#`-commented `tag:`/`revision:`
            // on the url line is never rewritten (and never latches the
            // summary flag away from the real field).
            if let Some(tag) = rw.tag.as_deref()
                && !summary.tag_rewritten
                && keyed_quoted_value(strip_trailing_comment(&line), "tag").is_some()
                && let Some(new_line) = replace_keyed_quoted(&line, "tag", tag)
            {
                line = new_line;
                summary.tag_rewritten = true;
            }
            if let Some(rev) = rw.revision.as_deref()
                && !summary.revision_rewritten
                && keyed_quoted_value(strip_trailing_comment(&line), "revision").is_some()
                && let Some(new_line) = replace_keyed_quoted(&line, "revision", rev)
            {
                line = new_line;
                summary.revision_rewritten = true;
            }
            // The stanza ends at the first line whose code portion (ignoring a
            // trailing comment) does not end with a comma.
            if !stanza_line_continues(&line) {
                in_url_stanza = false;
                url_stanza_done = true;
            }
        }
        // The source sha256 is rewritten only for the archive form.
        if !git_form && !summary.sha256_rewritten && is_source_sha256_line(&line) {
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
    if !url_seen {
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
    // Whitespace-tolerant: aligned git formulae write `tag:      "vX"` with
    // several spaces, which a fixed `tag: "vX"` substring would miss.
    if let Some(tag) = tag
        && text
            .lines()
            .any(|l| keyed_quoted_value(l, "tag") == Some(tag))
    {
        return true;
    }
    text.lines()
        .any(|l| is_version_line(l) && l.contains(&format!("\"{}\"", version)))
}
