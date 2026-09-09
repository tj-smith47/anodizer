#!/usr/bin/env bash
# Guard: an artifact's binary name is read through ONE accessor.
#
# Contract (crates/core/src/artifact/registry.rs): `Artifact::binary_name()`
# is the only answer to "what binary is this artifact". It reads the `binary`
# metadata the build stage records and, for a binary-like kind, falls back to
# the on-disk file name with a trailing `.exe` removed. Every raw read of the
# key supplied its own fallback — "" in one filter, the crate name in a bail,
# the file name WITH `.exe` in a template var — and each of those fallbacks
# named the same binary differently on the same run.
#
# This audit fails (exit 1) on every raw READ of the metadata key in
# production code under crates/*/src — `.get("binary")`, `["binary"]`,
# `contains_key("binary")`, `remove("binary")` — outside the accessor itself
# (BINARY_READ_OK below, keyed by `<file>::<enclosing fn>`, never by line
# number). Writes (`insert("binary", …)`, the producer side in stage-build and
# the per-binary registration in stage-archive) are not reads and are not
# matched.
#
# Not scanned: Cargo test targets (crates/*/tests/**), sibling test files
# (`tests.rs`, `*_tests.rs`), test-only gated items — an inline
# `#[cfg(test)] mod … { … }` body through its closing brace, with scanning
# resuming after it (lib/test-regions.awk) — and comment lines.
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"
ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# `<file>::<fn>` — the one function that must read the raw key.
BINARY_READ_OK=(
    "crates/core/src/artifact/registry.rs::binary_name — the accessor itself"
)

allow_keys="$(printf '%s\n' "${BINARY_READ_OK[@]}" | sed 's/ — .*$//')"

collect_files FILES -rlE --include='*.rs' \
    --exclude-dir=tests --exclude-dir=target --exclude='tests.rs' --exclude='*_tests.rs' \
    -- '\.get\("binary"\)|\["binary"\]|contains_key\("binary"\)|remove\("binary"\)' crates/*/src
if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "audit-binary-name: no raw binary-name reads found (the accessor itself is missing?)."
    exit 1
fi

run_scanner violations -v allow="$allow_keys" -f "$LIB_DIR/rust-lex.awk" -v skip_test_regions=1 -f "$LIB_DIR/test-regions.awk" -f - "${FILES[@]}" <<'AWK'
    BEGIN {
        n = split(allow, keys, "\n")
        for (i = 1; i <= n; i++) ok[keys[i]] = 1
    }
    function trim(s) { sub(/^[[:space:]]+/, "", s); return s }

    FNR == 1 { fname = "" }

    /^[[:space:]]*(pub(\([a-z]+\))? )?(async )?(const )?fn [A-Za-z0-9_]+/ {
        match($0, /fn [A-Za-z0-9_]+/)
        fname = substr($0, RSTART + 3, RLENGTH - 3)
    }

    (/\.get\("binary"\)/ || /\["binary"\]/ || /contains_key\("binary"\)/ || /remove\("binary"\)/) && $0 !~ /^[[:space:]]*\/\// {
        key = FILENAME "::" fname
        if (!(key in ok))
            printf("%s:%d (fn %s): %s\n", FILENAME, FNR, fname, trim($0))
    }
AWK

# A REQUIRED root, unlike the globs above: an absent one means the accessor
# moved, and a count of zero from a file that is not there would read as a
# violation of the one-accessor rule rather than as the rename it is.
REGISTRY="crates/core/src/artifact/registry.rs"
if [[ ! -f "$REGISTRY" ]]; then
    echo "audit-binary-name: ${REGISTRY} not found; the scan did not run." >&2
    exit 2
fi

collect_files ACCESSOR_READS -n -- '\.get("binary")' "$REGISTRY"
accessor_hits=${#ACCESSOR_READS[@]}
if [[ "$accessor_hits" -ne 1 ]]; then
    echo "audit-binary-name: expected exactly one raw read inside Artifact::binary_name, found $accessor_hits in $REGISTRY"
    exit 1
fi

if [[ -n "$violations" ]]; then
    echo "RAW BINARY-NAME READ — an artifact's binary name has one accessor."
    echo
    echo "$violations"
    echo
    echo "These lines read metadata[\"binary\"] directly and supply their own"
    echo "fallback for a missing key, so the same binary is named differently on"
    echo "different surfaces of one run."
    echo
    echo "Fix: read artifact.binary_name() (Option — metadata, else the file name"
    echo "without .exe for a binary-like kind) and keep only the LAST-RESORT"
    echo "substitution (a crate or package name) at the call site. The accessor"
    echo "is the one function listed in BINARY_READ_OK in"
    echo ".claude/scripts/audit-binary-name.sh."
    exit 1
fi

echo "audit-binary-name: ${#FILES[@]} file(s) scanned; every binary-name read goes through Artifact::binary_name."
