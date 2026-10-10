#!/usr/bin/env bash
# Guard: every test that swaps the process `PATH` takes `#[serial(path_env)]`.
#
# A stub-tool test prepends a directory of fake executables to the process
# `PATH` (`FakeToolDir::activate()`, or an `EnvGuard` over `PATH`). Every other
# test in the same test binary that resolves that tool from `PATH` meanwhile
# spawns the stub. The only thing that keeps the two apart is one shared
# serial_test key, and serial_test keeps one lock PER key: a mutator under
# `cargo_stub_path` and a reader under `path_env` run at the same time.
#
# So the key is `path_env`, for every mutator, with no second spelling. A test
# may name further keys beside it (`#[serial(path_env, npm_counter)]`).
#
# A mutator is a test (`#[test]`, `#[tokio::test]`, `#[rstest]`,
# `#[test_case(…)]`, any path to `test`) whose body
#   - calls `.activate()`,
#   - builds `EnvGuard::set("PATH", …)` / `EnvGuard::remove("PATH")`,
#   - calls `set_var("PATH", …)` / `remove_var("PATH")`, through an alias
#     (`use std::env::set_var as sv`) too, or
#   - calls a helper that does any of these. Helpers are DERIVED, not listed:
#     a non-test fn in test code holding one of the spellings is a helper, and
#     so is a non-test fn calling one, to a fixpoint. A helper is called by
#     name within its own crate; a helper under crates/core/src/test_helpers/
#     is called from every crate.
#
# The variable name is read from the call's first argument. A literal decides
# it. A name — a same-fn `let`, a same-crate `const` / `static` — is resolved;
# one that cannot be resolved is a finding of its own unless the call's line,
# or the line above it, says why it is not PATH:  // not-path: <why>
#
# A PATH-swapping non-test fn in test code that no test calls, directly or
# through other helpers, is a finding too: it is either dead, or a test whose
# attribute the scan did not read, and either way nothing keys it.
#
# The readers — tests that spawn a stubbed tool without stubbing it — cannot be
# found from source: the spawn sits frames down in production code. They are
# measured by running the tests, in audit-path-stub-readers.sh, which takes
# this script's `--records` as its one definition of a mutator.
#
# `--crates`  prints the crates holding a mutator, one per line.
# `--records` prints the raw records:
#   T <crate> <file>:<line> <fn> <0|1 keyed>   a mutating test
#   F <crate> <file>:<line> <fn> <0|1 keyed> <0|1 path-ok>
#                                              every test fn in test code
#   R <crate> <file>                           a file swapping PATH by hand
#                                              (not through `.activate()`)
#   H <crate|*> <fn> <file>:<line>             a mutating non-test fn
#   S <crate> <tool>                           a tool a `.tool("…")` names in a
#                                              fn that swaps PATH or is not a test
#   D <crate> <file> <tools…>                  a `// path-stubs: …` declaration
#   O <crate> <file>:<line> <fn>               an unresolvable variable name
#   C <crate> <caller> <callee>                a call to a helper
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"

MODE="scan"
case "${1:-}" in
    --crates) MODE="crates"; shift ;;
    --records) MODE="records"; shift ;;
esac

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

DIRECT_RE='\.activate\(\)|EnvGuard::(set|remove)\(|(set_var|remove_var)[[:space:]]*(\(|as[[:space:]])|EnvGuard[[:space:]]+as[[:space:]]'

# Every `const` / `static` string in the tree, as `<crate> <name> <value>`
# lines, so a variable name passed to a mutation call can be resolved.
CONSTS="$(mktemp "${TMPDIR:-/tmp}/path-stub-consts.XXXXXX")"
trap 'rm -f "$CONSTS"' EXIT
collect_files CONST_LINES -rHnE --include='*.rs' --exclude-dir=target \
    -- '(const|static)[[:space:]]+[A-Za-z_][A-Za-z0-9_]*[[:space:]]*:[[:space:]]*&' crates/*/src crates/*/tests
if ((${#CONST_LINES[@]})); then
    printf '%s\n' "${CONST_LINES[@]}" |
        sed -nE 's#^crates/([^/]+)/[^:]*:[0-9]+:.*(const|static)[[:space:]]+([A-Za-z_][A-Za-z0-9_]*)[[:space:]]*:[[:space:]]*&[^=]*=[[:space:]]*"([^"]*)".*$#\1 \3 \4#p' > "$CONSTS"
fi

# One scan over the files that can hold a mutator. `helpers` is the
# space-separated `<crate>:<fn>` list found so far (`*` for a workspace-wide
# helper).
scan_once() {
    local __out_var="$1" helpers="$2"
    shift 2
    run_scanner "$__out_var" -v helpers="$helpers" -v consts_file="$CONSTS" \
        -f "$LIB_DIR/rust-lex.awk" -f "$LIB_DIR/test-regions.awk" -f - "$@" <<'AWK'
        BEGIN {
            n = split(helpers, list, " ")
            for (i = 1; i <= n; i++) is_helper[list[i]] = 1
            while ((getline cl < consts_file) > 0) {
                split(cl, cp, " ")
                key = cp[1] ":" cp[2]
                val = substr(cl, length(cp[1]) + length(cp[2]) + 3)
                # Two declarations of one name: the PATH one wins the lookup.
                if (!(key in const_val) || val == "PATH") const_val[key] = val
            }
        }

        function crate_of(f,   parts, k, m) {
            m = split(f, parts, "/")
            for (k = 1; k < m; k++) if (parts[k] == "crates") return parts[k + 1]
            return ""
        }

        function trim(s) { sub(/^[[:space:]]+/, "", s); sub(/[[:space:]]+$/, "", s); return s }

        # The name a `fn` header declares: the shared rule, plus the
        # `fn $name` a macro_rules! body declares.
        function header_of(c,   s) {
            s = fn_header(c)
            if (s != "") return s
            if (match(c, /(^|[^A-Za-z0-9_])fn[[:space:]]+\$[A-Za-z_][A-Za-z0-9_]*/)) {
                s = substr(c, RSTART, RLENGTH)
                sub(/^.*fn[[:space:]]+/, "", s)
                return s
            }
            return ""
        }

        function is_test_attr(a) {
            gsub(/[[:space:]]+/, "", a)
            return a ~ /#\[([A-Za-z_][A-Za-z0-9_]*::)*(test|rstest|test_case)(\(|\])/
        }

        function is_keyed(a) {
            gsub(/[[:space:]]+/, "", a)
            return a ~ /#\[(serial_test::)?serial\(([A-Za-z0-9_]+,)*path_env(,[A-Za-z0-9_]+)*,?\)\]/
        }

        function flush_fn(   k, tl, m) {
            if (fn_name != "") {
                if (fn_raw) printf("R %s %s\n", crate, fn_file)
                if (fn_mutates) {
                    if (fn_is_test)
                        printf("T %s %s:%d %s %d\n", crate, fn_file, fn_line, fn_name, fn_keyed)
                    else
                        printf("H %s %s %s:%d\n", (helper_file ? "*" : crate), fn_name, fn_file, fn_line)
                }
                if (fn_tools != "" && (fn_mutates || !fn_is_test)) {
                    m = split(fn_tools, tl, " ")
                    for (k = 1; k <= m; k++) printf("S %s %s\n", crate, tl[k])
                }
            }
            fn_name = ""; fn_mutates = 0; fn_raw = 0; fn_tools = ""; delete fn_lets
            pend_arg = 0
        }

        # A call to a derived helper: the name as a whole word, then `(`.
        function calls_helper(c,   rest, name, found) {
            rest = c; found = 0
            while (match(rest, /[A-Za-z_][A-Za-z0-9_]*[[:space:]]*\(/)) {
                name = substr(rest, RSTART, RLENGTH)
                sub(/[[:space:]]*\($/, "", name)
                if (((crate ":" name) in is_helper) || (("*:" name) in is_helper)) {
                    found = 1
                    printf("C %s %s %s\n", crate, fn_name, name)
                }
                rest = substr(rest, RSTART + RLENGTH)
            }
            return found
        }

        # The variable a mutation call names, decided from its first argument.
        # Returns 1 for PATH, 0 for another variable, -1 for a name that could
        # not be resolved.
        function classify_arg(arg,   name, val) {
            arg = trim(arg)
            if (arg == "") return -1
            if (substr(arg, 1, 1) == "\"") return (substr(arg, 1, 6) == "\"PATH\"") ? 1 : 0
            name = arg
            sub(/^&(mut[[:space:]]+)?/, "", name)
            if (!match(name, /^[A-Za-z_][A-Za-z0-9_:.]*/)) return -1
            name = substr(name, RSTART, RLENGTH)
            sub(/^.*::/, "", name)
            sub(/\..*$/, "", name)
            if (name in fn_lets) val = fn_lets[name]
            else if ((crate ":" name) in const_val) val = const_val[crate ":" name]
            else if (helper_file && (("core:" name) in const_val)) val = const_val["core:" name]
            else return -1
            return (val == "PATH") ? 1 : 0
        }

        function note_arg(verdict, line_no) {
            if (verdict == 1) { fn_mutates = 1; fn_raw = 1 }
            else if (verdict == -1 && !marked) printf("O %s %s:%d %s\n", crate, FILENAME, line_no, fn_name)
        }

        FNR == 1 {
            flush_fn()
            crate = crate_of(FILENAME)
            helper_file = (FILENAME ~ /(^|\/)crates\/core\/src\/test_helpers\//)
            whole_file_is_test = is_test_file(FILENAME) || helper_file
            pend = ""; attr_open = 0; attr_depth = 0; prev_marked = 0; path_ok = 0
            delete alias
        }

        {
            raw = $0
            cmt = comment_part(raw)
            in_test = (whole_file_is_test || in_test_region)
            marked = (cmt ~ /not-path:[[:space:]]*[^[:space:]]/) || prev_marked
            # `// path-ok: <why>` in the attribute/comment run above a test.
            if (cmt ~ /path-ok:[[:space:]]*[^[:space:]]/) path_ok = 1
            prev_marked = (cmt ~ /not-path:[[:space:]]*[^[:space:]]/) && trim(code) == ""
            if (cmt ~ /^\/\/[[:space:]]*path-stubs:[[:space:]]*[^[:space:]]/) {
                decl = cmt
                sub(/^.*path-stubs:[[:space:]]*/, "", decl)
                sub(/[[:space:]]+(—|-|:).*$/, "", decl)
                printf("D %s %s %s\n", crate, FILENAME, decl)
            }
            if (!in_test) { flush_fn(); pend = ""; attr_open = 0; next }
            t = code
            if (t ~ /(set_var|remove_var|EnvGuard)[[:space:]]+as[[:space:]]+[A-Za-z_]/) {
                a = t
                sub(/^.*(set_var|remove_var|EnvGuard)[[:space:]]+as[[:space:]]+/, "", a)
                match(a, /^[A-Za-z_][A-Za-z0-9_]*/)
                alias[substr(a, 1, RLENGTH)] = 1
            }
            # Attributes, however many lines each spans, accumulate until the
            # item they decorate. A bracket depth across lines is what reads a
            # multi-line `#[cfg_attr(…)]` or `#[serial(\n path_env\n)]`.
            if (attr_open) {
                pend = pend " " t
                attr_depth += count_char(t, "[") - count_char(t, "]")
                if (attr_depth <= 0) attr_open = 0
                next
            }
            t = trim(t)
            while (substr(t, 1, 2) == "#[") {
                d = 0; closed = 0
                for (p = 2; p <= length(t); p++) {
                    ch = substr(t, p, 1)
                    if (ch == "[") d++
                    else if (ch == "]") { d--; if (d == 0) { closed = p; break } }
                }
                if (closed) {
                    pend = pend " " substr(t, 1, closed)
                    t = trim(substr(t, closed + 1))
                } else {
                    pend = pend " " t
                    attr_open = 1; attr_depth = d
                    t = ""
                    break
                }
            }
            if (attr_open || t == "") next
            name = header_of(t)
            if (name != "") {
                flush_fn()
                fn_name = name; fn_line = FNR; fn_file = FILENAME
                fn_is_test = is_test_attr(pend); fn_keyed = is_keyed(pend)
                if (fn_is_test) printf("F %s %s:%d %s %d %d\n", crate, fn_file, fn_line, fn_name, fn_keyed, path_ok)
                pend = ""; path_ok = 0
                # The rest of the header line may already be body (`fn f() { … }`).
                sub(/^.*fn[[:space:]]+\$?[A-Za-z_][A-Za-z0-9_]*/, "", t)
                if (t !~ /\{/) next
                sub(/^[^{]*\{/, "", t)
            } else { pend = ""; path_ok = 0 }
            if (fn_name == "") next

            if (pend_arg) {
                a = raw
                sub(/^[[:space:]]+/, "", a)
                note_arg(classify_arg(a), pend_line)
                pend_arg = 0
            }
            if (t ~ /\.activate\(\)/) fn_mutates = 1
            if (match(raw, /let[[:space:]]+(mut[[:space:]]+)?[A-Za-z_][A-Za-z0-9_]*[[:space:]]*(:[^=]*)?=[[:space:]]*"[^"]*"/)) {
                ls = substr(raw, RSTART, RLENGTH)
                ln = ls; sub(/^let[[:space:]]+(mut[[:space:]]+)?/, "", ln); sub(/[^A-Za-z0-9_].*$/, "", ln)
                lv = ls; sub(/^[^"]*"/, "", lv); sub(/"$/, "", lv)
                fn_lets[ln] = lv
            }
            # The mutation call is found on the code half; its first argument
            # is read off the raw line, where the literal still is.
            mre = "(^|[^A-Za-z0-9_])(EnvGuard::(set|remove)|set_var|remove_var"
            for (al in alias) mre = mre "|" al "(::(set|remove))?"
            mre = mre ")[[:space:]]*\\("
            rest = t
            while (match(rest, mre)) {
                callee = substr(rest, RSTART, RLENGTH)
                sub(/^[^A-Za-z_]/, "", callee)
                rest = substr(rest, RSTART + RLENGTH)
                at = index(raw, callee)
                if (at == 0) continue
                a = trim(substr(raw, at + length(callee)))
                if (a == "") { pend_arg = 1; pend_line = FNR }
                else note_arg(classify_arg(a), FNR)
            }
            if (t ~ /\.tool[[:space:]]*\(/) {
                r = raw
                while (match(r, /\.tool[[:space:]]*\([[:space:]]*"[^"]+"/)) {
                    tn = substr(r, RSTART, RLENGTH)
                    sub(/^[^"]*"/, "", tn); sub(/"$/, "", tn)
                    fn_tools = fn_tools " " tn
                    r = substr(r, RSTART + RLENGTH)
                }
            }
            if (calls_helper(t)) fn_mutates = 1
        }

        END { flush_fn() }
AWK
}

helpers=""
records=""
converged=0
# The fixpoint: each round may name new helpers, whose callers the next round
# has to find in files the direct spellings never matched.
for _round in $(seq 1 64); do
    pattern="$DIRECT_RE"
    for h in $helpers; do
        pattern+="|${h#*:}[[:space:]]*\\("
    done
    collect_files FILES -rlE --include='*.rs' --exclude-dir=target \
        -- "$pattern" crates/*/src crates/*/tests
    records=""
    if ((${#FILES[@]})); then
        scan_once records "$helpers" "${FILES[@]}"
    fi
    found="$(printf '%s\n' "$records" | sed -n 's/^H \([^ ]*\) \([^ ]*\) .*$/\1:\2/p' | sort -u | tr '\n' ' ')"
    found="${found% }"
    if [[ "$found" == "$helpers" ]]; then converged=1; break; fi
    helpers="$found"
done
if ((!converged)); then
    echo "audit-path-stub-serial: the helper derivation did not converge in 64 rounds; the scan did not run." >&2
    exit 2
fi

if [[ "$MODE" == "records" ]]; then
    # The reader audit needs an F record for every test of every crate that
    # holds a mutator, and most of those files never spell a PATH swap, so
    # the final scan reads every source of those crates.
    crates="$(printf '%s\n' "$records" | sed -n 's/^T \([^ ]*\) .*/\1/p' | sort -u)"
    roots=()
    for c in $crates; do roots+=("crates/$c/src"); done
    if ((${#roots[@]})); then
        collect_files ALL_FILES -rlE --include='*.rs' --exclude-dir=target -- '' "${roots[@]}"
        # Plus the files the narrow scan read, which hold the helpers and
        # declarations of crates that have no mutator of their own.
        mapfile -t FILES < <(printf '%s\n' "${FILES[@]}" "${ALL_FILES[@]}" | LC_ALL=C sort -u)
        scan_once records "$helpers" "${FILES[@]}"
    fi
    printf '%s\n' "$records"
    exit 0
fi

mutators="$(printf '%s\n' "$records" | sed -n 's/^T //p')"
crates="$(printf '%s\n' "$mutators" | sed -n 's/^\([^ ]*\) .*/\1/p' | sort -u)"

if [[ "$MODE" == "crates" ]]; then
    [[ -n "$crates" ]] && printf '%s\n' "$crates"
    exit 0
fi

violations="$(printf '%s\n' "$mutators" |
    sed -n 's/^[^ ]* \([^ ]*\) \([^ ]*\) 0$/\1: [mutator] fn \2 swaps PATH without #[serial(path_env)]/p')"

opaque="$(printf '%s\n' "$records" |
    sed -n 's/^O [^ ]* \([^ ]*\) \([^ ]*\)$/\1: [variable] fn \2 names the variable it mutates through a value the scan cannot resolve/p')"

# A helper no test reaches: walk the call edges from every test.
run_scanner orphans '
    $1 == "H" { helper[$2 ":" $3] = $4 }
    $1 == "T" { test[$2 ":" $4] = 1 }
    $1 == "C" { edge[$2 ":" $3 ":" $4] = 1 }
    END {
        # A test reaches the helpers it calls; a reached helper reaches its callees.
        changed = 1
        while (changed) {
            changed = 0
            for (e in edge) {
                split(e, p, ":")
                if (!((p[1] ":" p[2]) in test) && !((p[1] ":" p[2]) in used) && !(("*:" p[2]) in used)) continue
                for (k = 1; k <= 2; k++) {
                    key = ((k == 1) ? p[1] : "*") ":" p[3]
                    if ((key in helper) && !(key in used)) { used[key] = 1; changed = 1 }
                }
            }
        }
        for (h in helper) if (!(h in used)) print helper[h] ": [orphan] fn " substr(h, index(h, ":") + 1) " swaps PATH and no test calls it; if it is a test, its attribute was not read"
    }' < <(printf '%s\n' "$records")
orphans="$(printf '%s\n' "$orphans" | sort)"
orphan_lines=""
[[ -n "$orphans" ]] && orphan_lines="$orphans"$'\n'

if [[ -n "$violations$opaque$orphan_lines" ]]; then
    echo "PATH SWAPPED OUTSIDE THE path_env GROUP — a stub tool leaks into parallel tests."
    echo
    [[ -n "$violations" ]] && printf '%s\n' "$violations"
    [[ -n "$opaque" ]] && printf '%s\n' "$opaque"
    [[ -n "$orphan_lines" ]] && printf '%s' "$orphan_lines"
    echo
    echo "A [mutator] replaces or prepends the process PATH. Every other test in"
    echo "the same test binary resolves its tools from that PATH meanwhile, and"
    echo "serial_test keeps one lock per key, so a second key excludes nobody that"
    echo "is keyed path_env."
    echo "Fix: add path_env to the test's serial attribute, keeping any key it has:"
    echo "  #[serial_test::serial(path_env)]"
    echo "  #[serial(path_env, cargo_stub_path)]"
    echo
    echo "A [variable] passes a name the scan cannot resolve to EnvGuard::set /"
    echo "remove, set_var or remove_var. Pass the literal, name a same-fn let or a"
    echo "same-crate const, or say why it is not PATH on that line or the one above:"
    echo "  // not-path: <why>"
    echo
    echo "An [orphan] is a PATH-swapping fn in test code that no test calls."
    echo "Delete it, or give the test attribute the scan did not read a spelling it"
    echo "reads (#[test], #[tokio::test], #[rstest], #[test_case(…)])."
    echo
    echo "Preferred where the tool is configurable: point the config at"
    echo "FakeToolDir::tool_path(…) and do not touch PATH at all."
    exit 1
fi

n_mutators=0
[[ -n "$mutators" ]] && n_mutators="$(printf '%s\n' "$mutators" | wc -l | tr -d ' ')"
n_crates=0
[[ -n "$crates" ]] && n_crates="$(printf '%s\n' "$crates" | wc -l | tr -d ' ')"
n_helpers=0
[[ -n "$helpers" ]] && n_helpers="$(printf '%s\n' $helpers | wc -l | tr -d ' ')"
echo "audit-path-stub-serial: all $n_mutators PATH-swapping tests in $n_crates crate(s) take #[serial(path_env)] ($n_helpers helper(s) derived)."
