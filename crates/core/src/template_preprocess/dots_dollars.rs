//! `$` prefix stripping (Go loop vars), Pass 1 leading-dot stripping, and
//! Pass 5 numeric-index rewriting (`list.0` → `list[0]`).

use super::GO_BLOCK_RE;
use super::go_blocks::{extract_block_parts, push_char_at};
use super::string_lit::{copy_raw_string, is_string_delim};

/// Strip `$` prefix from Go variable references inside `{{ }}` and `{% %}` blocks.
///
/// Scans each block character by character, skipping quoted strings, and removes
/// `$` when followed by a word character (e.g., `$var` → `var`).
pub(super) fn strip_dollar_vars(template: &str) -> String {
    GO_BLOCK_RE
        .replace_all(template, |caps: &regex::Captures| {
            let block = &caps[0];
            let bytes = block.as_bytes();
            let mut result = String::with_capacity(block.len());
            let mut i = 0;

            while i < bytes.len() {
                // Skip quoted strings entirely
                if is_string_delim(bytes[i]) {
                    i = copy_raw_string(&mut result, block, i);
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
                if is_string_delim(bytes[i]) {
                    i = copy_raw_string(&mut result, inner, i);
                    continue;
                }

                // Only Go-style leading dots are handled here. Chained
                // numeric segments (`list.0`) are deliberately left intact:
                // `rewrite_numeric_index_segments` converts them to `[0]` as
                // the final pass, after the token-based passes have run —
                // rewriting here would split `list.0` into two tokens and
                // corrupt positional-argument detection.
                if bytes[i] == b'.'
                    && i + 1 < bytes.len()
                    && (bytes[i + 1].is_ascii_alphanumeric() || bytes[i + 1] == b'_')
                {
                    // Check if the preceding character is a word char — if so,
                    // this is chained access (e.g., `Env.VAR`) and we keep the dot.
                    // A preceding `?` is tera 2.0's optional-chaining operator
                    // (`?.`, lexed as one token): stripping this dot would
                    // silently corrupt `Some?.Missing` into `Some?Missing`,
                    // a parse error. A preceding `]` closes an index
                    // expression (`arr[0]` or tera 2.0's optional-index
                    // `arr?[0]`, its `?.` sibling token): the dot after it is
                    // chained field access too (`arr[0].Field`), never a
                    // Go-style leading dot to strip.
                    let prev_is_word = i > 0
                        && (bytes[i - 1].is_ascii_alphanumeric()
                            || bytes[i - 1] == b'_'
                            || bytes[i - 1] == b'?'
                            || bytes[i - 1] == b']');
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

/// True when `out` ends in something a `.N` numeric segment can index: an
/// identifier containing at least one non-digit, a closed index or call
/// (`]` / `)`), or an optional-chain `?` hanging off one of those. A
/// digits-only run is a number literal — `1.0` is a float, not an index
/// into `1` — so it is not a path head.
fn ends_with_path_head(out: &str) -> bool {
    let bytes = out.as_bytes();
    let mut k = bytes.len();
    // `a?.0` — hop the optional-chain `?` so `a` is inspected as the head.
    if k > 0 && bytes[k - 1] == b'?' {
        k -= 1;
    }
    if k == 0 {
        return false;
    }
    match bytes[k - 1] {
        b']' | b')' => true,
        c if c.is_ascii_alphanumeric() || c == b'_' => {
            let end = k;
            while k > 0 && (bytes[k - 1].is_ascii_alphanumeric() || bytes[k - 1] == b'_') {
                k -= 1;
            }
            bytes[k..end].iter().any(|c| !c.is_ascii_digit())
        }
        _ => false,
    }
}

/// Final pass: rewrite tera 1.x numeric path segments to tera 2.0 index
/// syntax — `list.0` → `list[0]`, `a.0.b` → `a[0].b`, `a.0.1` → `a[0][1]`,
/// and the optional-chaining form `a?.0` → `a?[0]` (tera 2.0's `?[` token).
///
/// tera 1.x accepted `.N` as array indexing; 2.0 removed it in favor of
/// `[N]`. User templates written against the 1.x-era anodizer DSL must keep
/// rendering, so the segment is rewritten wherever it appears in path
/// position: the digits must follow a path head (see [`ends_with_path_head`])
/// and end at a path boundary. Number literals (`1.0`, `{{ 1.5 | round }}`)
/// and string-literal contents pass through untouched.
///
/// Runs after every token-based pass so those passes still lex `list.0` as a
/// single dotted-path token (matching the 1.x pipeline they were written
/// against).
pub(super) fn rewrite_numeric_index_segments(template: &str) -> String {
    GO_BLOCK_RE
        .replace_all(template, |caps: &regex::Captures| {
            let block = &caps[0];
            let (open, inner, close) = extract_block_parts(block);

            let mut result = String::with_capacity(block.len());
            result.push_str(open);

            let bytes = inner.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                // Never rewrite inside string literals
                if is_string_delim(bytes[i]) {
                    i = copy_raw_string(&mut result, inner, i);
                    continue;
                }

                if bytes[i] == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                    let mut j = i + 1;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    // `.0x` is an (invalid) identifier-ish segment, not an
                    // index — leave it for tera's parser to report.
                    let ends_at_boundary =
                        j >= bytes.len() || (!bytes[j].is_ascii_alphabetic() && bytes[j] != b'_');
                    // Path context comes from `result`, not the raw input, so
                    // a chain like `a.0.1` sees the already-rewritten `a[0]`
                    // (trailing `]`) when deciding about `.1`.
                    if ends_at_boundary && ends_with_path_head(&result) {
                        result.push('[');
                        result.push_str(&inner[i + 1..j]);
                        result.push(']');
                        i = j;
                        continue;
                    }
                }

                i += push_char_at(&mut result, inner, i);
            }

            result.push_str(close);
            result
        })
        .to_string()
}
