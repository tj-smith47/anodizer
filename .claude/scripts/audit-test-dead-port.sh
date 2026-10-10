#!/usr/bin/env bash
# Guard: no test finds a "nothing listens here" address by binding a listener
# and letting it go.
#
#   let listener = TcpListener::bind("127.0.0.1:0").unwrap();
#   let addr = listener.local_addr().unwrap();
#   drop(listener);                      // or the end of a `{ … }` block
#
# The port returns to the ephemeral pool the moment the listener closes, and
# every HTTP responder in the suite binds `127.0.0.1:0` — so a test running
# beside this one can be handed the same port before the connect happens. The
# connect then reaches a stranger's server instead of being refused
# (`chocolatey_burn_probe_unreachable_gallery_is_an_error` failed 1 run in 15
# that way).
#
# The one address for a refused connect is
# `anodizer_core::test_helpers::refusing_addr::refusing_addr()` (127.0.0.1:1).
#
# A finding is a function in test code that reads `.local_addr()` off the bind
# expression itself (a temporary, closed at the end of the statement, whatever
# chain of `.unwrap()`, `?`, `.await` or `.and_then(|l| l.local_addr())` sits
# between), or that binds a `TcpListener` to a variable and then never KEEPS
# it. A variable is kept when it accepts (`.accept(`, `.incoming(`,
# `.poll_accept(`), is converted (`.into_…(`, `.try_into(`), or is handed on —
# as a call argument, a struct field, a tuple element, a return value, a
# capture of a closure that does one of these. `.local_addr(`,
# `.set_nonblocking(` and the other setters, `drop(name)` and `let _ = name`
# are not keeping it. A second `let` of the same name ends the first
# listener's life where it stands.
#
# One shape is NOT the race and is left alone: a listener bound at the
# function's top level, never dropped, never rebound, in a function that
# returns nothing — a server that never answers, alive to the end of the test.
# Every spelling of the class comes down to the rest: an explicit `drop`, a
# block that ends, a helper returning the port.
#
# `--count` prints the listener binds the scan read, one line:
#   named=<let-bound> chained=<address read off the temporary> other=<the rest>
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"

MODE="scan"
if [[ "${1:-}" == "--count" ]]; then
    MODE="count"
    shift
fi

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

collect_files FILES -rlE --include='*.rs' --exclude-dir=target \
    -- 'TcpListener::bind' crates/*/src crates/*/tests

# Emits `B` per listener bind read in test code and
# `V <file>:<line>: fn <name> …` per finding.
records=""
if ((${#FILES[@]})); then
    run_scanner records \
        -f "$LIB_DIR/rust-lex.awk" -f "$LIB_DIR/test-regions.awk" -f - "${FILES[@]}" <<'AWK'
        # Occurrences of `word` in `text` as a whole identifier.
        function count_word(text, word,   n, pos, before, after, rest, base) {
            n = 0; rest = text; base = 0
            while ((pos = index(rest, word)) > 0) {
                before = (base + pos == 1) ? "" : substr(text, base + pos - 1, 1)
                after = substr(text, base + pos + length(word), 1)
                if (before !~ /[A-Za-z0-9_]/ && after !~ /[A-Za-z0-9_]/) n++
                base += pos + length(word) - 1
                rest = substr(text, base + 1)
            }
            return n
        }

        function count_text(text, needle,   n, pos, rest) {
            n = 0; rest = text
            while ((pos = index(rest, needle)) > 0) {
                n++
                rest = substr(rest, pos + length(needle))
            }
            return n
        }

        # The end of the balanced `(…)` opening at `text[at]`, or 0.
        function close_paren(text, at,   d, i, n, c) {
            d = 0; n = length(text)
            for (i = at; i <= n; i++) {
                c = substr(text, i, 1)
                if (c == "(") d++
                else if (c == ")") { d--; if (d == 0) return i }
            }
            return 0
        }

        # The method chain hanging off `text[at]`: `.ident(…)`, `.ident`,
        # `.await`, `?`, until something else.
        function chain_after(text, at,   i, n, s, e) {
            i = at; n = length(text); s = ""
            while (i <= n) {
                if (substr(text, i, 1) == "?") { s = s "?"; i++; continue }
                if (substr(text, i, 1) == "." && match(substr(text, i + 1), /^[A-Za-z_][A-Za-z0-9_]*/)) {
                    s = s "." substr(text, i + 1, RLENGTH)
                    i += 1 + RLENGTH
                    if (substr(text, i, 1) == "(") {
                        e = close_paren(text, i)
                        if (e == 0) return s
                        s = s substr(text, i, e - i + 1)
                        i = e + 1
                    }
                    continue
                }
                return s
            }
            return s
        }

        function brace_depth(text,   d, i, n, c) {
            d = 0; n = length(text)
            for (i = 1; i <= n; i++) {
                c = substr(text, i, 1)
                if (c == "{") d++
                else if (c == "}") d--
            }
            return d
        }

        # Whether `name` is kept anywhere in `seg`; sets `dropped`.
        function kept_in(seg, name,   rest, base, pos, before, after, tail, m) {
            rest = seg; base = 0
            while ((pos = index(rest, name)) > 0) {
                before = substr(seg, base + pos - 1, 1)
                after = substr(seg, base + pos + length(name), 1)
                tail = substr(seg, base + pos + length(name))
                base += pos + length(name) - 1
                rest = substr(seg, base + 1)
                if (before ~ /[A-Za-z0-9_]/ || after ~ /[A-Za-z0-9_]/) continue
                if (substr(seg, 1, base - length(name)) ~ /drop\($/ && after == ")") { dropped = 1; continue }
                if (substr(seg, 1, base - length(name)) ~ /let _ ?= ?$/) continue
                if (after == ".") {
                    match(tail, /^\.[A-Za-z_][A-Za-z0-9_]*/)
                    m = substr(tail, 2, RLENGTH - 1)
                    if (m ~ /^(accept|incoming|poll_accept|try_into|into_[a-z_]*)$/) return 1
                    continue
                }
                return 1
            }
            return 0
        }

        function flush_fn(   rest, pos, at, e, chain, prefix, name, letpos, scope, seg, kind, sig) {
            if (fn_name == "") return
            # Whitespace runs become one space and the space a line break
            # leaves beside `.`, `(` and `)` is removed, so a statement broken
            # across lines reads the same as one written on a single line.
            gsub(/[[:space:]]+/, " ", body)
            gsub(/ ?\. ?/, ".", body)
            gsub(/\( /, "(", body)
            gsub(/ \)/, ")", body)
            sig = body
            sub(/\{.*$/, "", sig)
            at = 1
            while ((pos = index(substr(body, at), "TcpListener::bind(")) > 0) {
                pos += at - 1
                e = close_paren(body, pos + 17)
                if (e == 0) { print "B other"; break }
                chain = chain_after(body, e + 1)
                at = e + 1
                if (index(chain, "local_addr(") > 0) {
                    print "B chained"
                    printf("V %s:%d: fn %s reads the address of a listener it never keeps\n", fn_file, fn_line, fn_name)
                    continue
                }
                prefix = substr(body, 1, pos - 1)
                if (!match(prefix, /let (mut )?[A-Za-z_][A-Za-z0-9_]* ?(:[^=;{}]*)?= ?([A-Za-z_]+::)*$/)) {
                    print "B other"
                    continue
                }
                print "B named"
                letpos = RSTART
                name = substr(prefix, RSTART)
                sub(/^let (mut )?/, "", name)
                sub(/[ :=].*$/, "", name)
                # The listener lives from its bind to the next `let` of its
                # name, or the end of the function.
                seg = substr(body, e + 1 + length(chain))
                scope = 0
                if (match(seg, /(^|[^A-Za-z0-9_])let (mut )?[A-Za-z_][A-Za-z0-9_]* ?(:[^=;{}]*)?=/)) {
                    # The first rebinding of THIS name, if any.
                    rest = seg; scope = 0
                    while (match(rest, /(^|[^A-Za-z0-9_])let (mut )?[A-Za-z_][A-Za-z0-9_]* ?(:[^=;{}]*)?=/)) {
                        kind = substr(rest, RSTART, RLENGTH)
                        sub(/^[^l]*let (mut )?/, "", kind)
                        sub(/[ :=].*$/, "", kind)
                        if (kind == name) { scope = length(seg) - length(rest) + RSTART; break }
                        rest = substr(rest, RSTART + RLENGTH)
                    }
                }
                if (scope > 0) seg = substr(seg, 1, scope)
                dropped = 0
                if (kept_in(seg, name)) continue
                if (!dropped && scope == 0 && brace_depth(substr(body, 1, letpos)) == 1 && sig !~ /->/) continue
                printf("V %s:%d: fn %s binds listener `%s` only to read its address\n", fn_file, fn_line, fn_name, name)
            }
            fn_name = ""; body = ""
        }

        FNR == 1 { flush_fn() }

        {
            # The shared helpers are not under a test cfg, and a helper there
            # that hands out a freed port would spread the race to every caller.
            if (!(is_test_file(FILENAME) || in_test_region || FILENAME ~ /(^|\/)test_helpers\//)) { flush_fn(); next }
            if (fn_header(code) != "") {
                flush_fn()
                fn_name = fn_header(code); fn_line = FNR; fn_file = FILENAME
            }
            # String literals are elided from `code`, so the bind's address
            # argument is empty here and a bind quoted in a string is text.
            if (fn_name != "") body = body code "\n"
        }

        END { flush_fn() }
AWK
fi

n_binds=0
counts="named=0 chained=0 other=0"
if [[ -n "$records" ]]; then
    n_binds="$(printf '%s\n' "$records" | sed -n '/^B /p' | wc -l | tr -d ' ')"
    run_scanner counts '
        $1 == "B" { n[$2]++ }
        END { printf("named=%d chained=%d other=%d\n", n["named"], n["chained"], n["other"]) }' < <(printf '%s\n' "$records")
fi

if [[ "$MODE" == "count" ]]; then
    echo "$counts"
    exit 0
fi

violations="$(printf '%s\n' "$records" | sed -n 's/^V //p')"

if [[ -n "$violations" ]]; then
    count="$(printf '%s\n' "$violations" | wc -l | tr -d ' ')"
    echo "LISTENER BOUND AND RELEASED FOR ITS PORT — the freed port is not a dead one."
    echo
    printf '%s\n' "$violations"
    echo
    echo "$count function(s) above bind 127.0.0.1:0, read the address and let the"
    echo "listener close. The port goes back to the ephemeral pool, and a test"
    echo "running in parallel can bind it before the connect that was meant to be"
    echo "refused — which then succeeds against that test's server."
    echo
    echo "Fix: take the address every platform refuses, with no listener involved:"
    echo "  let addr = anodizer_core::test_helpers::refusing_addr::refusing_addr();"
    echo
    echo "See .claude/rules/test-refusing-addr.md."
    exit 1
fi

echo "audit-test-dead-port: none of the $n_binds listener binds in test code is released for its port."
