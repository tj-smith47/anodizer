#!/usr/bin/env bash
# Guard: comments speak in the third person, and the tool is called anodizer.
#
# Contract (.claude/rules, rule 8): a comment is written for whoever reads the
# code next, never as a record of the session that produced it. Rustdoc renders
# on a docs site and states WHAT the item does; an inline comment states the
# non-obvious WHY. Neither register has an author in it, so "we", "our", "us",
# "I" and "Claude" are out of place in both — a rustdoc `we` ships to users who
# have no idea who "we" are, and an inline `we` narrates a conversation the
# reader was not part of.
#
# The tool is spelled `anodizer`. `anodize` is the misspelling that shipped in
# messages, help strings and docs; the only legitimate occurrences are a
# `serde(alias = …)` accepting an old config spelling and a test asserting that
# alias still works.
#
# This audit fails (exit 1) on either count:
#
#   1. A first-person pronoun in the COMMENT half of a Rust line, or in a
#      whole-line `#` comment in a shell script, a workflow, or the Taskfile.
#   2. The tool name spelled `anodize`.
#   3. A figurative metaphor from the banned list (`load-bearing`, `blast
#      radius`, `bolted on`, `keystone`), in a comment or a doc page.
#
# Prose in markdown is NOT scanned: rule 8 governs comments, and the docsite's
# first person is deliberate project voice.
#
# A pronoun inside a string literal is CODE, so it cannot trip scan 1: the
# comment half comes from lib/rust-lex.awk. URLs, backtick spans, quoted runs
# and the literal `I/O` are blanked before the match, so `en-us`, a generic
# parameter written `I`, quoted upstream text and `no I/O` are not voice.
#
# Fix: rewrite the comment without the author. A rustdoc line states what the
# item does, so `We mirror the branch` becomes `The branch is mirrored`. An
# inline line states only the non-obvious WHY, so `We append, not overwrite`
# becomes `Appended: a host that set RUSTFLAGS meant it, and dropping it
# changes the build`.
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# The blanking runs before the match, in this order: URLs (a `/en-us/` path
# segment), then backtick spans, then quoted runs, then `I/O`. The boundary
# class excludes `-` as well as alphanumerics and `_`, so a hyphenated word
# never matches at either edge — `(I-5)`, `en-us` and `us-east-1` are not
# voice, and neither is the `us` inside `status`.
VOICE_RE="(^|[^A-Za-z0-9_'-])(we|We|we're|We're|we've|We've|our|Our|ours|Ours|us|Us|I|I'm|I've|I'd|let's|Let's|Claude)([^A-Za-z0-9_'-]|\$)"

# Scan 1a, Rust: the comment half of every line, via the shared lexer.
collect_files RS_FILES -rl --include='*.rs' --exclude-dir=target -- '//' crates
run_scanner rust_voice -v VOICE_RE="$VOICE_RE" -f "$LIB_DIR/rust-lex.awk" -f - \
    "${RS_FILES[@]}" <<'AWK'
    FNR == 1 { reset_lex() }
    {
        text = blanked(comment_part($0))
        if (text != "" && text ~ VOICE_RE) printf("%s:%d: %s\n", FILENAME, FNR, trim($0))
    }

    function blanked(t) {
        gsub(/https?:\/\/[^ \t>)\]]+/, " ", t)
        gsub(/`[^`]*`/, " ", t)
        gsub(/"[^"]*"/, " ", t)
        gsub(/I\/O/, " ", t)
        return t
    }

    function trim(l) { sub(/^[ \t]+/, "", l); return l }
AWK

# Scan 1b, shell / YAML: a whole-line `#` comment. A trailing `#` is not
# scanned because a `#` inside a shell string or a YAML scalar is data, and
# neither language has a lexer here to tell them apart.
collect_files SH_FILES -rl --include='*.sh' -- '#' .claude/scripts
collect_files YML_FILES -rl --include='*.yml' -- '#' .github/workflows
# The Taskfile is collected rather than named so a tree without one drops it
# instead of handing awk a path it cannot open, which exits 2 as a scan that
# did not run. `/dev/null` then keeps the operand list non-empty: awk with no
# file operand reads stdin, which the `-f -` program already drained.
collect_files TASKFILE -l -- '#' Taskfile.yml
run_scanner shell_voice -v VOICE_RE="$VOICE_RE" -f - \
    "${SH_FILES[@]}" "${YML_FILES[@]}" "${TASKFILE[@]}" /dev/null <<'AWK'
    {
        if ($0 !~ /^[ \t]*#/) next
        text = blanked($0)
        if (text ~ VOICE_RE) printf("%s:%d: %s\n", FILENAME, FNR, trim($0))
    }

    function blanked(t) {
        gsub(/https?:\/\/[^ \t>)\]]+/, " ", t)
        gsub(/`[^`]*`/, " ", t)
        gsub(/"[^"]*"/, " ", t)
        gsub(/I\/O/, " ", t)
        return t
    }

    function trim(l) { sub(/^[ \t]+/, "", l); return l }
AWK

# Scan 2, the tool name. The audit's own path is excluded because the pattern
# it searches for necessarily spells the misspelling, and so is every test
# fixture: a fixture carries the shape an audit must REPORT, so a fixture
# spelling the misspelling is the pin doing its job, not prose that shipped.
collect_files NAME_HITS -rnE --exclude-dir=target --exclude-dir=.git \
    --exclude-dir=fixtures --exclude='audit-prose.sh' \
    -- '\<[Aa]nodize\>' crates docs/site/content .github/workflows Taskfile.yml README.md

name_violations=""
for hit in "${NAME_HITS[@]}"; do
    # The config alias keeps an old spelling loadable, and the test that pins
    # the alias has to name it. Both are the spelling as DATA, not as prose.
    case "$hit" in
        *'alias = "anodize'* | *'alias("anodize'* | *'anodize_'*) continue ;;
    esac
    name_violations+="$hit"$'\n'
done

# Scan 3, figurative jargon. `load-bearing`, `blast radius`, `bolted on` and
# `keystone` are metaphors that say nothing the plain word does not: a field a
# comment calls "load-bearing" is required, a rollback with a small "blast
# radius" touches less state. They read as filler to anyone who has not heard
# them, and rustdoc ships them to users. Markdown IS scanned here (unlike the
# voice scan) because the docsite renders the same metaphors to the same
# readers. The audit's own path is excluded: the pattern necessarily spells
# every word it looks for.
collect_files JARGON_HITS -rnEi --exclude-dir=target --exclude-dir=.git \
    --exclude-dir=fixtures --exclude='audit-prose.sh' \
    -- 'load[- ]bearing|blast radius|bolted on|keystone' \
    crates docs/site/content .github/workflows .claude/scripts .claude/rules \
    Taskfile.yml README.md INCIDENT_RESPONSE.md .anodizer.yaml

jargon_violations=""
if ((${#JARGON_HITS[@]} > 0)); then
    printf -v jargon_violations '%s\n' "${JARGON_HITS[@]}"
fi

status=0

if [[ -n "$rust_voice$shell_voice" ]]; then
    echo "FIRST PERSON IN A COMMENT — the reader was not in the room."
    echo
    [[ -n "$rust_voice" ]] && echo "$rust_voice"
    [[ -n "$shell_voice" ]] && echo "$shell_voice"
    echo
    echo "A rustdoc comment renders for users who do not know who 'we' are, and an"
    echo "inline comment narrating a session tells the next reader nothing about the"
    echo "code. Rewrite in the third person: a rustdoc line states WHAT the item does,"
    echo "an inline line states only the non-obvious WHY."
    status=1
fi

if [[ -n "$name_violations" ]]; then
    [[ $status -eq 1 ]] && echo
    echo "THE TOOL IS CALLED ANODIZER — 'anodize' is the misspelling."
    echo
    printf '%s' "$name_violations"
    echo
    echo "Spell it anodizer in every message, help string, doc and workflow. The only"
    echo "legitimate 'anodize' is a serde alias keeping an old config spelling"
    echo "loadable, and the test that pins that alias."
    status=1
fi

if [[ -n "$jargon_violations" ]]; then
    [[ $status -eq 1 ]] && echo
    echo "FIGURATIVE JARGON — say the plain word instead."
    echo
    printf '%s' "$jargon_violations"
    echo
    echo "A field is required, not 'load-bearing'; a change touches less state, it"
    echo "does not have a smaller 'blast radius'; a feature was added, not 'bolted"
    echo "on'; a test pins an invariant, it is not a 'keystone'."
    status=1
fi

if [[ $status -eq 1 ]]; then
    exit 1
fi

echo "audit-prose: every comment speaks in the third person, in plain words; the tool is spelled anodizer."
