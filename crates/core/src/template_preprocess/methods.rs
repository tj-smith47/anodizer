//! Pass 4: rewrite Go-style method calls (`Now.Format "..."`) to Tera filter syntax.

use super::GO_BLOCK_RE;
use super::go_blocks::extract_block_parts;
use super::static_regex;
use super::string_lit::RAW_STRING_RE_ALT;
use regex::Regex;
use std::sync::LazyLock;

/// Regex matching `Now.Format` with a quoted format argument inside `{{ }}` blocks.
/// Captures: (1) the format string including quotes.
/// After Pass 1 (dot stripping), `{{ .Now.Format "2006-01-02" }}` becomes
/// `{{ Now.Format "2006-01-02" }}`. This regex rewrites it to
/// `{{ Now | now_format(format="2006-01-02") }}`.
static NOW_FORMAT_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(&format!(r"Now\.Format\s+({RAW_STRING_RE_ALT})")));

/// Pass 4: Rewrite Go-style method calls to Tera filter syntax.
///
/// Currently handles:
/// - `Now.Format "2006-01-02"` → `Now | now_format(format="2006-01-02")`
///
/// This runs after all other passes so that dot-stripping and positional
/// syntax rewrites have already been applied.
pub(super) fn preprocess_method_calls(template: &str) -> String {
    GO_BLOCK_RE
        .replace_all(template, |caps: &regex::Captures| {
            let block = &caps[0];
            if !block.contains("Now.Format") {
                return block.to_string();
            }
            let (open, inner, close) = extract_block_parts(block);
            let rewritten = NOW_FORMAT_RE
                .replace_all(inner, |mcaps: &regex::Captures| {
                    let fmt_arg = &mcaps[1];
                    format!("Now | now_format(format={})", fmt_arg)
                })
                .to_string();
            format!("{}{}{}", open, rewritten, close)
        })
        .to_string()
}
