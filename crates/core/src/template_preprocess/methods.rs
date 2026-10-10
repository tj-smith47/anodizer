//! Pass 4: rewrite Go-style method calls (`Now.Format "..."`) to Tera filter
//! syntax, and the context-taking call `targetVariant .` to `targetVariant()`.

use super::blocks::{live_blocks, replace_live_blocks};
use super::go_blocks::extract_block_parts;
use super::static_regex;
use super::string_lit::{RAW_STRING_RE_ALT, is_string_delim, raw_string_end};
use super::tokens::balanced_paren_end;
use anyhow::{Result, bail};
use regex::Regex;
use std::sync::LazyLock;

/// Regex matching `Now.Format` with a quoted format argument inside `{{ }}` blocks.
/// Captures: (1) the format string including quotes.
/// After Pass 1 (dot stripping), `{{ .Now.Format "2006-01-02" }}` becomes
/// `{{ Now.Format "2006-01-02" }}`. This regex rewrites it to
/// `{{ Now | now_format(format="2006-01-02") }}`.
static NOW_FORMAT_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(&format!(r"Now\.Format\s+({RAW_STRING_RE_ALT})")));

/// Regex matching `targetVariant .` and `targetVariant $` — the Go call
/// handing the function the whole template context. Captures: (1) whatever
/// ends the lone argument. Every other argument is refused by
/// [`check_target_variant_calls`] before this pass runs.
static TARGET_VARIANT_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"\btargetVariant\s+[.$](\s|\)|\||$)"));

/// Regex matching the piped spelling `. | targetVariant` / `$ | targetVariant`.
/// Captures: (1) whatever precedes the context argument.
static TARGET_VARIANT_PIPED_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(^|\s|\()[.$]\s*\|\s*targetVariant\b"));

const TARGET_VARIANT: &str = "targetVariant";

/// Refuse a `targetVariant` call that is handed anything but the template
/// context.
///
/// The function reads the per-target variables of the render it is called
/// in, so its one argument is the context itself: `{{ targetVariant . }}`,
/// `{{ targetVariant $ }}`, the piped `{{ . | targetVariant }}`, or the
/// engine-native `{{ targetVariant() }}`. A field (`.Env`, `.Abi`), a
/// literal, a named argument, no argument at all, and a `.` that an enclosing
/// `{{ with … }}` / `{{ range … }}` / `{{ block … }}` (or an `{{ else with … }}`
/// / `{{ else range … }}` arm) has rebound to its own value are each an
/// error naming what was passed and the spelling that works.
///
/// Runs against the source text, before any pass rewrites it, so the message
/// quotes what the author wrote.
pub fn check_target_variant_calls(template: &str) -> Result<()> {
    if !template.contains(TARGET_VARIANT) {
        return Ok(());
    }
    // One entry per open Go block: the block's own text when it rebinds `.`.
    let mut open_blocks: Vec<Option<&str>> = Vec::new();
    for block in live_blocks(template) {
        let (open, inner, _) = extract_block_parts(block);
        if open.starts_with("{{") {
            let keyword = inner.trim();
            if keyword == "end" {
                open_blocks.pop();
                continue;
            }
            let rebinds = |keyword: &str| {
                keyword.starts_with("with ")
                    || keyword.starts_with("range ")
                    || keyword.starts_with("block ")
            };
            if let Some(arm) = keyword.strip_prefix("else") {
                // The `else` arm of `with` / `range` sees the outer `.` again,
                // unless it opens a `with` / `range` of its own, which rebinds
                // `.` without opening a block of its own.
                let rebound = rebinds(arm.trim_start()).then_some(block);
                if let Some(top) = open_blocks.last_mut() {
                    *top = rebound;
                }
            } else if rebinds(keyword) {
                open_blocks.push(Some(block));
            } else if keyword.starts_with("if ") || keyword.starts_with("define ") {
                open_blocks.push(None);
            }
        }
        let rebound_by = open_blocks.iter().rev().find_map(|b| *b);
        check_target_variant_block(inner, rebound_by)?;
    }
    Ok(())
}

/// Check every `targetVariant` call inside one block's inner text.
fn check_target_variant_block(inner: &str, rebound_by: Option<&str>) -> Result<()> {
    let bytes = inner.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i < bytes.len() {
        if is_string_delim(bytes[i]) {
            i = raw_string_end(bytes, i);
            continue;
        }
        let at_call = inner[i..].starts_with(TARGET_VARIANT)
            && (i == 0 || !(is_word(bytes[i - 1]) || bytes[i - 1] == b'.'))
            && bytes
                .get(i + TARGET_VARIANT.len())
                .is_none_or(|b| !is_word(*b));
        if !at_call {
            i += 1;
            continue;
        }
        let after = i + TARGET_VARIANT.len();
        let got = target_variant_argument(inner, i, after);
        let dot_is_rebound = |arg: &str| arg == "." && rebound_by.is_some();
        match got {
            Argument::Context(arg) if !dot_is_rebound(arg) => {}
            Argument::Context(_) => bail!(
                "targetVariant: expected the template context, got the `.` that `{}` rebinds: \
                 use it as '{{{{ targetVariant $ }}}}'",
                rebound_by.unwrap_or_default()
            ),
            Argument::Other(what) => bail!(
                "targetVariant: expected the template context, got {what}: \
                 use it as '{{{{ targetVariant . }}}}'"
            ),
        }
        i = after;
    }
    Ok(())
}

/// What one `targetVariant` call is handed.
enum Argument<'a> {
    /// The template context: `.` or `$` (or nothing, for the engine-native
    /// `targetVariant()`).
    Context(&'a str),
    /// Anything else, worded for the diagnostic.
    Other(String),
}

/// Classify the argument of the `targetVariant` call whose name spans
/// `start..after` in `inner`.
fn target_variant_argument(inner: &str, start: usize, after: usize) -> Argument<'_> {
    let bytes = inner.as_bytes();
    let rest = &inner[after..];
    if rest.starts_with('(') {
        let end = balanced_paren_end(bytes, after).unwrap_or(bytes.len());
        let args = inner[after + 1..end.saturating_sub(1).max(after + 1)].trim();
        return if args.is_empty() {
            Argument::Context("")
        } else {
            Argument::Other(format!("`({args})`"))
        };
    }
    let arg_start = after + (rest.len() - rest.trim_start().len());
    let arg_end = match bytes.get(arg_start) {
        Some(b) if is_string_delim(*b) => raw_string_end(bytes, arg_start),
        Some(b'(') => balanced_paren_end(bytes, arg_start).unwrap_or(bytes.len()),
        _ => inner[arg_start..]
            .find(|c: char| c.is_whitespace() || c == ')' || c == '|')
            .map_or(bytes.len(), |n| arg_start + n),
    };
    let arg = &inner[arg_start..arg_end];
    if !arg.is_empty() {
        return match arg {
            "." | "$" => Argument::Context(arg),
            other => Argument::Other(format!("`{other}`")),
        };
    }
    // No argument follows: the context may still arrive through a pipe.
    let before = inner[..start].trim_end();
    let Some(piped) = before.strip_suffix('|') else {
        return Argument::Other("no argument".to_string());
    };
    let piped = piped.trim();
    match piped.rsplit(|c: char| c.is_whitespace() || c == '(').next() {
        Some(source @ ("." | "$")) => Argument::Context(source),
        _ => Argument::Other(format!("the piped value `{piped}`")),
    }
}

/// Pass 4: Rewrite Go-style method calls to Tera filter syntax.
///
/// Currently handles:
/// - `Now.Format "2006-01-02"` → `Now | now_format(format="2006-01-02")`
/// - `targetVariant .`, `targetVariant $` and `. | targetVariant` →
///   `targetVariant()`: the function reads the render's variables itself, so
///   the context argument has no Tera counterpart
///
/// This runs after all other passes so that dot-stripping and positional
/// syntax rewrites have already been applied.
pub(super) fn preprocess_method_calls(template: &str) -> String {
    replace_live_blocks(template, |block: &str| {
        if !block.contains("Now.Format") && !block.contains("targetVariant") {
            return block.to_string();
        }
        let (open, inner, close) = extract_block_parts(block);
        let inner = TARGET_VARIANT_PIPED_RE.replace_all(inner, "${1}targetVariant()");
        let inner = TARGET_VARIANT_RE.replace_all(&inner, "targetVariant()$1");
        let rewritten = NOW_FORMAT_RE
            .replace_all(&inner, |mcaps: &regex::Captures| {
                let fmt_arg = &mcaps[1];
                format!("Now | now_format(format={})", fmt_arg)
            })
            .to_string();
        format!("{}{}{}", open, rewritten, close)
    })
}
