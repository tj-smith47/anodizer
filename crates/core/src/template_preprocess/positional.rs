//! Pass 2c map syntax + Pass 3 positional → named-arg syntax rewriting.

use super::GO_BLOCK_RE;
use super::go_blocks::{extract_block_parts, try_rewrite_control_block};
use super::static_regex;
use super::tokens::{Token, significant_tokens, token_to_str, tokenize_block};
use regex::Regex;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Pass 2c: Go-style `map "k1" "v1" ...` → `map(pairs=["k1", "v1", ...])`.
// ---------------------------------------------------------------------------

/// Regex matching Go-style variadic `map "k1" "v1" "k2" "v2" ...` calls.
/// Each item can be a quoted string or a bare identifier.
static MAP_POSITIONAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Match `map` followed by 2+ space-separated args (quoted strings or identifiers).
    let item = r#"(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[a-zA-Z_][a-zA-Z0-9_.]*)"#;
    // Require at least two args (one key-value pair).
    // Use a capture group for the preceding character instead of look-behind.
    // No look-ahead needed; the greedy match of args handles the boundary
    // naturally, and we only apply this inside template blocks anyway.
    let pattern = format!(r"(?:^|(?P<pre>[^a-zA-Z0-9_]))map\s+(?P<args>{item}(?:\s+{item})+)");
    static_regex(&pattern)
});

/// Rewrite Go-style `map "k1" "v1" "k2" "v2"` to `map(pairs=["k1", "v1", "k2", "v2"])`.
pub(super) fn preprocess_map_syntax(template: &str) -> String {
    GO_BLOCK_RE
        .replace_all(template, |caps: &regex::Captures| {
            let block = &caps[0];
            // Fast path: skip blocks that don't contain `map `.
            if !block.contains("map ") {
                return block.to_string();
            }
            // Skip blocks that already have named-arg syntax for map.
            if block.contains("map(") {
                return block.to_string();
            }

            let (open, inner, close) = extract_block_parts(block);

            let rewritten = MAP_POSITIONAL_RE
                .replace_all(inner, |mcaps: &regex::Captures| {
                    let pre = mcaps.name("pre").map_or("", |m| m.as_str());
                    let args_str = mcaps.name("args").map_or("", |m| m.as_str());
                    // Tokenize the arguments.
                    static ITEM_RE: LazyLock<Regex> = LazyLock::new(|| {
                        static_regex(
                            r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[a-zA-Z_][a-zA-Z0-9_.]*"#,
                        )
                    });
                    let items: Vec<&str> =
                        ITEM_RE.find_iter(args_str).map(|m| m.as_str()).collect();
                    let array_literal = format!("[{}]", items.join(", "));
                    format!("{}map(pairs={})", pre, array_literal)
                })
                .to_string();

            format!("{}{}{}", open, rewritten, close)
        })
        .to_string()
}

/// Pass 3: Convert Go-style positional function calls to Tera named-arg syntax.
///
/// Handles two forms for `replace`, `split`, `contains`, `in`, and `reReplaceAll`:
///
/// **Standalone (function) form:**
/// - `{{ replace Version "v" "" }}` → `{{ replace(s=Version, old="v", new="") }}`
/// - `{{ split Version "." }}` → `{{ split(s=Version, sep=".") }}`
/// - `{{ contains Version "rc" }}` → `{{ contains(s=Version, substr="rc") }}`
/// - `{{ in ["a","b"] "a" }}` → `{{ in(items=["a","b"], value="a") }}`
/// - `{{ reReplaceAll "v" Tag "" }}` → `{{ reReplaceAll(pattern="v", input=Tag, replacement="") }}`
///
/// **Piped (filter) form:**
/// - `{{ Version | replace "v" "" }}` → `{{ Version | replace(from="v", to="") }}`
/// - `{{ Version | split "." }}` → `{{ Version | split(sep=".") }}`
/// - `{{ Version | contains "rc" }}` → `{{ Version | contains(substr="rc") }}`
/// - `{{ myList | in "val" }}` → `{{ myList | in(value="val") }}`
/// - `{{ Tag | reReplaceAll "v" "" }}` → `{{ Tag | reReplaceAll(pattern="v", replacement="") }}`
///
/// Already-named-arg syntax (contains `(`) is passed through unchanged.
pub(super) fn preprocess_positional_syntax(template: &str) -> String {
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

            // `slice item start [end]` rewrites to the piped filter form
            // `item | slice(start=…[, end=…])` because Go's `slice` operates
            // on the first arg (the item), which maps onto Tera's filter input.
            if let Some(rewritten) = try_rewrite_slice(&tokens) {
                return format!("{}{}{}", open, rewritten, close);
            }

            // `printf "fmt" a b …`, `print a b …`, `println a b …` collect
            // their trailing positional args into an `args=[…]` array.
            if let Some(rewritten) = try_rewrite_printf_like(&tokens) {
                return format!("{}{}{}", open, rewritten, close);
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
    PositionalSyntax {
        name: "filter",
        arity: 2,
        standalone_params: &["items", "regexp"],
        piped_params: &["regexp"],
    },
    PositionalSyntax {
        name: "reverseFilter",
        arity: 2,
        standalone_params: &["items", "regexp"],
        piped_params: &["regexp"],
    },
    PositionalSyntax {
        name: "readFile",
        arity: 1,
        standalone_params: &["path"],
        piped_params: &[],
    },
    PositionalSyntax {
        name: "mustReadFile",
        arity: 1,
        standalone_params: &["path"],
        piped_params: &[],
    },
    PositionalSyntax {
        name: "index",
        arity: 2,
        standalone_params: &["collection", "key"],
        piped_params: &["key"],
    },
    // Pasted GoReleaser `{{ time "2006-01-02" }}` is positional; the `time`
    // function takes a named `format=` arg, so rewrite arity-1 to that.
    PositionalSyntax {
        name: "time",
        arity: 1,
        standalone_params: &["format"],
        piped_params: &[],
    },
];

/// Look up a function name in the positional syntax table.
fn lookup_positional(name: &str) -> Option<&'static PositionalSyntax> {
    POSITIONAL_FUNCTIONS.iter().find(|p| p.name == name)
}

/// Extract the leading and trailing whitespace tokens of a block so the
/// rewrite preserves the original spacing (and Tera whitespace-control).
fn block_whitespace(tokens: &[Token]) -> (&str, &str) {
    let leading = tokens
        .first()
        .and_then(|t| match t {
            Token::Space(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("");
    let trailing = tokens
        .last()
        .and_then(|t| match t {
            Token::Space(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("");
    (leading, trailing)
}

/// Rewrite Go `slice item start [end]` to the Tera filter form
/// `item | slice(start=…[, end=…])`.
///
/// Unlike the table-driven rewrites, `slice`'s first positional arg is the
/// item being sliced, which maps onto Tera's pipe input — so the standalone
/// Go call becomes a piped filter rather than a function call. Accepts 2 or 3
/// positional args (`slice X 0` = start only; `slice X 0 7` = start + end).
fn try_rewrite_slice(tokens: &[Token]) -> Option<String> {
    let sig = significant_tokens(tokens);

    // Already named-arg syntax or already piped — leave it alone.
    if sig
        .iter()
        .any(|t| matches!(t, Token::Other(s) if s == "(") || matches!(t, Token::Pipe))
    {
        return None;
    }

    if !matches!(sig.first(), Some(Token::Ident(name)) if name == "slice") {
        return None;
    }

    // sig is `[slice, item, start]` (arity 2) or `[slice, item, start, end]` (arity 3).
    if sig.len() != 3 && sig.len() != 4 {
        return None;
    }

    let item = format_arg_value(sig[1])?;
    let start = format_arg_value(sig[2])?;
    let params = if sig.len() == 4 {
        let end = format_arg_value(sig[3])?;
        format!("start={}, end={}", start, end)
    } else {
        format!("start={}", start)
    };

    let (leading, trailing) = block_whitespace(tokens);
    Some(format!(
        "{}{} | slice({}){}",
        leading, item, params, trailing
    ))
}

/// Rewrite Go `printf "fmt" a b …`, `print a b …`, and `println a b …` to the
/// named-arg forms `printf(format="fmt", args=[a, b, …])` /
/// `print(args=[a, b, …])` / `println(args=[a, b, …])`.
///
/// These builtins are variadic, so trailing positional args collect into an
/// `args` array (mirroring the `map(pairs=[…])` rewrite).
fn try_rewrite_printf_like(tokens: &[Token]) -> Option<String> {
    let sig = significant_tokens(tokens);

    // Already named-arg syntax or piped — leave it alone.
    if sig
        .iter()
        .any(|t| matches!(t, Token::Other(s) if s == "(") || matches!(t, Token::Pipe))
    {
        return None;
    }

    let func_name = match sig.first() {
        Some(Token::Ident(name)) => name.as_str(),
        _ => return None,
    };
    if !matches!(func_name, "printf" | "print" | "println") {
        return None;
    }

    // `printf` consumes its first arg as the format string; `print`/`println`
    // treat every arg as a value to concatenate.
    let rest = &sig[1..];
    let (format_part, value_tokens) = if func_name == "printf" {
        let fmt = rest.first()?;
        (Some(format_arg_value(fmt)?), &rest[1..])
    } else {
        (None, rest)
    };

    let values: Vec<String> = value_tokens
        .iter()
        .map(|t| format_arg_value(t))
        .collect::<Option<Vec<_>>>()?;
    let args_literal = format!("args=[{}]", values.join(", "));

    let params = match format_part {
        Some(fmt) => format!("format={}, {}", fmt, args_literal),
        None => args_literal,
    };

    let (leading, trailing) = block_whitespace(tokens);
    Some(format!("{}{}({}){}", leading, func_name, params, trailing))
}

/// Try to rewrite standalone positional form:
/// `replace <arg> <quoted> <quoted>` → `replace(s=<arg>, old=<quoted>, new=<quoted>)`
/// `split <arg> <quoted>` → `split(s=<arg>, sep=<quoted>)`
/// `contains <arg> <quoted>` → `contains(s=<arg>, substr=<quoted>)`
///
/// Returns `None` if the pattern doesn't match.
pub(super) fn try_rewrite_standalone(tokens: &[Token]) -> Option<String> {
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
pub(super) fn try_rewrite_piped(tokens: &[Token]) -> Option<String> {
    // Find the LAST pipe in the token stream. This handles chained filters like
    // `Version | trimprefix(prefix="v") | replace "." "-"` — we only rewrite
    // the final segment after the last pipe.
    let last_pipe_idx = tokens.iter().rposition(|t| matches!(t, Token::Pipe))?;

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
