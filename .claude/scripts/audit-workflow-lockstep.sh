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
#      resolve-release-target default equal the producer's upload name.
#   6. Post-release gate: publish-npm and advance-master share one success if:.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

command -v yq >/dev/null 2>&1 || { echo "audit-workflow-lockstep: yq is required but not found on PATH." >&2; exit 2; }

REL=".github/workflows/release.yml"
DET=".github/workflows/determinism.yml"
CI=".github/workflows/ci.yml"
NIGHTLY=".github/workflows/nightly.yml"
RESOLVE=".github/actions/resolve-release-target/action.yml"

for f in "$REL" "$DET" "$CI" "$NIGHTLY" "$RESOLVE"; do
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
    printf '%s' "$out"
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

# --- 6. Post-release gate --------------------------------------------------
npm_if=$(yqr -r '.jobs["publish-npm"].if' "$REL")
adv_if=$(yqr -r '.jobs["advance-master"].if' "$REL")
if [[ "$npm_if" != "$adv_if" ]]; then
    fail "post-release gate drift: publish-npm if [${npm_if}] != advance-master if [${adv_if}]."
fi

if [[ -n "$failures" ]]; then
    echo "audit-workflow-lockstep: FAIL — hand-synced workflow copies have drifted." >&2
    echo "" >&2
    printf '%s' "$failures" >&2
    exit 1
fi

echo "audit-workflow-lockstep: OK — shard roster, secret env, trigger gate, release/nightly mutex, bootstrap artifact, and post-release gate are in lockstep."
