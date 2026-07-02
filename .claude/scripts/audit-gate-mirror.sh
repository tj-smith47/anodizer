#!/usr/bin/env bash
# Guard: `task gate` mirrors every blocking job in .github/workflows/ci.yml.
#
# release.yml only runs after the whole ci.yml workflow succeeds, so any
# ci.yml job with no local equivalent reachable from `task gate` is a hole
# through which a red CI slips past a local pre-push check. This audit fails
# (exit 1) when a ci.yml job id has no registered local-mirror mapping, or
# when a mapped local target is not actually reachable from `task gate`.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

CI_WORKFLOW=".github/workflows/ci.yml"

command -v yq >/dev/null 2>&1 || { echo "audit-gate-mirror: yq is required but not found on PATH." >&2; exit 2; }

if [[ ! -f "$CI_WORKFLOW" ]]; then
    echo "audit-gate-mirror: FAIL — ${CI_WORKFLOW} not found." >&2
    exit 2
fi

# job id (ci.yml) -> local Taskfile target it must be reachable through via
# `task gate`. This map IS the registration point: when a ci.yml job is
# added, add its local mirror here AND to `task gate`'s cmds.
declare -A JOB_MIRROR=(
    [actionlint]="audit:workflows"
    [fmt]="fmt:check"
    [clippy]="clippy"
    [cargo-audit]="audit:deps"
    [test]="test"
    [package]="package"
    [snapshot]="snapshot"
    [validate-readme]="docs:validate-readme"
    [check-config]="check:config"
    [docs-check]="docs:check"
    [coverage]="coverage:gate"
)

# Capture yq's output (and exit status) BEFORE the loop — a `while … < <(yq)`
# process substitution hides yq's failure, so a parse error would read zero
# rows and the audit would falsely pass. Hard-fail on a parse error instead.
if ! jobs=$(yq -r '.jobs | keys | .[]' "$CI_WORKFLOW"); then
    echo "audit-gate-mirror: yq failed to parse ${CI_WORKFLOW} — refusing to report a pass on unparsed input." >&2
    exit 2
fi

if [[ -z "$jobs" ]]; then
    echo "audit-gate-mirror: FAIL — parsed 0 jobs from ${CI_WORKFLOW}; yq produced no rows." >&2
    exit 2
fi

unmapped=""
job_count=0
while IFS= read -r job; do
    [[ -z "$job" ]] && continue
    job_count=$((job_count + 1))
    if [[ -z "${JOB_MIRROR[$job]:-}" ]]; then
        unmapped+="  ${CI_WORKFLOW}: job '${job}' has no registered local mirror — add it to JOB_MIRROR in $(basename "$0") AND to task gate's cmds."$'\n'
    fi
done <<< "$jobs"

if [[ -n "$unmapped" ]]; then
    echo "audit-gate-mirror: FAIL — every ci.yml job must have a local mirror registered." >&2
    echo "" >&2
    printf '%s' "$unmapped" >&2
    exit 1
fi

# Reachability: does every mapped local target actually run as part of
# `task gate`? Prefer `task -n gate` (a real dry-run of the composed task
# graph); each leaf task prints as `task: [<name>] <cmd>`. CI's actionlint
# job has no `task` binary on PATH, so fall back to grepping Taskfile.yml's
# `gate:` cmds block + the `ci:` cmds block it composes — less precise, but
# it doesn't require the binary.
unreachable=""
if command -v task >/dev/null 2>&1; then
    if ! dryrun=$(task -n gate 2>&1); then
        echo "audit-gate-mirror: 'task -n gate' failed — refusing to report a pass on unparsed input." >&2
        echo "$dryrun" >&2
        exit 2
    fi
    for job in "${!JOB_MIRROR[@]}"; do
        local_target="${JOB_MIRROR[$job]}"
        if ! grep -qF -- "[${local_target}]" <<< "$dryrun"; then
            unreachable+="  job '${job}' -> local target '${local_target}' does not appear in \`task -n gate\` output — wire it into task gate's cmds."$'\n'
        fi
    done
else
    # No `task` binary (e.g. the actionlint CI job): grep Taskfile.yml directly.
    # gate: composes ci: plus a fixed list of `task: <name>` cmds, so a target
    # is reachable if it appears as a `task: <name>` line inside EITHER block.
    taskfile="Taskfile.yml"
    if [[ ! -f "$taskfile" ]]; then
        echo "audit-gate-mirror: FAIL — no \`task\` binary on PATH and ${taskfile} not found for the grep fallback." >&2
        exit 2
    fi
    gate_block=$(awk '/^  gate:/{flag=1} flag{print} flag && /^  [A-Za-z_].*:$/ && !/^  gate:/{if(NR>1)exit}' "$taskfile")
    ci_block=$(awk '/^  ci:/{flag=1} flag{print} flag && /^  [A-Za-z_].*:$/ && !/^  ci:/{if(NR>1)exit}' "$taskfile")
    combined="${gate_block}"$'\n'"${ci_block}"
    for job in "${!JOB_MIRROR[@]}"; do
        local_target="${JOB_MIRROR[$job]}"
        # Anchor on "task: <name>" followed by whitespace/comment/EOL so e.g.
        # target `test` doesn't false-match a substring like `test:os`.
        if ! grep -qE -- "task: ${local_target}([[:space:]]|\$)" <<< "$combined"; then
            unreachable+="  job '${job}' -> local target '${local_target}' does not appear in Taskfile.yml's gate:/ci: cmds — wire it into task gate's cmds."$'\n'
        fi
    done
fi

if [[ -n "$unreachable" ]]; then
    echo "audit-gate-mirror: FAIL — every mapped local target must be reachable from task gate." >&2
    echo "" >&2
    printf '%s' "$unreachable" >&2
    exit 1
fi

echo "audit-gate-mirror: OK — all ${job_count} ci.yml job(s) have a local mirror reachable from task gate."
