//! Pass 0: convert Go template block syntax to Tera block syntax.
//!
//! Tracks a stack of block types (`if`, `for`, `with`) so each `{{ end }}`
//! emits the correct closing tag. Also exposes the shared block-extraction
//! helper (`extract_block_parts`) and the `if`/`elif` rewrite hook
//! (`try_rewrite_control_block`) used by the positional pass.

use super::dots_dollars::strip_dollar_vars;
use super::positional::{try_rewrite_piped, try_rewrite_standalone};
use super::static_regex;
use super::tokens::{Token, significant_tokens, token_to_str, tokenize_block};
use regex::Regex;
use std::sync::LazyLock;

/// Regexes for matching Go template block constructs.
///
/// These match `{{ if ... }}`, `{{ else }}`, `{{ else if ... }}`, `{{ end }}`,
/// `{{ range ... }}`, `{{ with ... }}`, and `{{ $var := ... }}` patterns.
/// Whitespace trimming markers (`-`) are preserved. `(?s)` lets the lazy
/// condition captures cross newlines — a block broken across lines is valid
/// Go template syntax and must still convert; laziness keeps each capture
/// from swallowing an adjacent block's `}}`.
static GO_IF_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?s)^\{\{(-?)\s*if\s+(.+?)\s*(-?)\}\}"));
static GO_ELSE_IF_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?s)^\{\{(-?)\s*else\s+if\s+(.+?)\s*(-?)\}\}"));
static GO_ELSE_RE: LazyLock<Regex> = LazyLock::new(|| static_regex(r"^\{\{(-?)\s*else\s*(-?)\}\}"));
static GO_END_RE: LazyLock<Regex> = LazyLock::new(|| static_regex(r"^\{\{(-?)\s*end\s*(-?)\}\}"));
static GO_RANGE_KV_RE: LazyLock<Regex> = LazyLock::new(|| {
    // {{ range $k, $v := .Map }}
    static_regex(r"(?s)^\{\{(-?)\s*range\s+\$(\w+)\s*,\s*\$(\w+)\s*:=\s*(.+?)\s*(-?)\}\}")
});
static GO_RANGE_V_RE: LazyLock<Regex> = LazyLock::new(|| {
    // {{ range $v := .Slice }} or {{ range .Slice }}
    static_regex(r"(?s)^\{\{(-?)\s*range\s+(?:\$(\w+)\s*:=\s*)?(.+?)\s*(-?)\}\}")
});
static GO_WITH_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?s)^\{\{(-?)\s*with\s+(.+?)\s*(-?)\}\}"));
static GO_VAR_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    // {{ $var := expr }}
    static_regex(r"(?s)^\{\{(-?)\s*\$(\w+)\s*:=\s*(.+?)\s*(-?)\}\}")
});
/// Match `{{ . }}` (bare dot reference to current context).
static GO_DOT_RE: LazyLock<Regex> = LazyLock::new(|| static_regex(r"^\{\{(-?)\s*\.\s*(-?)\}\}"));

/// Push the full UTF-8 char that starts at byte index `i` of `s`, returning its
/// byte length. Keeps multibyte chars intact instead of Latin-1-decoding each
/// byte (which would double-encode every non-ASCII codepoint into mojibake).
pub(super) fn push_char_at(out: &mut String, s: &str, i: usize) -> usize {
    let ch = s[i..].chars().next().expect("byte index at char boundary");
    out.push(ch);
    ch.len_utf8()
}

/// Format a Tera block tag with optional whitespace trim markers.
fn tera_block(ltrim: &str, content: &str, rtrim: &str) -> String {
    let l = if ltrim == "-" { "{%-" } else { "{%" };
    let r = if rtrim == "-" { "-%}" } else { "%}" };
    format!("{l} {content} {r}")
}

/// Convert Go template block syntax to Tera block syntax.
///
/// Tracks a stack of block types (`if`, `for`, `with`) to emit the correct
/// closing tag (`endif`, `endfor`, `endif`) for each `{{ end }}`.
pub(super) fn preprocess_go_blocks(template: &str) -> String {
    // Strategy: scan for Go block patterns and replace them.
    // We need a stack to track what `{{ end }}` should become.
    //
    // Process line-by-line isn't suitable since blocks can be inline.
    // Instead, scan left to right, replacing each Go block pattern.

    let mut result = String::with_capacity(template.len());
    // Stack tracks block type and context variable (for `with`/`range` dot-rewriting).
    // The context var is used to rewrite `{{ . }}` to `{{ var }}` inside the block.
    let mut block_stack: Vec<(&str, Option<String>)> = Vec::new();
    let mut pos = 0;
    let bytes = template.as_bytes();

    while pos < bytes.len() {
        // Look for `{{` at current position
        if pos + 1 < bytes.len() && bytes[pos] == b'{' && bytes[pos + 1] == b'{' {
            let remaining = &template[pos..];

            // Try each pattern in order of specificity

            // Bare dot reference: {{ . }} → {{ <context_var> }}
            // Inside `with` or `range` blocks, `{{ . }}` refers to the block's context variable.
            if let Some(cap) = GO_DOT_RE.captures(remaining) {
                let full = &cap[0];
                let ltrim = &cap[1];
                let rtrim = &cap[2];
                // Find the innermost context variable from the block stack
                let context_var = block_stack
                    .iter()
                    .rev()
                    .find_map(|(_, var)| var.as_deref())
                    .unwrap_or(".");
                let l = if ltrim == "-" { "{{-" } else { "{{" };
                let r = if rtrim == "-" { "-}}" } else { "}}" };
                result.push_str(&format!("{l} {context_var} {r}"));
                pos += full.len();
                continue;
            }

            // Variable assignment: {{ $var := expr }}
            // Must check before other patterns since $var could look like other things
            if let Some(cap) = GO_VAR_ASSIGN_RE.captures(remaining) {
                let full = &cap[0];
                // Make sure this isn't an `if`, `range`, `with`, or `else` block
                // (those are handled by their own patterns)
                let inner_trimmed = remaining[2..].trim_start_matches('-').trim_start();
                if !inner_trimmed.starts_with("if ")
                    && !inner_trimmed.starts_with("else")
                    && !inner_trimmed.starts_with("end")
                    && !inner_trimmed.starts_with("range ")
                    && !inner_trimmed.starts_with("with ")
                {
                    let ltrim = &cap[1];
                    let var = &cap[2];
                    let expr = &cap[3];
                    let rtrim = &cap[4];
                    result.push_str(&tera_block(ltrim, &format!("set {var} = {expr}"), rtrim));
                    pos += full.len();
                    continue;
                }
            }

            // else if: {{ else if ... }}
            if let Some(cap) = GO_ELSE_IF_RE.captures(remaining) {
                let full = &cap[0];
                result.push_str(&tera_block(&cap[1], &format!("elif {}", &cap[2]), &cap[3]));
                pos += full.len();
                continue;
            }

            // if: {{ if ... }}
            if let Some(cap) = GO_IF_RE.captures(remaining) {
                let full = &cap[0];
                result.push_str(&tera_block(&cap[1], &format!("if {}", &cap[2]), &cap[3]));
                block_stack.push(("if", None));
                pos += full.len();
                continue;
            }

            // else: {{ else }}
            if let Some(cap) = GO_ELSE_RE.captures(remaining) {
                let full = &cap[0];
                result.push_str(&tera_block(&cap[1], "else", &cap[2]));
                pos += full.len();
                continue;
            }

            // end: {{ end }}
            if let Some(cap) = GO_END_RE.captures(remaining) {
                let full = &cap[0];
                let end_tag = match block_stack.pop() {
                    Some(("for", _)) => "endfor",
                    _ => "endif", // if, with, or unknown
                };
                result.push_str(&tera_block(&cap[1], end_tag, &cap[2]));
                pos += full.len();
                continue;
            }

            // range with key-value: {{ range $k, $v := .Map }}
            if let Some(cap) = GO_RANGE_KV_RE.captures(remaining) {
                let full = &cap[0];
                let (key, val, collection) = (&cap[2], &cap[3], &cap[4]);
                result.push_str(&tera_block(
                    &cap[1],
                    &format!("for {key}, {val} in {collection}"),
                    &cap[5],
                ));
                block_stack.push(("for", Some(val.to_string())));
                pos += full.len();
                continue;
            }

            // range with value or bare: {{ range $v := .Slice }} or {{ range .Slice }}
            if let Some(cap) = GO_RANGE_V_RE.captures(remaining) {
                let full = &cap[0];
                let loop_var = cap.get(2).map(|m| m.as_str()).unwrap_or("val");
                let collection = &cap[3];
                result.push_str(&tera_block(
                    &cap[1],
                    &format!("for {loop_var} in {collection}"),
                    &cap[4],
                ));
                block_stack.push(("for", Some(loop_var.to_string())));
                pos += full.len();
                continue;
            }

            // with: {{ with .Field }}
            // Tera has no `with`. Convert to `{% if Field %}` and note on stack.
            // The field becomes the context variable for `{{ . }}` rewriting.
            if let Some(cap) = GO_WITH_RE.captures(remaining) {
                let full = &cap[0];
                let field = cap[2].to_string();
                result.push_str(&tera_block(&cap[1], &format!("if {field}"), &cap[3]));
                block_stack.push(("with", Some(field)));
                pos += full.len();
                continue;
            }
        }

        // No match at this position — copy one full UTF-8 char and advance.
        pos += push_char_at(&mut result, template, pos);
    }

    // Post-pass: strip `$` prefix from Go variable references inside template blocks.
    // Go templates use `$var` for loop/assignment variables; Tera uses plain `var`.
    // Must NOT strip `$` inside quoted strings (e.g., regex `$1` replacements).
    strip_dollar_vars(&result)
}

/// Extract the open delimiter, inner content, and close delimiter from a template block.
/// Handles Tera whitespace-control variants: `{{-`, `-}}`, `{%-`, `-%}`.
pub(super) fn extract_block_parts(block: &str) -> (&str, &str, &str) {
    let open_len = if block.starts_with("{{-") || block.starts_with("{%-") {
        3
    } else {
        2
    };
    let close_len = if block.ends_with("-}}") || block.ends_with("-%}") {
        3
    } else {
        2
    };
    let open = &block[..open_len];
    let close = &block[block.len() - close_len..];
    let inner = &block[open_len..block.len() - close_len];
    (open, inner, close)
}

/// Try to rewrite positional function calls inside `{% %}` control blocks.
///
/// Handles patterns like:
/// - `{% if contains Version "rc" %}` → `{% if contains(s=Version, substr="rc") %}`
/// - `{% if replace Tag "v" "" %}` → `{% if replace(s=Tag, old="v", new="") %}`
/// - ` if Version | replace "v" "" ` → ` if Version | replace(from="v", to="") `
///
/// The approach: identify the block keyword (`if`, `elif`, etc.),
/// then attempt positional rewriting on the expression that follows it.
pub(super) fn try_rewrite_control_block(inner: &str) -> Option<String> {
    let tokens = tokenize_block(inner);
    let sig = significant_tokens(&tokens);

    if sig.is_empty() {
        return None;
    }

    // Identify the control keyword and find where the expression starts.
    // Keywords: `if`, `elif`, `set ... =`, etc.
    // We care about `if` and `elif` (which contain expressions that might use
    // positional function syntax).
    let keyword = match sig.first() {
        Some(Token::Ident(k)) => k.as_str(),
        _ => return None,
    };

    // Only handle `if` and `elif` — these take expressions.
    // `for`, `endfor`, `endif`, `else`, `set`, etc. don't use positional funcs.
    if keyword != "if" && keyword != "elif" {
        return None;
    }

    // Find the index of the keyword token in the full (with-whitespace) token list.
    let keyword_end_idx = tokens
        .iter()
        .position(|t| matches!(t, Token::Ident(k) if k == keyword))
        .map(|i| i + 1)?;

    // The expression portion is everything after the keyword.
    let expr_tokens: Vec<Token> = tokens[keyword_end_idx..].to_vec();

    // Try standalone rewrite on the expression.
    if let Some(rewritten) = try_rewrite_standalone(&expr_tokens) {
        let prefix: String = tokens[..keyword_end_idx]
            .iter()
            .map(|t| token_to_str(t))
            .collect();
        return Some(format!("{}{}", prefix, rewritten));
    }

    // Try piped rewrite on the expression.
    if let Some(rewritten) = try_rewrite_piped(&expr_tokens) {
        let prefix: String = tokens[..keyword_end_idx]
            .iter()
            .map(|t| token_to_str(t))
            .collect();
        return Some(format!("{}{}", prefix, rewritten));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::preprocess_go_blocks;

    #[test]
    fn multiline_go_if_and_else_if_convert() {
        assert_eq!(
            preprocess_go_blocks(
                "{{ if eq .Os\n\"linux\" }}L{{ else if eq .Os\n\"darwin\" }}D{{ end }}"
            ),
            "{% if eq .Os\n\"linux\" %}L{% elif eq .Os\n\"darwin\" %}D{% endif %}"
        );
    }

    #[test]
    fn multiline_go_range_converts() {
        assert_eq!(
            preprocess_go_blocks("{{ range $v := index .M\n\"k\" }}{{ $v }}{{ end }}"),
            "{% for v in index .M\n\"k\" %}{{ v }}{% endfor %}"
        );
    }

    #[test]
    fn multiline_go_with_converts() {
        assert_eq!(
            preprocess_go_blocks("{{ with index .M\n\"k\" }}{{ . }}{{ end }}"),
            "{% if index .M\n\"k\" %}{{ index .M\n\"k\" }}{% endif %}"
        );
    }

    #[test]
    fn multiline_go_var_assign_converts() {
        assert_eq!(
            preprocess_go_blocks("{{ $v := printf \"%s\"\n.Name }}"),
            "{% set v = printf \"%s\"\n.Name %}"
        );
    }

    #[test]
    fn adjacent_multiline_blocks_do_not_merge() {
        // The lazy condition captures must stop at each block's OWN close —
        // `(?s)` must not let the first condition swallow across `}}` into
        // the second block.
        assert_eq!(
            preprocess_go_blocks("{{ if eq .A\n1 }}x{{ end }}\n{{ if eq .B\n2 }}y{{ end }}"),
            "{% if eq .A\n1 %}x{% endif %}\n{% if eq .B\n2 %}y{% endif %}"
        );
    }
}
