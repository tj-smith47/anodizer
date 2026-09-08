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
# (`tests.rs`, `*_tests.rs`), inline `#[cfg(test)] mod … { … }` bodies
# (everything from that module's opening line to end of file), and comment
# lines.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# `<file>::<fn>` — the one function that must read the raw key.
BINARY_READ_OK=(
    "crates/core/src/artifact/registry.rs::binary_name — the accessor itself"
)

allow_keys="$(printf '%s\n' "${BINARY_READ_OK[@]}" | sed 's/ — .*$//')"

mapfile -t FILES < <(
    grep -rlE '\.get\("binary"\)|\["binary"\]|contains_key\("binary"\)|remove\("binary"\)' crates/*/src --include='*.rs' 2>/dev/null \
        | grep -vE '(/tests/|/tests\.rs$|_tests\.rs$)' \
        || true
)
if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "audit-binary-name: no raw binary-name reads found (the accessor itself is missing?)."
    exit 1
fi

violations="$(
awk -v allow="$allow_keys" '
    BEGIN {
        n = split(allow, keys, "\n")
        for (i = 1; i <= n; i++) ok[keys[i]] = 1
    }
    function trim(s) { sub(/^[[:space:]]+/, "", s); return s }

    FNR == 1 { fname = ""; cfgtest = 0; skipping = 0 }

    skipping { next }

    # A test-only cfg gate: the next `mod … {` is an inline test module and
    # runs to end of file; any other single gated line is skipped on its own.
    /^[[:space:]]*#\[cfg\((all\()?test[,)]/ { cfgtest = 1; next }
    cfgtest {
        cfgtest = 0
        if ($0 ~ /^[[:space:]]*(pub(\([a-z]+\))? )?mod [A-Za-z0-9_]+ \{/) { skipping = 1 }
        next
    }

    /^[[:space:]]*(pub(\([a-z]+\))? )?(async )?(const )?fn [A-Za-z0-9_]+/ {
        match($0, /fn [A-Za-z0-9_]+/)
        fname = substr($0, RSTART + 3, RLENGTH - 3)
    }

    (/\.get\("binary"\)/ || /\["binary"\]/ || /contains_key\("binary"\)/ || /remove\("binary"\)/) && $0 !~ /^[[:space:]]*\/\// {
        key = FILENAME "::" fname
        if (!(key in ok))
            printf("%s:%d (fn %s): %s\n", FILENAME, FNR, fname, trim($0))
    }
' "${FILES[@]}"
)"

accessor_hits="$(grep -c '\.get("binary")' crates/core/src/artifact/registry.rs || true)"
if [[ "$accessor_hits" -ne 1 ]]; then
    echo "audit-binary-name: expected exactly one raw read inside Artifact::binary_name, found $accessor_hits in crates/core/src/artifact/registry.rs"
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
