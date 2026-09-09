# The one way an audit scanner runs awk. Sourced, not executed:
#
#   LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
#   source "$LIB_DIR/scan.sh"
#   run_scanner violations -f "$LIB_DIR/rust-lex.awk" -f - "${FILES[@]}" <<'AWK'
#       …program…
#   AWK
#
# `run_scanner <var> <awk args…>` puts awk's stdout in <var> and, on ANY
# non-zero awk status, exits 2.
#
# WHY a runner rather than `var="$(awk …)"` at each site: bare capture under
# `set -e` turns a scanner that could not run — a missing awk library, a
# syntax error in the program, a bad dynamic regex — into exit 1, which is
# this repo's "violations found" code, printed with an EMPTY findings block.
# The audit then reads as a clean scan of nothing while it scanned nothing.
# Exit 2 is reserved for "the scan did not run"; awk's own stderr is left
# unredirected so the real diagnostic reaches the caller alongside it.
run_scanner() {
    local __scan_var="$1"
    shift
    local __scan_out __scan_status=0
    __scan_out="$(awk "$@")" || __scan_status=$?
    if ((__scan_status != 0)); then
        printf '%s: awk scanner exited %d; the scan did not run.\n' \
            "$(basename "$0" .sh)" "$__scan_status" >&2
        exit 2
    fi
    printf -v "$__scan_var" '%s' "$__scan_out"
}

# The one way an audit collects the files (or hit lines) a scan runs over:
#
#   collect_files FILES -rlE '<pattern>' crates/*/src --include='*.rs'
#
# `collect_files <array-var> <grep args…>` fills <array-var> with grep's
# output lines. grep's "no match" (status 1) is a legitimate empty result;
# anything above it — grep missing from PATH, an unreadable path, a pattern
# the build cannot compile — means the collection did not run, and is exited 2
# on with grep's own stderr left visible.
#
# WHY, the same reason run_scanner exists one step later: a
# `grep … 2>/dev/null || true` collection turns a grep that never ran into an
# empty file list, and an audit over an empty file list prints its clean-tree
# line and exits 0.
collect_files() {
    local -n __collect_out="$1"
    shift
    local __collect_text __collect_status=0
    __collect_text="$(grep "$@")" || __collect_status=$?
    if ((__collect_status > 1)); then
        printf '%s: file collection exited %d; the scan did not run.\n' \
            "$(basename "$0" .sh)" "$__collect_status" >&2
        exit 2
    fi
    __collect_out=()
    if [[ -n "$__collect_text" ]]; then
        mapfile -t __collect_out <<< "$__collect_text"
    fi
    return 0
}
