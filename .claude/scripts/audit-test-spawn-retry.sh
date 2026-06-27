#!/usr/bin/env bash
# Guard: every TEST-context `git`/`node` spawn routes through the spawn-retry helper.
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
#   anodizer_core::test_helpers::output_with_spawn_retry(|| { ...Command... }, "git")
# which retries up to 5× on a transient spawn-init failure (and only those, so
# it masks no genuine error). See crates/core/src/test_helpers/mod.rs.
#
# This audit fails (exit 1) when a `Command::new("git")` / `Command::new("node")`
# (incl. the `std::process::`-qualified form) appears in TEST context WITHOUT
# either:
#   - sitting inside an `output_with_spawn_retry(...)` closure body, OR
#   - carrying an inline  // spawn-retry-ok: <why>  marker (on the call's line
#     or the line directly above it) for a legitimately-unconvertible site
#     (e.g. an availability probe whose Err means "skip", not "retry").
#
# TEST context = a `Command::new` after a `#[cfg(test)]` / `mod tests {`
# boundary in a `crates/*/src/**` file, OR any file under `crates/*/tests/**`,
# OR a file named `tests.rs`. Production spawns (the real release path, which
# runs serially and not under nextest) are OUT OF SCOPE. The helper's own home
# (crates/core/src/test_helpers/) is exempt — it IS the helper.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# Candidate files: any source under crates/ that spawns git or node. The awk
# pass then decides per-file whether each call site is in test context.
mapfile -t FILES < <(
    grep -rlP 'Command::new\("(git|node)"\)' crates/ --include='*.rs' 2>/dev/null \
        | grep -v '/target/' \
        | grep -v 'crates/core/src/test_helpers/' \
        || true
)

if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "audit-test-spawn-retry: no git/node spawn call sites found."
    exit 0
fi

# Per-file awk scan. State resets at FNR==1 (awk carries vars across files).
#
# Test-region detection mirrors audit-test-isolation.sh: a file's production
# prologue precedes its `#[cfg(test)]` / `mod tests {` boundary; everything from
# that boundary to EOF is test code. A `tests.rs` file or a `crates/*/tests/**`
# integration file is test code in its entirety (in_test forced on at FNR==1).
# `in_test` latches on and never resets: a production spawn placed *below* a
# test mod would false-positive, but that errs safe (a loud, fixable commit
# block) — brace-depth tracking over Rust raw strings could miscount and let a
# real test spawn slip through, which is the failure mode that must never occur.
#
# A test-context `Command::new("git"|"node")` PASSES iff:
#   - it is inside an `output_with_spawn_retry(` closure — tracked by a small
#     window (retry_window) opened by the helper-call line and the `|| {`
#     opener, which precede the `Command::new` by a handful of lines; OR
#   - a `// spawn-retry-ok: <non-space>` marker sits on its line or the line
#     directly above it.
report() {
    awk '
        function is_test_file(f) {
            return (f ~ /\/tests\.rs$/) || (f ~ /\/crates\/[^/]+\/tests\//)
        }

        FNR == 1 {
            in_test = is_test_file(FILENAME)
            prev_ok = 0; this_ok = 0
            retry_window = 0
            pending_cfg_test = 0
        }

        {
            line = $0
            is_comment = (line ~ /^[[:space:]]*\/\//) ? 1 : 0
            # A `// spawn-retry-ok:` marker arms an exemption that stays live
            # across the contiguous comment block directly above the spawn (a
            # multi-line rationale is common), so the marker need not sit on
            # the spawn line. Any non-comment line that is NOT the spawn site
            # disarms it (handled in the Command::new block + the fall-through).
            if (line ~ /\/\/[[:space:]]*spawn-retry-ok:[[:space:]]*[^[:space:]]/) marker_armed = 1
            # Opening the helper (or its closure) starts a short exemption
            # window covering the Command::new a few lines below — 8 lines
            # tolerates a closure that binds locals before building the Command.
            if (line ~ /output_with_spawn_retry[[:space:]]*\(/ || line ~ /\|\|[[:space:]]*\{/) {
                if (retry_window < 8) retry_window = 8
            }
        }

        # Enter test context only when a `#[cfg(test)]` attribute introduces an
        # INLINE module/item (the next code line opens a `{` block), NOT an
        # external `mod tests;` declaration (its body is a separate tests.rs,
        # already covered by the is_test_file path). The attribute and the item
        # sit on adjacent lines, so a one-line `pending_cfg_test` latch bridges
        # them; an intervening `mod NAME;` cancels it (external module).
        /#\[cfg\(test\)\]/ {
            if (line ~ /\{[[:space:]]*$/) in_test = 1   # same-line `#[cfg(test)] mod x {`
            else if (line ~ /;[[:space:]]*$/) { }       # same-line external decl — ignore
            else pending_cfg_test = 1
            next
        }
        pending_cfg_test {
            if (line ~ /\{[[:space:]]*$/) in_test = 1    # inline block follows the attr
            pending_cfg_test = 0
        }

        /Command::new\("(git|node)"\)/ {
            # A `Command::new(...)` mentioned inside a comment (`//` / `///`
            # appears before it on the line) is documentation, not a spawn.
            if (in_test && !is_comment && !retry_window && !marker_armed) {
                printf("%s:%d: %s\n", FILENAME, FNR, gensub(/^[[:space:]]+/, "", 1, line))
                bad = 1
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

        END { exit bad ? 2 : 0 }
    ' "$@"
}

violations="$(report "${FILES[@]}" || true)"

# awk exits 2 on a finding; re-derive pass/fail from emptiness so `set -e` does
# not abort on the expected non-zero status.
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
    exit 1
fi

echo "audit-test-spawn-retry: all ${#FILES[@]} git/node-spawning files route test fixtures through output_with_spawn_retry (or mark // spawn-retry-ok:)."
