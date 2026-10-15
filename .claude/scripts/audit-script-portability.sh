#!/usr/bin/env bash
# Guard: no script the macOS CI job can run uses a GNU-only option of a
# coreutils tool.
#
# The macOS runner ships the BSD tools, which reject GNU's extensions with an
# error and a non-zero status, so the script fails there and nowhere else:
#
#   grep -P / -oP / --perl-regexp   BSD grep has no Perl regex
#   sed -i (no suffix argument)     BSD sed reads the next word as the suffix
#   sed ':a;…;ba}' on one line      BSD sed reads `a}` as the label name
#   \n in a sed replacement         BSD sed writes a literal `n`
#   readlink -f, date -d, stat -c   GNU-only options
#
# awk, `sed -e` with one command per expression, and `grep -E` read the same
# on both. A line that runs only on a GNU host carries a trailing
# `# portable-ok: <why>` marker.
#
# The scan reads every shell script under `.claude/scripts`, `scripts` and
# `packaging`, the scripts of `.github/actions`, and the Taskfile, comment
# lines and this script's own patterns aside.
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"
source "$LIB_DIR/plural.sh"

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

collect_files FILES -rl --include='*.sh' --include='*.sh.tpl' -- '' \
    .claude/scripts scripts packaging .github/actions
collect_files TASKFILE -l -- '' Taskfile.yml
FILES+=("${TASKFILE[@]}")

if ((${#FILES[@]} == 0)); then
    echo "audit-script-portability: no scripts found."
    exit 0
fi

run_scanner violations -f - "${FILES[@]}" <<'AWK'
    FNR == 1 { file = FILENAME; own = (file ~ /audit-script-portability\.sh$/) }
    own { next }
    /^[[:space:]]*#/ { next }
    /# portable-ok:/ { next }
    {
        line = $0
        why = ""
        if (line ~ /(^|[^[:alnum:]_-])grep[[:space:]]([^|;&]*[[:space:]])?-[[:alpha:]]*P/ || line ~ /--perl-regexp/) {
            why = "grep -P (BSD grep has no Perl regex)"
        } else if (line ~ /(^|[^[:alnum:]_-])sed[[:space:]]([^|;&]*[[:space:]])?-i([[:space:]]|$)/) {
            why = "sed -i with no suffix (BSD sed reads the next word as the suffix)"
        } else if (line ~ /(^|[^[:alnum:]_-])sed[[:space:]]/ && line ~ /(:[[:alpha:]][[:alnum:]]*;|;b[[:alpha:]][[:alnum:]]*[;}])/) {
            why = "sed label and branch on one line (BSD sed reads to the end of the line as the label)"
        } else if (line ~ /(^|[^[:alnum:]_-])sed[[:space:]]/ && line ~ /s\/[^\/]*\/[^\/]*\\n/) {
            why = "\\n in a sed replacement (BSD sed writes a literal n)"
        } else if (line ~ /readlink[[:space:]]+-[[:alpha:]]*f/) {
            why = "readlink -f (GNU-only)"
        } else if (line ~ /(^|[^[:alnum:]_-])date[[:space:]]([^|;&]*[[:space:]])?-d[[:space:]]/) {
            why = "date -d (GNU-only)"
        } else if (line ~ /(^|[^[:alnum:]_-])stat[[:space:]]([^|;&]*[[:space:]])?-c[[:space:]]/) {
            why = "stat -c (GNU-only)"
        }
        if (why != "") {
            sub(/^[[:space:]]+/, "", line)
            printf "%s:%d: %s: %s\n", file, FNR, why, line
        }
    }
AWK

if [[ -n "$violations" ]]; then
    count=$(printf '%s\n' "$violations" | wc -l | tr -d ' ')
    echo "audit-script-portability: $(plural "$count" line lines) use a GNU-only tool option:"
    printf '%s\n' "$violations"
    echo
    echo "The macOS runner has BSD grep, sed, readlink, date and stat. Use awk or a"
    echo "form both read, or mark a GNU-only line with '# portable-ok: <why>'."
    exit 1
fi

echo "audit-script-portability: OK — $(plural "${#FILES[@]}" script scripts) read; no GNU-only tool option."
