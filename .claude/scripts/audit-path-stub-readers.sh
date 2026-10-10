#!/usr/bin/env bash
# Guard: a test that spawns a tool its own test binary stubs on `PATH` takes
# `#[serial(path_env)]`.
#
# audit-path-stub-serial.sh holds the mutators to one key. This is the other
# half: the READERS. A reader is an ordinary test that reaches, frames down in
# production code, a spawn of `hdiutil` / `docker` / `cosign` by bare name. It
# stubs nothing and mutates nothing, so nothing in its source says it reads
# `PATH`; while a mutator in the same binary has a stub directory active, the
# reader runs the stub.
#
# No source pattern finds them. Measured over the 13 test binaries that hold a
# mutator, against the 25 tests that really spawn a stubbed tool:
#
#   trigger on the test body              flagged   found   false positives
#   every test in the binary                 9444   25/25              9419
#   a called fn reaches the tool (ws)        8077   25/25              8052
#   a called fn reaches the tool (crate)     1715   22/25              1693
#   the tool's name in a string literal       282    7/25               275
#   `dry_run: false`                           42    4/25                38
#
# So the readers are measured instead. Every test of every mutator-holding
# binary is run alone, with a directory of tripwires first on `PATH` — one per
# tool name the workspace stubs. A tripwire records that it ran and hands over
# to the real tool. A test that reaches the tripwire of a tool ITS OWN binary
# stubs is a reader, and must be keyed.
#
# The tripwire finds the test that spawned it by walking its parent chain to
# the test process, which the runner registered under its pid before starting
# the test. It carries no state in the environment, so a test that spawns the
# tool under `env_clear()` is measured like any other. Only a DIRECT spawn
# fails the audit (the first parent is the test process). A tool reached
# through another tool — `gpg`, run by a `git commit` on a host whose git
# config signs commits — depends on the host and is reported as a note.
#
# What each crate stubs comes from the mutator audit's records: every
# `.tool("…")` named in a fn that swaps PATH or is a helper, plus a file-level
# declaration for the stubs a file builds by hand under an `EnvGuard`:
#   // path-stubs: cargo npm — <why>      or     // path-stubs: none — <why>
# A file that swaps PATH by hand and carries no declaration stops the audit.
#
# One exemption, with a reason, in the attribute/comment run above a test fn:
#   // path-ok: <why>    this reader's verdict does not depend on which of
#                        the two it ran.
#
# Exit 1 is a finding. Exit 2 is "the scan did not run": no mutator, a PATH
# swap in an integration test (a binary this script does not build), an
# undeclared hand-built stub, a build failure, a test binary that is not
# executable or cannot list its tests, a test that timed out or could not be
# started, or no test measured.
#
# Slow (it builds and runs the test binaries), so it runs in `task gate`, not
# `task lint`.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIB_DIR="$SCRIPT_DIR/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

not_run() {
    printf 'audit-path-stub-readers: %s; the scan did not run.\n' "$1" >&2
    exit 2
}

records=""
status=0
records="$(bash "$SCRIPT_DIR/audit-path-stub-serial.sh" --records "$ROOT")" || status=$?
((status == 0)) || not_run "audit-path-stub-serial.sh --records exited $status"

# A mutator in an integration test is a different binary from the crate's
# unit tests, and this script measures the unit-test binaries only.
run_scanner integration '$1 == "T" && $3 ~ /(^|\/)crates\/[^\/]+\/tests\//' < <(printf '%s\n' "$records")
integration="$(printf '%s\n' "$integration" | sort -u)"
if [[ -n "$integration" ]]; then
    printf 'audit-path-stub-readers: a PATH swap under crates/*/tests is not measured; the scan did not run.\n' >&2
    printf '%s\n' "$integration" | sed 's/^T [^ ]* \([^ ]*\) .*/  \1/' >&2
    exit 2
fi

run_scanner crate_list '$1 == "T" { print $2 }' < <(printf '%s\n' "$records")
CRATES=()
[[ -n "$crate_list" ]] && mapfile -t CRATES < <(printf '%s\n' "$crate_list" | sort -u)
((${#CRATES[@]})) || not_run "no test swaps PATH"

# A file swapping PATH by hand declares what it puts there.
run_scanner undeclared '
    $1 == "D" { declared[$3] = 1 }
    $1 == "R" { raw[$3] = 1 }
    END { for (f in raw) if (!(f in declared)) print f }' < <(printf '%s\n' "$records")
undeclared="$(printf '%s\n' "$undeclared" | sort)"
if [[ -n "$undeclared" ]]; then
    printf 'audit-path-stub-readers: a file swaps PATH by hand and does not say what it puts there; the scan did not run.\n' >&2
    printf '%s\n' "$undeclared" | sed 's/^/  /' >&2
    printf '  Add a line   // path-stubs: <tool names> — <why>   or   // path-stubs: none — <why>\n' >&2
    exit 2
fi

WORK="${CARGO_TARGET_DIR:-$ROOT/target}/path-stub-readers"
rm -rf "$WORK"
mkdir -p "$WORK/trip" "$WORK/pids"

# crate -> the tool names its tests put on PATH.
declare -A STUBBED=()
all_tools=""
for crate in "${CRATES[@]}"; do
    run_scanner names -v c="$crate" '
        $1 == "S" && $2 == c { print $3 }
        $1 == "D" && $2 == c { for (i = 4; i <= NF; i++) if ($i != "none") print $i }' < <(printf '%s\n' "$records")
    names="$(printf '%s\n' "$names" | sort -u | tr '\n' ' ')"
    STUBBED[$crate]="$names"
    all_tools+=" $names"
done

# The tripwire knows its log and the audit's PATH from its own text: a child
# spawned with a cleared environment has neither.
ORIG_PATH="$PATH"
for tool in $(printf '%s\n' $all_tools | sort -u); do
    {
        printf '#!/bin/sh\n'
        printf 'orig_path=%q\n' "$ORIG_PATH"
        printf 'work=%q\n' "$WORK"
        cat <<'TRIP'
[ -n "${PATH:-}" ] || PATH="$orig_path"
export PATH
me=$(basename "$0")
self=$(dirname "$0")
# Walk up to the registered test process: the first parent is a direct spawn.
p=$PPID; how=""; hop=0
while [ -n "$p" ] && [ "$p" -gt 1 ] 2>/dev/null; do
    if [ -f "$work/pids/$p" ]; then
        if [ "$hop" = 0 ]; then how=direct; else how=indirect; fi
        printf '%s\t%s\t%s\n' "$(cat "$work/pids/$p")" "$me" "$how" >> "$work/pids/$p.hits"
        break
    fi
    hop=$((hop + 1))
    p=$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' ')
done
IFS=:
for d in $PATH; do
    [ "$d" = "$self" ] && continue
    if [ -x "$d/$me" ] && [ ! -d "$d/$me" ]; then exec "$d/$me" "$@"; fi
done
echo "$me: not found" >&2
exit 127
TRIP
    } > "$WORK/trip/$tool"
    chmod +x "$WORK/trip/$tool"
done

# `<crate> <test binary>` per line: given by the caller, or built here with
# the features `task gate` tests under (the defaults).
BINARIES="$WORK/binaries.txt"
if [[ -n "${PATH_STUB_READERS_BINARIES:-}" ]]; then
    cp "$PATH_STUB_READERS_BINARIES" "$BINARIES"
else
    if ! cargo test --no-run --workspace --lib --bins --message-format=json \
        > "$WORK/build.json" 2> "$WORK/build.err"; then
        cat "$WORK/build.err" >&2
        not_run "building the workspace tests failed"
    fi
    : > "$BINARIES"
    for crate in "${CRATES[@]}"; do
        # One compiler-artifact line per test executable, filtered by the
        # manifest it was built from.
        sed -n "/\"manifest_path\":\"[^\"]*\/crates\/$crate\/Cargo.toml\"/p" "$WORK/build.json" |
            sed -n '/"test":true/p' |
            sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' |
            while IFS= read -r exe; do
                [[ -n "$exe" ]] && printf '%s %s\n' "$crate" "$exe" >> "$BINARIES"
            done
    done
fi

HAVE_TIMEOUT=0
command -v timeout >/dev/null 2>&1 && HAVE_TIMEOUT=1
# Registers the test process under its own pid, then becomes it.
REGISTER_THEN_EXEC='printf "%s" "$1" > "$2/pids/$$"; exec "${@:3}"'

# Runs one test alone. The test process is registered under its pid before it
# starts, so a tripwire can name it. The test's own verdict is not this
# audit's question: only a run that could not happen — a timeout, a binary
# that could not be started — is recorded.
run_one() {
    local bin="$1" name="$2" out="$3" index="$4" code=0
    local -a args=(_ "$name" "$WORK" "$bin" --exact "$name" --test-threads=1)
    if ((HAVE_TIMEOUT)); then
        PATH="$WORK/trip:$PATH" timeout 300 bash -c "$REGISTER_THEN_EXEC" "${args[@]}" >/dev/null 2>&1 || code=$?
    else
        PATH="$WORK/trip:$PATH" bash -c "$REGISTER_THEN_EXEC" "${args[@]}" >/dev/null 2>&1 || code=$?
    fi
    case "$code" in
        124 | 125 | 126 | 127) printf '%s\n' "$code" > "$out/failed.$index" ;;
    esac
}

violations=""
indirect=0
measured=0
binaries=0
while read -r crate bin; do
    [[ -n "$crate" ]] || continue
    binaries=$((binaries + 1))
    out="$WORK/$crate.$binaries"
    mkdir -p "$out"
    [[ -x "$bin" ]] || not_run "test binary $bin is not executable"
    # A test binary is started from its crate directory, the way cargo does it.
    crate_dir="$ROOT/crates/$crate"
    if ! bash -c 'cd "$1" && exec "$2" --list --format terse' _ "$crate_dir" "$bin" \
        > "$out/listing.txt" 2> "$out/listing.err"; then
        cat "$out/listing.err" >&2
        not_run "$bin could not list its tests"
    fi
    sed -n 's/: test$//p' "$out/listing.txt" > "$out/tests.txt"
    count="$(wc -l < "$out/tests.txt" | tr -d ' ')"
    measured=$((measured + count))

    rm -f "$WORK"/pids/*
    index=0
    running=0
    while IFS= read -r name; do
        [[ -n "$name" ]] || continue
        index=$((index + 1))
        (cd "$crate_dir" && run_one "$bin" "$name" "$out" "$index") &
        running=$((running + 1))
        if ((running >= ${PATH_STUB_READERS_JOBS:-4})); then
            wait -n
            running=$((running - 1))
        fi
    done < "$out/tests.txt"
    wait
    if compgen -G "$out/failed.*" > /dev/null; then
        for f in "$out"/failed.*; do
            index="${f##*.}"
            name="$(sed -n "${index}p" "$out/tests.txt")"
            printf 'audit-path-stub-readers: %s %s exited %s (timed out or could not be started)\n' \
                "$bin" "$name" "$(cat "$f")" >&2
        done
        not_run "a test could not be run"
    fi
    cat "$WORK"/pids/*.hits > "$out/hits.tsv" 2>/dev/null || :
    [[ -s "$out/hits.tsv" ]] || continue

    declare -A reader_tools=()
    while IFS=$'\t' read -r name tool how; do
        [[ " ${STUBBED[$crate]:-} " == *" $tool "* ]] || continue
        if [[ "$how" != "direct" ]]; then
            indirect=$((indirect + 1))
            continue
        fi
        [[ " ${reader_tools[$name]:-} " == *" $tool "* ]] && continue
        reader_tools[$name]+="${reader_tools[$name]:+ }$tool"
    done < <(sort -u "$out/hits.tsv")
    ((${#reader_tools[@]})) || { unset reader_tools; continue; }

    for name in "${!reader_tools[@]}"; do
        short="${name##*::}"
        tools="${reader_tools[$name]// /, }"
        # Two tests of one name in two modules cannot be told apart from the
        # binary's own listing, so every unkeyed one is reported.
        run_scanner sites -v c="$crate" -v n="$short" \
            '$1 == "F" && $2 == c && $4 == n { print $3, ($5 || $6) }' < <(printf '%s\n' "$records")
        if [[ -z "$sites" ]]; then
            violations+="crates/$crate: [reader] $name spawns $tools and its source was not found"$'\n'
            continue
        fi
        while read -r site ok; do
            [[ "$ok" == "1" ]] && continue
            violations+="$site: [reader] fn $short spawns $tools, which tests in this binary stub on PATH, without #[serial(path_env)]"$'\n'
        done <<< "$sites"
    done
    unset reader_tools
done < "$BINARIES"

if ((binaries == 0 || measured == 0)); then
    printf 'audit-path-stub-readers: no test was run (%d binaries, %d tests); the scan did not run.\n' \
        "$binaries" "$measured" >&2
    exit 2
fi

if [[ -n "$violations" ]]; then
    count="$(printf '%s' "$violations" | wc -l | tr -d ' ')"
    echo "UNKEYED PATH READER — spawns a tool that a parallel test replaces with a stub."
    echo
    printf '%s' "$violations" | sort
    echo
    echo "$count test(s) above spawn a tool by name while other tests in the same"
    echo "binary prepend a stub of that tool to the process PATH. Run in parallel,"
    echo "the reader executes the stub: a canned answer, or a file already deleted."
    echo
    echo "Fix: add  #[serial_test::serial(path_env)]  to the test."
    echo
    echo "If the test's verdict cannot depend on which of the two it ran, say why in"
    echo "the comment run above it:  // path-ok: <why>"
    exit 1
fi

echo "audit-path-stub-readers: $measured tests in $binaries binaries of ${#CRATES[@]} crate(s) run alone; every test that spawns a tool its binary stubs takes #[serial(path_env)]."
if ((indirect > 0)); then
    echo "audit-path-stub-readers: note — $indirect spawn(s) of a stubbed tool were made by another tool, not by a test (host configuration, such as a git config that signs commits)."
fi
