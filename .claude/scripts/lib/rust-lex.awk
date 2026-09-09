# Shared Rust lexing helpers for the .claude/scripts/audit-*.sh scanners.
# Load with `awk -f .claude/scripts/lib/rust-lex.awk -f - <files> <<'AWK' … AWK`
# (a program given by -f cannot be mixed with inline program text).
#
# `strip_code` carries multi-line raw strings, normal strings and block
# comments across lines in the globals `in_raw`, `raw_hashes`, `in_str` and
# `in_bcomment`; a consumer resets them per file with `reset_lex()`.

function reset_lex() { in_raw = 0; raw_hashes = 0; in_str = 0; in_bcomment = 0 }

# Returns l with string/char literals and comments elided, carrying
# multi-line raw strings, multi-line normal strings and block comments
# across lines in globals. Elision (rather than skipping the line)
# keeps the surrounding real code visible to the brace counter.
function strip_code(l,   out, i, n, c, k, endm, m) {
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
            if (k == 0) return out
            i = i + k + 1; in_bcomment = 0; continue
        }
        c = substr(l, i, 1)
        if (c == "/" && substr(l, i + 1, 1) == "/") return out
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

# Whether l is an attribute whose cfg(...) predicate can hold ONLY under
# `cargo test`: bare `test`, or an `all(...)` one of whose top-level terms is
# itself test-only. An `any(...)` is satisfiable by its other terms and a
# `not(...)` inverts the gate, so neither ever gates test code. The Rust twin
# is crates/core/src/test_helpers/test_sources.rs `is_test_only_cfg`, and
# crates/cli/tests/audit_scripts.rs drives both over one vector.
function is_test_only_cfg(l,   start, i, n, depth, c) {
    if (l !~ /^[[:space:]]*#\[cfg\(/) return 0
    start = index(l, "#[cfg(") + 6
    depth = 1; n = length(l)
    for (i = start; i <= n; i++) {
        c = substr(l, i, 1)
        if (c == "(") depth++
        else if (c == ")") {
            depth--
            if (depth == 0) return pred_is_test_only(substr(l, start, i - start))
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
