# Shared Rust lexing helpers for the .claude/scripts/audit-*.sh scanners.
# Load with `awk -f .claude/scripts/lib/rust-lex.awk -f - <files> <<'AWK' … AWK`
# (a program given by -f cannot be mixed with inline program text).
#
# One pass over a line yields BOTH halves of it — `strip_code`, the code with
# literals and comments elided, and `comment_part`, the comment text that
# elision hides — and the result is memoised per input record: re-running the
# state machine over a line whose entry state has already been consumed reads
# the closing `"` of a multi-line string as an opening one.
#
# The pass carries multi-line raw strings, normal strings and block comments
# across lines in the globals `in_raw`, `raw_hashes`, `in_str` and
# `in_bcomment`; a consumer resets them per file with `reset_lex()`.

function reset_lex() { in_raw = 0; raw_hashes = 0; in_str = 0; in_bcomment = 0 }

# Returns l with string/char literals and comments elided. Elision (rather
# than skipping the line) keeps the surrounding real code visible to the brace
# counter.
function strip_code(l) { lex_scan(l); return lex_code }

# Returns the comment half of l: the `//` tail that opens outside a string
# literal (the slashes included) plus the body of any block comment on the
# line. An audit marker spelled inside a string literal lands in the CODE
# half, so reading a marker from here is what stops a literal from forging it.
function comment_part(l) { lex_scan(l); return lex_comment }

function lex_scan(l) {
    if (lex_nr == NR && lex_text == (l "")) return
    lex_nr = NR; lex_text = l ""; lex_comment = ""
    lex_code = lex_halves(l)
}

function lex_halves(l,   out, i, n, c, k, endm, m) {
    out = ""; i = 1; n = length(l)
    while (i <= n) {
        if (in_raw) {
            endm = "\""
            for (k = 0; k < raw_hashes; k++) endm = endm "#"
            k = index(substr(l, i), endm)
            if (k == 0) return out
            i = i + k - 1 + length(endm); in_raw = 0; continue
        }
        if (in_str) {
            while (i <= n) {
                c = substr(l, i, 1)
                if (c == "\\") { i += 2; continue }
                if (c == "\"") { i++; in_str = 0; break }
                i++
            }
            if (in_str) return out
            continue
        }
        if (in_bcomment) {
            k = index(substr(l, i), "*/")
            if (k == 0) { lex_comment = lex_comment substr(l, i); return out }
            lex_comment = lex_comment substr(l, i, k - 1)
            i = i + k + 1; in_bcomment = 0; continue
        }
        c = substr(l, i, 1)
        if (c == "/" && substr(l, i + 1, 1) == "/") { lex_comment = lex_comment substr(l, i); return out }
        if (c == "/" && substr(l, i + 1, 1) == "*") { in_bcomment = 1; i += 2; continue }
        if (c == "r") {
            m = 0
            while (substr(l, i + 1 + m, 1) == "#") m++
            if (substr(l, i + 1 + m, 1) == "\"") {
                in_raw = 1; raw_hashes = m; i = i + m + 2; continue
            }
        }
        if (c == "\"") { in_str = 1; i++; continue }
        # A char literal is elided; a lifetime (no closing quote two
        # characters on) is left alone so it cannot swallow real code.
        if (c == "'") {
            if (substr(l, i + 1, 1) == "\\") {
                k = index(substr(l, i + 2), "'")
                if (k > 0) { i = i + 2 + k; continue }
            } else if (substr(l, i + 2, 1) == "'") { i += 3; continue }
        }
        out = out c
        i++
    }
    return out
}

function count_char(s, ch,   i, n, t) {
    t = 0; n = length(s)
    for (i = 1; i <= n; i++) if (substr(s, i, 1) == ch) t++
    return t
}

# Whether l is an OUTER attribute (`#[cfg(...)]`, gating the item below it)
# whose predicate can hold ONLY under `cargo test`: bare `test`, or an
# `all(...)` one of whose top-level terms is itself test-only. An `any(...)` is
# satisfiable by its other terms and a `not(...)` inverts the gate, so neither
# ever gates test code. The Rust twin is
# crates/core/src/test_helpers/test_sources.rs `is_test_only_cfg`, and
# crates/cli/tests/audit_scripts.rs drives both over one vector.
function is_test_only_cfg(l) { return cfg_attr_is_test_only(l, "#[cfg(") }

# Whether l is an INNER attribute (`#![cfg(...)]`, gating the whole enclosing
# file) that is test-only by the same predicate rules. Kept apart from
# is_test_only_cfg because the two answer different questions: an inner
# attribute bounds no item, so a region scanner must not treat it as the head
# of one.
function is_test_only_inner_cfg(l) { return cfg_attr_is_test_only(l, "#![cfg(") }

# Shared body of the two above: tok is the attribute opening that must start
# the line (leading whitespace aside), and the predicate it delimits decides.
function cfg_attr_is_test_only(l, tok,   s, start, i, n, depth, c) {
    s = l
    sub(/^[[:space:]]+/, "", s)
    if (substr(s, 1, length(tok)) != tok) return 0
    start = length(tok) + 1
    depth = 1; n = length(s)
    for (i = start; i <= n; i++) {
        c = substr(s, i, 1)
        if (c == "(") depth++
        else if (c == ")") {
            depth--
            if (depth == 0) return pred_is_test_only(substr(s, start, i - start))
        }
    }
    return 0
}

# Whether one cfg predicate term is test-only; see is_test_only_cfg.
function pred_is_test_only(p,   n, i, depth, c, end_at, inner, start) {
    sub(/^[[:space:]]+/, "", p); sub(/[[:space:]]+$/, "", p)
    if (p == "test") return 1
    if (substr(p, 1, 4) != "all(") return 0
    depth = 1; n = length(p); end_at = 0
    for (i = 5; i <= n; i++) {
        c = substr(p, i, 1)
        if (c == "(") depth++
        else if (c == ")") { depth--; if (depth == 0) { end_at = i; break } }
    }
    # Text after the closing paren means `all(...)` was not the whole term.
    if (end_at != n) return 0
    inner = substr(p, 5, n - 5)
    depth = 0; start = 1; n = length(inner)
    for (i = 1; i <= n; i++) {
        c = substr(inner, i, 1)
        if (c == "(") depth++
        else if (c == ")") depth--
        else if (c == "," && depth == 0) {
            if (pred_is_test_only(substr(inner, start, i - start))) return 1
            start = i + 1
        }
    }
    return pred_is_test_only(substr(inner, start, n - start + 1))
}
