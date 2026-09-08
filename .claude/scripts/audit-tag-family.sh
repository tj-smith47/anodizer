#!/usr/bin/env bash
# Guard: a crate's tag family is read through ONE accessor.
#
# Contract (crates/core/src/config/build.rs): `CrateConfig::tag_family_template()`
# is the only answer to "what tag family does this crate release under". The raw
# `tag_template` field is an Option that two folds fill at config load
# (`defaults.crates.tag_template`, then the repo-level family derived from
# `tag.tag_prefix` / a Cargo lockstep workspace); a consumer that reads the raw
# field supplies its own fallback for `None`, and every such fallback has drifted
# from the accessor at least once — crate selection that matched no crate, a
# per-crate tag engine that minted one colliding `v<version>` for every group.
#
# This audit fails (exit 1) on every `.tag_template` read in production code
# under crates/*/src that is neither
#   - inside an allow-listed FUNCTION (TAG_FAMILY_RAW_OK below, keyed by
#     `<file>::<enclosing fn>` — never by line number, which rots on the first
#     edit above it), nor
#   - tagged with a `// tag-family-ok: <why>` marker on the line or the line
#     directly above it.
#
# Not scanned: Cargo test targets (crates/*/tests/**), sibling test files
# (`tests.rs`, `*_tests.rs`), inline `#[cfg(test)] mod … { … }` bodies (everything
# from that module's opening line to end of file), and comment lines.
#
# The prefix form of the accessor is `git::per_crate_tag_prefix(name, &family)`;
# `Config::repo_tag_prefix()` is the repo-level prefix. Anything else that
# answers "family for a crate" is the defect this guard exists to catch.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# `<file>::<fn>` — one entry per function that must read the raw field, with why.
TAG_FAMILY_RAW_OK=(
    "crates/core/src/config/build.rs::tag_family_template — the accessor itself"
    "crates/core/src/defaults_merge.rs::apply_to_crate — WRITES the field (defaults.crates.tag_template fold)"
    "crates/core/src/config/accessors.rs::populate_derived_tag_templates — WRITES the field (derived repo-level family)"
    "crates/cli/src/commands/tag/repo_shape.rs::prefix_groups — shape detection asks what a shared namespace was DECLARED as; the <name>-v fallback is never shared"
    "crates/cli/src/commands/tag/repo_shape.rs::shared_tag_prefix — same: a declared-prefix question"
    "crates/cli/src/commands/check/config/structure.rs::check_workspaces — validates the operator's WRITTEN template"
    "crates/cli/src/commands/check/config/structure.rs::check_top_level_tag_templates — validates the operator's WRITTEN template"
    "crates/stage-release/src/github/spec.rs::contains — NightlyRetentionFamily's own &str field, not CrateConfig"
    "crates/stage-release/src/github/spec.rs::excluded_prefixes — NightlyRetentionFamily's own &str field, not CrateConfig"
    "crates/stage-release/src/github/spec.rs::describe — NightlyRetentionFamily's own &str field, not CrateConfig"
)

allow_keys="$(printf '%s\n' "${TAG_FAMILY_RAW_OK[@]}" | sed 's/ — .*$//')"

mapfile -t FILES < <(
    grep -rlE '\.tag_template' crates/*/src --include='*.rs' 2>/dev/null \
        | grep -vE '(/tests/|/tests\.rs$|_tests\.rs$)' \
        || true
)
if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "audit-tag-family: no raw tag_template reads found."
    exit 0
fi

violations="$(
awk -v allow="$allow_keys" '
    BEGIN {
        n = split(allow, keys, "\n")
        for (i = 1; i <= n; i++) ok[keys[i]] = 1
    }
    function trim(s) { sub(/^[[:space:]]+/, "", s); return s }

    FNR == 1 { fname = ""; cfgtest = 0; skipping = 0; prev = "" }

    skipping { next }

    # A test-only cfg gate: the next `mod … {` is an inline test module and
    # runs to end of file; any other single gated line is skipped on its own.
    /^[[:space:]]*#\[cfg\((all\()?test[,)]/ { cfgtest = 1; prev = $0; next }
    cfgtest {
        cfgtest = 0
        if ($0 ~ /^[[:space:]]*(pub(\([a-z]+\))? )?mod [A-Za-z0-9_]+ \{/) { skipping = 1 }
        prev = $0
        next
    }

    /^[[:space:]]*(pub(\([a-z]+\))? )?(async )?(const )?fn [A-Za-z0-9_]+/ {
        match($0, /fn [A-Za-z0-9_]+/)
        fname = substr($0, RSTART + 3, RLENGTH - 3)
    }

    /\.tag_template/ && $0 !~ /^[[:space:]]*\/\// {
        key = FILENAME "::" fname
        if (!(key in ok) && $0 !~ /tag-family-ok:/ && prev !~ /tag-family-ok:/)
            printf("%s:%d (fn %s): %s\n", FILENAME, FNR, fname, trim($0))
    }

    { prev = $0 }
' "${FILES[@]}"
)"

if [[ -n "$violations" ]]; then
    echo "RAW TAG FAMILY READ — a crate's tag family has one accessor."
    echo
    echo "$violations"
    echo
    echo "These lines read CrateConfig.tag_template directly. The field is an Option"
    echo "that two config-load folds fill; a raw read supplies its own fallback and"
    echo "puts this surface in a different tag family from every other one."
    echo
    echo "Fix: read crate_cfg.tag_family_template() (or"
    echo "git::per_crate_tag_prefix(&name, &crate_cfg.tag_family_template()) for the"
    echo "prefix form). A read that must see the WRITTEN value — the accessor, a fold"
    echo "that writes the field, shape detection, config validation — is listed in"
    echo "TAG_FAMILY_RAW_OK in .claude/scripts/audit-tag-family.sh by file::fn; a"
    echo "one-off exemption tags the line, or the line directly above it, with"
    echo "  // tag-family-ok: <why>"
    echo "See .claude/rules/tag-family-ssot.md."
    exit 1
fi

echo "audit-tag-family: every production tag-family read goes through the accessor."
