#!/usr/bin/env bash
# Guard: no production source file exceeds the god-file ceiling.
#
# A file is oversized when its PRODUCTION code exceeds GOD_FILE_LIMIT lines.
# TEST code does not count toward the ceiling at all: a pure test file is never
# a god file no matter how long, and an inline `#[cfg(test)] mod tests { … }`
# block is subtracted from its host file's total. The remedy for a finding is
# to split the production code into modules — and, where a file still carries
# an inline test module, to move that module out to a sibling `tests.rs` so the
# two concerns stop sharing a file.
#
# Without this guard the rule is a census a human remembers to re-run, so a new
# god file lands green and is only noticed long after the commit that created
# it.
#
# ── what counts as test code ───────────────────────────────────────────────
# Three independent sources, all derived rather than name-matched:
#
#   1. Cargo test/bench targets — everything under `crates/*/tests/` and
#      `crates/*/benches/` is compiled only for `cargo test`/`cargo bench`.
#   2. A file carrying the whole-file inner attribute `#![cfg(test)]`.
#   3. A file reachable from a `#[cfg(test)] mod NAME;` declaration, plus
#      everything THAT file declares in turn (transitively). This is what makes
#      the check name-agnostic: `tests.rs`, `orchestrator_tests.rs` and
#      `partial_rollback_tests.rs` are all classified from the declaration that
#      gates them, not from a filename convention that a new file can miss.
#
# Inside a file that is NOT wholly test code, a test region is a test-only
# `#[cfg(…)]` attribute plus the item it gates. "Test-only" means the cfg
# predicate cannot hold outside `cargo test`:
#
#   #[cfg(test)]                                   → test-only
#   #[cfg(all(test, unix))]                        → test-only  (all ⇒ test)
#   #[cfg(all(test, feature = "test-helpers"))]    → test-only  (all ⇒ test)
#   #[cfg(any(windows, test))]                     → NOT: builds on windows
#   #[cfg(any(test, feature = "test-support"))]    → NOT: builds under a feature
#   #[cfg(feature = "test-helpers")]               → NOT: no bare test predicate
#
# A plain `#[cfg(test)]` scan that only looks for the exact string `#[cfg(test)]`
# misses `#[cfg(all(test, unix))]`, which is why `stage-notarize/src/run.rs` and
# `stage-sign/src/verify_assets.rs` used to over-report.
#
# ── why the gated item is measured, not truncated at the marker ────────────
# The naive counter treats the first `#[cfg(test)]` as end-of-production and
# counts only the lines above it. That is wrong for a module-dir `mod.rs`, where
# `#[cfg(test)] mod tests;` sits MID-file with the `pub use` re-export block
# below it: `crates/core/src/artifact/mod.rs` truncates to 5 lines that way and
# is 18 production lines in reality. So the gated ITEM is measured and
# subtracted — a `mod tests;` declaration costs 2 lines, an inline
# `mod tests { … }` costs its whole brace-delimited body — and scanning resumes
# afterwards.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

GOD_FILE_LIMIT=1000

# Two lists, kept apart on purpose. Conflating them is how a temporary
# over-limit file becomes a permanent second ceiling nobody remembers agreeing
# to, so each entry states which it is.
#
# GOD_FILE_EXEMPT — a file whose size is a deliberate design outcome. Splitting
# it would make the code worse, so it is not going to shrink.
#
# GOD_FILE_DEBT — a real violation that predates this guard. It is NOT blessed:
# it is pinned at its current size and printed on every green run so it stays
# visible until it is split.
#
# Both are PINNED ceilings, never blank cheques: the file may shrink freely, but
# a commit that grows it fails. That makes the entry a ratchet — the only
# direction either list can move a file is down and then out.
#
# Format: "<path>:<pinned ceiling>:<why>".
GOD_FILE_EXEMPT=(
    "crates/cli/src/lib.rs:1093:CLI surface aggregator — clap command wiring, one flat block per subcommand; splitting it scatters the argument surface across files without reducing it. Ratified 2026-07-26 as a standing exemption from the >1000-production-line rule; do not re-open it as a split target"
)

GOD_FILE_DEBT=()

# `--others --exclude-standard` alongside the tracked set: a god file written
# but not yet `git add`ed is exactly the case this guard exists to catch, and a
# tracked-only listing would pass it silently. `-u` dedupes the overlap.
#
# `-f -` drops index entries with no file on disk: a split in progress deletes
# `foo.rs` in the worktree before that deletion is staged, and `--cached` keeps
# listing it. Without the filter every downstream reader (grep, awk) fails on
# the missing path, and the line counter charges the file 0 production lines —
# a vanishing act that would let a genuinely oversized file pass.
mapfile -t ALL_RS < <(
    git ls-files --cached --others --exclude-standard -- 'crates/**/*.rs' 'crates/*.rs' 2>/dev/null |
        sort -u |
        while IFS= read -r _f; do [[ -f "$_f" ]] && printf '%s\n' "$_f"; done
)
if [[ ${#ALL_RS[@]} -eq 0 ]]; then
    echo "audit-god-files: no crates/**/*.rs tracked; nothing to scan." >&2
    exit 1
fi

# Resolve `mod NAME;` declared inside DECL_FILE to the file that provides it.
# rustc looks in the declaring module's directory: `src/lib.rs` (or `main.rs`,
# or `foo/mod.rs`) declares into its OWN directory, while `src/foo.rs` declares
# into the sibling `src/foo/` directory.
# Resolve `mod NAME;` declared inside DECL_FILE to the file that provides it,
# into the global RESOLVED (empty when the module has no file, e.g. a `mod`
# declared inside a `#[cfg]`-gated inline block). Assigning a global rather than
# printing keeps this out of a command substitution: it is called once per
# declaration across the whole tree, and a subshell each time dominated the
# runtime.
#
# rustc looks in the declaring module's directory: `src/lib.rs` (or `main.rs`,
# or `foo/mod.rs`) declares into its OWN directory, while `src/foo.rs` declares
# into the sibling `src/foo/` directory.
resolve_mod_file() {
    local decl_file="$1" name="$2" dir base cand_dir c
    dir="${decl_file%/*}"
    base="${decl_file##*/}"
    base="${base%.rs}"
    case "$base" in
        mod | lib | main) cand_dir="$dir" ;;
        *) cand_dir="$dir/$base" ;;
    esac
    RESOLVED=""
    for c in "$cand_dir/$name.rs" "$cand_dir/$name/mod.rs"; do
        if [[ -f "$c" ]]; then
            RESOLVED="$c"
            return 0
        fi
    done
}

# Every `mod NAME;` declaration in the whole tree, in ONE awk pass, as
# "<file>\t<gated>\t<name>" where <gated> is 1 when the declaration is preceded
# by (or shares a line with) a test-only cfg attribute. One pass rather than one
# awk per file: at ~800 files the process spawns cost more than the scan.
collect_mod_decls() {
    awk '
        function is_test_only_cfg(l) {
            if (l !~ /#!?\[cfg\(/) return 0
            if (l ~ /\(any\(/) return 0            # any(): satisfiable outside test
            return (l ~ /(^|[(,[:space:]])test([),]|$)/)
        }
        FNR == 1 { pend = 0 }
        { line = $0 }
        is_test_only_cfg(line) { pend = 1 }
        match(line, /^[[:space:]]*(pub[[:space:]]*(\([^)]*\)[[:space:]]*)?)?mod[[:space:]]+[A-Za-z_][A-Za-z_0-9]*[[:space:]]*;/) {
            name = substr(line, RSTART, RLENGTH)
            sub(/[[:space:]]*;[[:space:]]*$/, "", name)
            sub(/^.*[[:space:]]/, "", name)
            printf("%s\t%d\t%s\n", FILENAME, pend, name)
            pend = 0
            next
        }
        # Only attributes and doc comments may sit between the cfg attribute and
        # the item it gates; anything else means the attribute governed something
        # other than a module declaration.
        /^[[:space:]]*(#|\/\/)/ { next }
        /^[[:space:]]*$/        { next }
        { pend = 0 }
    ' "$@"
}

declare -A IS_TEST_FILE=()
declare -A GATED_DECLS=()
declare -A ALL_DECLS=()
WORKLIST=()

while IFS=$'\t' read -r file gated name; do
    [[ -n "$file" ]] || continue
    ALL_DECLS["$file"]+="$name "
    [[ "$gated" == "1" ]] && GATED_DECLS["$file"]+="$name "
done < <(collect_mod_decls "${ALL_RS[@]}")

# One grep for the whole-file inner attribute instead of one per file.
declare -A INNER_CFG_TEST=()
while IFS= read -r f; do
    [[ -n "$f" ]] && INNER_CFG_TEST["$f"]=1
done < <(grep -rlE '^[[:space:]]*#!\[cfg\(test\)\]' -- "${ALL_RS[@]}" 2>/dev/null || true)

for f in "${ALL_RS[@]}"; do
    case "$f" in
        crates/*/tests/* | crates/*/benches/*)
            IS_TEST_FILE["$f"]=1
            WORKLIST+=("$f")
            continue
            ;;
    esac
    if [[ -n "${INNER_CFG_TEST[$f]:-}" ]]; then
        IS_TEST_FILE["$f"]=1
        WORKLIST+=("$f")
        continue
    fi
    for name in ${GATED_DECLS[$f]:-}; do
        resolve_mod_file "$f" "$name"
        if [[ -n "$RESOLVED" && -z "${IS_TEST_FILE[$RESOLVED]:-}" ]]; then
            IS_TEST_FILE["$RESOLVED"]=1
            WORKLIST+=("$RESOLVED")
        fi
    done
done

# Transitive closure: a submodule of a test-only module is itself test-only,
# whether or not its own declaration repeats the `#[cfg(test)]`.
wl_head=0
while [[ $wl_head -lt ${#WORKLIST[@]} ]]; do
    f="${WORKLIST[$wl_head]}"
    wl_head=$((wl_head + 1))
    for name in ${ALL_DECLS[$f]:-}; do
        resolve_mod_file "$f" "$name"
        if [[ -n "$RESOLVED" && -z "${IS_TEST_FILE[$RESOLVED]:-}" ]]; then
            IS_TEST_FILE["$RESOLVED"]=1
            WORKLIST+=("$RESOLVED")
        fi
    done
done

PROD_FILES=()
for f in "${ALL_RS[@]}"; do
    [[ -n "${IS_TEST_FILE[$f]:-}" ]] || PROD_FILES+=("$f")
done

# Per-file production-line count. Emits "<count>\t<path>" per input file.
#
# The gated item's extent is found by BRACE DEPTH over code with string and
# comment content elided (`strip_code`). Two cheaper designs were measured
# against a reference implementation over all 818 tracked files and both were
# wrong on real code, so neither is used:
#
#   * naive trailing-comment strip (`sub(/\/\/.*$/, "")`) — cuts the line at the
#     `//` inside `"https://…"`, so a `const URL: &str = "https://…";` no longer
#     looks statement-terminated and the region swallows the rest of the file.
#     `stage-publish/src/util/attribution.rs` under-reported 21 lines as 8, and
#     under-reporting is the fail-OPEN direction.
#   * indent-anchored close (first `}` at the opener's indent) — a `}` at column
#     0 inside a multi-line raw-string fixture ends the region early;
#     `schema_validation/nix.rs` over-reported 412 lines as 709.
#
# Eliding literals first removes both failure modes at their shared root: every
# brace, semicolon and `//` the scanner sees is then real code.
count_prod_lines() {
    awk '
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
                if (c == "'"'"'") {
                    if (substr(l, i + 1, 1) == "\\") {
                        k = index(substr(l, i + 2), "'"'"'")
                        if (k > 0) { i = i + 2 + k; continue }
                    } else if (substr(l, i + 2, 1) == "'"'"'") { i += 3; continue }
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
        function flush() { if (cur != "") printf("%d\t%s\n", prod, cur) }

        FNR == 1 {
            flush()
            cur = FILENAME; prod = 0; state = "idle"
            depth = 0; opened = 0
            in_raw = 0; raw_hashes = 0; in_str = 0; in_bcomment = 0
        }

        {
            line = $0
            code = strip_code(line)
        }

        # Inside the gated item: consume until its brace block closes, or — for
        # a braceless item (`mod tests;`, a `use`, a multi-line `const`) — until
        # the statement terminates.
        state == "region" {
            depth += count_char(code, "{") - count_char(code, "}")
            if (index(code, "{") > 0) opened = 1
            if (opened) {
                if (depth <= 0) state = "idle"
            } else if (code ~ /;[[:space:]]*$/) {
                state = "idle"
            }
            next
        }

        {
            if (is_test_only_cfg(line)) {
                state = "region"; depth = 0; opened = 0
                # Re-run the region rule against this same line: a one-liner
                # (`#[cfg(test)] use x;`) is its whole own region.
                depth += count_char(code, "{") - count_char(code, "}")
                if (index(code, "{") > 0) opened = 1
                if (opened && depth <= 0) state = "idle"
                next
            }
            prod++
        }

        END { flush() }
    ' "$@"
}

# Both pinned lists indexed by path once, so the per-file loop below is a hash
# lookup rather than a scan inside a command substitution.
declare -A PIN_CEILING=()
declare -A PIN_REASON=()
declare -A PIN_LIST=()
for listname in GOD_FILE_EXEMPT GOD_FILE_DEBT; do
    declare -n _list="$listname"
    for e in ${_list[@]+"${_list[@]}"}; do
        _path="${e%%:*}"
        _rest="${e#*:}"
        PIN_CEILING["$_path"]="${_rest%%:*}"
        PIN_REASON["$_path"]="${_rest#*:}"
        PIN_LIST["$_path"]="$listname"
    done
    unset -n _list
done

violations=""
exempt_seen=""
debt_seen=""
largest_count=0
largest_path=""

while IFS=$'\t' read -r count path; do
    [[ -n "$path" ]] || continue

    matched=""
    ceiling="${PIN_CEILING[$path]:-}"
    if [[ -n "$ceiling" ]]; then
        reason="${PIN_REASON[$path]}"
        listname="${PIN_LIST[$path]}"
        matched=1
        if [[ "$listname" == GOD_FILE_EXEMPT ]]; then
            exempt_seen+="  $path: $count prod lines (pinned $ceiling) — $reason"$'\n'
            kind="exempt"
        else
            debt_seen+="  $path: $count prod lines (pinned $ceiling, over the ${GOD_FILE_LIMIT} ceiling — awaiting a split) — $reason"$'\n'
            kind="recorded debt"
        fi
        if [[ "$count" -gt "$ceiling" ]]; then
            violations+="$path: $count production lines — $kind at a PINNED ceiling of $ceiling and it GREW; a pinned entry may only fall, so split the file instead of raising the pin"$'\n'
        fi
    fi
    [[ -z "$matched" ]] || continue

    if [[ "$count" -gt "$largest_count" ]]; then
        largest_count="$count"
        largest_path="$path"
    fi
    if [[ "$count" -gt "$GOD_FILE_LIMIT" ]]; then
        violations+="$path: $count production lines (ceiling $GOD_FILE_LIMIT)"$'\n'
    fi
done < <(count_prod_lines "${PROD_FILES[@]}" | sort -rn)

if [[ -n "$violations" ]]; then
    echo "GOD FILE — production code over the ${GOD_FILE_LIMIT}-line ceiling."
    echo
    printf '%s' "$violations"
    echo
    echo "Production lines are total lines minus test code: files under"
    echo "crates/*/tests|benches, files carrying #![cfg(test)], files reached"
    echo "through a #[cfg(test)] mod NAME; declaration, and inline test-only"
    echo "cfg regions are all excluded. A pure test file is never a god file."
    echo
    echo "Fix: split the production code into cohesive modules. If the file"
    echo "still holds an inline #[cfg(test)] mod tests { … }, move it to a"
    echo "sibling tests.rs in the same commit — that keeps the split reviewable"
    echo "and leaves one concern per file."
    echo
    echo "Recording the file instead of splitting it is a last resort and needs"
    echo "the user's agreement: add it to GOD_FILE_DEBT in this script with a"
    echo "pinned ceiling and a written reason. GOD_FILE_EXEMPT is only for a size"
    echo "that is a deliberate design outcome. Neither pin may ever be raised."
    exit 1
fi

echo "audit-god-files: ${#PROD_FILES[@]} production files scanned (${#IS_TEST_FILE[@]} test files excluded); largest unpinned is $largest_path at $largest_count production lines, ceiling $GOD_FILE_LIMIT."
if [[ -n "$exempt_seen" ]]; then
    echo "audit-god-files: ${#GOD_FILE_EXEMPT[@]} named exemption(s) — deliberate, not going to shrink:"
    printf '%s' "$exempt_seen"
fi
if [[ -n "$debt_seen" ]]; then
    echo "audit-god-files: ${#GOD_FILE_DEBT[@]} recorded debt file(s) — OVER the ceiling and still owed a split:"
    printf '%s' "$debt_seen"
fi
