//! Pass 2c map syntax + Pass 3 positional → named-arg syntax rewriting.

use super::blocks::replace_live_blocks;
use super::go_blocks::{extract_block_parts, try_rewrite_control_block};
use super::static_regex;
use super::string_lit::RAW_STRING_RE_ALT;
use super::tokens::{MAX_EXPR_NESTING, Token, significant_tokens, token_to_str, tokenize_block};
use regex::Regex;
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Pass 2c: Go-style `map "k1" "v1" ...` → `map(pairs=["k1", "v1", ...])`.
// ---------------------------------------------------------------------------

/// Regex matching Go-style variadic `map "k1" "v1" "k2" "v2" ...` calls.
/// Each item can be a quoted string or a bare identifier.
static MAP_POSITIONAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    // Match `map` followed by 2+ space-separated args (quoted strings or identifiers).
    let item = format!(r"(?:{RAW_STRING_RE_ALT}|[a-zA-Z_][a-zA-Z0-9_.]*)");
    // Require at least two args (one key-value pair).
    // Use a capture group for the preceding character instead of look-behind.
    // No look-ahead needed: the greedy match of args handles the boundary,
    // and the pattern only ever runs against one template block's contents.
    let pattern = format!(r"(?:^|(?P<pre>[^a-zA-Z0-9_]))map\s+(?P<args>{item}(?:\s+{item})+)");
    static_regex(&pattern)
});

/// Rewrite Go-style `map "k1" "v1" "k2" "v2"` to `map(pairs=["k1", "v1", "k2", "v2"])`.
pub(super) fn preprocess_map_syntax(template: &str) -> String {
    replace_live_blocks(template, |block: &str| {
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
                    static_regex(&format!(r"{RAW_STRING_RE_ALT}|[a-zA-Z_][a-zA-Z0-9_.]*"))
                });
                let items: Vec<&str> = ITEM_RE.find_iter(args_str).map(|m| m.as_str()).collect();
                let array_literal = format!("[{}]", items.join(", "));
                format!("{}map(pairs={})", pre, array_literal)
            })
            .to_string();

        format!("{}{}{}", open, rewritten, close)
    })
}

/// Pass 3: Convert Go-style positional function calls to Tera named-arg syntax.
///
/// Every builtin in [`POSITIONAL_FUNCTIONS`] and [`UNARY_FUNCTIONS`] is handled
/// in standalone form; those with an argument-taking filter form are also
/// handled piped. Two representative examples of each:
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
/// Already-named-arg syntax (an identifier glued to `(`) is passed through
/// unchanged.
///
/// **Sub-expression arguments** are rewritten recursively, at any depth and in
/// any argument position:
/// - `{{ trimprefix (base Path) "v" }}` →
///   `{{ trimprefix(s=(base(s=Path)), prefix="v") }}`
/// - `{{ printf "%s" (tolower Os) }}` →
///   `{{ printf(format="%s", args=[(tolower(s=Os))]) }}`
///
/// The author's parentheses are kept: Tera accepts a parenthesized expression
/// wherever it accepts a value, so grouping survives even when the inner form
/// needs no rewrite.
pub(super) fn preprocess_positional_syntax(template: &str) -> String {
    replace_live_blocks(template, |block: &str| {
        // Extract the open/close delimiters and inner content, accounting
        // for Tera's whitespace-control variants (`{{-`, `-}}`, `{%-`, `-%}`).
        let (open, inner, close) = extract_block_parts(block);

        if block.starts_with("{%") {
            // A control block (`{% if contains Version "rc" %}`) carries its
            // expression after the keyword, so only that portion is rewritten.
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

        match rewrite_expr_tokens(&tokens) {
            Some(rewritten) => format!("{}{}{}", open, rewritten, close),
            // No positional syntax detected; return unchanged.
            None => block.to_string(),
        }
    })
}

/// Rewrite one Go expression — the inside of a `{{ }}` block, or the inside of
/// a sub-expression — into Tera syntax. `None` means nothing matched.
///
/// A `|` splits the expression into a head and one filter segment per pipe,
/// and each segment is rewritten on its own. Go accepts a positional call in
/// every one of those slots (`trimprefix .Tag "v" | upper`,
/// `.Version | replace "v" "" | upper`), so treating the presence of a pipe as
/// grounds to skip the head — or rewriting only the last segment — loses the
/// call and takes the whole template's parse with it.
pub(super) fn rewrite_expr_tokens(tokens: &[Token]) -> Option<String> {
    rewrite_expr_tokens_at(tokens, 0)
}

/// [`rewrite_expr_tokens`], carrying the sub-expression nesting depth reached
/// so far so the recursion is bounded (see [`rewrite_subexpr`]).
fn rewrite_expr_tokens_at(tokens: &[Token], depth: usize) -> Option<String> {
    let segments = pipe_segments(tokens);
    match segments.as_slice() {
        [only] => rewrite_head_segment(only, depth),
        _ => rewrite_pipeline(&segments, depth),
    }
}

/// Split `tokens` on the pipes that separate pipeline segments.
///
/// Only a top-level [`Token::Pipe`] is a boundary. A `|` inside a string
/// literal, an array literal, or a sub-expression never reaches this function
/// as its own token — the tokenizer captured each of those whole — so the
/// segmentation obeys exactly the literal and paren rules the sub-expression
/// rewrite obeys.
fn pipe_segments(tokens: &[Token]) -> Vec<&[Token]> {
    let mut segments = Vec::new();
    let mut start = 0;
    for (i, token) in tokens.iter().enumerate() {
        if matches!(token, Token::Pipe) {
            segments.push(&tokens[start..i]);
            start = i + 1;
        }
    }
    segments.push(&tokens[start..]);
    segments
}

/// Rewrite the head of a pipeline (or a whole pipe-free expression): the slot
/// that carries a value rather than a filter applied to one.
///
/// The forms are tried most-specific first:
/// 1. `slice item start [end]` → the piped filter `item | slice(start=…)`,
///    because Go's `slice` operates on its first arg, which maps onto Tera's
///    filter input.
/// 2. `printf "fmt" a b …` / `print` / `println` / `list a b …` — the variadic
///    builtins, whose trailing args collect into one named array.
/// 3. `funcname arg1 arg2 [arg3]` — the standalone (function) form.
/// 4. Anything else that merely *contains* a sub-expression, so a Go call
///    nested inside an expression this pass does not otherwise touch
///    (`{{ (tolower Os) ~ "-" }}`) still gets rewritten.
fn rewrite_head_segment(tokens: &[Token], depth: usize) -> Option<String> {
    try_rewrite_slice(tokens, depth)
        .or_else(|| try_rewrite_variadic(tokens, depth))
        .or_else(|| try_rewrite_standalone(tokens, depth))
        .or_else(|| rewrite_subexprs_only(tokens, depth))
}

/// Rewrite each segment of a pipeline and rejoin them on `|`, preserving the
/// author's spacing around every pipe. `None` when no segment changed, so a
/// pipeline of Tera-native filters is passed through byte-identical.
fn rewrite_pipeline(segments: &[&[Token]], depth: usize) -> Option<String> {
    let mut out = String::new();
    let mut changed = false;
    for (i, segment) in segments.iter().enumerate() {
        if i > 0 {
            out.push('|');
        }
        let rewritten = if i == 0 {
            rewrite_head_segment(segment, depth)
        } else {
            try_rewrite_filter(segment, depth).or_else(|| rewrite_subexprs_only(segment, depth))
        };
        match rewritten {
            Some(text) => {
                changed = true;
                out.push_str(&text);
            }
            None => out.extend(segment.iter().map(|t| token_to_str(t))),
        }
    }
    changed.then_some(out)
}

/// Reconstruct `tokens` verbatim except for sub-expressions, which are
/// rewritten recursively. `None` when there is no sub-expression to descend
/// into, so callers can distinguish "nothing to do" from "rewritten".
pub(super) fn rewrite_subexprs_only(tokens: &[Token], depth: usize) -> Option<String> {
    tokens
        .iter()
        .any(|t| matches!(t, Token::SubExpr(_)))
        .then(|| render_tokens(tokens, depth))
}

/// Reconstruct a token slice, recursively rewriting every sub-expression.
fn render_tokens(tokens: &[Token], depth: usize) -> String {
    tokens
        .iter()
        .map(|t| match t {
            Token::SubExpr(text) => rewrite_subexpr(text, depth),
            other => token_to_str(other).into_owned(),
        })
        .collect()
}

/// Rewrite the inside of a `( … )` sub-expression, keeping the parentheses.
///
/// Recursion bottoms out because the inner slice is strictly shorter than the
/// token that contained it, and is additionally capped at
/// [`MAX_EXPR_NESTING`] so author-controlled nesting cannot exhaust the stack
/// or memory. `depth` is the nesting level of the expression this group sits
/// in, so the group itself opens level `depth + 1` — the same level
/// `tokens::scan_parens` counts. The two caps therefore agree exactly: a
/// template `template_preprocess::check_block_expressions` accepts is rewritten
/// all the way down. Past the cap the group is emitted verbatim, which only
/// keeps the pass terminating for a caller that skipped that check.
fn rewrite_subexpr(text: &str, depth: usize) -> String {
    if depth + 1 > MAX_EXPR_NESTING {
        return text.to_string();
    }
    // The token always carries both delimiters, so trimming one byte from each
    // end yields the inner expression.
    let inner = &text[1..text.len() - 1];
    match rewrite_expr_tokens_at(&tokenize_block(inner), depth + 1) {
        Some(rewritten) => format!("({})", rewritten),
        None => text.to_string(),
    }
}

/// Positional syntax signature for a function/filter.
#[derive(Clone, Copy)]
struct PositionalSyntax {
    /// Function name (e.g. "replace").
    name: &'static str,
    /// Parameter names for standalone form (e.g. `replace(s=..., old=..., new=...)`).
    /// Its length is the Go positional arity.
    standalone_params: &'static [&'static str],
    /// Parameter names for piped form (e.g. `| replace(from=..., to=...)`).
    /// The first standalone param is implicit (it comes from the pipe), so this
    /// is one shorter. Empty means the builtin has no argument-taking filter
    /// form and a pipe is left alone.
    piped_params: &'static [&'static str],
}

/// Data-driven table of the multi-argument positional syntax rewrites.
/// Single-argument builtins live in [`UNARY_FUNCTIONS`].
static POSITIONAL_FUNCTIONS: &[PositionalSyntax] = &[
    PositionalSyntax {
        name: "replace",
        standalone_params: &["s", "old", "new"],
        piped_params: &["from", "to"],
    },
    PositionalSyntax {
        name: "split",
        standalone_params: &["s", "sep"],
        piped_params: &["sep"],
    },
    PositionalSyntax {
        name: "contains",
        standalone_params: &["s", "substr"],
        piped_params: &["substr"],
    },
    PositionalSyntax {
        name: "in",
        standalone_params: &["items", "value"],
        piped_params: &["value"],
    },
    PositionalSyntax {
        name: "reReplaceAll",
        standalone_params: &["pattern", "input", "replacement"],
        piped_params: &["pattern", "replacement"],
    },
    PositionalSyntax {
        name: "filter",
        standalone_params: &["items", "regexp"],
        piped_params: &["regexp"],
    },
    PositionalSyntax {
        name: "reverseFilter",
        standalone_params: &["items", "regexp"],
        piped_params: &["regexp"],
    },
    PositionalSyntax {
        name: "index",
        standalone_params: &["collection", "key"],
        piped_params: &["key"],
    },
    PositionalSyntax {
        name: "trimprefix",
        standalone_params: &["s", "prefix"],
        piped_params: &["prefix"],
    },
    PositionalSyntax {
        name: "trimsuffix",
        standalone_params: &["s", "suffix"],
        piped_params: &["suffix"],
    },
    PositionalSyntax {
        name: "envOrDefault",
        standalone_params: &["name", "default"],
        piped_params: &[],
    },
    PositionalSyntax {
        name: "indexOrDefault",
        standalone_params: &["map", "key", "default"],
        piped_params: &[],
    },
];

const PARAM_S: &[&str] = &["s"];
const PARAM_V: &[&str] = &["v"];
const PARAM_NAME: &[&str] = &["name"];
const PARAM_PATH: &[&str] = &["path"];
const PARAM_ITEMS: &[&str] = &["items"];
const PARAM_FORMAT: &[&str] = &["format"];

/// Builtins whose Go form carries exactly one positional argument, paired with
/// the named parameter that argument maps onto. None of them has an
/// argument-taking filter form, so a piped occurrence is left alone.
static UNARY_FUNCTIONS: &[(&str, &[&str])] = &[
    ("abs", PARAM_S),
    ("base", PARAM_S),
    ("blake2b", PARAM_S),
    ("blake2s", PARAM_S),
    ("blake3", PARAM_S),
    ("crc32", PARAM_S),
    ("dir", PARAM_S),
    ("englishJoin", PARAM_ITEMS),
    ("incmajor", PARAM_V),
    ("incminor", PARAM_V),
    ("incpatch", PARAM_V),
    ("isEnvSet", PARAM_NAME),
    ("md5", PARAM_S),
    ("mdv2escape", PARAM_S),
    ("mustReadFile", PARAM_PATH),
    ("readFile", PARAM_PATH),
    ("sha1", PARAM_S),
    ("sha224", PARAM_S),
    ("sha256", PARAM_S),
    ("sha384", PARAM_S),
    ("sha512", PARAM_S),
    ("sha3_224", PARAM_S),
    ("sha3_256", PARAM_S),
    ("sha3_384", PARAM_S),
    ("sha3_512", PARAM_S),
    // Pasted GoReleaser `{{ time "2006-01-02" }}` is positional; the `time`
    // function takes a named `format=` arg.
    ("time", PARAM_FORMAT),
    ("title", PARAM_S),
    ("tolower", PARAM_S),
    ("toupper", PARAM_S),
    ("trim", PARAM_S),
    ("urlPathEscape", PARAM_S),
];

/// Look up a function name in the positional syntax tables.
fn lookup_positional(name: &str) -> Option<PositionalSyntax> {
    if let Some(spec) = POSITIONAL_FUNCTIONS.iter().find(|p| p.name == name) {
        return Some(*spec);
    }
    UNARY_FUNCTIONS
        .iter()
        .find(|(unary_name, _)| *unary_name == name)
        .map(|(name, standalone_params)| PositionalSyntax {
            name,
            standalone_params,
            piped_params: &[],
        })
}

/// Builtins the preprocessor rewrites outside [`lookup_positional`], each named
/// with the pass that owns it: `list` and `map` are variadic constructors
/// ([`try_rewrite_variadic`] / [`preprocess_map_syntax`]), `printf` / `print` /
/// `println` are variadic formatters ([`try_rewrite_variadic`]), and `slice`
/// becomes a piped filter ([`try_rewrite_slice`]).
#[cfg(test)]
pub(super) const PREPROCESSED_ELSEWHERE: &[&str] =
    &["list", "map", "print", "printf", "println", "slice"];

/// Builtins that have no Go positional form to rewrite, each with its reason:
///
/// - `contains_any` — anodizer's alias for `in`, added because Tera reserves
///   `in` as a keyword inside `{% %}` bodies. The Go spelling is `in`.
/// - `date` — restores the tera 1.x `date` filter; not a Go builtin.
/// - `now_format` — the target Pass 4 rewrites Go's `.Now.Format "…"` method
///   call into. Users never write the name, so there is no call form to accept.
/// - `ruby_escape` — anodizer's Homebrew formula/cask literal escaper, with no
///   Go counterpart.
#[cfg(test)]
pub(super) const NO_POSITIONAL_FORM: &[&str] =
    &["contains_any", "date", "now_format", "ruby_escape"];

/// Every builtin name the positional syntax tables cover.
#[cfg(test)]
pub(super) fn positional_builtin_names() -> impl Iterator<Item = &'static str> {
    positional_specs().map(|(name, _)| name)
}

/// Every rostered builtin paired with its standalone parameter list, so a test
/// can synthesize a full-arity call for each without restating the arity.
#[cfg(test)]
pub(super) fn positional_specs() -> impl Iterator<Item = (&'static str, &'static [&'static str])> {
    POSITIONAL_FUNCTIONS
        .iter()
        .map(|spec| (spec.name, spec.standalone_params))
        .chain(UNARY_FUNCTIONS.iter().copied())
}

/// True when the tokens already contain a bare `(` — Tera's named-arg call
/// syntax, which every rewrite helper leaves alone. Shared so the check lives
/// in one place.
///
/// A balanced Go sub-expression is its own [`Token::SubExpr`], never a bare
/// `(`, so `trimprefix (base Path) "v"` is still recognised as positional.
fn is_named_arg_call(tokens: &[&Token]) -> bool {
    tokens
        .iter()
        .any(|t| matches!(t, Token::Other(s) if s == "("))
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
fn try_rewrite_slice(tokens: &[Token], depth: usize) -> Option<String> {
    let sig = significant_tokens(tokens);

    if is_named_arg_call(&sig) {
        return None;
    }

    if !matches!(sig.first(), Some(Token::Ident(name)) if name == "slice") {
        return None;
    }

    // sig is `[slice, item, start]` (arity 2) or `[slice, item, start, end]` (arity 3).
    if sig.len() != 3 && sig.len() != 4 {
        return None;
    }

    let item = format_arg_value(sig[1], depth)?;
    let start = format_arg_value(sig[2], depth)?;
    let params = if sig.len() == 4 {
        let end = format_arg_value(sig[3], depth)?;
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

/// Variadic Go builtins, paired with the named array parameter their trailing
/// positional args collect into.
const VARIADIC_FUNCTIONS: &[(&str, &str)] = &[
    ("printf", "args"),
    ("print", "args"),
    ("println", "args"),
    ("list", "items"),
];

/// Rewrite the variadic Go builtins to their named-arg forms:
/// `printf "fmt" a b …` → `printf(format="fmt", args=[a, b, …])`,
/// `print a b …` → `print(args=[a, b, …])`,
/// `println a b …` → `println(args=[a, b, …])`, and
/// `list a b …` → `list(items=[a, b, …])`.
///
/// Trailing positional args collect into one array parameter, mirroring the
/// `map(pairs=[…])` rewrite. `(list …)` used as a subexpression is already an
/// array literal by this point — Pass 2 rewrote it — so only the bare call form
/// reaches here.
fn try_rewrite_variadic(tokens: &[Token], depth: usize) -> Option<String> {
    let sig = significant_tokens(tokens);

    if is_named_arg_call(&sig) {
        return None;
    }

    let func_name = match sig.first() {
        Some(Token::Ident(name)) => name.as_str(),
        _ => return None,
    };
    let array_param = VARIADIC_FUNCTIONS
        .iter()
        .find(|(name, _)| *name == func_name)
        .map(|(_, param)| *param)?;

    // A bare `{{ list }}` is far likelier to be a variable reference than an
    // empty constructor — `list` is the canonical stand-in name, and `list.0`
    // indexing is a supported compat form. Require an actual argument before
    // claiming the name. The printf family carries no such ambiguity.
    if func_name == "list" && sig.len() < 2 {
        return None;
    }

    // `printf` consumes its first arg as the format string; the rest treat
    // every arg as a value.
    let rest = &sig[1..];
    let (format_part, value_tokens) = if func_name == "printf" {
        let fmt = rest.first()?;
        (Some(format_arg_value(fmt, depth)?), &rest[1..])
    } else {
        (None, rest)
    };

    let values: Vec<String> = value_tokens
        .iter()
        .map(|t| format_arg_value(t, depth))
        .collect::<Option<Vec<_>>>()?;
    let args_literal = format!("{}=[{}]", array_param, values.join(", "));

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
pub(super) fn try_rewrite_standalone(tokens: &[Token], depth: usize) -> Option<String> {
    let sig = significant_tokens(tokens);

    // Parens mean already named-arg syntax.
    if is_named_arg_call(&sig) {
        return None;
    }

    let func_name = match sig.first() {
        Some(Token::Ident(name)) => name.as_str(),
        _ => return None,
    };

    let spec = lookup_positional(func_name)?;

    // sig should be: [funcname, arg1, arg2, ...] with one arg per parameter.
    if sig.len() != spec.standalone_params.len() + 1 {
        return None;
    }

    // Collect formatted arg values.
    let args: Vec<String> = sig[1..]
        .iter()
        .map(|t| format_arg_value(t, depth))
        .collect::<Option<Vec<_>>>()?;

    // Build the named-arg call string.
    let params_str: String = spec
        .standalone_params
        .iter()
        .zip(args.iter())
        .map(|(name, val)| format!("{}={}", name, val))
        .collect::<Vec<_>>()
        .join(", ");

    let (leading_ws, trailing_ws) = block_whitespace(tokens);
    Some(format!(
        "{}{}({}){}",
        leading_ws, func_name, params_str, trailing_ws
    ))
}

/// Try to rewrite one filter segment of a pipeline — the tokens between two
/// pipes, or after the last one:
/// `replace <quoted> <quoted>` → `replace(from=<quoted>, to=<quoted>)`
/// `split <quoted>` → `split(sep=<quoted>)`
/// `contains <quoted>` → `contains(substr=<quoted>)`
///
/// Returns `None` if the pattern doesn't match, which leaves the segment — and
/// so the whole pipeline's spacing around it — byte-identical.
fn try_rewrite_filter(tokens: &[Token], depth: usize) -> Option<String> {
    let sig = significant_tokens(tokens);

    // A `(` glued to the filter name is Tera's own named-arg syntax already.
    if is_named_arg_call(&sig) {
        return None;
    }

    let func_name = match sig.first() {
        Some(Token::Ident(name)) => name.as_str(),
        _ => return None,
    };

    let spec = lookup_positional(func_name)?;

    // An empty `piped_params` means no argument-taking filter form exists, so
    // there is nothing to rewrite a pipe into — leave the segment untouched and
    // let Tera report the unknown filter against the text the user wrote.
    if spec.piped_params.is_empty() {
        return None;
    }

    // Piped form has one fewer arg than standalone (the first arg comes from the pipe).
    if sig.len() != spec.piped_params.len() + 1 {
        return None;
    }

    // Collect formatted arg values.
    let args: Vec<String> = sig[1..]
        .iter()
        .map(|t| format_arg_value(t, depth))
        .collect::<Option<Vec<_>>>()?;

    // Build the named-arg call string.
    let params_str: String = spec
        .piped_params
        .iter()
        .zip(args.iter())
        .map(|(name, val)| format!("{}={}", name, val))
        .collect::<Vec<_>>()
        .join(", ");

    let (leading_ws, trailing_ws) = block_whitespace(tokens);
    Some(format!(
        "{}{}({}){}",
        leading_ws, func_name, params_str, trailing_ws
    ))
}

/// Format a token as a Tera argument value.
/// - Quoted strings are used as-is (they already have quotes).
/// - Identifiers are used bare (they reference template variables).
/// - Array literals are used as-is (e.g., `["a", "b"]`).
/// - Sub-expressions are rewritten recursively, parentheses kept.
fn format_arg_value(token: &Token, depth: usize) -> Option<String> {
    match token {
        Token::Quoted(s) => Some(s.clone()),
        Token::Ident(s) => Some(s.clone()),
        Token::ArrayLiteral(s) => Some(s.clone()),
        Token::SubExpr(s) => Some(rewrite_subexpr(s, depth)),
        _ => None,
    }
}
