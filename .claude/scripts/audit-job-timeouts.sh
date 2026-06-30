#!/usr/bin/env bash
# Guard: every job in every workflow declares a numeric `timeout-minutes`.
#
# A job with no `timeout-minutes` inherits GitHub's implicit 6-hour ceiling, so
# a wedged step (a hung publish leg, a deadlocked harness, a stalled network
# call) burns a full runner-hour budget before the scheduler reaps it. Bounding
# every job with an explicit, evidence-sized timeout turns "silently hangs for
# hours" into "fails fast and visibly".
#
# This audit fails (exit 1) and lists offenders when any job under
# `.github/workflows/*.yml` lacks a positive-integer `timeout-minutes`.
#
# Reusable-workflow callers are EXEMPT: a job that is `uses: ./…/foo.yml`
# (a `workflow_call`) may not carry `timeout-minutes` — GitHub rejects it, and
# actionlint flags it. The bound for that work lives on the jobs INSIDE the
# called workflow (which this audit checks when it scans that file). Such jobs
# are identified by a top-level `.uses` key and skipped here.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

command -v yq >/dev/null 2>&1 || { echo "audit-job-timeouts: yq is required but not found on PATH." >&2; exit 2; }

shopt -s nullglob
FILES=(.github/workflows/*.yml .github/workflows/*.yaml)
shopt -u nullglob
if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "audit-job-timeouts: no workflow files found under .github/workflows/."
    exit 0
fi

offenders=""
checked=0
for f in "${FILES[@]}"; do
    # Emit one TSV row per non-reusable-workflow job: <job> <timeout-tag> <timeout-value>.
    # `tag` is yq's type tag ("!!int" only for a literal integer); a missing
    # key surfaces as "!!null", a quoted/expression value as "!!str".
    #
    # Capture yq's output (and exit status) BEFORE the loop — a `while … < <(yq)`
    # process substitution hides yq's failure, so a parse error would read zero
    # rows and the audit would falsely pass. Hard-fail on a parse error instead.
    if ! rows=$(yq -r '
            (.jobs // {}) | to_entries | .[]
            | select(.value.uses == null)
            | [.key, (.value["timeout-minutes"] | tag), (.value["timeout-minutes"] // "absent" | tostring)] | @tsv
        ' "$f"); then
        echo "audit-job-timeouts: yq failed to parse ${f} — refusing to report a pass on unparsed input." >&2
        exit 2
    fi
    while IFS=$'\t' read -r job tag value; do
        [[ -z "$job" ]] && continue
        checked=$((checked + 1))
        if [[ "$tag" != "!!int" || "$value" -le 0 ]]; then
            offenders+="  ${f}: job '${job}' has no positive-integer timeout-minutes (found: ${value})"$'\n'
        fi
    done <<< "$rows"
done

# Every real workflow declares at least one job; zero parsed jobs means yq
# emitted nothing across the board (a silent parse/format regression), not a
# clean tree. Treat it as a failure rather than print "OK — all 0 job(s)".
if [[ "$checked" -eq 0 ]]; then
    echo "audit-job-timeouts: FAIL — parsed 0 jobs across ${#FILES[@]} workflow file(s); yq produced no rows." >&2
    exit 2
fi

if [[ -n "$offenders" ]]; then
    echo "audit-job-timeouts: FAIL — every workflow job must declare a positive-integer timeout-minutes." >&2
    echo "" >&2
    printf '%s' "$offenders" >&2
    echo "" >&2
    echo "Pick headroom = ceil(observed-or-expected wall-time * 1.5); 15-20 min is fine for lint/docs jobs." >&2
    echo "Reusable-workflow callers (\`uses: ./…\`) are exempt — bound the jobs inside the called workflow instead." >&2
    exit 1
fi

echo "audit-job-timeouts: OK — all ${checked} job(s) across ${#FILES[@]} workflow file(s) declare a positive timeout-minutes."
