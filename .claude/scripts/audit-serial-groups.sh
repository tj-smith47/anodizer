#!/usr/bin/env bash
# Guard: every `#[serial]` names the group it serialises on.
#
# serial_test keeps ONE lock per group key. An unkeyed `#[serial]` takes the
# lock for the *unkeyed* group, and that group is unrelated to every keyed one —
# `#[serial]` and `#[serial(cwd)]` can therefore run AT THE SAME TIME. A test
# marked plain `#[serial]` beside cwd-swapping tests reads as protected and is
# not, which is exactly how a `#[serial(cwd)]` test moved the working directory
# out from under a plain `#[serial]` one (fixed in 06b87d6e).
#
# The failure is order-dependent — green locally, red at a different shard
# count — so it does not reproduce on demand and cannot be relied on to surface
# in review. Naming the group is what makes the protection real, and it is a
# purely syntactic property, so it is enforced here rather than remembered.
#
# Choosing the key: name the shared resource the test actually mutates, so that
# two tests serialise only when they genuinely contend. The groups in use are
# listed by `--groups`. Do NOT reach for one broad key: putting every test in a
# single group serialises the whole suite and trades a correctness bug for a
# wall-clock one. If a test mutates nothing process-global, the right fix is to
# delete `#[serial]` rather than invent a key for it.
set -euo pipefail

MODE="scan"
if [[ "${1:-}" == "--groups" ]]; then
    MODE="groups"
    shift
fi

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# `#[serial]`, `#[serial()]`, and the fully-qualified spellings, as an ATTRIBUTE
# — the leading `#[` must open the line (modulo indentation) so a rustdoc line
# or a string that merely mentions the attribute is not a finding.
UNKEYED_RE='^[[:space:]]*#\[(serial_test::)?(file_)?serial(\(\))?\][[:space:]]*$'

if [[ "$MODE" == "groups" ]]; then
    grep -rhoE '#\[(serial_test::)?(file_)?serial\([a-z_, ]+\)\]' crates/ --include='*.rs' |
        sed -E 's/.*serial\(([a-z_, ]+)\)\]/\1/' | tr ',' '\n' | tr -d ' ' |
        sort | uniq -c | sort -rn
    exit 0
fi

# Sites awaiting conversion, pinned as "<path>:<line>". This is a RATCHET, not
# an exemption: the list may only shrink, and the audit fails if a pinned line
# no longer holds an unkeyed attribute (it was converted — drop the entry) or if
# any unlisted one appears. That blocks every NEW unkeyed attribute while the
# remaining conversions land.
SERIAL_PENDING=(
    # crates/core/src/template/** was under concurrent edit when the rest of the
    # tree was converted. Both are cwd readers (the tests' own comments say so)
    # and want #[serial(cwd)].
    "crates/core/src/template/tests.rs:3339"
    "crates/core/src/template/tests.rs:3757"
)

all_unkeyed="$(grep -rnE "$UNKEYED_RE" crates/ --include='*.rs' || true)"

violations=""
pending_hit=()
while IFS= read -r line; do
    [[ -n "$line" ]] || continue
    site="$(printf '%s' "$line" | cut -d: -f1,2)"
    matched=""
    for p in "${SERIAL_PENDING[@]}"; do
        if [[ "$p" == "$site" ]]; then
            matched=1
            pending_hit+=("$site")
            break
        fi
    done
    [[ -n "$matched" ]] || violations+="$line"$'\n'
done <<<"$all_unkeyed"

# A pinned site that no longer matches has been converted; the pin must go with
# it, otherwise the list silently grows stale and stops meaning anything.
stale=""
for p in "${SERIAL_PENDING[@]}"; do
    found=""
    for h in ${pending_hit[@]+"${pending_hit[@]}"}; do
        [[ "$h" == "$p" ]] && found=1 && break
    done
    [[ -n "$found" ]] || stale+="  $p"$'\n'
done
if [[ -n "$stale" ]]; then
    echo "STALE SERIAL_PENDING ENTRY — pinned site no longer carries an unkeyed #[serial]."
    echo
    printf '%s' "$stale"
    echo
    echo "The conversion landed (or the line moved). Remove the entry from"
    echo "SERIAL_PENDING in this script so the list keeps meaning what it says."
    exit 1
fi

if [[ -n "$violations" ]]; then
    count="$(printf '%s\n' "$violations" | wc -l | tr -d ' ')"
    echo "UNKEYED #[serial] — does not serialise against any keyed group."
    echo
    printf '%s\n' "$violations"
    echo
    echo "$count attribute(s) above take the UNKEYED serial_test lock, which is"
    echo "independent of every keyed group. A test holding it runs concurrently"
    echo "with #[serial(cwd)], #[serial(path_env)] and every other keyed test —"
    echo "so it is not protected from any of them, only from other unkeyed ones."
    echo
    echo "Fix: name the shared resource the test mutates, e.g."
    echo "  #[serial(cwd)]        process working directory"
    echo "  #[serial(path_env)]   the PATH variable (binary stubbing)"
    echo "  #[serial(git_env)]    GIT_* identity variables"
    echo "  #[serial(<var>_env)]  one specific environment variable"
    echo "  #[serial(<name>)]     any other named process-global"
    echo
    echo "Run '$0 --groups' for the groups already in use — reuse one before"
    echo "coining a new key, and never widen a key to cover unrelated tests."
    echo
    echo "If the test mutates nothing process-global, DELETE the attribute"
    echo "instead: an unnecessary #[serial] costs wall-clock and hides the"
    echo "question of what the test actually contends on."
    exit 1
fi

total_keyed="$(grep -rhoE '#\[(serial_test::)?(file_)?serial\([a-z_, ]+\)\]' crates/ --include='*.rs' | wc -l | tr -d ' ')"
n_groups="$(grep -rhoE '#\[(serial_test::)?(file_)?serial\([a-z_, ]+\)\]' crates/ --include='*.rs' |
    sed -E 's/.*serial\(([a-z_, ]+)\)\]/\1/' | tr ',' '\n' | tr -d ' ' | sort -u | wc -l | tr -d ' ')"
echo "audit-serial-groups: all $total_keyed #[serial] attributes name a group ($n_groups distinct groups)."
if [[ ${#SERIAL_PENDING[@]} -gt 0 ]]; then
    echo "audit-serial-groups: ${#SERIAL_PENDING[@]} site(s) still unkeyed and pinned for conversion:"
    printf '  %s\n' "${SERIAL_PENDING[@]}"
fi
