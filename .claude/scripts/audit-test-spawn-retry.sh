#!/usr/bin/env bash
# Guard: every TEST-context `git`/`node` spawn routes through the spawn-retry
# helper.
#
# Contract: Windows GitHub runners intermittently fail to *create* a child
# process under heavy parallel nextest load — the loader aborts before the
# program runs, surfacing as an NTSTATUS init-failure exit code (0xC0000142
# STATUS_DLL_INIT_FAILED and kin), or on `node` an empty-stderr non-success.
# This is the OS failing to START the process, not the program erroring: a real
# `git`/`node` failure returns 1/128, never these codes. Test fixtures that
# spawn `git init` / `node --check` directly and unretried therefore flake.
#
# The fix the codebase standardises on: route every test-fixture spawn through
#   anodizer_core::test_helpers::output_with_spawn_retry(
#       || { ...Command... }, "git")
# which retries up to 5× on a transient spawn-init failure (and only those, so
# it masks no genuine error). See crates/core/src/test_helpers/mod.rs.
#
# The same helper is where a fixture `git` is cut off from the host's global
# and system config (`isolate_git_config`), so a spawn outside it also inherits
# the developer's `commit.gpgsign`, `gpg.program` and `core.hooksPath`.
#
# This audit fails (exit 1) when a `Command::new("git")` /
# `Command::new("node")` (incl. the `std::process::`-qualified form) appears in
# TEST context WITHOUT
# either:
#   - sitting inside an `output_with_spawn_retry(...)` closure body, OR
#   - carrying an inline  // spawn-retry-ok: <why>  marker (on the call's line
#     or the line directly above it) for a legitimately-unconvertible site
#     (e.g. an availability probe whose Err means "skip", not "retry").
#
# TEST context (per lib/test-regions.awk, the shared rule) = a `Command::new`
# inside the brace block of a test-only `#[cfg(…)]` item in a
# `crates/*/src/**` file, OR anywhere in a file named `tests.rs` or
# `<name>_tests.rs`, OR anywhere under `crates/*/tests/**`. A bare
# `mod tests {` with no test-only attribute is production. Production spawns
# (the real release path, which runs serially and outside nextest) are OUT OF
# SCOPE.
# The helper's own home (crates/core/src/test_helpers/) is exempt — it IS the
# helper.
#
# The second rule follows from the first: because every fixture git goes
# through the helper, every repository a fixture creates already carries the
# fixture config (`commit.gpgsign`, `tag.gpgsign`, `core.hooksPath`, an
# identity — crates/core/src/test_helpers/spawn.rs, `FIXTURE_REPO_CONFIG`).
# A test that sets one of those keys by hand is restating part of that
# config, and the part it leaves out is what the host still decides. So a
# test-context mention of one of those keys is a finding, unless the line or
# the line above it says why:  // git-config-ok: <why>
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"
ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# Candidate files: any source under crates/ that spawns git or node. The awk
# pass then decides per-file whether each call site is in test context.
# The retry helper's own home is exempt — it IS the helper. The exemption is
# a DIRECTORY NAME, not one path: a `test_helpers/` under any crate is exempt,
# on the reading that anything so named is scaffolding rather than a test.
collect_files FILES -rlE --include='*.rs' \
    --exclude-dir=target --exclude-dir=test_helpers \
    -- 'Command::new\("(git|node)"\)' crates/


# Per-file awk scan. State resets at FNR==1 (awk carries vars across files).
#
# Test-region detection is the shared lib's (lib/test-regions.awk), the same
# one audit-test-isolation.sh and the god-file scanner use: a `tests.rs`, a
# `<name>_tests.rs` or a `crates/*/tests/**` integration file is test code in
# its entirety, and inside any other file a `#[cfg(test)]` item is test code
# from the attribute to the close of its brace block — counted on `strip_code`
# output, so a brace inside a raw string cannot end the region early.
# Production code that FOLLOWS an inline test module is production again.
#
# A test-context `Command::new("git"|"node")` PASSES iff:
#   - it is inside an `output_with_spawn_retry(` closure — tracked by a small
#     window (retry_window) opened by the helper-call line and the `|| {`
#     opener, which precede the `Command::new` by a handful of lines; OR
#   - a `// spawn-retry-ok: <non-space>` marker sits on its line or the line
#     directly above it.
violations=""
((${#FILES[@]})) && run_scanner violations -f "$LIB_DIR/rust-lex.awk" -f "$LIB_DIR/test-regions.awk" -f - "${FILES[@]}" <<'AWK'
    function ltrim(s) { sub(/^[[:space:]]+/, "", s); return s }
    FNR == 1 {
        whole_file_is_test = is_test_file(FILENAME)
        prev_ok = 0; this_ok = 0
        retry_window = 0
    }

    {
        line = $0
        in_test = (whole_file_is_test || in_test_region)
        is_comment = (line ~ /^[[:space:]]*\/\//) ? 1 : 0
        # A `// spawn-retry-ok:` marker arms an exemption that stays live
        # across the contiguous comment block directly above the spawn (a
        # multi-line rationale is common), so the marker need not sit on
        # the spawn line. Any non-comment line that is NOT the spawn site
        # disarms it (handled in the Command::new block + the fall-through).
        # The marker is read off the comment half of the line, so a string
        # literal quoting it is text, not an exemption.
        if (comment_part(line) ~ /\/\/[[:space:]]*spawn-retry-ok:[[:space:]]*[^[:space:]]/) marker_armed = 1
        # Opening the helper (or its closure) starts a short exemption
        # window covering the Command::new a few lines below — 8 lines
        # tolerates a closure that binds locals before building the Command.
        if (line ~ /output_with_spawn_retry[[:space:]]*\(/ || line ~ /\|\|[[:space:]]*\{/) {
            if (retry_window < 8) retry_window = 8
        }
    }

    /Command::new\("(git|node)"\)/ {
        # A `Command::new(...)` mentioned inside a comment (`//` / `///`
        # appears before it on the line) is documentation, not a spawn.
        if (in_test && !is_comment && !retry_window && !marker_armed) {
            printf("%s:%d: %s\n", FILENAME, FNR, ltrim(line))
        }
    }

    # Disarm the spawn-retry-ok marker once a non-comment, non-blank line
    # that is NOT itself the spawn passes — the marker only covers the
    # comment block immediately preceding its spawn.
    {
        if (marker_armed && !is_comment && line !~ /^[[:space:]]*$/ && line !~ /Command::new\("(git|node)"\)/) marker_armed = 0
    }

    # Decrement the window AFTER the Command::new check so the spawn line
    # itself is still covered.
    { if (retry_window > 0) retry_window-- }
AWK

FIXTURE_KEYS='commit\.gpgsign|tag\.gpgsign|core\.hooksPath|gpg\.program'
collect_files KEY_FILES -rlE --include='*.rs' \
    --exclude-dir=target --exclude-dir=test_helpers \
    -- "$FIXTURE_KEYS" crates/
config_violations=""
if ((${#KEY_FILES[@]})); then
    # awk reads escapes in a -v value, so the backslashes are doubled.
    run_scanner config_violations -v keys="${FIXTURE_KEYS//\\/\\\\}" \
        -f "$LIB_DIR/rust-lex.awk" -f "$LIB_DIR/test-regions.awk" -f - "${KEY_FILES[@]}" <<'AWK'
        function ltrim(s) { sub(/^[[:space:]]+/, "", s); return s }
        FNR == 1 { whole_file_is_test = is_test_file(FILENAME); prev_ok = 0 }
        {
            # The key is read off the raw line: it sits inside a string literal,
            # which the code half elides. A comment naming it is prose.
            cmt = comment_part($0)
            ok = (cmt ~ /git-config-ok:[[:space:]]*[^[:space:]]/)
            in_test = (whole_file_is_test || in_test_region)
            if (in_test && code !~ /^[[:space:]]*$/ && $0 ~ ("\"[^\"]*(" keys ")")) {
                if (!ok && !prev_ok) printf("%s:%d: [fixture-config] %s\n", FILENAME, FNR, ltrim($0))
            }
            prev_ok = ok && (code ~ /^[[:space:]]*$/)
        }
AWK
fi

if [[ -n "$config_violations" ]]; then
    echo "FIXTURE GIT CONFIG SET BY HAND — the fixture helper already sets it, whole."
    echo
    echo "$config_violations"
    echo
    echo "Every repository a fixture creates through output_with_spawn_retry carries"
    echo "FIXTURE_REPO_CONFIG (crates/core/src/test_helpers/spawn.rs): signing off"
    echo "for commits and tags, hooks pinned to the repository, a fixed identity."
    echo "A line setting one of those keys restates part of it; delete the line."
    echo
    echo "A test that needs a different value says why on the line or the line above:"
    echo "  // git-config-ok: <why>"
    [[ -n "$violations" ]] && echo
fi

if [[ -n "$violations" ]]; then
    echo "UNRETRIED git/node SPAWN IN TESTS — Windows nextest process-creation flake."
    echo
    echo "$violations"
    echo
    echo "Each call above spawns git/node in TEST code without routing through the"
    echo "spawn-retry helper. On Windows CI the OS intermittently fails to *create*"
    echo "the child under parallel nextest load (NTSTATUS 0xC0000142 and kin), which"
    echo "flakes the test even though the program never ran."
    echo
    echo "Fix: wrap the spawn in"
    echo "  anodizer_core::test_helpers::output_with_spawn_retry("
    echo "      || { let mut cmd = Command::new(\"git\"); cmd.args(..).current_dir(..); cmd },"
    echo "      \"git\","
    echo "  )"
    echo "(build a FRESH Command in the closure — it is consumed by .output())."
    echo
    echo "If the site is legitimately unconvertible (e.g. an availability probe"
    echo "whose Err means \"skip the test\", not \"retry\"), mark it with"
    echo "  // spawn-retry-ok: <why>  on the call's line or the line above it."
fi
[[ -z "$violations$config_violations" ]] || exit 1

echo "audit-test-spawn-retry: all ${#FILES[@]} git/node-spawning files route test fixtures through output_with_spawn_retry (or mark // spawn-retry-ok:)."
