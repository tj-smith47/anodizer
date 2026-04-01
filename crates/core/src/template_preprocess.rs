// Template preprocessing: converts Go-style syntax to Tera-native syntax.
//
// Pass 1 (`preprocess_strip_dots`): strips leading dots from `{{ .Field }}` → `{{ Field }}`.
// Pass 2 (`preprocess_positional_syntax`): converts positional function calls to named-arg syntax:
//   `{{ replace Version "v" "" }}` → `{{ replace(s=Version, old="v", new="") }}`
//   `{{ Version | replace "v" "" }}` → `{{ Version | replace(from="v", to="") }}`

use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

/// Regex to match `{{ ... }}` and `{% ... %}` blocks for Go-style preprocessing.
// SAFETY: This is a compile-time regex literal; it is known to be valid.
static GO_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{.*?\}\}|\{%.*?%\}").unwrap());

/// Preprocess a template: convert Go-style syntax to Tera-native syntax.
///
/// Pass 1: strip Go-style leading dots (`{{ .Field }}` → `{{ Field }}`).
/// Pass 2: rewrite Go-style `(list ...)` subexpressions to Tera array literals.
/// Pass 3: convert positional function syntax to named-arg syntax.
pub fn preprocess(template: &str) -> String {
    // Pass 1: strip Go-style leading dots.
    let dot_stripped = preprocess_strip_dots(template);
    // Pass 2: rewrite `(list "a" "b")` → `["a", "b"]`.
    let list_rewritten = preprocess_list_subexpr(&dot_stripped);
    // Pass 3: convert positional function syntax to named-arg syntax.
    preprocess_positional_syntax(&list_rewritten)
}

/// Pass 1: Strip Go-style leading dots from variable references.
fn preprocess_strip_dots(template: &str) -> String {
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
                            result.push(bytes[i] as char);
                            result.push(bytes[i + 1] as char);
                            i += 2;
                        } else {
                            result.push(bytes[i] as char);
                            i += 1;
                        }
                    }
                    if i < bytes.len() {
                        result.push(bytes[i] as char); // closing quote
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
                } else {
                    result.push(bytes[i] as char);
                }
                i += 1;
            }

            result.push_str(close);
            result
        })
        .to_string()
}

/// Regex matching `(list "a" "b" ...)` subexpressions inside template blocks.
/// Captures the inner quoted strings (variadic args to `list`).
/// Each item independently matches either double- or single-quoted strings,
/// supporting mixed quote styles and escaped quotes within strings.
// SAFETY: Built from deterministic string literals; the resulting pattern is known to be valid.
static LIST_SUBEXPR_RE: LazyLock<Regex> = LazyLock::new(|| {
    // A single quoted item: double-quoted with escaped-quote support, OR single-quoted with same.
    let item = r#"(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')"#;
    let pattern = format!(r"\(list\s+({item}(?:\s+{item})*)\)");
    Regex::new(&pattern).unwrap()
});

/// Pass 2: Rewrite Go-style `(list "a" "b" "c")` subexpressions to Tera array literals.
///
/// `(list "a" "b" "c")` → `["a", "b", "c"]`
///
/// This runs before positional syntax rewriting so that the `in` function can
/// receive a Tera array literal as its first argument.
fn preprocess_list_subexpr(template: &str) -> String {
    GO_BLOCK_RE
        .replace_all(template, |caps: &regex::Captures| {
            let block = &caps[0];
            // Only process blocks that contain `(list ` — fast path for the common case.
            if !block.contains("(list ") {
                return block.to_string();
            }
            LIST_SUBEXPR_RE
                .replace_all(block, |lcaps: &regex::Captures| {
                    let inner = &lcaps[1];
                    // Split quoted strings and rejoin as a Tera array literal.
                    // Handles escaped quotes inside strings (e.g., "hello \"world\"").
                    static QUOTED_RE: LazyLock<Regex> =
                        LazyLock::new(|| Regex::new(r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'"#).unwrap());
                    let items: Vec<&str> = QUOTED_RE.find_iter(inner).map(|m| m.as_str()).collect();
                    format!("[{}]", items.join(", "))
                })
                .to_string()
        })
        .to_string()
}

/// Pass 3: Convert Go-style positional function calls to Tera named-arg syntax.
///
/// Handles two forms for `replace`, `split`, and `contains`:
///
/// **Standalone (function) form:**
/// - `{{ replace Version "v" "" }}` → `{{ replace(s=Version, old="v", new="") }}`
/// - `{{ split Version "." }}` → `{{ split(s=Version, sep=".") }}`
/// - `{{ contains Version "rc" }}` → `{{ contains(s=Version, substr="rc") }}`
///
/// **Piped (filter) form:**
/// - `{{ Version | replace "v" "" }}` → `{{ Version | replace(from="v", to="") }}`
/// - `{{ Version | split "." }}` → `{{ Version | split(sep=".") }}`
/// - `{{ Version | contains "rc" }}` → `{{ Version | contains(substr="rc") }}`
///
/// Already-named-arg syntax (contains `(`) is passed through unchanged.
fn preprocess_positional_syntax(template: &str) -> String {
    GO_BLOCK_RE
        .replace_all(template, |caps: &regex::Captures| {
            let block = &caps[0];

            // Extract the open/close delimiters and inner content, accounting
            // for Tera's whitespace-control variants (`{{-`, `-}}`, `{%-`, `-%}`).
            let (open, inner, close) = extract_block_parts(block);

            if block.starts_with("{%") {
                // For control blocks like `{% if contains Version "rc" %}`,
                // we need to rewrite the expression portion after the keyword.
                if let Some(rewritten) = try_rewrite_control_block(inner) {
                    return format!("{}{}{}", open, rewritten, close);
                }
                return block.to_string();
            }

            // Tokenize the inner content of `{{ }}` blocks.
            let tokens = tokenize_block(inner);
            if tokens.is_empty() {
                return block.to_string();
            }

            // Try standalone form: `funcname arg1 arg2 [arg3]`
            if let Some(rewritten) = try_rewrite_standalone(&tokens) {
                return format!("{}{}{}", open, rewritten, close);
            }

            // Try piped form: `expr | funcname arg1 [arg2]`
            if let Some(rewritten) = try_rewrite_piped(&tokens) {
                return format!("{}{}{}", open, rewritten, close);
            }

            // No positional syntax detected; return unchanged.
            block.to_string()
        })
        .to_string()
}

/// Extract the open delimiter, inner content, and close delimiter from a template block.
/// Handles Tera whitespace-control variants: `{{-`, `-}}`, `{%-`, `-%}`.
fn extract_block_parts(block: &str) -> (&str, &str, &str) {
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
fn try_rewrite_control_block(inner: &str) -> Option<String> {
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

/// A token from inside a `{{ }}` block.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// A bare identifier or dotted path (e.g., `Version`, `Env.VAR`).
    Ident(String),
    /// A quoted string literal including its quotes (e.g., `"v"`).
    Quoted(String),
    /// A Tera array literal including brackets (e.g., `["a", "b", "c"]`).
    ArrayLiteral(String),
    /// The pipe operator `|`.
    Pipe,
    /// Whitespace (preserved for reconstruction).
    Space(String),
    /// Anything else (parentheses, operators, etc.).
    Other(String),
}

/// Tokenize the inner content of a `{{ }}` block.
/// Splits into identifiers, quoted strings, pipes, spaces, and other chars.
fn tokenize_block(inner: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let bytes = inner.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Whitespace
        if bytes[i].is_ascii_whitespace() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            tokens.push(Token::Space(inner[start..i].to_string()));
            continue;
        }

        // Quoted string
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            if i < bytes.len() {
                i += 1; // closing quote
            }
            tokens.push(Token::Quoted(inner[start..i].to_string()));
            continue;
        }

        // Array literal: `[...]` — capture the entire bracketed expression as one token.
        // This handles Tera array syntax like `["a", "b", "c"]`.
        if bytes[i] == b'[' {
            let start = i;
            let mut depth = 1;
            i += 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'[' {
                    depth += 1;
                } else if bytes[i] == b']' {
                    depth -= 1;
                } else if bytes[i] == b'"' || bytes[i] == b'\'' {
                    // Skip quoted strings inside the array
                    let quote = bytes[i];
                    i += 1;
                    while i < bytes.len() && bytes[i] != quote {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            i += 2;
                        } else {
                            i += 1;
                        }
                    }
                    if i < bytes.len() {
                        i += 1; // closing quote
                    }
                    continue;
                }
                i += 1;
            }
            tokens.push(Token::ArrayLiteral(inner[start..i].to_string()));
            continue;
        }

        // Pipe
        if bytes[i] == b'|' {
            tokens.push(Token::Pipe);
            i += 1;
            continue;
        }

        // Identifier or dotted path (e.g., `Env.VAR`, `Version`)
        if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
            {
                i += 1;
            }
            tokens.push(Token::Ident(inner[start..i].to_string()));
            continue;
        }

        // Everything else (parentheses, operators, etc.)
        // Use chars().next() to handle multi-byte UTF-8 characters correctly.
        let ch = inner[i..].chars().next().unwrap();
        tokens.push(Token::Other(ch.to_string()));
        i += ch.len_utf8();
    }

    tokens
}

/// Collect non-whitespace tokens from a slice.
fn significant_tokens(tokens: &[Token]) -> Vec<&Token> {
    tokens
        .iter()
        .filter(|t| !matches!(t, Token::Space(_)))
        .collect()
}

/// Positional syntax signature for a function/filter.
struct PositionalSyntax {
    /// Function name (e.g. "replace").
    name: &'static str,
    /// Number of positional args (excluding the function name).
    arity: usize,
    /// Parameter names for standalone form (e.g. `replace(s=..., old=..., new=...)`).
    standalone_params: &'static [&'static str],
    /// Parameter names for piped form (e.g. `| replace(from=..., to=...)`).
    /// First standalone param is implicit (comes from the pipe), so piped has one fewer.
    piped_params: &'static [&'static str],
}

/// Data-driven table of known positional syntax rewrites.
static POSITIONAL_FUNCTIONS: &[PositionalSyntax] = &[
    PositionalSyntax {
        name: "replace",
        arity: 3,
        standalone_params: &["s", "old", "new"],
        piped_params: &["from", "to"],
    },
    PositionalSyntax {
        name: "split",
        arity: 2,
        standalone_params: &["s", "sep"],
        piped_params: &["sep"],
    },
    PositionalSyntax {
        name: "contains",
        arity: 2,
        standalone_params: &["s", "substr"],
        piped_params: &["substr"],
    },
    PositionalSyntax {
        name: "in",
        arity: 2,
        standalone_params: &["items", "value"],
        piped_params: &["value"],
    },
    PositionalSyntax {
        name: "reReplaceAll",
        arity: 3,
        standalone_params: &["pattern", "input", "replacement"],
        piped_params: &["pattern", "replacement"],
    },
];

/// Look up a function name in the positional syntax table.
fn lookup_positional(name: &str) -> Option<&'static PositionalSyntax> {
    POSITIONAL_FUNCTIONS.iter().find(|p| p.name == name)
}

/// Try to rewrite standalone positional form:
/// `replace <arg> <quoted> <quoted>` → `replace(s=<arg>, old=<quoted>, new=<quoted>)`
/// `split <arg> <quoted>` → `split(s=<arg>, sep=<quoted>)`
/// `contains <arg> <quoted>` → `contains(s=<arg>, substr=<quoted>)`
///
/// Returns `None` if the pattern doesn't match.
fn try_rewrite_standalone(tokens: &[Token]) -> Option<String> {
    let sig = significant_tokens(tokens);

    // If there are parentheses anywhere, this is already named-arg syntax.
    if sig.iter().any(|t| matches!(t, Token::Other(s) if s == "(")) {
        return None;
    }

    // If there's a pipe, this isn't standalone form.
    if sig.iter().any(|t| matches!(t, Token::Pipe)) {
        return None;
    }

    let func_name = match sig.first() {
        Some(Token::Ident(name)) => name.as_str(),
        _ => return None,
    };

    let spec = lookup_positional(func_name)?;

    // sig should be: [funcname, arg1, arg2, ...] with `arity` args.
    if sig.len() != spec.arity + 1 {
        return None;
    }

    // Collect formatted arg values.
    let args: Vec<String> = sig[1..]
        .iter()
        .map(|t| format_arg_value(t))
        .collect::<Option<Vec<_>>>()?;

    // Build the named-arg call string.
    let params_str: String = spec
        .standalone_params
        .iter()
        .zip(args.iter())
        .map(|(name, val)| format!("{}={}", name, val))
        .collect::<Vec<_>>()
        .join(", ");

    // Preserve leading/trailing whitespace from the original block.
    let leading_ws = tokens
        .first()
        .and_then(|t| match t {
            Token::Space(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("");
    let trailing_ws = tokens
        .last()
        .and_then(|t| match t {
            Token::Space(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("");

    Some(format!(
        "{}{}({}){}",
        leading_ws, func_name, params_str, trailing_ws
    ))
}

/// Try to rewrite piped positional form:
/// `<expr> | replace <quoted> <quoted>` → `<expr> | replace(from=<quoted>, to=<quoted>)`
/// `<expr> | split <quoted>` → `<expr> | split(sep=<quoted>)`
/// `<expr> | contains <quoted>` → `<expr> | contains(substr=<quoted>)`
///
/// Returns `None` if the pattern doesn't match.
fn try_rewrite_piped(tokens: &[Token]) -> Option<String> {
    // Find the LAST pipe in the token stream. This handles chained filters like
    // `Version | trimprefix(prefix="v") | replace "." "-"` — we only rewrite
    // the final segment after the last pipe.
    let last_pipe_idx = tokens
        .iter()
        .rposition(|t| matches!(t, Token::Pipe))?;

    // Everything before the pipe (the expression being piped).
    let before_pipe = &tokens[..last_pipe_idx];
    // Everything after the pipe.
    let after_pipe = &tokens[last_pipe_idx + 1..];

    // If there are parentheses in the after-pipe tokens, the last filter is
    // already using named-arg syntax — nothing to rewrite.
    if after_pipe
        .iter()
        .any(|t| matches!(t, Token::Other(s) if s == "("))
    {
        return None;
    }

    let sig_after = significant_tokens(after_pipe);
    if sig_after.is_empty() {
        return None;
    }

    let func_name = match sig_after.first() {
        Some(Token::Ident(name)) => name.as_str(),
        _ => return None,
    };

    let spec = lookup_positional(func_name)?;

    // Piped form has one fewer arg than standalone (the first arg comes from the pipe).
    let piped_arity = spec.arity - 1;
    if sig_after.len() != piped_arity + 1 {
        return None;
    }

    // Collect formatted arg values.
    let args: Vec<String> = sig_after[1..]
        .iter()
        .map(|t| format_arg_value(t))
        .collect::<Option<Vec<_>>>()?;

    // Build the named-arg call string.
    let params_str: String = spec
        .piped_params
        .iter()
        .zip(args.iter())
        .map(|(name, val)| format!("{}={}", name, val))
        .collect::<Vec<_>>()
        .join(", ");

    // Reconstruct the before-pipe portion as a string.
    let before_str: String = before_pipe.iter().map(|t| token_to_str(t)).collect();
    // Preserve trailing whitespace from the original block.
    let trailing_ws = tokens
        .last()
        .and_then(|t| match t {
            Token::Space(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("");

    Some(format!(
        "{} | {}({}){}",
        before_str.trim_end(),
        func_name,
        params_str,
        trailing_ws
    ))
}

/// Format a token as a Tera argument value.
/// - Quoted strings are used as-is (they already have quotes).
/// - Identifiers are used bare (they reference template variables).
/// - Array literals are used as-is (e.g., `["a", "b"]`).
fn format_arg_value(token: &Token) -> Option<String> {
    match token {
        Token::Quoted(s) => Some(s.clone()),
        Token::Ident(s) => Some(s.clone()),
        Token::ArrayLiteral(s) => Some(s.clone()),
        _ => None,
    }
}

/// Convert a token back to its string representation.
fn token_to_str(token: &Token) -> Cow<'_, str> {
    match token {
        Token::Ident(s)
        | Token::Quoted(s)
        | Token::ArrayLiteral(s)
        | Token::Space(s)
        | Token::Other(s) => Cow::Borrowed(s.as_str()),
        Token::Pipe => Cow::Borrowed("|"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preprocess_positional_replace() {
        // Unit test for the preprocessor output
        let input = "{{ replace Version \"v\" \"\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ replace(s=Version, old=\"v\", new=\"\") }}");
    }

    #[test]
    fn test_preprocess_positional_replace_piped() {
        let input = "{{ Version | replace \"v\" \"\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ Version | replace(from=\"v\", to=\"\") }}"
        );
    }

    #[test]
    fn test_preprocess_positional_split() {
        let input = "{{ split Version \".\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ split(s=Version, sep=\".\") }}");
    }

    #[test]
    fn test_preprocess_positional_contains() {
        let input = "{{ contains Version \"rc\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ contains(s=Version, substr=\"rc\") }}");
    }

    #[test]
    fn test_preprocess_positional_piped_split() {
        let input = "{{ Version | split \".\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Version | split(sep=\".\") }}");
    }

    #[test]
    fn test_preprocess_positional_piped_contains() {
        let input = "{{ Version | contains \"rc\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Version | contains(substr=\"rc\") }}");
    }

    #[test]
    fn test_preprocess_named_args_unchanged() {
        // Already-named-arg syntax should pass through unmodified
        let input = "{{ replace(s=Version, old=\"v\", new=\"\") }}";
        let result = preprocess(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_preprocess_named_filter_unchanged() {
        let input = "{{ Version | replace(from=\"v\", to=\"\") }}";
        let result = preprocess(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_preprocess_control_block_rewritten() {
        // {% if contains Version "rc" %} should be rewritten to named-arg form
        let input = "{% if contains Version \"rc\" %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% if contains(s=Version, substr=\"rc\") %}yes{% endif %}"
        );
    }

    #[test]
    fn test_preprocess_control_block_non_positional_unchanged() {
        // {% if Version %} should not be touched (no positional func)
        let input = "{% if Version %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_positional_replace_with_dot_var() {
        // Dot-stripping + positional rewrite combined:
        // {{ replace .Tag "v" "" }} → dot-strip → {{ replace Tag "v" "" }} → positional → {{ replace(s=Tag, old="v", new="") }}
        let input = "{{ replace .Tag \"v\" \"\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ replace(s=Tag, old=\"v\", new=\"\") }}");
    }

    #[test]
    fn test_positional_piped_with_dot_var() {
        // {{ .Tag | replace "v" "" }} → dot-strip → {{ Tag | replace "v" "" }} → positional
        let input = "{{ .Tag | replace \"v\" \"\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Tag | replace(from=\"v\", to=\"\") }}");
    }

    #[test]
    fn test_positional_no_spaces_compact() {
        // Compact form: {{replace .Tag "v" ""}}
        let input = "{{replace .Tag \"v\" \"\"}}";
        let result = preprocess(input);
        assert_eq!(result, "{{replace(s=Tag, old=\"v\", new=\"\")}}");
    }

    #[test]
    fn test_unrelated_expression_unchanged() {
        // A simple variable reference should not be affected
        let input = "{{ Version }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Version }}");
    }

    #[test]
    fn test_unrelated_filter_unchanged() {
        // A normal filter chain should not be affected
        let input = "{{ Version | upper }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Version | upper }}");
    }

    #[test]
    fn test_positional_replace_whitespace_control() {
        // Tera whitespace control: {{- and -}}
        let input = "{{- replace Version \"v\" \"\" -}}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{- replace(s=Version, old=\"v\", new=\"\") -}}"
        );
    }

    #[test]
    fn test_positional_replace_whitespace_control_left_only() {
        let input = "{{- replace Version \"v\" \"\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{- replace(s=Version, old=\"v\", new=\"\") }}"
        );
    }

    #[test]
    fn test_chained_named_filter_then_positional_rewrite() {
        // Chained: named-arg filter followed by positional rewrite.
        // The preprocessor should rewrite ONLY the last segment's positional args.
        let input = "{{ Version | trimprefix(prefix=\"v\") | replace \".\" \"-\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ Version | trimprefix(prefix=\"v\") | replace(from=\".\", to=\"-\") }}"
        );
    }

    // --- `in` positional syntax preprocessing tests ---

    #[test]
    fn test_preprocess_in_with_list_subexpr() {
        // Go-style: {{ in (list "a" "b" "c") "b" }}
        // Pass 2: (list "a" "b" "c") → ["a", "b", "c"]
        // Pass 3: in ["a", "b", "c"] "b" → in(items=["a", "b", "c"], value="b")
        let input = "{{ in (list \"a\" \"b\" \"c\") \"b\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ in(items=[\"a\", \"b\", \"c\"], value=\"b\") }}"
        );
    }

    #[test]
    fn test_preprocess_in_with_variable() {
        // Positional: {{ in myList "b" }} → {{ in(items=myList, value="b") }}
        let input = "{{ in myList \"b\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ in(items=myList, value=\"b\") }}");
    }

    #[test]
    fn test_preprocess_in_named_args_unchanged() {
        let input = "{{ in(items=[\"a\", \"b\"], value=\"a\") }}";
        let result = preprocess(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_preprocess_in_with_dot_var() {
        // {{ in .MyList "val" }} → dot-strip → {{ in MyList "val" }} → positional
        let input = "{{ in .MyList \"val\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ in(items=MyList, value=\"val\") }}");
    }

    #[test]
    fn test_preprocess_in_control_block() {
        // {% if in myList "b" %} → {% if in(items=myList, value="b") %}
        let input = "{% if in myList \"b\" %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% if in(items=myList, value=\"b\") %}yes{% endif %}"
        );
    }

    #[test]
    fn test_preprocess_list_subexpr_rewrite() {
        // Verify the list subexpression rewrite pass in isolation:
        // (list "a" "b" "c") → ["a", "b", "c"]
        let input = "{{ in (list \"x\" \"y\") \"x\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ in(items=[\"x\", \"y\"], value=\"x\") }}");
    }

    #[test]
    fn test_preprocess_in_control_block_with_list_subexpr() {
        // {% if in (list "a" "b") "a" %} → list rewrite → {% if in ["a", "b"] "a" %}
        // → positional → {% if in(items=["a", "b"], value="a") %}
        let input = "{% if in (list \"a\" \"b\") \"a\" %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% if in(items=[\"a\", \"b\"], value=\"a\") %}yes{% endif %}"
        );
    }

    // --- `reReplaceAll` positional syntax preprocessing tests ---

    #[test]
    fn test_preprocess_re_replace_all_positional() {
        // {{ reReplaceAll "(.*)" "hello" "$1-world" }}
        // → {{ reReplaceAll(pattern="(.*)", input="hello", replacement="$1-world") }}
        let input = "{{ reReplaceAll \"(.*)\" \"hello\" \"$1-world\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ reReplaceAll(pattern=\"(.*)\", input=\"hello\", replacement=\"$1-world\") }}"
        );
    }

    #[test]
    fn test_preprocess_re_replace_all_with_variable() {
        // {{ reReplaceAll "(v)(.*)" Tag "prefix-$2" }}
        // → {{ reReplaceAll(pattern="(v)(.*)", input=Tag, replacement="prefix-$2") }}
        let input = "{{ reReplaceAll \"(v)(.*)\" Tag \"prefix-$2\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ reReplaceAll(pattern=\"(v)(.*)\", input=Tag, replacement=\"prefix-$2\") }}"
        );
    }

    #[test]
    fn test_preprocess_re_replace_all_named_args_unchanged() {
        let input = "{{ reReplaceAll(pattern=\"x\", input=\"ax\", replacement=\"y\") }}";
        let result = preprocess(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_preprocess_re_replace_all_piped() {
        // {{ Message | reReplaceAll "(.*)" "$1-done" }}
        // → {{ Message | reReplaceAll(pattern="(.*)", replacement="$1-done") }}
        let input = "{{ Message | reReplaceAll \"(.*)\" \"$1-done\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ Message | reReplaceAll(pattern=\"(.*)\", replacement=\"$1-done\") }}"
        );
    }

    #[test]
    fn test_preprocess_re_replace_all_control_block() {
        // {% if reReplaceAll "v" Tag "" %} → named-arg form
        let input = "{% if reReplaceAll \"v\" Tag \"\" %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% if reReplaceAll(pattern=\"v\", input=Tag, replacement=\"\") %}yes{% endif %}"
        );
    }

    // --- `in` piped form preprocessing tests ---

    #[test]
    fn test_preprocess_in_piped() {
        // {{ myList | in "val" }} → {{ myList | in(value="val") }}
        let input = "{{ myList | in \"val\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ myList | in(value=\"val\") }}");
    }

    // --- list subexpr: escaped quotes and mixed quote styles ---

    #[test]
    fn test_preprocess_list_subexpr_escaped_double_quotes() {
        // (list "hello \"world\"" "plain") should parse correctly
        let input = r#"{{ in (list "hello \"world\"" "plain") "plain" }}"#;
        let result = preprocess(input);
        assert_eq!(
            result,
            r#"{{ in(items=["hello \"world\"", "plain"], value="plain") }}"#
        );
    }

    #[test]
    fn test_preprocess_list_subexpr_escaped_single_quotes() {
        // (list 'it\'s' 'fine') should parse correctly
        let input = "{{ in (list 'it\\'s' 'fine') \"fine\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ in(items=['it\\'s', 'fine'], value=\"fine\") }}"
        );
    }

    #[test]
    fn test_preprocess_list_subexpr_mixed_quote_styles() {
        // (list "double" 'single' "another") — each item uses its own quote style
        let input = "{{ in (list \"double\" 'single' \"another\") \"double\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ in(items=[\"double\", 'single', \"another\"], value=\"double\") }}"
        );
    }
}
