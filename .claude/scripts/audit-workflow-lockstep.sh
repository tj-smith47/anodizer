#!/usr/bin/env bash
# Guard: cross-file / cross-job workflow invariants that GitHub Actions cannot
# express as a shared constant (no anchors, no cross-file variables) and that
# therefore live as hand-synced copies. Each copy is correct only while it
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
#   3. Release trigger gate: ci.yml's RELEASE_BOOTSTRAP env equals the
#      snapshot job's own if:.
#   4. Release/nightly mutex: both concurrency groups are identical AND both
#      set cancel-in-progress: false.
#   5. CI-bootstrap artifact: every literal `from-artifact:` and the
#      resolve-release-target default equal the producer's upload name, AND
#      every literal `artifact-workflow:` names the producer workflow's own
#      filename (the NAME and the producing FILE both matter). Scans
#      release.yml AND publish-oidc.yml (both install anodizer from the CI build).
#   6. Atomic tag topology: the auto-tag step pushes the bump commit and the
#      tag together (`--push`, never `--push-tags-only`) and no job re-introduces
#      a deferred branch fast-forward — the shape whose publish-then-advance
#      window raced any push to master.
#   7. Cross-OS suite fallback: the go-task-less fallback in test-os-suite.sh
#      reproduces every cargo pass of the Taskfile `test` target verbatim.
#   8. skip_publishers prose + hosted set: the static input description still
#      names every HOSTED_PUBLISHERS token, AND publish-oidc.yml's copy of
#      HOSTED_PUBLISHERS stays byte-equal to release.yml's (the --skip and
#      --publishers selectors must remain exact complements across the split).
#   9. Tokenless OIDC job: no step of publish-oidc.yml carries an NPM_TOKEN in
#      its env. The job's comment promises the absence, and the absence is what
#      makes the npm publish go through Trusted Publishing at all — a token
#      re-added here silently publishes new package names with it and leaves
#      the Trusted Publisher unexercised.
#  10. One rust-cache per job: no job runs two Swatinem/rust-cache instances,
#      whether direct, through a local composite (setup-rust with cache on,
#      setup-docs), or through anodizer-action's from-source / from-branch /
#      determinism paths. The FIRST instance's post step prunes ~/.cargo/bin
#      down to cargo-installed binaries (rustup's cargo/rustc proxies
#      included), so the second post step finds no `cargo` for its `cargo
#      metadata` and saves a cache it could not scope. The job still passes,
#      so nothing else notices.
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

command -v yq >/dev/null 2>&1 || { echo "audit-workflow-lockstep: yq is required but not found on PATH." >&2; exit 2; }

REL=".github/workflows/release.yml"
OIDC=".github/workflows/publish-oidc.yml"
DET=".github/workflows/determinism.yml"
CI=".github/workflows/ci.yml"
NIGHTLY=".github/workflows/nightly.yml"
RESOLVE=".github/actions/resolve-release-target/action.yml"
SUITE=".claude/scripts/test-os-suite.sh"
TASKFILE="Taskfile.yml"

for f in "$REL" "$OIDC" "$DET" "$CI" "$NIGHTLY" "$RESOLVE" "$SUITE" "$TASKFILE"; do
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
collect_files REL_EXPECTED -oE -- 'expected=\([^)]*\)' "$REL"
rel_expected_raw=$(printf '%s\n' "${REL_EXPECTED[@]}")
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
pf_env=$(yqr -o=json -I=0 '.jobs.preflight.steps[] | select(.name == "Run anodizer preflight") | .env' "$REL")
rl_env=$(yqr -o=json -I=0 '.jobs.release.steps[] | select(.name == "Run anodizer release --publish-only") | .env' "$REL")
if [[ -z "$pf_env" || "$pf_env" == "null" ]]; then
    fail "secret env: could not read the preflight 'Run anodizer preflight' env block from ${REL}."
elif [[ -z "$rl_env" || "$rl_env" == "null" ]]; then
    fail "secret env: could not read the release publish-only env block from ${REL}."
elif [[ "$pf_env" != "$rl_env" ]]; then
    fail "secret env drift: the preflight gate and the release job env blocks differ — the pre-tag gate no longer validates what the post-tag publish consumes."
fi

pf_keys=$(yqr -o=json -I=0 '.jobs.preflight.steps[] | select(.name == "Run anodizer preflight") | [.with["gpg-private-key"], .with["apk-private-key"]]' "$REL")
rl_keys=$(yqr -o=json -I=0 '.jobs.release.steps[] | select(.name == "Run anodizer release --publish-only") | [.with["gpg-private-key"], .with["apk-private-key"]]' "$REL")
if [[ "$pf_keys" != "$rl_keys" ]]; then
    fail "secret env drift: gpg/apk key with-inputs differ between preflight [${pf_keys}] and release [${rl_keys}]."
fi

# --- 3. Release trigger gate -----------------------------------------------
# ci.yml's test job gates the release-binary bootstrap build/upload on
# RELEASE_BOOTSTRAP, and the snapshot job that consumes the uploaded artifact
# gates itself on the same expression. When the two drift, `snapshot` (needs:
# test) waits on an `anodizer-linux` artifact the test job never uploaded.
# Compare with the `${{ }}` wrapper and the `&& matrix.os == …` tail stripped,
# so only the trigger half has to match.
strip_gate() { sed -E 's/^\$\{\{[[:space:]]*//; s/[[:space:]]*\}\}$//; s/^\(//; s/\)[[:space:]]*&&.*$//; s/[[:space:]]+/ /g; s/^ //; s/ $//'; }
boot_env=$(yqr -r '.jobs.test.env.RELEASE_BOOTSTRAP' "$CI")
snap_if=$(yqr -r '.jobs.snapshot.if' "$CI")
if [[ -z "$boot_env" || "$boot_env" == "null" ]]; then
    fail "bootstrap gate: could not read .jobs.test.env.RELEASE_BOOTSTRAP from ${CI}."
elif [[ -z "$snap_if" || "$snap_if" == "null" ]]; then
    fail "bootstrap gate: could not read .jobs.snapshot.if from ${CI}."
else
    boot_gate=$(printf '%s' "$boot_env" | strip_gate)
    snap_gate=$(printf '%s' "$snap_if" | strip_gate)
    if [[ "$boot_gate" != "$snap_gate" ]]; then
        fail "bootstrap gate drift: ${CI} RELEASE_BOOTSTRAP [${boot_gate}] != snapshot if [${snap_gate}] — snapshot would wait on an artifact the test job never uploaded."
    fi
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
    for f in "$CI" "$REL" "$OIDC"; do
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
    collect_files RESOLVE_DEFAULTS -oE -- 'from_artifact="[^"]+"' "$RESOLVE"
    for rv in "${RESOLVE_DEFAULTS[@]}"; do
        rv="${rv#from_artifact=\"}"
        rv="${rv%\"}"
        [[ "$rv" == "$producer" ]] || fail "bootstrap artifact drift: resolve-release-target default '${rv}' != producer '${producer}'."
    done
fi

# Second half of the bootstrap contract: every literal `artifact-workflow:`
# consumer must name the producer workflow's own FILENAME. Derived from $CI (the
# single producer reference already used above), not a second hardcoded copy of
# the string being protected. The release `workflow_run` trigger keys on the
# workflow NAME, not the filename, so a file rename is not otherwise a loud
# failure — a stale artifact-workflow 404s only at post-tag publish time.
producer_wf=$(basename "$CI")
for f in "$CI" "$REL" "$OIDC"; do
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

# --- 6. Atomic tag topology ------------------------------------------------
# `--push-tags-only` leaves the version-sync bump commit reachable only from the
# tag and needs a separate post-publish branch fast-forward; any push reaching
# master inside that window makes the fast-forward impossible (422, release half
# published). `--push` is atomic — branch HEAD and tag are pushed together, or the tag job
# fails before anything publishes.
# Matched as a whole word (the args are space-padded first): `--push` is a
# prefix of `--push-tags-only` and `--push-dry-run`, and both of those leave the
# branch un-pushed — a substring match would pass them.
tag_args=$(yqr -r '.jobs.tag.steps[] | select(.name == "Auto-tag release") | .with.args' "$REL")
case "$tag_args" in
    "" | null)
        fail "tag topology: could not read the auto-tag step args from ${REL}." ;;
    *) case " ${tag_args} " in
        *" --push-tags-only "*)
            fail "tag topology: the auto-tag step passes --push-tags-only [${tag_args}] — the deferred-branch shape races any push to master. Use --push." ;;
        *" --push-dry-run "*)
            fail "tag topology: the auto-tag step passes --push-dry-run [${tag_args}] — it only prints the push commands, so no tag ever reaches the remote." ;;
        *" --push "*) ;;
        *)
            fail "tag topology: the auto-tag step args [${tag_args}] push nothing — the cut tag would never reach the remote." ;;
    esac ;;
esac
if [[ "$(yqr -r '.jobs | has("advance-master")' "$REL")" != "false" ]]; then
    fail "tag topology: ${REL} re-introduces an advance-master job — with an atomic tag push there is no stranded bump commit left to fast-forward onto."
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

# The main release job --skip=s HOSTED_PUBLISHERS; publish-oidc.yml --publishers=es
# the same set. GHA has no cross-file constant, so the two copies must stay
# byte-equal or the selectors stop being exact complements (a mismatch
# double-publishes or silently drops a hosted publisher).
oidc_hosted=$(yqr -r '.env.HOSTED_PUBLISHERS' "$OIDC")
if [[ -z "$oidc_hosted" || "$oidc_hosted" == "null" ]]; then
    fail "hosted set: could not read env.HOSTED_PUBLISHERS from ${OIDC}."
elif [[ "$oidc_hosted" != "$hosted" ]]; then
    fail "hosted set drift: ${OIDC} HOSTED_PUBLISHERS [${oidc_hosted}] != ${REL} [${hosted}] — the main job's --skip set and the OIDC job's --publishers set are no longer complements."
fi

# --- 9. publish-oidc.yml carries no npm token ------------------------------
# The whole point of the split job is that npm authenticates from the job's OIDC
# context. An NPM_TOKEN re-added to any step reverts that silently: `auth: auto`
# picks the token for every package name that does not exist yet, and a stale
# one takes PyPI and crates.io down with it through the shared preflight.
while IFS= read -r env_key; do
    [[ -z "$env_key" || "$env_key" == "null" ]] && continue
    case "${env_key^^}" in
        *NPM_TOKEN*) fail "tokenless OIDC job: ${OIDC} sets '${env_key}' — this job publishes npm through Trusted Publishing and must carry no npm token (a brand-new package name goes through manual-publish.yml instead)." ;;
    esac
done < <(yqr -r '[.env // {}] + [.jobs[].env // {}] + [.jobs[].steps[]?.env // {}] | .[] | keys[]' "$OIDC")

# --- 10. One rust-cache per job --------------------------------------------
# Which local composites carry a rust-cache step, and the input that turns it
# off. Derived from the composites themselves so a composite that gains a
# rust-cache step without an entry here fails the audit instead of slipping
# through as "not counted".
declare -A COMPOSITE_CACHE_OFF=(
    ["./.github/actions/setup-rust"]="cache"
    ["./.github/actions/setup-docs"]=""
)
for action_yml in .github/actions/*/action.yml; do
    action_dir="./${action_yml%/action.yml}"
    if grep -q 'uses: Swatinem/rust-cache' "$action_yml"; then
        [[ -v COMPOSITE_CACHE_OFF["$action_dir"] ]] || fail "rust-cache per job: composite ${action_dir} carries a Swatinem/rust-cache step but is unknown to this audit — add it to COMPOSITE_CACHE_OFF."
    elif [[ -v COMPOSITE_CACHE_OFF["$action_dir"] ]]; then
        fail "rust-cache per job: composite ${action_dir} no longer carries a Swatinem/rust-cache step — drop it from COMPOSITE_CACHE_OFF."
    fi
done
# anodizer-action provisions its own rust-cache whenever it builds from source
# (classify.sh: determinism, from-source, or from-branch set needs_cargo_cache).
for wf in .github/workflows/*.yml; do
    while IFS= read -r job; do
        [[ -z "$job" ]] && continue
        count=0
        # `|` rather than a tab: read collapses runs of IFS whitespace, so an
        # empty middle field would shift the columns.
        while IFS='|' read -r uses cache_input from_source from_branch determinism; do
            [[ -z "$uses" || "$uses" == "null" ]] && continue
            case "$uses" in
                Swatinem/rust-cache@*) count=$((count + 1)) ;;
                tj-smith47/anodizer-action@*)
                    if [[ "$from_source" == "true" || "$determinism" == "true" || ( -n "$from_branch" && "$from_branch" != "null" ) ]]; then
                        count=$((count + 1))
                    fi
                    ;;
                ./.github/actions/*)
                    if [[ -v COMPOSITE_CACHE_OFF["$uses"] ]]; then
                        off_input="${COMPOSITE_CACHE_OFF[$uses]}"
                        if [[ -z "$off_input" || "$cache_input" != "false" ]]; then
                            count=$((count + 1))
                        fi
                    fi
                    ;;
            esac
        done < <(yqr -r ".jobs[\"${job}\"].steps[]? | [.uses, .with.cache, .with[\"from-source\"], .with[\"from-branch\"], .with.determinism] | map(. // \"\") | join(\"|\")" "$wf")
        if (( count > 1 )); then
            fail "rust-cache per job: ${wf} job '${job}' runs ${count} Swatinem/rust-cache instances (direct, via a local composite, or via anodizer-action from-source/from-branch/determinism) — the first post step deletes the rustup proxies from ~/.cargo/bin and the second cannot run cargo metadata. Keep one."
        fi
    done < <(yqr -r '.jobs | keys[]' "$wf")
done

if [[ -n "$failures" ]]; then
    echo "audit-workflow-lockstep: FAIL — hand-synced workflow copies have drifted." >&2
    echo "" >&2
    printf '%s' "$failures" >&2
    exit 1
fi

echo "audit-workflow-lockstep: OK — shard roster, secret env, trigger gate, CI bootstrap gate, release/nightly mutex, bootstrap artifact (name + workflow file), atomic tag topology, cross-OS suite fallback, skip_publishers prose, the tokenless OIDC job, and one rust-cache per job are in lockstep."
