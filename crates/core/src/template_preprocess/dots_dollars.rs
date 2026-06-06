//! `$` prefix stripping (Go loop vars) and Pass 1 leading-dot stripping.

use super::GO_BLOCK_RE;
use super::go_blocks::{extract_block_parts, push_char_at};
use super::static_regex;
use regex::Regex;
use std::sync::LazyLock;

/// Strip `$` prefix from Go variable references inside `{{ }}` and `{% %}` blocks.
///
/// Scans each block character by character, skipping quoted strings, and removes
/// `$` when followed by a word character (e.g., `$var` → `var`).
pub(super) fn strip_dollar_vars(template: &str) -> String {
    // Match both {{ ... }} and {% ... %} blocks
    static BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| static_regex(r"\{\{.*?\}\}|\{%.*?%\}"));

    BLOCK_RE
        .replace_all(template, |caps: &regex::Captures| {
            let block = &caps[0];
            let bytes = block.as_bytes();
            let mut result = String::with_capacity(block.len());
            let mut i = 0;

            while i < bytes.len() {
                // Skip quoted strings entirely
                if bytes[i] == b'"' || bytes[i] == b'\'' {
                    let quote = bytes[i];
                    result.push(quote as char);
                    i += 1;
                    while i < bytes.len() && bytes[i] != quote {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            // Backslash is ASCII; the escaped char may be multibyte.
                            result.push('\\');
                            i += 1;
                            i += push_char_at(&mut result, block, i);
                        } else {
                            i += push_char_at(&mut result, block, i);
                        }
                    }
                    if i < bytes.len() {
                        result.push(quote as char);
                        i += 1;
                    }
                    continue;
                }

                // Strip `$` when followed by a word character (variable reference)
                if bytes[i] == b'$'
                    && i + 1 < bytes.len()
                    && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_')
                {
                    // Skip the `$`, keep the variable name
                    i += 1;
                    continue;
                }

                i += push_char_at(&mut result, block, i);
            }

            result
        })
        .to_string()
}

/// Pass 1: Strip Go-style leading dots from variable references.
pub(super) fn preprocess_strip_dots(template: &str) -> String {
    GO_BLOCK_RE
        .replace_all(template, |caps: &regex::Captures| {
            let block = &caps[0];
            let (open, inner, close) = extract_block_parts(block);

            let mut result = String::with_capacity(block.len());
            result.push_str(open);

            let bytes = inner.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                // Skip over quoted strings entirely
                if bytes[i] == b'"' || bytes[i] == b'\'' {
                    let quote = bytes[i];
                    result.push(quote as char);
                    i += 1;
                    while i < bytes.len() && bytes[i] != quote {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            // Backslash is ASCII; the escaped char may be multibyte.
                            result.push('\\');
                            i += 1;
                            i += push_char_at(&mut result, inner, i);
                        } else {
                            i += push_char_at(&mut result, inner, i);
                        }
                    }
                    if i < bytes.len() {
                        result.push(quote as char); // closing quote
                        i += 1;
                    }
                    continue;
                }

                if bytes[i] == b'.'
                    && i + 1 < bytes.len()
                    && (bytes[i + 1].is_ascii_alphanumeric() || bytes[i + 1] == b'_')
                {
                    // Check if the preceding character is a word char — if so,
                    // this is chained access (e.g., `Env.VAR`) and we keep the dot.
                    let prev_is_word =
                        i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                    if prev_is_word {
                        result.push('.');
                    }
                    // else: Go-style leading dot — skip it
                    i += 1;
                } else {
                    // `.` is ASCII; any non-`.` byte may begin a multibyte char.
                    i += push_char_at(&mut result, inner, i);
                }
            }

            result.push_str(close);
            result
        })
        .to_string()
}
