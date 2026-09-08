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

# A cfg predicate is test-only when it cannot hold outside `cargo test`:
# bare `test`, or an `all(…)` one of whose terms is `test`. An `any(…)`
# is satisfiable by its other terms, so it is production.
function is_test_only_cfg(l) {
    if (l !~ /^[[:space:]]*#\[cfg\(/) return 0
    if (l ~ /\(any\(/) return 0
    return (l ~ /(^|[(,[:space:]])test([),]|$)/)
}
