#!/usr/bin/env bash
# Guard: `task gate` mirrors every blocking job in .github/workflows/ci.yml.
#
# release.yml only runs after the whole ci.yml workflow succeeds, so any
# ci.yml job with no local equivalent reachable from `task gate` is a hole
# through which a red CI slips past a local pre-push check. This audit fails
# (exit 1) when a ci.yml job id has no registered local-mirror mapping, or
# when a mapped local target, or a target a ci.yml step runs through
# `task <target>`, is not actually reachable from `task gate`.
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"

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
    [rustdoc]="doc"
    [cargo-audit]="audit:deps"
    [test]="test"
    [package]="package"
    [snapshot]="snapshot"
    [validate-readme]="docs:validate-readme"
    [check-config]="check:config"
    [msrv]="check:msrv"
    [docs-check]="docs:check"
    [native-object-determinism]="check:native-object-determinism"
    # Both coverage jobs mirror `coverage:gate`: the measurement is the part a
    # local pre-push can reproduce, and `coverage-publish` only pushes the
    # badge, which publish-coverage.sh refuses to do outside CI.
    [coverage]="coverage:gate"
    [coverage-publish]="coverage:gate"
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

# Every `task <target>` a ci.yml step runs must be reachable from `task gate`
# as well. JOB_MIRROR maps a whole job to one target, so a step added to an
# already-mapped job (the macOS leg's `task test:brew-style`) would otherwise
# run in CI and never in the local gate.
if ! step_runs=$(yq -r '.jobs[] | .steps[]? | select(.run != null) | .run' "$CI_WORKFLOW"); then
    echo "audit-gate-mirror: yq failed to read the ci.yml step commands — refusing to report a pass on unparsed input." >&2
    exit 2
fi
declare -A STEP_TARGETS=()
task_word='(^|[;&|[:space:]])task[[:space:]]+([A-Za-z][A-Za-z0-9:_.-]*)(.*)'
while IFS= read -r line; do
    rest="$line"
    while [[ "$rest" =~ $task_word ]]; do
        STEP_TARGETS[${BASH_REMATCH[2]}]=1
        rest="${BASH_REMATCH[3]}"
    done
done <<< "$step_runs"

# Reachability: does every mapped local target, and every target a ci.yml
# step runs, actually run as part of `task gate`? Prefer `task -n gate` (a
# real dry-run of the composed task graph); each leaf task prints as
# `task: [<name>] <cmd>`. CI's actionlint job has no `task` binary on PATH, so
# fall back to walking Taskfile.yml's `- task:` cmds and `deps:` from `gate`
# with yq — it does not evaluate `platforms:` or vars, but it doesn't require
# the binary.
#
# go-task evaluates `preconditions:` even under `-n`, so the host-resource
# floors `task doc` declares would decide this walk. They gate running the
# work, not naming it; ANODIZER_STRUCTURAL_WALK lets each leg stand down for
# a walk that spawns none of it.
unreachable=""
if command -v task >/dev/null 2>&1; then
    if ! dryrun=$(ANODIZER_STRUCTURAL_WALK=1 task -n gate 2>&1); then
        echo "audit-gate-mirror: 'task -n gate' failed — refusing to report a pass on unparsed input." >&2
        echo "$dryrun" >&2
        exit 2
    fi
    # A target whose sources and generates are current prints `Task "<name>" is
    # up to date` in place of its `[<name>]` command lines, so either form
    # means the gate reached it.
    reachable() { grep -qF -e "[$1]" -e "Task \"$1\" is up to date" <<< "$dryrun"; }
    where="\`task -n gate\` output"
else
    taskfile="Taskfile.yml"
    if [[ ! -f "$taskfile" ]]; then
        echo "audit-gate-mirror: FAIL — no \`task\` binary on PATH and ${taskfile} not found for the static walk." >&2
        exit 2
    fi
    declare -A REACHED=()
    queue=(gate)
    while ((${#queue[@]})); do
        current="${queue[0]}"
        queue=("${queue[@]:1}")
        [[ -n "${REACHED[$current]:-}" ]] && continue
        REACHED[$current]=1
        if ! children=$(T="$current" yq -r '(.tasks[strenv(T)] | ((.cmds // []) + (.deps // []))[] | select(tag == "!!map") | .task // ""), (.tasks[strenv(T)].deps // [] | .[] | select(tag == "!!str"))' "$taskfile"); then
            echo "audit-gate-mirror: yq failed to read target '${current}' from ${taskfile} — refusing to report a pass on unparsed input." >&2
            exit 2
        fi
        while IFS= read -r child; do
            [[ -n "$child" ]] && queue+=("$child")
        done <<< "$children"
    done
    reachable() { [[ -n "${REACHED[$1]:-}" ]]; }
    where="the targets Taskfile.yml's gate: reaches"
fi
for job in "${!JOB_MIRROR[@]}"; do
    local_target="${JOB_MIRROR[$job]}"
    if ! reachable "$local_target"; then
        unreachable+="  job '${job}' -> local target '${local_target}' does not appear in ${where} — wire it into task gate's cmds."$'\n'
    fi
done
for step_target in "${!STEP_TARGETS[@]}"; do
    if ! reachable "$step_target"; then
        unreachable+="  ${CI_WORKFLOW}: a step runs 'task ${step_target}', which does not appear in ${where} — wire it into task gate's cmds."$'\n'
    fi
done

if [[ -n "$unreachable" ]]; then
    echo "audit-gate-mirror: FAIL — every mapped local target, and every target a ci.yml step runs, must be reachable from task gate." >&2
    echo "" >&2
    printf '%s' "$unreachable" >&2
    exit 1
fi

# Flag-set equality for the snapshot mirror. JOB_MIRROR proves the snapshot CI
# job maps to a local target, but not that the two run the SAME pipeline. Both
# invoke `anodizer release --snapshot …`; if their flag sets drift (a --skip
# added in CI, a --single-target dropped locally) the local pre-commit/pre-push
# gate silently proves a different pipeline than the CI job that gates
# release.yml — the exact hole this audit exists to close, one level below its
# job-mapping resolution. Normalize each invocation to the argv from the
# `release` token onward and require exact equality.
TASKFILE="Taskfile.yml"
if [[ ! -f "$TASKFILE" ]]; then
    echo "audit-gate-mirror: FAIL — ${TASKFILE} not found for the snapshot flag-set check." >&2
    exit 2
fi

normalize_release_args() {
    # stdin: a full command/args string. stdout: the substring from the first
    # `release ` token onward, with internal/trailing whitespace collapsed.
    sed -E 's/.*(release )/\1/; s/[[:space:]]+/ /g; s/[[:space:]]+$//'
}

if ! ci_snapshot_raw=$(yq -r '.jobs.snapshot.steps[] | select(.with.args != null) | .with.args' "$CI_WORKFLOW"); then
    echo "audit-gate-mirror: yq failed to read the ci.yml snapshot args — refusing to report a pass on unparsed input." >&2
    exit 2
fi
if ! task_snapshot_raw=$(yq -r '.tasks.snapshot.cmds[0]' "$TASKFILE"); then
    echo "audit-gate-mirror: yq failed to read the Taskfile snapshot cmd — refusing to report a pass on unparsed input." >&2
    exit 2
fi

ci_snapshot_args=$(printf '%s\n' "$ci_snapshot_raw" | head -n1 | normalize_release_args)
task_snapshot_args=$(printf '%s\n' "$task_snapshot_raw" | head -n1 | normalize_release_args)

if [[ -z "$ci_snapshot_args" || "$ci_snapshot_args" != release\ * ]]; then
    echo "audit-gate-mirror: FAIL — could not extract a 'release …' snapshot invocation from ${CI_WORKFLOW} (found: '${ci_snapshot_args}')." >&2
    exit 2
fi
if [[ -z "$task_snapshot_args" || "$task_snapshot_args" != release\ * ]]; then
    echo "audit-gate-mirror: FAIL — could not extract a 'release …' snapshot invocation from ${TASKFILE} (found: '${task_snapshot_args}')." >&2
    exit 2
fi

if [[ "$ci_snapshot_args" != "$task_snapshot_args" ]]; then
    echo "audit-gate-mirror: FAIL — the snapshot flag sets have drifted; the local gate proves a different pipeline than CI." >&2
    echo "" >&2
    echo "  ${CI_WORKFLOW} snapshot job: ${ci_snapshot_args}" >&2
    echo "  ${TASKFILE} snapshot task:  ${task_snapshot_args}" >&2
    exit 1
fi

echo "audit-gate-mirror: OK — all ${job_count} ci.yml job(s) and ${#STEP_TARGETS[@]} \`task\` step target(s) are reachable from task gate; snapshot flag sets match."
