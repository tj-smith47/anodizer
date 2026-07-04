#!/usr/bin/env bash
# Guard: no unjustified process-global mutation in test code.
#
# `cargo test` parallelises tests within a single binary, so any test that
# mutates PROCESS-GLOBAL state races against other tests in the same binary.
# Two classes of such state are governed here, each by its own inline marker:
#
#   env  — `std::env::set_var` / `remove_var`           marker: // env-ok: <why>
#   cwd  — `std::env::set_current_dir`                  marker: // cwd-ok: <why>
#
# ── env class ──────────────────────────────────────────────────────────────
# A test that mutates the process environment races other tests reading those
# variables. The npm #111 flake and the
# `close_pr_via_api_failed_when_target_unreachable` flake were both this class.
# The fix the codebase standardises on, in order of preference:
#   1. Route the var through an `EnvSource` seam: call the fn's `*_with_env`
#      variant with an `anodizer_core::MapEnvSource`. The test injects the value
#      and contains NO `set_var`/`remove_var` — invisible to this audit, the
#      preferred outcome.
#   2. No injection seam (`PATH` for binary stubbing, `GIT_*` identity, a var a
#      spawned child reads): annotate the enclosing test
#      `#[serial_test::serial(<group>)]`, grouped by shared resource.
#   3. Provably-safe-without-serialisation (an idempotent `OnceLock` set of
#      constant values, or an `env_mutex().lock()`-held block): leave the
#      mutation in place.
# Every remaining `set_var`/`remove_var` in TEST code MUST carry an inline
# `// env-ok: <why>` marker naming the governance.
#
# ── cwd class ──────────────────────────────────────────────────────────────
# A test that swaps the process cwd (`set_current_dir`) races every other cwd
# swapper in the same binary: one test can capture another's soon-to-be-deleted
# tempdir as its restore target and fail `NotFound` on restore (the v0.12.x
# `resolve_git_context_no_tag_non_snapshot_bails` flake). The fix the codebase
# standardises on, in order of preference:
#   1. Use the RAII `anodizer_core::test_helpers::CwdGuard` (swap on `new`,
#      panic-safe restore on `Drop`). The raw `set_current_dir` lives only inside
#      the guard — invisible to this audit at the call site, the preferred
#      outcome.
#   2. If a raw `set_current_dir` is unavoidable in a test, serialise the
#      enclosing test under the workspace-canonical `#[serial_test::serial(cwd)]`
#      group (one cwd key per binary — see `CwdGuard`'s rustdoc).
# Every remaining `set_current_dir` in TEST code MUST carry an inline
# `// cwd-ok: <why>` marker naming the governance (the serial(cwd) group, or that
# it is the blessed CwdGuard implementation).
#
# In BOTH classes the marker (on the call's own line, or the line directly above
# it) makes "why is this race-free?" explicit and grep-auditable at the call
# site instead of inferred from fragile scope heuristics. A bare marker with no
# reason is rejected.
#
# Production mutations (outside `#[cfg(test)]` / `mod tests` — e.g. the CLI
# resolving `TARGET` at single-threaded startup so user hooks inherit it) are
# OUT OF SCOPE: this audit only scans test regions, detected by a `mod tests {`
# / `#[cfg(test)]` boundary.
#
# ── cwd-helper pairing class ────────────────────────────────────────────────
# The raw-call-site cwd class above is blind to a cwd swap performed through a
# helper fn rather than a direct `set_current_dir` — exactly how the cwd class
# 2 fix (CwdGuard) is meant to be consumed, so most swaps route through a
# helper and never hit the raw-call scan at all. `auto_detect_github_fills_
# workspace_crates_from_remote` landed calling `with_empty_git_repo_cwd`
# without `#[cfg(unix)]` (helper is unix-only: it shells to `git`) or
# `#[serial_test::serial(cwd)]` (the shared restore-race group) and neither
# class above caught it. This class enforces, per allow-listed helper, that
# every `#[test]` fn calling it carries BOTH attributes. See
# `report_cwd_helper_pairing` below for the state machine.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

mapfile -t FILES < <(grep -rlP 'std::env::(set_var|remove_var|set_current_dir)\(' crates/*/src --include='*.rs' 2>/dev/null || true)

# No global early-exit on an empty FILES: the cwd-helper pairing check below
# is independent of raw set_current_dir/set_var call sites (its whole point
# is catching swaps that route through a helper INSTEAD of a raw call — the
# very migration path this script's own [cwd] fix recommends), so it must
# still run even when FILES is empty.

# Per-file awk scan. State is reset at FNR==1 because awk carries variables
# across files in a multi-file invocation.
#
# Test-region detection: a file's production prologue precedes its
# `#[cfg(test)]` / `mod tests {` boundary; everything from that boundary to EOF
# is test code (the test module is conventionally the file's tail). Production
# mutation before the boundary is exempt.
#
# A test-region `set_var`/`remove_var` PASSES iff an `// env-ok: <non-space>`
# marker sits on its line or the line directly above it; a test-region
# `set_current_dir` PASSES iff a `// cwd-ok: <non-space>` marker does. No
# enclosing-scope inference (brace/`fn` counting corrupts on `fn`/`{` inside
# string literals, e.g. an embedded stub-program source) — the explicit marker
# is the contract.
report() {
    awk '
        FNR == 1 { in_test = 0; prev_envok = 0; this_envok = 0; prev_cwdok = 0; this_cwdok = 0 }

        {
            line = $0
            prev_envok = this_envok
            this_envok = (line ~ /\/\/[[:space:]]*env-ok:[[:space:]]*[^[:space:]]/) ? 1 : 0
            prev_cwdok = this_cwdok
            this_cwdok = (line ~ /\/\/[[:space:]]*cwd-ok:[[:space:]]*[^[:space:]]/) ? 1 : 0
        }

        /#\[cfg\(test\)\]/         { in_test = 1 }
        /\<mod[[:space:]]+tests\>/ { in_test = 1 }

        /std::env::(set_var|remove_var)\(/ {
            if (!in_test) next                  # production startup code
            if (this_envok || prev_envok) next  # justified at the call site
            printf("%s:%d: [env] %s\n", FILENAME, FNR, gensub(/^[[:space:]]+/, "", 1, line))
            bad = 1
        }

        /std::env::set_current_dir\(/ {
            if (!in_test) next                  # production / library code
            if (this_cwdok || prev_cwdok) next  # justified at the call site
            printf("%s:%d: [cwd] %s\n", FILENAME, FNR, gensub(/^[[:space:]]+/, "", 1, line))
            bad = 1
        }

        END { exit bad ? 2 : 0 }
    ' "$@"
}

# The #[cfg(unix)]-gated cwd-swap test helpers in
# crates/cli/src/commands/helpers.rs. WHY a named allow-list rather than
# discovering "any fn matching with_*_cwd": a third helper joining this club
# is a deliberate policy decision (it inherits the unix-only + serial(cwd)
# contract), so adding one here is the one-line acknowledgment of that, not
# silent pattern-matching.
CWD_SWAP_HELPERS=(
    with_empty_git_repo_cwd
    with_tagged_dirty_repo_cwd
)

helper_alt="$(IFS='|'; echo "${CWD_SWAP_HELPERS[*]}")"

# Per-file awk state machine over crates/cli/src/commands/helpers.rs (or any
# future file reaching one of CWD_SWAP_HELPERS): tracks the attribute block
# immediately preceding each top-level `    fn NAME` line (4-space indent —
# the mod-tests fn nesting depth in this file; a deeper nested `fn` would
# defeat this, but the test module here has none) and the set of
# CWD_SWAP_HELPERS calls textually inside that fn's body (bounded by the next
# top-level fn line or EOF). A fn is flagged iff it is #[test], its body
# calls an allow-listed helper, and its attribute block lacks #[cfg(unix)] or
# serial(cwd).
#
# Attribute-block accumulation is order- and interleaving-robust: `#[...]`
# lines OR-accumulate flags (any order), `///` doc-comment lines and blank
# lines are transparent (skipped without resetting), and any other line
# resets the pending flags to 0 — so a stale attribute block never leaks
# into an unrelated fn. The two helper DEFINITIONS themselves are excluded
# without name-matching: their `fn with_..._cwd(` line is consumed by the
# fn-boundary rule (which `next`s before the call-scanning catch-all), so a
# definition's own signature is never scored as a call inside a tracked body,
# and definitions carry no #[test] so `fn_is_test` gates them out regardless.
#
# Fail-closed structural backstop: the fn-boundary rule only recognizes a
# 4-space-indent `    fn NAME` (the flat mod-tests nesting depth), so a
# `#[test]` fn nested one level deeper (inside a sub-`mod` in the test
# module) is never tracked as its own fn — its body folds into whichever
# 4-indent fn was last open, so a genuinely unattributed nested test would
# silently pass. Per file, the count of `#[test]` lines (any indent) vs the
# count of tracked fns whose attribute block carried `#[test]` must match;
# a mismatch means the flat-module assumption broke and the scanner can no
# longer trust what it attributed, so it fails closed instead of passing on
# data it can't faithfully parse. The check is PER FILE, not global: a global
# net-zero could offset a +1 in one file against a -1 in another and silently
# pass a real defect (fail-open), so each file's counts are compared on its
# own at the file boundary.
#
# Scope contract: matching is by helper NAME only (a literal `NAME(`
# substring), not import resolution — an aliased import
# (`use ...with_empty_git_repo_cwd as x; x(...)`) would not be recognized as
# a call. No such alias exists in-tree; adding one under a new name requires
# adding that name to CWD_SWAP_HELPERS.
report_cwd_helper_pairing() {
    local helper_alt="$1"
    shift
    awk -v helper_alt="$helper_alt" '
        BEGIN { n_helpers = split(helper_alt, helpers, "|") }

        # A dynamic `\<name\(`-style regex built via -v is NOT usable here:
        # gawk C-escape-processes -v assignments, silently stripping the
        # backslashes before the regex engine ever sees them (confirmed via
        # `awk: warning: escape sequence .. treated as plain`). Word-boundary
        # + literal-paren matching is done in plain string ops instead.
        function is_word_char(c) { return c ~ /[A-Za-z0-9_]/ }

        function line_calls_helper(line,    i, name, off, pos, before) {
            for (i = 1; i <= n_helpers; i++) {
                name = helpers[i]
                off = 0
                while (1) {
                    pos = index(substr(line, off + 1), name "(")
                    if (pos == 0) break
                    pos += off
                    before = (pos == 1) ? "" : substr(line, pos - 1, 1)
                    if (before == "" || !is_word_char(before)) return 1
                    off = pos
                }
            }
            return 0
        }

        function finalize() {
            if (have_fn && fn_is_test) file_tracked_test_fns++
            if (have_fn && fn_is_test && fn_calls_helper && !(fn_has_cfg_unix && fn_has_serial_cwd)) {
                missing = ""
                if (!fn_has_cfg_unix)   missing = missing " #[cfg(unix)]"
                if (!fn_has_serial_cwd) missing = missing " #[serial_test::serial(cwd)]"
                printf("%s:%d: fn %s calls a cwd-swap helper but is missing:%s\n", fn_file, fn_line, fn_name, missing)
                bad = 1
            }
        }

        # Per-file (never global) structural cross-check. Counting per file is
        # the fail-closed direction: a global net-zero could hide +1 in one
        # file and -1 in another, silently passing a nested-mod defect. `f` is
        # the file that just ENDED — at FNR==1 it is the PREVIOUS file (FILENAME
        # has already advanced), so the ended file is named via `cur_file`.
        function check_counts(f) {
            if (file_test_attrs != file_tracked_test_fns) {
                printf("%s: unexpected test-module structure: %d #[test] attrs but %d attributed to a flat 4-space fn; the cwd-helper pairing scanner assumes a flat, 4-space-indented test module (no nested `mod`) — flatten the test module or extend report_cwd_helper_pairing\n", f, file_test_attrs, file_tracked_test_fns)
                bad = 1
            }
        }

        FNR == 1 {
            finalize()             # flush the prior file last tracked fn …
            check_counts(cur_file) # … then compare that file attrs-vs-tracked
            file_test_attrs = 0; file_tracked_test_fns = 0
            pend_test = 0; pend_cfg_unix = 0; pend_serial_cwd = 0
            have_fn = 0; fn_name = ""; fn_line = 0; fn_file = ""
            fn_is_test = 0; fn_has_cfg_unix = 0; fn_has_serial_cwd = 0; fn_calls_helper = 0
        }

        # Counted AFTER the FNR==1 reset so a `#[test]` on the first line of a
        # file is not wiped; matches at ANY indent (a nested-mod #[test] the
        # 4-space fn-boundary rule can never attribute still bumps this count).
        /#\[test\]/ { file_test_attrs++ }

        {
            line = $0
            cur_file = FILENAME
        }

        /^[[:space:]]*#\[/ {
            if (line ~ /#\[test\]/)       pend_test = 1
            if (line ~ /#\[cfg\(unix\)\]/) pend_cfg_unix = 1
            if (line ~ /serial\(cwd\)/)   pend_serial_cwd = 1
            next
        }

        /^[[:space:]]*\/\/\// { next }  # doc comment: transparent to the pending attribute block
        /^[[:space:]]*$/      { next }  # blank line: transparent

        match(line, /^    fn [A-Za-z_][A-Za-z_0-9]*/) {
            finalize()
            fn_name = substr(line, RSTART + 7, RLENGTH - 7)
            fn_file = FILENAME
            fn_line = FNR
            have_fn = 1
            fn_is_test = pend_test
            fn_has_cfg_unix = pend_cfg_unix
            fn_has_serial_cwd = pend_serial_cwd
            fn_calls_helper = 0
            pend_test = 0; pend_cfg_unix = 0; pend_serial_cwd = 0
            next
        }

        {
            # a real code line breaks any not-yet-consumed attribute block …
            pend_test = 0; pend_cfg_unix = 0; pend_serial_cwd = 0
            # … and, inside a tracked fn body, may itself be a helper call.
            if (have_fn && line_calls_helper(line)) fn_calls_helper = 1
        }

        END {
            finalize()             # flush the last file last tracked fn …
            check_counts(cur_file) # … then compare that final file counts
            exit bad ? 3 : 0
        }
    ' "$@"
}

violations=""
if [[ ${#FILES[@]} -gt 0 ]]; then
    violations="$(report "${FILES[@]}" || true)"
fi

mapfile -t HELPER_FILES < <(grep -rlE "(${helper_alt})\\(" crates/*/src --include='*.rs' 2>/dev/null || true)

helper_violations=""
if [[ ${#HELPER_FILES[@]} -gt 0 ]]; then
    helper_violations="$(report_cwd_helper_pairing "$helper_alt" "${HELPER_FILES[@]}" || true)"
fi

# awk exits non-zero on a finding; re-derive pass/fail from emptiness so
# `set -e` does not abort on the expected non-zero status.
if [[ -n "$violations" ]]; then
    echo "UNJUSTIFIED PROCESS-GLOBAL MUTATION IN TESTS — parallel tests race."
    echo
    echo "$violations"
    echo
    echo "Each [env] call mutates a process-global env var, and each [cwd] call"
    echo "swaps the process working directory, inside test code without an inline"
    echo "marker justifying why it is race-free under parallel test execution."
    echo
    echo "[env] Fix (preferred): route the var through an EnvSource seam — call"
    echo "the fn's  *_with_env  variant with an anodizer_core::MapEnvSource, so"
    echo "the test injects the value and never touches process env (then DELETE"
    echo "the set_var entirely — no marker needed)."
    echo "[env] Fix (no seam): annotate the enclosing test"
    echo "#[serial_test::serial(<grp>)] grouped by shared resource (path_env /"
    echo "git_env / <var>_env), then add  // env-ok: serialised by #[serial(<grp>)]"
    echo "at the call site."
    echo
    echo "[cwd] Fix (preferred): use the RAII"
    echo "anodizer_core::test_helpers::CwdGuard (swap on new, panic-safe restore"
    echo "on Drop), so no raw set_current_dir remains at the call site."
    echo "[cwd] Fix (raw call needed): serialise the enclosing test under the"
    echo "canonical #[serial_test::serial(cwd)] group, then add"
    echo "  // cwd-ok: serialised by #[serial(cwd)]  at the call site."
    echo
    echo "Provably safe (idempotent OnceLock of constants, or env_mutex().lock()"
    echo "block): add  // env-ok: <the reason>  on the call's line or the line"
    echo "above it."
    if [[ -n "$helper_violations" ]]; then
        echo
    fi
fi

if [[ -n "$helper_violations" ]]; then
    echo "CWD-SWAP HELPER CALLED WITHOUT ITS REQUIRED ATTRIBUTE PAIR."
    echo
    echo "$helper_violations"
    echo
    echo "Each finding above is a #[test] fn that calls one of the allow-listed"
    echo "cwd-swap helpers (CWD_SWAP_HELPERS: ${CWD_SWAP_HELPERS[*]}) without"
    echo "BOTH #[cfg(unix)] (the helper shells to git and is unix-only) and"
    echo "#[serial_test::serial(cwd)] (joins the shared restore-race group all"
    echo "other cwd swappers in this binary use). Add whichever attribute is"
    echo "missing to the fn's attribute block."
fi

if [[ -n "$violations" || -n "$helper_violations" ]]; then
    exit 1
fi

echo "audit-test-isolation: all ${#FILES[@]} env/cwd-mutating files justify each call with // env-ok: / // cwd-ok:; all ${#HELPER_FILES[@]} cwd-swap-helper files pair every caller with #[cfg(unix)] + serial(cwd)."
