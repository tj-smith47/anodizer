// Template preprocessing: converts Go-style syntax to Tera-native syntax,
// plus tera 1.x compatibility rewrites. The authoritative pass list lives on
// [`preprocess`] below. Representative rewrites:
//   `{{ .Field }}` → `{{ Field }}`
//   `(list .Os "windows")` → (after dot-strip) `[Os, "windows"]`
//   `{{ replace Version "v" "" }}` → `{{ replace(s=Version, old="v", new="") }}`
//   `{{ Version | replace "v" "" }}` → `{{ Version | replace(from="v", to="") }}`
//   `{{ in (list "a" "b") "a" }}` → `{{ in(items=["a", "b"], value="a") }}`
//   `{{ reReplaceAll "v" Tag "" }}` → `{{ reReplaceAll(pattern="v", input=Tag, replacement="") }}`
//   `{{ time "2006-01-02" }}` → `{{ time(format="2006-01-02") }}`
//   `{{ slice Commit 0 7 }}` → `{{ Commit | slice(start=0, end=7) }}`
//   `{{ printf "%04d" Patch }}` → `{{ printf(format="%04d", args=[Patch]) }}`
//   `{{ .Now.Format "2006-01-02" }}` → `{{ Now | now_format(format="2006-01-02") }}`
//   `{{ list.0 }}` → `{{ list[0] }}`

use anyhow::{Result, bail};
use regex::Regex;

mod blocks;
mod builtins;
mod dots_dollars;
mod go_blocks;
mod methods;
mod positional;
mod shell_guard;
pub(crate) mod string_lit;
mod tokens;

#[cfg(test)]
mod tests;

use blocks::live_blocks;
pub(crate) use blocks::{raw_span_at, raw_spans};
use builtins::{preprocess_go_builtins, preprocess_list_subexpr};
use dots_dollars::{preprocess_strip_dots, rewrite_numeric_index_segments};
use go_blocks::{extract_block_parts, preprocess_go_blocks};
use methods::preprocess_method_calls;
use positional::{preprocess_map_syntax, preprocess_positional_syntax};
pub(crate) use shell_guard::{protect_shell_param_length, restore_shell_param_length};
use tokens::{MAX_EXPR_NESTING, ParenImbalance, scan_parens};

/// Compile a regex from a static literal. Panics with a diagnostic if the
/// literal fails to parse. Every call site is a `LazyLock::new(…)` initializer
/// over a hardcoded pattern, so failure is a programmer bug caught the first
/// time the static is touched, not a runtime-path crash. Exists because the project-wide
/// anti-pattern hook forbids bare panicking error helpers in lib code, and
/// `regex::Regex::new` on a hardcoded literal is inherently infallible.
fn static_regex(pattern: &str) -> Regex {
    Regex::new(pattern)
        .unwrap_or_else(|e| panic!("invalid static regex literal `{}`: {}", pattern, e))
}

/// Maximum bytes of a block quoted back in an imbalance diagnostic, so one
/// runaway expression cannot spill an entire template into an error message.
const MAX_BLOCK_SNIPPET: usize = 160;

/// Reject a template whose `{{ … }}` / `{% … %}` blocks carry an expression no
/// pass can make sense of — unbalanced parentheses, or parentheses nested past
/// [`MAX_EXPR_NESTING`] — before any pass tries.
///
/// A Go sub-expression argument (`{{ trimprefix (base Path) "v" }}`) is
/// rewritten by descending into the balanced group. An unclosed group has no
/// meaning in either Go or Tera, and the engine's own parse error points at the
/// wrong token. A runaway nest costs stack and memory per level and kills the
/// process before the engine is reached at all. Catching both here names the
/// real problem.
///
/// Text between `{% raw %}` and `{% endraw %}` is skipped: it is emitted
/// literally, so a template that deliberately quotes broken syntax must keep
/// rendering.
pub(crate) fn check_block_expressions(template: &str) -> Result<()> {
    for block in live_blocks(template) {
        let (_, inner, _) = extract_block_parts(block);
        let scan = scan_parens(inner);
        if let Some(imbalance) = scan.imbalance {
            const PAREN_HINT: &str = "A sub-expression argument must open and close \
                 inside the same block — for example `trimprefix (base Path) \"v\"`.";
            let detail = match imbalance {
                ParenImbalance::Unclosed(1) => format!("1 unclosed `(`. {PAREN_HINT}"),
                ParenImbalance::Unclosed(n) => format!("{n} unclosed `(`. {PAREN_HINT}"),
                ParenImbalance::UnmatchedClose => {
                    format!("a `)` with no matching `(`. {PAREN_HINT}")
                }
                ParenImbalance::UnterminatedString => "an unterminated string literal, so the \
                     rest of the block — including any `)` — reads as string contents. A literal \
                     closes at the next matching quote; a backslash does not escape it."
                    .to_string(),
            };
            bail!(
                "unbalanced expression in template {}: {detail}",
                quote_block(block)
            );
        }
        if scan.max_depth > MAX_EXPR_NESTING {
            bail!(
                "over-nested expression in template {}: parentheses nest {} deep, past the \
                 limit of {MAX_EXPR_NESTING}. Every group is rewritten by descending into it, \
                 so a deeper nest exhausts memory or the stack before the engine sees the \
                 block. Bind an intermediate result to a variable and nest fewer calls.",
                quote_block(block),
                scan.max_depth
            );
        }
    }
    Ok(())
}

/// Quote a block for a diagnostic, truncating on a char boundary so a very long
/// expression (or a multibyte one) can neither flood nor panic the message.
fn quote_block(block: &str) -> String {
    if block.len() <= MAX_BLOCK_SNIPPET {
        return format!("`{block}`");
    }
    let mut end = MAX_BLOCK_SNIPPET;
    while end > 0 && !block.is_char_boundary(end) {
        end -= 1;
    }
    format!("`{}…`", &block[..end])
}

/// Preprocess a template: convert Go-style syntax to Tera-native syntax.
///
/// Text between `{% raw %}` and `{% endraw %}` is exempt from every pass: the
/// engine emits it literally, so it must reach the engine byte-identical. The
/// exemption is structural rather than per-pass — see [`blocks`].
///
/// Pass 0: convert Go template block syntax (`{{ if }}`, `{{ range }}`, `{{ end }}`, etc.)
///         to Tera block syntax (`{% if %}`, `{% for %}`, `{% endif %}`, etc.).
/// Pass 1: strip Go-style leading dots (`{{ .Field }}` → `{{ Field }}`).
/// Pass 2: rewrite Go-style `(list ...)` subexpressions to Tera array literals.
/// Pass 2b: rewrite Go comparison functions (`eq`, `ne`, `gt`, `lt`, `ge`, `le`)
///          to Tera infix operators, `and`/`or` prefix functions to infix, and
///          `len .X` to `X | length`.
/// Pass 2c: rewrite Go-style `map "k1" "v1" ...` variadic positional to
///          `map(pairs=["k1", "v1", ...])` named-arg syntax.
/// Pass 3: convert positional function syntax to named-arg syntax.
/// Pass 4: rewrite Go-style `.Now.Format "..."` method calls to Tera filter syntax.
/// Pass 5: rewrite tera 1.x numeric path segments (`list.0`) to tera 2.0
///         index syntax (`list[0]`).
pub fn preprocess(template: &str) -> String {
    // Pass 0: convert Go block syntax to Tera block syntax.
    let block_converted = preprocess_go_blocks(template);
    // Pass 1: strip Go-style leading dots.
    let dot_stripped = preprocess_strip_dots(&block_converted);
    // Pass 2: rewrite `(list "a" "b")` → `["a", "b"]`.
    let list_rewritten = preprocess_list_subexpr(&dot_stripped);
    // Pass 2b: rewrite Go comparison/logical/len functions.
    let comparison_rewritten = preprocess_go_builtins(&list_rewritten);
    // Pass 2c: rewrite Go-style `map "k1" "v1" ...` to `map(pairs=[...])`.
    let map_rewritten = preprocess_map_syntax(&comparison_rewritten);
    // Pass 3: convert positional function syntax to named-arg syntax.
    let positional_rewritten = preprocess_positional_syntax(&map_rewritten);
    // Pass 4: rewrite `Now.Format "..."` → `Now | now_format(format="...")`.
    let method_rewritten = preprocess_method_calls(&positional_rewritten);
    // Pass 5: rewrite `list.0` → `list[0]` (tera 2.0 dropped `.N` indexing).
    // Must run last: earlier passes lex `list.0` as one dotted-path token.
    rewrite_numeric_index_segments(&method_rewritten)
}
