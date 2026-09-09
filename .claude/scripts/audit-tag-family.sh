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
# (`tests.rs`, `*_tests.rs`), test-only gated items — an inline
# `#[cfg(test)] mod … { … }` body through its closing brace, with scanning
# resuming after it (lib/test-regions.awk) — and comment lines.
#
# The prefix form of the accessor is `git::per_crate_tag_prefix(name, &family)`;
# `Config::repo_tag_prefix()` is the repo-level prefix. Anything else that
# answers "family for a crate" is the defect this guard exists to catch.
#
# Second rule, same shape: the repo-level prefix is composed ONCE, in
# `Config::repo_tag_prefix()` ("tag.tag_prefix else v"). Any other
# `unwrap_or("v")` / `unwrap_or_else(|| "v")` / `DEFAULT_TAG_PREFIX` fallback
# in production code is a second copy of that composition and fails unless its
# enclosing function is listed in TAG_PREFIX_RAW_OK or the line carries the
# same `// tag-family-ok: <why>` marker.
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"
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

# `<file>::<fn>` — the only functions allowed to spell "<prefix> else v".
TAG_PREFIX_RAW_OK=(
    "crates/core/src/config/accessors.rs::repo_tag_prefix — the composition itself"
    "crates/core/src/config/accessors.rs::derived_repo_tag_prefix — the fold's lockstep rung mints the v family from the same constant"
    "crates/cli/src/commands/tag/repo_shape.rs::check_shared_prefix_version_coherence — message-only: the DECLARED shared prefix else v, a different question from the repo prefix"
)

allow_keys="$(printf '%s\n' "${TAG_FAMILY_RAW_OK[@]}" | sed 's/ — .*$//')"
prefix_keys="$(printf '%s\n' "${TAG_PREFIX_RAW_OK[@]}" | sed 's/ — .*$//')"

collect_files FILES -rlE --include='*.rs' \
    --exclude-dir=tests --exclude-dir=target --exclude='tests.rs' --exclude='*_tests.rs' \
    -- '\.tag_template|DEFAULT_TAG_PREFIX|unwrap_or(_else)?\((\|\| *)?"v"' crates/*/src
if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "audit-tag-family: no raw tag_template reads or tag-prefix compositions found."
    exit 0
fi

run_scanner violations -v allow="$allow_keys" -v pallow="$prefix_keys" -f "$LIB_DIR/rust-lex.awk" -v skip_test_regions=1 -f "$LIB_DIR/test-regions.awk" -f - "${FILES[@]}" <<'AWK'
    BEGIN {
        n = split(allow, keys, "\n")
        for (i = 1; i <= n; i++) ok[keys[i]] = 1
        n = split(pallow, keys, "\n")
        for (i = 1; i <= n; i++) pok[keys[i]] = 1
    }
    function trim(s) { sub(/^[[:space:]]+/, "", s); return s }

    FNR == 1 { fname = ""; prev_cmt = "" }

    # The marker is read off the comment half of the line — a string literal
    # quoting `tag-family-ok:` is text, not an exemption — and `prev_cmt`
    # carries the line above's comment so a marker may sit there instead.
    { cmt = comment_part($0) }

    /^[[:space:]]*(pub(\([a-z]+\))? )?(async )?(const )?fn [A-Za-z0-9_]+/ {
        match($0, /fn [A-Za-z0-9_]+/)
        fname = substr($0, RSTART + 3, RLENGTH - 3)
    }

    /\.tag_template/ && $0 !~ /^[[:space:]]*\/\// {
        key = FILENAME "::" fname
        if (!(key in ok) && cmt !~ /tag-family-ok:/ && prev_cmt !~ /tag-family-ok:/)
            printf("%s:%d (fn %s): %s\n", FILENAME, FNR, fname, trim($0))
    }

    # The constant declaration is the one place the literal is allowed to live.
    (/DEFAULT_TAG_PREFIX/ || /unwrap_or(_else)?\((\|\| *)?"v"/) && $0 !~ /^[[:space:]]*\/\// && $0 !~ /const DEFAULT_TAG_PREFIX/ {
        key = FILENAME "::" fname
        if (!(key in pok) && cmt !~ /tag-family-ok:/ && prev_cmt !~ /tag-family-ok:/)
            printf("%s:%d (fn %s): [prefix composition] %s\n", FILENAME, FNR, fname, trim($0))
    }

    { prev_cmt = cmt }
AWK

if [[ -n "$violations" ]]; then
    echo "RAW TAG FAMILY READ — a crate's tag family has one accessor."
    echo
    echo "$violations"
    echo
    echo "These lines read CrateConfig.tag_template directly, or re-compose the"
    echo "repo tag prefix (\"tag.tag_prefix else v\", marked [prefix composition])."
    echo "The field is an Option that two config-load folds fill; a raw read supplies"
    echo "its own fallback and puts this surface in a different tag family from every"
    echo "other one. A second prefix composition drifts the same way."
    echo
    echo "Fix: read crate_cfg.tag_family_template() (or"
    echo "git::per_crate_tag_prefix(&name, &crate_cfg.tag_family_template()) for the"
    echo "prefix form) and config.repo_tag_prefix() for the repo-level prefix. A read"
    echo "that must see the WRITTEN value — the accessor, a fold that writes the"
    echo "field, shape detection, config validation — is listed in TAG_FAMILY_RAW_OK"
    echo "(or TAG_PREFIX_RAW_OK) in .claude/scripts/audit-tag-family.sh by file::fn;"
    echo "a one-off exemption tags the line, or the line directly above it, with"
    echo "  // tag-family-ok: <why>"
    echo "See .claude/rules/tag-family-ssot.md."
    exit 1
fi

echo "audit-tag-family: every production tag-family read goes through the accessor; the repo tag prefix is composed once."
