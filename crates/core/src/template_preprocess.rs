// Template preprocessing: converts Go-style syntax to Tera-native syntax.
//
// Pass 1 (`preprocess_strip_dots`): strips leading dots from `{{ .Field }}` → `{{ Field }}`.
// Pass 2 (`preprocess_list_subexpr`): rewrites `(list ...)` subexpressions to Tera array literals:
//   `(list "a" "b" "c")` → `["a", "b", "c"]`
//   `(list .Os "windows")` → (after dot-strip) `[Os, "windows"]`
// Pass 3 (`preprocess_positional_syntax`): converts positional function calls to named-arg syntax
//   for `replace`, `split`, `contains`, `in`, and `reReplaceAll`:
//   `{{ replace Version "v" "" }}` → `{{ replace(s=Version, old="v", new="") }}`
//   `{{ Version | replace "v" "" }}` → `{{ Version | replace(from="v", to="") }}`
//   `{{ in (list "a" "b") "a" }}` → `{{ in(items=["a", "b"], value="a") }}`
//   `{{ reReplaceAll "v" Tag "" }}` → `{{ reReplaceAll(pattern="v", input=Tag, replacement="") }}`

use regex::Regex;
use std::borrow::Cow;
use std::sync::LazyLock;

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

// ---------------------------------------------------------------------------
// Pass 0: Go template block syntax → Tera block syntax
// ---------------------------------------------------------------------------

/// Regexes for matching Go template block constructs.
///
/// These match `{{ if ... }}`, `{{ else }}`, `{{ else if ... }}`, `{{ end }}`,
/// `{{ range ... }}`, `{{ with ... }}`, and `{{ $var := ... }}` patterns.
/// Whitespace trimming markers (`-`) are preserved.
static GO_IF_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"^\{\{(-?)\s*if\s+(.+?)\s*(-?)\}\}"));
static GO_ELSE_IF_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"^\{\{(-?)\s*else\s+if\s+(.+?)\s*(-?)\}\}"));
static GO_ELSE_RE: LazyLock<Regex> = LazyLock::new(|| static_regex(r"^\{\{(-?)\s*else\s*(-?)\}\}"));
static GO_END_RE: LazyLock<Regex> = LazyLock::new(|| static_regex(r"^\{\{(-?)\s*end\s*(-?)\}\}"));
static GO_RANGE_KV_RE: LazyLock<Regex> = LazyLock::new(|| {
    // {{ range $k, $v := .Map }}
    static_regex(r"^\{\{(-?)\s*range\s+\$(\w+)\s*,\s*\$(\w+)\s*:=\s*(.+?)\s*(-?)\}\}")
});
static GO_RANGE_V_RE: LazyLock<Regex> = LazyLock::new(|| {
    // {{ range $v := .Slice }} or {{ range .Slice }}
    static_regex(r"^\{\{(-?)\s*range\s+(?:\$(\w+)\s*:=\s*)?(.+?)\s*(-?)\}\}")
});
static GO_WITH_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"^\{\{(-?)\s*with\s+(.+?)\s*(-?)\}\}"));
static GO_VAR_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| {
    // {{ $var := expr }}
    static_regex(r"^\{\{(-?)\s*\$(\w+)\s*:=\s*(.+?)\s*(-?)\}\}")
});
/// Match `{{ . }}` (bare dot reference to current context).
static GO_DOT_RE: LazyLock<Regex> = LazyLock::new(|| static_regex(r"^\{\{(-?)\s*\.\s*(-?)\}\}"));

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
fn preprocess_go_blocks(template: &str) -> String {
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
                result.push_str(&tera_block(&cap[1], &format!("if {}", &field), &cap[3]));
                block_stack.push(("with", Some(field)));
                pos += full.len();
                continue;
            }
        }

        // No match at this position — copy one byte and advance.
        result.push(bytes[pos] as char);
        pos += 1;
    }

    // Post-pass: strip `$` prefix from Go variable references inside template blocks.
    // Go templates use `$var` for loop/assignment variables; Tera uses plain `var`.
    // Must NOT strip `$` inside quoted strings (e.g., regex `$1` replacements).
    strip_dollar_vars(&result)
}

/// Strip `$` prefix from Go variable references inside `{{ }}` and `{% %}` blocks.
///
/// Scans each block character by character, skipping quoted strings, and removes
/// `$` when followed by a word character (e.g., `$var` → `var`).
fn strip_dollar_vars(template: &str) -> String {
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
                            result.push(bytes[i] as char);
                            result.push(bytes[i + 1] as char);
                            i += 2;
                        } else {
                            result.push(bytes[i] as char);
                            i += 1;
                        }
                    }
                    if i < bytes.len() {
                        result.push(bytes[i] as char);
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

                result.push(bytes[i] as char);
                i += 1;
            }

            result
        })
        .to_string()
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
/// Captures the inner arguments (variadic args to `list`).
/// Each item independently matches:
/// - Double-quoted strings with escaped-quote support: `"hello \"world\""`
/// - Single-quoted strings with escaped-quote support: `'it\'s'`
/// - Bare identifiers (variable references): `Os`, `Env.FOO`, `Version`
// SAFETY: Built from deterministic string literals; the resulting pattern is known to be valid.
static LIST_SUBEXPR_RE: LazyLock<Regex> = LazyLock::new(|| {
    // A single item: quoted string OR bare identifier (dotted paths like Env.FOO allowed).
    let item = r#"(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[a-zA-Z_][a-zA-Z0-9_.]*)"#;
    let pattern = format!(r"\(list\s+({item}(?:\s+{item})*)\)");
    static_regex(&pattern)
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
                    // Split items (quoted strings or bare identifiers) and rejoin as a Tera array literal.
                    // Bare identifiers pass through as variable references: `[Os, "windows"]`.
                    static ITEM_RE: LazyLock<Regex> = LazyLock::new(|| {
                        static_regex(
                            r#""(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[a-zA-Z_][a-zA-Z0-9_.]*"#,
                        )
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
fn preprocess_go_builtins(template: &str) -> String {
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

    // Phase 1: Rewrite `and`/`or` with parenthesized args.
    // Pattern: `and/or (EXPR1) (EXPR2)` where EXPR can contain `eq`/`ne`/etc.
    // We process the inner expressions first, then combine with the logical op.
    result = rewrite_logical_with_paren_args(&result);

    // Phase 2: Rewrite `not (COMPARISON_FUNC X Y)` → `not X OP Y`
    result = rewrite_not_with_paren_comparison(&result);

    // Phase 3: Rewrite top-level comparison functions: `eq X Y` → `X == Y`
    for (func_name, operator) in COMPARISON_OPS {
        result = rewrite_prefix_to_infix(&result, func_name, operator);
    }

    // Phase 4: Rewrite top-level logical functions: `and X Y` → `X and Y`
    result = rewrite_prefix_to_infix(&result, "and", "and");
    result = rewrite_prefix_to_infix(&result, "or", "or");

    // Phase 5: Rewrite `len X` → `X | length`
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

    let arg_pattern = r#"(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\((?:[^()]*(?:\([^()]*\))*[^()]*)\)|[a-zA-Z_][a-zA-Z0-9_.]*|\d+)"#;

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
            let arg = r#"(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\((?:[^()]*(?:\([^()]*\))*[^()]*)\)|[a-zA-Z_][a-zA-Z0-9_.]*|\d+)"#;
            static_regex(arg)
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
        let arg_pattern =
            r#"(?:"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|\([^()]*\)|[a-zA-Z_][a-zA-Z0-9_.]*)"#;
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
fn preprocess_map_syntax(template: &str) -> String {
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

/// Regex matching `Now.Format` with a quoted format argument inside `{{ }}` blocks.
/// Captures: (1) the format string including quotes.
/// After Pass 1 (dot stripping), `{{ .Now.Format "2006-01-02" }}` becomes
/// `{{ Now.Format "2006-01-02" }}`. This regex rewrites it to
/// `{{ Now | now_format(format="2006-01-02") }}`.
static NOW_FORMAT_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r#"Now\.Format\s+("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')"#));

/// Pass 4: Rewrite Go-style method calls to Tera filter syntax.
///
/// Currently handles:
/// - `Now.Format "2006-01-02"` → `Now | now_format(format="2006-01-02")`
///
/// This runs after all other passes so that dot-stripping and positional
/// syntax rewrites have already been applied.
fn preprocess_method_calls(template: &str) -> String {
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
        // Loop condition `i < inner.len()` guarantees `inner[i..]` is non-empty
        // so `chars().next()` always yields Some(_); the `break` is a
        // defensive no-op that keeps the function panic-free.
        let Some(ch) = inner[i..].chars().next() else {
            break;
        };
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
        assert_eq!(result, "{{ Version | replace(from=\"v\", to=\"\") }}");
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
        assert_eq!(result, "{{- replace(s=Version, old=\"v\", new=\"\") -}}");
    }

    #[test]
    fn test_positional_replace_whitespace_control_left_only() {
        let input = "{{- replace Version \"v\" \"\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{- replace(s=Version, old=\"v\", new=\"\") }}");
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
        assert_eq!(result, "{{ in(items=[\"a\", \"b\", \"c\"], value=\"b\") }}");
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
        assert_eq!(result, "{{ in(items=['it\\'s', 'fine'], value=\"fine\") }}");
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

    // --- Finding 5: `(list ...)` with bare identifiers (variable references) ---

    #[test]
    fn test_preprocess_list_subexpr_with_bare_identifier() {
        // (list .Os "windows") → after dot-strip: (list Os "windows") → [Os, "windows"]
        let input = "{{ in (list .Os \"windows\") \"linux\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ in(items=[Os, \"windows\"], value=\"linux\") }}");
    }

    #[test]
    fn test_preprocess_list_subexpr_with_dotted_path() {
        // (list .Env.FOO "fallback") → after dot-strip: (list Env.FOO "fallback") → [Env.FOO, "fallback"]
        let input = "{{ in (list .Env.FOO \"fallback\") \"val\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ in(items=[Env.FOO, \"fallback\"], value=\"val\") }}"
        );
    }

    #[test]
    fn test_preprocess_list_subexpr_all_bare_identifiers() {
        // (list .Os .Arch) → after dot-strip: (list Os Arch) → [Os, Arch]
        let input = "{{ in (list .Os .Arch) \"linux\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ in(items=[Os, Arch], value=\"linux\") }}");
    }

    #[test]
    fn test_preprocess_list_subexpr_mixed_vars_and_strings() {
        // (list .Os "windows" .Arch) → after dot-strip: (list Os "windows" Arch) → [Os, "windows", Arch]
        let input = "{{ in (list .Os \"windows\" .Arch) \"test\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ in(items=[Os, \"windows\", Arch], value=\"test\") }}"
        );
    }

    // --- Now.Format method call rewrite tests ---

    #[test]
    fn test_preprocess_now_format_go_style() {
        // {{ .Now.Format "2006-01-02" }} → {{ Now | now_format(format="2006-01-02") }}
        let input = "{{ .Now.Format \"2006-01-02\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Now | now_format(format=\"2006-01-02\") }}");
    }

    #[test]
    fn test_preprocess_now_format_no_dot_prefix() {
        // {{ Now.Format "2006-01-02" }} (without leading dot) should also work
        let input = "{{ Now.Format \"2006-01-02\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Now | now_format(format=\"2006-01-02\") }}");
    }

    #[test]
    fn test_preprocess_now_format_with_time_pattern() {
        // {{ .Now.Format "2006-01-02 15:04:05" }}
        let input = "{{ .Now.Format \"2006-01-02 15:04:05\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ Now | now_format(format=\"2006-01-02 15:04:05\") }}"
        );
    }

    #[test]
    fn test_preprocess_now_format_single_quotes() {
        // {{ .Now.Format '2006-01-02' }} (single quotes)
        let input = "{{ .Now.Format '2006-01-02' }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Now | now_format(format='2006-01-02') }}");
    }

    #[test]
    fn test_preprocess_now_format_whitespace_control() {
        // {{- .Now.Format "2006-01-02" -}}
        let input = "{{- .Now.Format \"2006-01-02\" -}}";
        let result = preprocess(input);
        assert_eq!(result, "{{- Now | now_format(format=\"2006-01-02\") -}}");
    }

    #[test]
    fn test_preprocess_now_format_compact() {
        // {{.Now.Format "2006-01-02"}} (no spaces after {{ or before }})
        let input = "{{.Now.Format \"2006-01-02\"}}";
        let result = preprocess(input);
        assert_eq!(result, "{{Now | now_format(format=\"2006-01-02\")}}");
    }

    #[test]
    fn test_preprocess_now_format_does_not_affect_other_blocks() {
        // Other blocks should not be affected
        let input = "{{ Version }} - {{ .Now.Format \"2006-01-02\" }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{{ Version }} - {{ Now | now_format(format=\"2006-01-02\") }}"
        );
    }

    // -----------------------------------------------------------------------
    // Pass 0: Go block syntax tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_go_if_end() {
        let input = "{{ if .IsSnapshot }}pre{{ end }}";
        let result = preprocess(input);
        assert_eq!(result, "{% if IsSnapshot %}pre{% endif %}");
    }

    #[test]
    fn test_go_if_else_end() {
        let input = "{{ if .IsSnapshot }}pre{{ else }}stable{{ end }}";
        let result = preprocess(input);
        assert_eq!(result, "{% if IsSnapshot %}pre{% else %}stable{% endif %}");
    }

    #[test]
    fn test_go_if_else_if_end() {
        let input = "{{ if eq .Os \"windows\" }}win{{ else if eq .Os \"darwin\" }}mac{{ else }}linux{{ end }}";
        let result = preprocess(input);
        // `eq Os "windows"` is rewritten to `Os == "windows"` by Pass 2b
        assert_eq!(
            result,
            "{% if Os == \"windows\" %}win{% elif Os == \"darwin\" %}mac{% else %}linux{% endif %}"
        );
    }

    #[test]
    fn test_go_range_bare() {
        let input = "{{ range .Maintainers }}# {{ . }}{{ end }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% for val in Maintainers %}# {{ val }}{% endfor %}"
        );
    }

    #[test]
    fn test_go_range_with_variable() {
        let input = "{{ range $release := .Packages }}{{ $release.Name }}{{ end }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% for release in Packages %}{{ release.Name }}{% endfor %}"
        );
    }

    #[test]
    fn test_go_range_kv() {
        let input = "{{ range $key, $value := .Checksums }}{{ $value }} {{ $key }}{{ end }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% for key, value in Checksums %}{{ value }} {{ key }}{% endfor %}"
        );
    }

    #[test]
    fn test_go_with() {
        let input = "{{ with .Arm }}v{{ . }}{{ end }}";
        let result = preprocess(input);
        // `with` becomes `if`, `{{ . }}` rewrites to the with argument
        assert_eq!(result, "{% if Arm %}v{{ Arm }}{% endif %}");
    }

    #[test]
    fn test_go_var_assignment() {
        let input = "{{ $m := map \"a\" \"1\" }}{{ index $m \"a\" }}";
        let result = preprocess(input);
        // Pass 2c rewrites `map "a" "1"` to `map(pairs=["a", "1"])`
        // Pass 3 rewrites `index m "a"` to `index(collection=m, key="a")`
        assert_eq!(
            result,
            "{% set m = map(pairs=[\"a\", \"1\"]) %}{{ index(collection=m, key=\"a\") }}"
        );
    }

    #[test]
    fn test_go_whitespace_trim() {
        let input = "{{- if .Cond -}}yes{{- end -}}";
        let result = preprocess(input);
        assert_eq!(result, "{%- if Cond -%}yes{%- endif -%}");
    }

    #[test]
    fn test_go_nested_if_range() {
        let input = "{{ range .Items }}{{ if .Active }}*{{ end }}{{ end }}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% for val in Items %}{% if Active %}*{% endif %}{% endfor %}"
        );
    }

    #[test]
    fn test_go_blocks_plain_expressions_unchanged() {
        // Plain Go expressions (no block keywords) should pass through
        let input = "{{ .ProjectName }}_{{ .Version }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ ProjectName }}_{{ Version }}");
    }

    #[test]
    fn test_go_complex_nfpm_template() {
        // Real-world GoReleaser template: nfpm default name_template
        let input = "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}{{ with .Arm }}v{{ . }}{{ end }}{{ if not (eq .Amd64 \"v1\") }}{{ .Amd64 }}{{ end }}";
        let result = preprocess(input);
        // `(eq Amd64 "v1")` is rewritten to `Amd64 == "v1"` by Pass 2b
        // Parens are stripped because Tera doesn't support comparisons inside parens.
        assert_eq!(
            result,
            "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if not Amd64 == \"v1\" %}{{ Amd64 }}{% endif %}"
        );
    }

    // -----------------------------------------------------------------------
    // Pass 2b: comparison functions (eq/ne/gt/lt/ge/le), and/or, len
    // -----------------------------------------------------------------------

    #[test]
    fn test_eq_in_if_block() {
        let input = "{% if eq Os \"windows\" %}win{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Os == \"windows\" %}win{% endif %}");
    }

    #[test]
    fn test_eq_variadic_three_args() {
        // Go's eq is variadic: eq X Y Z means X == Y || X == Z
        let input = r#"{% if eq Os "linux" "darwin" %}unix{% endif %}"#;
        let result = preprocess(input);
        assert_eq!(
            result,
            r#"{% if Os == "linux" or Os == "darwin" %}unix{% endif %}"#
        );
    }

    #[test]
    fn test_eq_variadic_four_args() {
        let input = r#"{% if eq Arch "amd64" "arm64" "386" %}supported{% endif %}"#;
        let result = preprocess(input);
        assert_eq!(
            result,
            r#"{% if Arch == "amd64" or Arch == "arm64" or Arch == "386" %}supported{% endif %}"#
        );
    }

    #[test]
    fn test_ne_in_if_block() {
        let input = "{% if ne Os \"windows\" %}not-win{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Os != \"windows\" %}not-win{% endif %}");
    }

    #[test]
    fn test_gt_in_if_block() {
        let input = "{% if gt Major 1 %}gt1{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Major > 1 %}gt1{% endif %}");
    }

    #[test]
    fn test_lt_in_if_block() {
        let input = "{% if lt Minor 5 %}lt5{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Minor < 5 %}lt5{% endif %}");
    }

    #[test]
    fn test_ge_in_if_block() {
        let input = "{% if ge Patch 3 %}ge3{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Patch >= 3 %}ge3{% endif %}");
    }

    #[test]
    fn test_le_in_if_block() {
        let input = "{% if le Patch 3 %}le3{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Patch <= 3 %}le3{% endif %}");
    }

    #[test]
    fn test_eq_with_string_literal() {
        let input = "{% if eq Arch \"amd64\" %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Arch == \"amd64\" %}yes{% endif %}");
    }

    #[test]
    fn test_eq_with_numeric_literal() {
        let input = "{% if eq Major 1 %}v1{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Major == 1 %}v1{% endif %}");
    }

    #[test]
    fn test_eq_parenthesized_not() {
        // not (eq .Os "windows") → not Os == "windows"
        // Tera doesn't support comparison operators inside parens, so parens are stripped.
        let input = "{% if not (eq Os \"windows\") %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if not Os == \"windows\" %}yes{% endif %}");
    }

    #[test]
    fn test_eq_in_elif_block() {
        let input = "{% if eq Os \"linux\" %}lin{% elif eq Os \"darwin\" %}mac{% endif %}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% if Os == \"linux\" %}lin{% elif Os == \"darwin\" %}mac{% endif %}"
        );
    }

    #[test]
    fn test_eq_in_expression_block() {
        // eq can also appear in {{ }} expression blocks
        let input = "{{ eq Os \"linux\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Os == \"linux\" }}");
    }

    #[test]
    fn test_eq_with_already_stripped_dot_var() {
        // After dot stripping: eq Os "windows"
        let input = "{% if eq Os \"windows\" %}win{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Os == \"windows\" %}win{% endif %}");
    }

    #[test]
    fn test_eq_with_dotted_path() {
        // eq Env.FOO "bar"
        let input = "{% if eq Env.FOO \"bar\" %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Env.FOO == \"bar\" %}yes{% endif %}");
    }

    // --- and/or prefix to infix ---

    #[test]
    fn test_and_prefix_to_infix() {
        let input = "{% if and A B %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if A and B %}yes{% endif %}");
    }

    #[test]
    fn test_or_prefix_to_infix() {
        let input = "{% if or A B %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if A or B %}yes{% endif %}");
    }

    #[test]
    fn test_and_with_parenthesized_or() {
        // and .A (or .B .C) → A and (B or C)
        let input = "{% if and A (or B C) %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if A and (B or C) %}yes{% endif %}");
    }

    #[test]
    fn test_or_with_parenthesized_eq() {
        // or (eq Os "linux") (eq Os "darwin") → Os == "linux" or Os == "darwin"
        // Tera doesn't support comparisons inside parens, so all parens are stripped.
        let input = "{% if or (eq Os \"linux\") (eq Os \"darwin\") %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% if Os == \"linux\" or Os == \"darwin\" %}yes{% endif %}"
        );
    }

    // --- len function ---

    #[test]
    fn test_len_in_expression() {
        let input = "{{ len Items }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Items | length }}");
    }

    #[test]
    fn test_len_in_if_block() {
        let input = "{% if len Items %}has items{% endif %}";
        let result = preprocess(input);
        assert_eq!(result, "{% if Items | length %}has items{% endif %}");
    }

    #[test]
    fn test_len_with_dotted_path() {
        let input = "{{ len Env.PATH }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Env.PATH | length }}");
    }

    #[test]
    fn test_len_does_not_match_partial_word() {
        // "length" should not be rewritten
        let input = "{{ Items | length }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ Items | length }}");
    }

    // --- map positional syntax ---

    #[test]
    fn test_map_positional_two_args() {
        let input = "{{ map \"a\" \"1\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ map(pairs=[\"a\", \"1\"]) }}");
    }

    #[test]
    fn test_map_positional_four_args() {
        let input = "{{ map \"a\" \"1\" \"b\" \"2\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ map(pairs=[\"a\", \"1\", \"b\", \"2\"]) }}");
    }

    #[test]
    fn test_map_named_args_unchanged() {
        let input = "{{ map(pairs=[\"a\", \"1\"]) }}";
        let result = preprocess(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_map_in_set_block() {
        let input = "{% set m = map \"x\" \"y\" %}";
        let result = preprocess(input);
        assert_eq!(result, "{% set m = map(pairs=[\"x\", \"y\"]) %}");
    }

    // --- index positional syntax ---

    #[test]
    fn test_index_positional_two_args() {
        let input = "{{ index myMap \"key\" }}";
        let result = preprocess(input);
        assert_eq!(result, "{{ index(collection=myMap, key=\"key\") }}");
    }

    #[test]
    fn test_index_named_args_unchanged() {
        let input = "{{ index(collection=myMap, key=\"key\") }}";
        let result = preprocess(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_index_in_control_block() {
        let input = "{% if index myMap \"key\" %}yes{% endif %}";
        let result = preprocess(input);
        assert_eq!(
            result,
            "{% if index(collection=myMap, key=\"key\") %}yes{% endif %}"
        );
    }

    // --- Combined pass tests ---

    #[test]
    fn test_go_style_full_pipeline_eq_and_map() {
        // Full Go-style pipeline:
        // {{ $m := map "a" "1" }}{{ if eq (index $m "a") "1" }}yes{{ end }}
        let input = "{{ $m := map \"a\" \"1\" }}{{ if eq (index $m \"a\") \"1\" }}yes{{ end }}";
        let result = preprocess(input);
        // Pass 2b rewrites `eq (index m "a") "1"` to `(index m "a") == "1"`.
        // Parens around `index m "a"` are kept (no comparison operator inside).
        // Pass 2c rewrites `map "a" "1"` to `map(pairs=["a", "1"])`.
        // Note: `index m "a"` inside parens is NOT rewritten by Pass 3
        // (positional rewriter only handles top-level standalone/piped forms).
        assert_eq!(
            result,
            "{% set m = map(pairs=[\"a\", \"1\"]) %}{% if (index m \"a\") == \"1\" %}yes{% endif %}"
        );
    }
}
