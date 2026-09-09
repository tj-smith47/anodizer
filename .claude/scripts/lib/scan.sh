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
