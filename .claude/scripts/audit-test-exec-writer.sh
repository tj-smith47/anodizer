#!/usr/bin/env bash
# Guard: every TEST-context write of an EXECUTABLE file routes through
# `anodizer_core::test_helpers::fake_tool::write_executable_script`.
#
# Contract: `execve` refuses a file that any process still holds open for
# writing (`ETXTBSY`). Under `--test-threads=N` a sibling test thread's `fork`
# landing inside another thread's write window inherits that writable
# descriptor and keeps it until its own `exec` — the descriptor is `CLOEXEC`,
# which releases at `exec`, not at `fork`. So a test that writes a stub and
# spawns it can fail with "Text file busy" no matter how promptly the writer
# closes its own file, and the spawn that trips is usually PRODUCTION code,
# which has no business retrying `ETXTBSY`.
#
# The fix the codebase standardises on: write every test executable through
#   anodizer_core::test_helpers::fake_tool::write_executable_script(&path, script)
# which inserts a probe guard under the script's shebang and execs it once
# under a marker env var before returning. A successful probe proves the
# inode carries no writer, so the caller's real spawn cannot see `ETXTBSY`.
# See crates/core/src/test_helpers/fake_tool.rs.
#
# This audit fails (exit 1) when TEST code sets an OWNER-EXECUTABLE mode
# WITHOUT carrying an inline
#   // exec-writer-ok: <why>
# marker on that line or the line directly above it. The marker is for a mode
# set on something that is never exec'd — a directory (a `GNUPGHOME` at 0o700),
# a `tar::Header` field that never becomes a file, or a fixture file a
# tree-walk only stats.
#
# Both spellings of the mode count, because the two are interchangeable at a
# call site and only one of them was visible to the first version of this
# audit:
#   std::fs::set_permissions(p, Permissions::from_mode(0o755))
#   let mut perms = metadata(p)?.permissions(); perms.set_mode(0o755);
# `0o[1357]` is any mode whose OWNER bits carry the execute bit, so 0o755,
# 0o700 and 0o500 all match.
#
# TEST context is lib/test-regions.awk's: a whole test file (a sibling
# `tests.rs` or `<name>_tests.rs`, anything under `crates/*/tests/`) or an
# inline `#[cfg(test)]` region bounded by the gated item's braces. Production
# chmods — the stages that stage a real binary 0755 into a package tree — are
# OUT OF SCOPE: they write artifacts nothing in the same process then execs.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"

# Files carrying a writer that is IN the class but not yet routed. Listed by
# path with the reason, printed on every run so the set stays visible, and
# checked to still exist so a rename cannot silently drop one. The list is a
# ratchet: a NEW hand-rolled writer anywhere else fails the audit.
UNROUTED=()

# The helper's own home is exempt — it IS the helper.
mapfile -t FILES < <(
    grep -rlP '(Permissions::from_mode|set_mode)\(0o[1357]' crates/ --include='*.rs' 2>/dev/null \
        | grep -v '/target/' \
        | grep -v 'crates/core/src/test_helpers/' \
        || true
)

# Drop the listed files from the scan, failing if one no longer exists.
KEPT=()
for f in "${FILES[@]}"; do
    skip=""
    for entry in ${UNROUTED[@]+"${UNROUTED[@]}"}; do
        [[ "$f" == "${entry%%|*}" ]] && skip=1 && break
    done
    [[ -n "$skip" ]] || KEPT+=("$f")
done
for entry in ${UNROUTED[@]+"${UNROUTED[@]}"}; do
    path="${entry%%|*}"
    if [[ ! -f "$path" ]]; then
        echo "audit-test-exec-writer: listed file $path no longer exists — drop or repoint its entry." >&2
        exit 1
    fi
done
FILES=(${KEPT[@]+"${KEPT[@]}"})

if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "audit-test-exec-writer: no executable-mode call sites found."
    exit 0
fi

report() {
    awk -f "$LIB_DIR/rust-lex.awk" -f "$LIB_DIR/test-regions.awk" -f - "$@" <<'AWK'
        FNR == 1 { whole_file_is_test = is_test_file(FILENAME); marker_armed = 0 }

        {
            line = $0
            in_test = (whole_file_is_test || in_test_region)
            is_comment = (line ~ /^[[:space:]]*\/\//) ? 1 : 0
            # The marker arms across the contiguous comment block directly
            # above its chmod, so a multi-line rationale need not be crammed
            # onto the call's own line.
            if (line ~ /\/\/[[:space:]]*exec-writer-ok:[[:space:]]*[^[:space:]]/) marker_armed = 1
        }

        /(Permissions::from_mode|set_mode)\(0o[1357]/ {
            if (in_test && !is_comment && !marker_armed) {
                printf("%s:%d: %s\n", FILENAME, FNR, gensub(/^[[:space:]]+/, "", 1, line))
                bad = 1
            }
        }

        {
            if (marker_armed && !is_comment && line !~ /^[[:space:]]*$/ \
                && line !~ /(Permissions::from_mode|set_mode)\(0o[1357]/) marker_armed = 0
        }

        END { exit bad ? 2 : 0 }
AWK
}

violations="$(report "${FILES[@]}" || true)"

if [[ -n "$violations" ]]; then
    echo "HAND-ROLLED EXECUTABLE WRITE IN TESTS — ETXTBSY flake under --test-threads."
    echo
    echo "$violations"
    echo
    echo "Each line above makes a file executable in TEST code without routing the"
    echo "write through the helper that drains the ETXTBSY window. A sibling test"
    echo "thread forking inside the write inherits the writable descriptor until its"
    echo "own exec, so a later spawn of this file fails with \"Text file busy\"."
    echo
    echo "Fix: write the script (shebang included) through"
    echo "  anodizer_core::test_helpers::fake_tool::write_executable_script(&path, script)"
    echo "and drop the separate write + set_permissions pair."
    echo
    echo "If the mode is set on something never exec'd (a GNUPGHOME directory, a"
    echo "fixture a tree-walk only stats), mark it with"
    echo "  // exec-writer-ok: <why>  on the line or the line above it."
    exit 1
fi

echo "audit-test-exec-writer: all ${#FILES[@]} executable-mode files route test writes through write_executable_script (or mark // exec-writer-ok:)."
if [[ ${#UNROUTED[@]} -gt 0 ]]; then
    echo "audit-test-exec-writer: ${#UNROUTED[@]} listed writer(s) still hand-rolled:"
    for entry in ${UNROUTED[@]+"${UNROUTED[@]}"}; do
        echo "  ${entry%%|*} — ${entry#*|}"
    done
fi
