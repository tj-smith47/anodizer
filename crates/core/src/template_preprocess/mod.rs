// Template preprocessing: converts Go-style syntax to Tera-native syntax.
//
// Pass 1 (`preprocess_strip_dots`): strips leading dots from `{{ .Field }}` → `{{ Field }}`.
// Pass 2 (`preprocess_list_subexpr`): rewrites `(list ...)` subexpressions to Tera array literals:
//   `(list "a" "b" "c")` → `["a", "b", "c"]`
//   `(list .Os "windows")` → (after dot-strip) `[Os, "windows"]`
// Pass 3 (`preprocess_positional_syntax`): converts positional function calls to named-arg syntax
//   for `replace`, `split`, `contains`, `in`, `reReplaceAll`, `time`, `slice`,
//   `printf`, `print`, and `println`:
//   `{{ replace Version "v" "" }}` → `{{ replace(s=Version, old="v", new="") }}`
//   `{{ Version | replace "v" "" }}` → `{{ Version | replace(from="v", to="") }}`
//   `{{ in (list "a" "b") "a" }}` → `{{ in(items=["a", "b"], value="a") }}`
//   `{{ reReplaceAll "v" Tag "" }}` → `{{ reReplaceAll(pattern="v", input=Tag, replacement="") }}`
//   `{{ time "2006-01-02" }}` → `{{ time(format="2006-01-02") }}`
//   `{{ slice Commit 0 7 }}` → `{{ Commit | slice(start=0, end=7) }}`
//   `{{ printf "%04d" Patch }}` → `{{ printf(format="%04d", args=[Patch]) }}`

use regex::Regex;
use std::sync::LazyLock;

mod builtins;
mod dots_dollars;
mod go_blocks;
mod methods;
mod positional;
mod shell_guard;
mod tokens;

#[cfg(test)]
mod tests;

use builtins::{preprocess_go_builtins, preprocess_list_subexpr};
use dots_dollars::preprocess_strip_dots;
use go_blocks::preprocess_go_blocks;
use methods::preprocess_method_calls;
use positional::{preprocess_map_syntax, preprocess_positional_syntax};
pub(crate) use shell_guard::{protect_shell_param_length, restore_shell_param_length};

/// Compile a regex from a static literal. Panics with a diagnostic if the
/// literal fails to parse — only called from `LazyLock::new(…)` initializers,
/// so failure is a programmer bug caught the first time the static is
/// touched, not a runtime-path crash. Exists because the project-wide
/// anti-pattern hook forbids bare panicking error helpers in lib code, and
/// `regex::Regex::new` on a hardcoded literal is inherently infallible.
fn static_regex(pattern: &str) -> Regex {
    Regex::new(pattern)
        .unwrap_or_else(|e| panic!("invalid static regex literal `{}`: {}", pattern, e))
}

/// Regex to match `{{ ... }}` and `{% ... %}` blocks for Go-style preprocessing.
static GO_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| static_regex(r"\{\{.*?\}\}|\{%.*?%\}"));

/// Preprocess a template: convert Go-style syntax to Tera-native syntax.
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
    preprocess_method_calls(&positional_rewritten)
}
