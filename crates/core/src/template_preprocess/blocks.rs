//! Block iteration for every preprocessing pass, raw-aware by construction.
//!
//! Text between `{% raw %}` and `{% endraw %}` is emitted literally by the
//! engine, so no pass may rewrite it. Raw-span detection therefore lives here
//! once, and the block regex is private to this module: the only ways to reach
//! a template's blocks are [`replace_live_blocks`] and [`live_blocks`], and
//! both skip raw spans. A pass added later inherits the exemption from the API
//! it is obliged to use, instead of from its author remembering to re-derive
//! it — a per-pass check is what let raw content be rewritten in the first
//! place.
//!
//! Rewrites that scan characters rather than blocks — Pass 0's Go-block
//! converter and the engine adapter's backslash shim — cannot go through those
//! two entry points, so they consume [`raw_spans`] / [`raw_span_at`] directly.

use super::go_blocks::extract_block_parts;
use super::static_regex;
use regex::Regex;
use std::ops::Range;
use std::sync::LazyLock;

/// Matches one `{{ … }}` or `{% … %}` block. `(?s)` lets `.` cross newlines: a
/// multiline expression block (`{{\n x }}`) is valid tera and must receive
/// every pass, not skip preprocessing.
///
/// Private on purpose — see the module docs.
static GO_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| static_regex(r"(?s)\{\{.*?\}\}|\{%.*?%\}"));

/// Byte ranges the template marks as raw, each covering its `{% raw %}` and
/// `{% endraw %}` delimiters as well as the literal text between them.
///
/// Nested `{% raw %}` has no meaning in tera — the inner tag is literal text —
/// so an already-open span swallows it. An unterminated `{% raw %}` covers the
/// rest of the template: the engine rejects it, and skipping the tail keeps
/// the passes from rewriting text the author marked literal before that error
/// is reported.
pub(crate) fn raw_spans(template: &str) -> Vec<Range<usize>> {
    // Every raw span contains the substring in both its delimiters, so its
    // absence rules one out without running the block regex.
    if !template.contains("raw") {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut open: Option<usize> = None;
    for m in GO_BLOCK_RE.find_iter(template) {
        // Only a `{% … %}` tag delimits a span. Tera's lexer scans for the next
        // `{%` and never inspects a `{{ … }}`, so a literal `{{ endraw }}`
        // between the delimiters is span content — treating it as a terminator
        // would end the exemption early and rewrite the rest of the span.
        if !m.as_str().starts_with("{%") {
            continue;
        }
        let (_, inner, _) = extract_block_parts(m.as_str());
        match inner.trim() {
            "raw" if open.is_none() => open = Some(m.start()),
            "endraw" => {
                if let Some(start) = open.take() {
                    spans.push(start..m.end());
                }
            }
            _ => {}
        }
    }
    if let Some(start) = open {
        spans.push(start..template.len());
    }
    spans
}

/// The raw span covering byte offset `at`, if any.
pub(crate) fn raw_span_at(spans: &[Range<usize>], at: usize) -> Option<&Range<usize>> {
    spans.iter().find(|span| span.contains(&at))
}

/// Rewrite every live (non-raw) block through `rewrite`, leaving raw spans and
/// the text between blocks byte-identical.
pub(super) fn replace_live_blocks(
    template: &str,
    mut rewrite: impl FnMut(&str) -> String,
) -> String {
    let spans = raw_spans(template);
    let mut out = String::with_capacity(template.len());
    let mut copied = 0;
    for m in GO_BLOCK_RE.find_iter(template) {
        if raw_span_at(&spans, m.start()).is_some() {
            continue;
        }
        out.push_str(&template[copied..m.start()]);
        out.push_str(&rewrite(m.as_str()));
        copied = m.end();
    }
    out.push_str(&template[copied..]);
    out
}

/// Every live (non-raw) block, for the inspection-only checks that validate a
/// template rather than rewriting it.
pub(super) fn live_blocks(template: &str) -> impl Iterator<Item = &str> {
    let spans = raw_spans(template);
    GO_BLOCK_RE
        .find_iter(template)
        .filter(move |m| raw_span_at(&spans, m.start()).is_none())
        .map(|m| m.as_str())
}
