//! Pass 2 list-subexpression rewriting and Pass 2b Go builtin/comparison/logical/`len` rewrites.

use super::GO_BLOCK_RE;
use super::go_blocks::extract_block_parts;
use super::static_regex;
use super::string_lit::RAW_STRING_RE_ALT;
use regex::Regex;
use std::sync::LazyLock;

/// Regex matching `(list "a" "b" ...)` subexpressions inside template blocks.
/// Captures the inner arguments (variadic args to `list`).
/// Each item independently matches:
/// - String literals under the raw boundary rule ([`RAW_STRING_RE_ALT`])
/// - Bare identifiers (variable references): `Os`, `Env.FOO`, `Version`
// SAFETY: Built from deterministic string literals; the resulting pattern is known to be valid.
static LIST_SUBEXPR_RE: LazyLock<Regex> = LazyLock::new(|| {
    // A single item: quoted string OR bare identifier (dotted paths like Env.FOO allowed).
    let item = format!(r"(?:{RAW_STRING_RE_ALT}|[a-zA-Z_][a-zA-Z0-9_.]*)");
    let pattern = format!(r"\(list\s+({item}(?:\s+{item})*)\)");
    static_regex(&pattern)
});

/// Pass 2: Rewrite Go-style `(list "a" "b" "c")` subexpressions to Tera array literals.
///
/// `(list "a" "b" "c")` → `["a", "b", "c"]`
///
/// This runs before positional syntax rewriting so that the `in` function can
/// receive a Tera array literal as its first argument.
pub(super) fn preprocess_list_subexpr(template: &str) -> String {
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
                    // Split items (quoted strings or bare identifiers) and rejoin as a Tera array literal.
                    // Bare identifiers pass through as variable references: `[Os, "windows"]`.
                    static ITEM_RE: LazyLock<Regex> = LazyLock::new(|| {
                        static_regex(&format!(r"{RAW_STRING_RE_ALT}|[a-zA-Z_][a-zA-Z0-9_.]*"))
                    });
                    let items: Vec<&str> = ITEM_RE.find_iter(inner).map(|m| m.as_str()).collect();
                    format!("[{}]", items.join(", "))
                })
                .to_string()
        })
        .to_string()
}

// ---------------------------------------------------------------------------
// Pass 2b: Go comparison functions, logical operators, and len → Tera syntax
// ---------------------------------------------------------------------------

/// Known Go comparison functions and their Tera infix operator equivalents.
const COMPARISON_OPS: &[(&str, &str)] = &[
    ("eq", "=="),
    ("ne", "!="),
    ("gt", ">"),
    ("lt", "<"),
    ("ge", ">="),
    ("le", "<="),
];

/// Rewrite Go-style comparison functions (`eq X Y` → `X == Y`), logical
/// functions (`and X Y` → `X and Y`, `or X Y` → `X or Y`), and `len`
/// (`len X` → `X | length`) inside `{% %}` and `{{ }}` blocks.
///
/// This runs after dot-stripping and list subexpr rewriting, so arguments
/// are already in Tera-native form (no leading dots, list subexprs already
/// converted to array literals).
pub(super) fn preprocess_go_builtins(template: &str) -> String {
    GO_BLOCK_RE
        .replace_all(template, |caps: &regex::Captures| {
            let block = &caps[0];
            let (open, inner, close) = extract_block_parts(block);

            // Quick check: does this block contain any Go builtin we care about?
            let needs_rewrite = COMPARISON_OPS.iter().any(|(name, _)| {
                // Check for function name followed by whitespace (avoid matching
                // substrings like "request" containing "eq").
                let with_space = format!("{} ", name);
                inner.contains(&*with_space)
            }) || inner.contains("and ")
                || inner.contains("or ")
                || inner.contains("len ");

            if !needs_rewrite {
                return block.to_string();
            }

            let rewritten = rewrite_go_builtins_in_expr(inner);
            format!("{}{}{}", open, rewritten, close)
        })
        .to_string()
}

/// Rewrite Go builtin functions in an expression string.
///
/// Strategy: handle specific patterns in priority order:
/// 1. `and/or` with parenthesized comparison args (most complex)
/// 2. `not` with parenthesized comparison arg
/// 3. Simple top-level comparisons (`eq X Y`)
/// 4. Simple top-level `and`/`or` with bare args
/// 5. `len X` → `X | length`
///
/// Tera doesn't support comparison operators (`==`, `!=`, etc.) inside
/// parentheses, so all comparison-containing parens are stripped.
fn rewrite_go_builtins_in_expr(expr: &str) -> String {
    let mut result = expr.to_string();

    // Rewrite `and`/`or` with parenthesized args first.
    // Pattern: `and/or (EXPR1) (EXPR2)` where EXPR can contain `eq`/`ne`/etc.
    // Inner expressions are processed first, then combined with the logical op.
    result = rewrite_logical_with_paren_args(&result);

    // Rewrite `not (COMPARISON_FUNC X Y)` → `not X OP Y`
    result = rewrite_not_with_paren_comparison(&result);

    // Rewrite top-level comparison functions: `eq X Y` → `X == Y`
    for (func_name, operator) in COMPARISON_OPS {
        result = rewrite_prefix_to_infix(&result, func_name, operator);
    }

    // Rewrite top-level logical functions: `and X Y` → `X and Y`
    result = rewrite_prefix_to_infix(&result, "and", "and");
    result = rewrite_prefix_to_infix(&result, "or", "or");

    // Rewrite `len X` → `X | length`
    result = rewrite_len(&result);

    result
}

/// Rewrite `and/or (EXPR1) (EXPR2)` patterns.
/// Each parenthesized argument is individually rewritten and then combined
/// with the logical operator (without parens, since Tera doesn't allow
/// comparisons inside parens).
fn rewrite_logical_with_paren_args(expr: &str) -> String {
    // Match: (and|or) (EXPR1) (EXPR2)
    // where EXPR can contain nested parens, quotes, etc.
    static LOGICAL_PAREN_RE: LazyLock<Regex> = LazyLock::new(|| {
        // Match `and` or `or` followed by two parenthesized groups.
        // The paren groups can contain anything except unbalanced parens.
        let paren_group = r#"\(([^()]*(?:\([^()]*\)[^()]*)*)\)"#;
        let pattern = format!(
            r"(?:^|(?P<pre>[^a-zA-Z0-9_]))(?P<op>and|or)\s+{}\s+{}",
            paren_group, paren_group
        );
        static_regex(&pattern)
    });

    LOGICAL_PAREN_RE
        .replace_all(expr, |caps: &regex::Captures| {
            let pre = caps.name("pre").map_or("", |m| m.as_str());
            let logical_op = caps.name("op").map_or("", |m| m.as_str());
            let inner1 = &caps[3]; // first paren group content
            let inner2 = &caps[4]; // second paren group content

            // Rewrite each inner expression (e.g., `eq Os "linux"` → `Os == "linux"`)
            let rewritten1 = rewrite_comparison_expr(inner1);
            let rewritten2 = rewrite_comparison_expr(inner2);

            format!("{}{} {} {}", pre, rewritten1, logical_op, rewritten2)
        })
        .to_string()
}

/// Rewrite `not (COMPARISON_FUNC X Y)` → `not X OP Y`.
fn rewrite_not_with_paren_comparison(expr: &str) -> String {
    static NOT_PAREN_RE: LazyLock<Regex> = LazyLock::new(|| {
        let paren_group = r#"\(([^()]*)\)"#;
        static_regex(&format!(r"not\s+{}", paren_group))
    });

    NOT_PAREN_RE
        .replace_all(expr, |caps: &regex::Captures| {
            let inner = &caps[1];
            let rewritten = rewrite_comparison_expr(inner);
            if rewritten != inner {
                // Comparison was rewritten — strip parens
                format!("not {}", rewritten)
            } else {
                // No rewrite needed — keep original
                caps[0].to_string()
            }
        })
        .to_string()
}

/// Rewrite a simple comparison expression: `eq X Y` → `X == Y`.
/// Also handles `len X` → `X | length`.
fn rewrite_comparison_expr(expr: &str) -> String {
    let mut result = expr.to_string();
    for (func_name, operator) in COMPARISON_OPS {
        result = rewrite_prefix_to_infix(&result, func_name, operator);
    }
    result = rewrite_len(&result);
    result
}

/// Rewrite a Go-style prefix function call with two or more arguments to infix form.
/// Matches: `funcname ARG1 ARG2 [ARG3 ...]` where `funcname` is at a word boundary.
///
/// Go's `eq` is variadic: `eq X Y Z` means `X == Y || X == Z`.
/// This function handles: `eq X Y` → `X == Y`, `eq X Y Z` → `X == Y or X == Z`.
///
/// Args can be: quoted strings, numbers, identifiers/dotted paths, or `(parens)`.
fn rewrite_prefix_to_infix(expr: &str, func_name: &str, operator: &str) -> String {
    // Cache compiled regexes per function name to avoid recompilation.
    use std::collections::HashMap;
    use std::sync::Mutex;
    static REGEX_CACHE: LazyLock<Mutex<HashMap<String, Regex>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    let arg_pattern = format!(
        r"(?:{RAW_STRING_RE_ALT}|\((?:[^()]*(?:\([^()]*\))*[^()]*)\)|[a-zA-Z_][a-zA-Z0-9_.]*|\d+)"
    );

    // Build regex that captures the first arg and ALL remaining args as a tail.
    let pattern = format!(
        r"(?:^|(?P<pre>[^a-zA-Z0-9_])){}\s+(?P<a1>{})\s+(?P<tail>{}(?:\s+{})*)",
        regex::escape(func_name),
        arg_pattern,
        arg_pattern,
        arg_pattern
    );

    let re = {
        // SAFETY: Mutex poison only happens when a prior holder panicked
        // while mutating the cache, leaving it in an indeterminate state.
        // Recovering into_inner() here is safe because the cache only
        // stores compiled regex objects — no half-written invariants to
        // worry about — and continuing is strictly better than
        // cascade-panicking every subsequent template preprocess call.
        let mut cache = REGEX_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .entry(func_name.to_string())
            .or_insert_with(|| static_regex(&pattern))
            .clone()
    };

    // Regex to split the tail into individual args.
    let split_re = {
        static SPLIT_RE: LazyLock<Regex> = LazyLock::new(|| {
            let arg = format!(
                r"(?:{RAW_STRING_RE_ALT}|\((?:[^()]*(?:\([^()]*\))*[^()]*)\)|[a-zA-Z_][a-zA-Z0-9_.]*|\d+)"
            );
            static_regex(&arg)
        });
        &*SPLIT_RE
    };

    re.replace_all(expr, |caps: &regex::Captures| {
        let pre = caps.name("pre").map_or("", |m| m.as_str());
        let arg1 = caps.name("a1").map_or("", |m| m.as_str());
        let tail = caps.name("tail").map_or("", |m| m.as_str());
        let rest_args: Vec<&str> = split_re.find_iter(tail).map(|m| m.as_str()).collect();

        if rest_args.len() == 1 {
            // Simple binary: eq X Y → X == Y
            format!("{}{} {} {}", pre, arg1, operator, rest_args[0])
        } else {
            // Variadic: eq X Y Z → X == Y or X == Z
            let parts: Vec<String> = rest_args
                .iter()
                .map(|a| format!("{} {} {}", arg1, operator, a))
                .collect();
            format!("{}{}", pre, parts.join(" or "))
        }
    })
    .to_string()
}

/// Rewrite `len X` → `X | length`.
/// X can be a quoted string, identifier/dotted path, or parenthesized expression.
fn rewrite_len(expr: &str) -> String {
    static LEN_RE: LazyLock<Regex> = LazyLock::new(|| {
        let arg_pattern = format!(r"(?:{RAW_STRING_RE_ALT}|\([^()]*\)|[a-zA-Z_][a-zA-Z0-9_.]*)");
        // Use a capture group for the preceding character instead of look-behind.
        let pattern = format!(
            r"(?:^|(?P<pre>[^a-zA-Z0-9_]))len\s+(?P<arg>{})",
            arg_pattern
        );
        static_regex(&pattern)
    });

    LEN_RE
        .replace_all(expr, |caps: &regex::Captures| {
            let pre = caps.name("pre").map_or("", |m| m.as_str());
            let arg = caps.name("arg").map_or("", |m| m.as_str());
            format!("{}{} | length", pre, arg)
        })
        .to_string()
}
