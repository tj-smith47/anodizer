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
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

mapfile -t FILES < <(grep -rlP 'std::env::(set_var|remove_var|set_current_dir)\(' crates/*/src --include='*.rs' 2>/dev/null || true)

if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "audit-test-isolation: no env/cwd-mutating call sites found."
    exit 0
fi

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

violations="$(report "${FILES[@]}" || true)"

# awk exits 2 on a finding; re-derive pass/fail from emptiness so `set -e` does
# not abort on the expected non-zero status.
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
    exit 1
fi

echo "audit-test-isolation: all ${#FILES[@]} env/cwd-mutating files justify each call with // env-ok: / // cwd-ok:."
