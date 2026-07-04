#!/usr/bin/env bash
# Guard: cross-file / cross-job workflow invariants that GitHub Actions cannot
# express as a shared constant (no anchors, no cross-file variables) and that
# therefore live as hand-synced copies. Each copy is load-bearing only while it
# stays byte-identical to its sibling; GHA fails SILENTLY when they drift (a
# stale concurrency group stops serializing, a missing shard is never asserted,
# a forgotten publish secret aborts post-tag). This audit turns each such
# silent drift into a red CI, the repo's standing pattern for workflow lockstep
# (see audit-job-timeouts.sh / audit-gate-mirror.sh).
#
# Checks:
#   1. Determinism shard roster: determinism.yml's matrix shard set ==
#      release.yml's "Assert all shards present" expected=() array.
#   2. Publish secret env block: the preflight gate and the release job carry
#      an identical env map (so the pre-tag gate validates exactly what the
#      post-tag publish consumes), plus identical gpg/apk key `with:` inputs.
#   3. Release trigger gate: preflight and blob-preflight share one trigger if:.
#   4. Release/nightly mutex: both concurrency groups are identical AND both
#      set cancel-in-progress: false.
#   5. CI-bootstrap artifact: every literal `from-artifact:` and the
#      resolve-release-target default equal the producer's upload name, AND
#      every literal `artifact-workflow:` names the producer workflow's own
#      filename (the NAME and the producing FILE are both load-bearing).
#   6. Post-release gate: publish-npm and advance-master share one success if:.
#   7. Cross-OS suite fallback: the go-task-less fallback in test-os-suite.sh
#      reproduces every cargo pass of the Taskfile `test` target verbatim.
#   8. skip_publishers prose: the static input description still names every
#      HOSTED_PUBLISHERS token (a change to the hosted set can't leave it stale).
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

command -v yq >/dev/null 2>&1 || { echo "audit-workflow-lockstep: yq is required but not found on PATH." >&2; exit 2; }

REL=".github/workflows/release.yml"
DET=".github/workflows/determinism.yml"
CI=".github/workflows/ci.yml"
NIGHTLY=".github/workflows/nightly.yml"
RESOLVE=".github/actions/resolve-release-target/action.yml"
SUITE=".claude/scripts/test-os-suite.sh"
TASKFILE="Taskfile.yml"

for f in "$REL" "$DET" "$CI" "$NIGHTLY" "$RESOLVE" "$SUITE" "$TASKFILE"; do
    [[ -f "$f" ]] || { echo "audit-workflow-lockstep: FAIL — ${f} not found." >&2; exit 2; }
done

failures=""
fail() { failures+="  $1"$'\n'; }

# yq wrapper that hard-fails (exit 2) on a parse error rather than reading an
# empty result and reporting a false pass.
yqr() {
    local out
    if ! out=$(yq "$@"); then
        echo "audit-workflow-lockstep: yq failed on: yq $* — refusing to report a pass on unparsed input." >&2
        exit 2
    fi
    # Emit a trailing newline: `$(yq …)` already stripped it, and a `while read`
    # consumer drops any final line that lacks one — which would silently skip
    # the LAST from-artifact/artifact-workflow/Taskfile-cmd value from its check.
    printf '%s\n' "$out"
}

sorted_words() { tr ' ' '\n' | sed '/^$/d' | sort | tr '\n' ' '; }

# --- 1. Determinism shard roster -------------------------------------------
det_labels=$(yqr -r '.jobs.shard.strategy.matrix.include[].shard' "$DET" | sorted_words)
rel_expected_raw=$(grep -oE 'expected=\([^)]*\)' "$REL" || true)
if [[ -z "$rel_expected_raw" ]]; then
    fail "shard roster: no 'expected=(…)' assert array found in ${REL}."
else
    rel_expected=$(printf '%s' "$rel_expected_raw" | sed -E 's/expected=\(//; s/\)//' | sorted_words)
    if [[ -z "$det_labels" ]]; then
        fail "shard roster: parsed 0 shard labels from ${DET} matrix."
    elif [[ "$det_labels" != "$rel_expected" ]]; then
        fail "shard roster drift: ${DET} matrix [${det_labels}] != ${REL} assert [${rel_expected}]."
    fi
fi

# --- 2. Publish secret env block -------------------------------------------
pf_env=$(yqr -o=json -I=0 '.jobs.preflight.steps[] | select(.name == "Validate publish secrets") | .env' "$REL")
rl_env=$(yqr -o=json -I=0 '.jobs.release.steps[] | select(.name == "Run anodizer release --publish-only") | .env' "$REL")
if [[ -z "$pf_env" || "$pf_env" == "null" ]]; then
    fail "secret env: could not read the preflight 'Validate publish secrets' env block from ${REL}."
elif [[ -z "$rl_env" || "$rl_env" == "null" ]]; then
    fail "secret env: could not read the release publish-only env block from ${REL}."
elif [[ "$pf_env" != "$rl_env" ]]; then
    fail "secret env drift: the preflight gate and the release job env blocks differ — the pre-tag gate no longer validates what the post-tag publish consumes."
fi

pf_keys=$(yqr -o=json -I=0 '.jobs.preflight.steps[] | select(.name == "Validate publish secrets") | [.with["gpg-private-key"], .with["apk-private-key"]]' "$REL")
rl_keys=$(yqr -o=json -I=0 '.jobs.release.steps[] | select(.name == "Run anodizer release --publish-only") | [.with["gpg-private-key"], .with["apk-private-key"]]' "$REL")
if [[ "$pf_keys" != "$rl_keys" ]]; then
    fail "secret env drift: gpg/apk key with-inputs differ between preflight [${pf_keys}] and release [${rl_keys}]."
fi

# --- 3. Release trigger gate -----------------------------------------------
pf_if=$(yqr -r '.jobs.preflight.if' "$REL")
blob_if=$(yqr -r '.jobs["blob-preflight"].if' "$REL")
if [[ "$pf_if" != "$blob_if" ]]; then
    fail "trigger gate drift: preflight if [${pf_if}] != blob-preflight if [${blob_if}]."
fi

# --- 4. Release/nightly mutex ----------------------------------------------
rel_grp=$(yqr -r '.concurrency.group' "$REL")
ngt_grp=$(yqr -r '.concurrency.group' "$NIGHTLY")
if [[ "$rel_grp" != "$ngt_grp" ]]; then
    fail "mutex drift: release concurrency group [${rel_grp}] != nightly [${ngt_grp}] — the serialization stops locking."
fi
rel_cip=$(yqr -r '.concurrency["cancel-in-progress"]' "$REL")
ngt_cip=$(yqr -r '.concurrency["cancel-in-progress"]' "$NIGHTLY")
if [[ "$rel_cip" != "false" || "$ngt_cip" != "false" ]]; then
    fail "mutex drift: cancel-in-progress must be false on both (release=${rel_cip}, nightly=${ngt_cip}); a cancel breaks the publish mutex."
fi

# --- 5. CI-bootstrap artifact ----------------------------------------------
producer=$(yqr -r '.jobs.test.steps[] | select(.name == "Upload release binary") | .with.name' "$CI")
if [[ -z "$producer" || "$producer" == "null" ]]; then
    fail "bootstrap artifact: could not read the producer upload name from ${CI}."
else
    for f in "$CI" "$REL"; do
        while IFS= read -r val; do
            [[ -z "$val" ]] && continue
            # A GHA expression (e.g. the resolve output) is not a literal name.
            # shellcheck disable=SC2016  # the literal ${{ is the match target, not an expansion
            case "$val" in
                *'${{'*) continue ;;
            esac
            if [[ "$val" != "$producer" ]]; then
                fail "bootstrap artifact drift: literal from-artifact '${val}' in ${f} != producer '${producer}'."
            fi
        done < <(yqr -r '.. | select(tag == "!!map" and has("from-artifact")) | .["from-artifact"]' "$f")
    done
    # resolve-release-target's shell default (the non-empty branch).
    while IFS= read -r rv; do
        [[ "$rv" == "$producer" ]] || fail "bootstrap artifact drift: resolve-release-target default '${rv}' != producer '${producer}'."
    done < <(grep -oE 'from_artifact="[^"]+"' "$RESOLVE" | sed -E 's/from_artifact="([^"]+)"/\1/')
fi

# Second half of the bootstrap contract: every literal `artifact-workflow:`
# consumer must name the producer workflow's own FILENAME. Derived from $CI (the
# single producer reference already used above), not a second hardcoded copy of
# the string being protected. The release `workflow_run` trigger keys on the
# workflow NAME, not the filename, so a file rename is not otherwise a loud
# failure — a stale artifact-workflow 404s only at post-tag publish time.
producer_wf=$(basename "$CI")
for f in "$CI" "$REL"; do
    while IFS= read -r wf; do
        [[ -z "$wf" ]] && continue
        # A GHA expression is not a literal filename.
        # shellcheck disable=SC2016  # the literal ${{ is the match target, not an expansion
        case "$wf" in
            *'${{'*) continue ;;
        esac
        if [[ "$wf" != "$producer_wf" ]]; then
            fail "bootstrap artifact drift: literal artifact-workflow '${wf}' in ${f} != producer workflow '${producer_wf}'."
        fi
    done < <(yqr -r '.. | select(tag == "!!map" and has("artifact-workflow")) | .["artifact-workflow"]' "$f")
done

# --- 6. Post-release gate --------------------------------------------------
npm_if=$(yqr -r '.jobs["publish-npm"].if' "$REL")
adv_if=$(yqr -r '.jobs["advance-master"].if' "$REL")
if [[ "$npm_if" != "$adv_if" ]]; then
    fail "post-release gate drift: publish-npm if [${npm_if}] != advance-master if [${adv_if}]."
fi

# --- 7. Cross-OS suite fallback parity -------------------------------------
# test-os-suite.sh runs `task test` when go-task is present; its fallback (for
# validation hosts that lack go-task) hand-reproduces the Taskfile `test`
# passes. Assert every cargo pass in the Taskfile `test` target appears verbatim
# in the fallback, so a pass added to `task test` can't silently bypass the
# go-task-less hosts.
while IFS= read -r cmd; do
    [[ "$cmd" == cargo* ]] || continue
    grep -Fq -- "$cmd" "$SUITE" || fail "cross-OS suite fallback: Taskfile 'test' pass [${cmd}] missing from ${SUITE} — a go-task-less host would run a different suite than 'task test'."
done < <(yqr -r '.tasks.test.cmds[]' "$TASKFILE")

# --- 8. skip_publishers prose vs hosted set --------------------------------
# The skip_publishers input DESCRIPTION is static (a GHA inputs.*.description
# cannot read env), so it names the always-skipped hosted publisher(s) as prose.
# Assert every HOSTED_PUBLISHERS token still appears in that prose, so a change
# to the hosted set fails CI here instead of leaving stale operator-facing docs.
# Presence-only — reworded prose still passes; only a dropped/renamed publisher
# trips it.
sp_desc=$(yqr -r '.on.workflow_dispatch.inputs.skip_publishers.description' "$REL")
hosted=$(yqr -r '.env.HOSTED_PUBLISHERS' "$REL")
if [[ -z "$sp_desc" || "$sp_desc" == "null" ]]; then
    fail "skip_publishers prose: could not read the skip_publishers input description from ${REL}."
elif [[ -z "$hosted" || "$hosted" == "null" ]]; then
    fail "skip_publishers prose: could not read env.HOSTED_PUBLISHERS from ${REL}."
else
    for pub in ${hosted//,/ }; do
        [[ -z "$pub" ]] && continue
        case "$sp_desc" in
            *"$pub"*) ;;
            *) fail "skip_publishers prose drift: description does not name hosted publisher '${pub}' (HOSTED_PUBLISHERS='${hosted}') — the always-skipped set changed but the operator-facing prose is stale." ;;
        esac
    done
fi

if [[ -n "$failures" ]]; then
    echo "audit-workflow-lockstep: FAIL — hand-synced workflow copies have drifted." >&2
    echo "" >&2
    printf '%s' "$failures" >&2
    exit 1
fi

echo "audit-workflow-lockstep: OK — shard roster, secret env, trigger gate, release/nightly mutex, bootstrap artifact (name + workflow file), post-release gate, cross-OS suite fallback, and skip_publishers prose are in lockstep."
