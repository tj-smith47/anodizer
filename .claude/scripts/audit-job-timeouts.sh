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
#
# A `${{ matrix.<key> }}` timeout is ACCEPTED, but only after every matrix leg
# is resolved and checked: a per-shard bound is the honest shape when legs
# differ structurally (a Windows leg that is minutes-slower than Linux should
# not force every leg to carry the worst case). The expression is only as
# strong as its legs, so a leg missing the key — or carrying a non-positive
# value — fails exactly as a bare missing timeout does.
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

# Resolve `${{ matrix.<key> }}` to the value each matrix leg supplies, one per
# line. Reads both matrix shapes: `include:` entries and a bare `key: [a, b]`
# axis. Prints nothing when the key is absent from the matrix entirely, which
# the caller treats as a failure (an unresolvable timeout is an unbounded job).
matrix_timeout_values() {
    local file="$1" job="$2" key="$3"
    # `include:` legs. A leg that omits the key yields the `absent` sentinel
    # rather than an empty line, so "this leg is unbounded" stays reportable
    # instead of collapsing into "no legs to check".
    yq -r "(.jobs[\"${job}\"].strategy.matrix.include // [])[] | .[\"${key}\"] // \"absent\"" "$file"
    # Bare `key: [a, b]` axis. The seq guard keeps a scalar axis value from
    # being splatted, which yq would reject outright.
    yq -r "(.jobs[\"${job}\"].strategy.matrix[\"${key}\"] // []) | select(tag == \"!!seq\") | .[]" "$file"
}

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
        if [[ "$tag" == "!!int" && "$value" -gt 0 ]]; then
            continue
        fi
        if [[ "$value" =~ ^\$\{\{[[:space:]]*matrix\.([A-Za-z0-9_-]+)[[:space:]]*\}\}$ ]]; then
            key="${BASH_REMATCH[1]}"
            legs=$(matrix_timeout_values "$f" "$job" "$key")
            if [[ -z "$legs" ]]; then
                offenders+="  ${f}: job '${job}' times out via ${value} but no matrix leg defines '${key}'"$'\n'
                continue
            fi
            while IFS= read -r leg; do
                [[ -z "$leg" ]] && continue
                if [[ "$leg" == "absent" ]]; then
                    offenders+="  ${f}: job '${job}' times out via ${value} but a matrix leg omits '${key}'"$'\n'
                elif ! [[ "$leg" =~ ^[0-9]+$ ]] || [[ "$leg" -le 0 ]]; then
                    offenders+="  ${f}: job '${job}' matrix leg sets ${key}=${leg}, not a positive integer"$'\n'
                fi
            done <<< "$legs"
            continue
        fi
        offenders+="  ${f}: job '${job}' has no positive-integer timeout-minutes (found: ${value})"$'\n'
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
