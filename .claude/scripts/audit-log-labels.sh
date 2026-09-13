#!/usr/bin/env bash
# Guard: status labels come ONLY from the log module, never open-coded.
#
# Contract (crates/core/src/log/): the Warning / Error / Note status labels
# are rendered by render_warning / render_error / render_note (and surfaced via
# StageLogger::warn / ::error / the tracing formatter). Those are the single
# source of truth for the label text, palette, AND format — a right-aligned
# gutter label with NO colon, aligned to the section-verb column. A stage that
# open-codes `format!("Warning: …")` / `.status("Error: …")` bypasses that
# authority and reintroduces the anti-Cargo `:`-suffixed, mis-aligned line the
# format test pins against.
#
# This audit fails (exit 1) when a string literal OPENS (immediately after its
# `"`) with a `Warning: ` / `Error: ` / `Note: ` label — colon then one space —
# anywhere in crate source outside the log module authority. That exact shape is the
# canonical open-coded status line; the audit intentionally does NOT chase
# labels assembled dynamically (e.g. `format!("{}: ", lbl)`) or mid-literal,
# which carry no `"<Label>: ` opener.
#
# Fix: call log.warn(msg) / log.error(msg) (or render_warning/render_error/
# render_note for the loggerless tracing path) with the bare message — the
# label and its format are added for you.
#
# The second pass holds the same authority over the DOCS: a fenced block in
# docs/site/content that quotes a line no renderer can produce sends a reader
# to file a bug about output anodizer never printed. Three shapes, all read
# off crates/core/src/log/:
#
#   1. `Warning: ` / `Error: ` / `Note: ` — the label carries no colon.
#   2. A `•` body line at an EVEN leading-space count — a body line is
#      `indent()` (two spaces per open section, so always even) plus the
#      3-space BODY_INDENT, so its column is always odd.
#   3. A `•` body line at 5 under a command that opens no log section. Only
#      `release`, `publish`, `continue` and `check determinism` ever call
#      `log.group`, so under every other subcommand the only reachable body
#      column is 3. Those four are held to the parity rule alone: a release
#      prints its stage lines inside a section (5) and its own orchestration
#      lines — `--split` / `--merge`, the `setup` group's contents — outside
#      one (3), and the fence cannot tell which a given line is.
#   4. A `[stage]` prefix — the stage name is carried by the section header
#      and the gutter, never repeated per line.
#
# Rules 2 and 3 read the fence's `$ anodizer <subcommand>` line to know which
# command the block transcribes; a fence that invokes several takes the most
# recent one above the bullet.
#
# Markdown admonitions (`> **Warning:** …`) live outside a fence and are
# prose, so they are untouched. Rule 4 asks for content after the `]`, which
# leaves a bare TOML section header (`[package]`) alone; a header that carries
# a trailing comment has content, so `toml` / `ini` fences and a `#` tail are
# skipped as well.
#
# `--self-test` runs the rules over a fixture tree and checks the verdicts.
# It runs ahead of every real scan, so a rule that stops firing fails here
# rather than in the next docs review.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Absolute: the scan `cd`s into the tree it audits before the self-test
# re-invokes this file.
SELF="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"
LIB_DIR="$SCRIPT_DIR/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"

# Write the fixture tree the self-test scans. Two pages: one where every rule
# must fire, one where none may.
write_self_test_fixture() {
    local dir="$1/docs/site/content/docs"
    mkdir -p "$dir"
    cat > "$dir/reported.md" <<'FIXTURE'
```text
Warning: colon suffixed
• even column zero bullet
  • even column two bullet
[verify-release] a stage prefix line
```

```bash
$ anodizer build
     • a section column under a command that opens none
```
FIXTURE
    cat > "$dir/clean.md" <<'FIXTURE'
• a bullet in prose is not a transcript

```bash
$ anodizer release
   • an orchestration line outside any section
     • a stage line inside one
```

```bash
$ anodizer build
   • the only column an ungrouped command reaches
```

```bash
$ anodizer check config
   • validating configuration
```

```toml
[package]
[dependencies] # the crate manifest
```

```ini
[section] # an ini header reads the same way
```
FIXTURE
}

# Run the docs rules over that tree and hold them to the verdicts above.
run_self_test() {
    local dir out status=0
    dir="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '$dir'" RETURN
    write_self_test_fixture "$dir"
    out="$(ANODIZER_AUDIT_LOG_LABELS_SELFTEST=1 bash "$SELF" "$dir" 2>&1)" || status=$?
    local expected='colon-suffixed label
unreachable bullet column: • even column zero bullet
unreachable bullet column:   • even column two bullet
stage-name prefix
ungrouped bullet column'
    local missing="" want
    while IFS= read -r want; do
        [[ "$out" == *"$want"* ]] || missing+="  $want"$'\n'
    done <<< "$expected"
    if ((status != 1)) || [[ -n "$missing" ]]; then
        echo "audit-log-labels --self-test FAILED (exit $status)" >&2
        [[ -n "$missing" ]] && printf 'not reported:\n%s' "$missing" >&2
        echo "$out" >&2
        exit 2
    fi
    if [[ "$out" == *clean.md* ]]; then
        echo "audit-log-labels --self-test FAILED: clean.md was reported" >&2
        echo "$out" >&2
        exit 2
    fi
}

if [[ "${1:-}" == "--self-test" ]]; then
    shift
    run_self_test
    echo "audit-log-labels: self-test passed."
    exit 0
fi

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# The rules are only worth running while they still fire; a scan whose rules
# went quiet reports a clean tree. The recursive call sets the marker so the
# self-test does not re-enter itself.
if [[ -z "${ANODIZER_AUDIT_LOG_LABELS_SELFTEST:-}" ]]; then
    run_self_test
fi

# A string literal opening with the label + colon: `"Warning: `, `"Error: `,
# `"Note: `. The leading quote distinguishes a printed label from prose like
# `// Note: …`, which carries no quote.
LABEL_RE='"(Warning|Error|Note): '

# The log module is the authority, so it is out of scope. The exemption is a
# DIRECTORY NAME, not one path: a `log/` under any crate is exempt, on the
# reading that a module named `log` is that crate's own label rendering.
collect_files LABEL_HITS -rnP --include='*.rs' \
    --exclude-dir=target --exclude-dir=log \
    -- "$LABEL_RE" crates/*/src

violations=""
for hit in "${LABEL_HITS[@]}"; do
    # Drop whole-line comments (// , /// , //! , leading * of a block comment):
    # an example label quoted inside a comment is documentation, not output.
    text="${hit#*:*:}"
    trimmed="${text#"${text%%[![:space:]]*}"}"
    case "$trimmed" in
        //* | '*'*) continue ;;
    esac
    violations+="$hit"$'\n'
done

if [[ -n "$violations" ]]; then
    echo "OPEN-CODED STATUS LABEL — Warning/Error/Note come only from log.rs."
    echo
    echo "$violations"
    echo "These string literals open-code a status-label prefix instead of going"
    echo "through the single authority in crates/core/src/log/. That reintroduces"
    echo "the colon-suffixed, mis-aligned line the format is pinned against."
    echo
    echo "Fix: call log.warn(msg) / log.error(msg) (or render_warning / render_error"
    echo "/ render_note for the loggerless tracing path) with the bare message; the"
    echo "label and gutter format are added for you."
    exit 1
fi

# The same authority governs every rendered transcript a docs page quotes.
DOCS_DIR="docs/site/content"
if [[ -d "$DOCS_DIR" ]]; then
    collect_files DOCS_FILES -rlE --include='*.md' \
        -- '^[[:space:]]*((Warning|Error|Note): |• |\[[a-z][a-z0-9-]*\][[:space:]])' \
        "$DOCS_DIR"
    if ((${#DOCS_FILES[@]} > 0)); then
        run_scanner docs_violations -f - "${DOCS_FILES[@]}" <<'AWK'
# The subcommands whose runs open a log section; every other one prints its
# body lines ungrouped, where 3 is the only reachable column.
function grouping(command) {
    return command == "" || command == "release" || command == "publish" ||
           command == "continue" || command == "check determinism"
}
FNR == 1 { fenced = 0; cmd = "" }
/^[[:space:]]*```/ {
    if (fenced) { fenced = 0; next }
    fenced = 1
    cmd = ""
    info = $0
    sub(/^[[:space:]]*```[[:space:]]*/, "", info)
    sub(/[[:space:]].*$/, "", info)
    next
}
fenced && $0 ~ /\$[[:space:]]+anodizer[[:space:]]/ {
    rest = $0
    sub(/^.*\$[[:space:]]+anodizer[[:space:]]+/, "", rest)
    split(rest, word, /[[:space:]]+/)
    cmd = word[1]
    if (cmd == "check" && word[2] != "" && word[2] !~ /^-/) {
        cmd = cmd " " word[2]
    }
}
fenced && $0 ~ /^[[:space:]]*(Warning|Error|Note): / {
    printf "%s:%d: colon-suffixed label: %s\n", FILENAME, FNR, $0
}
fenced && $0 ~ /^[[:space:]]*• / {
    leading = match($0, /[^ ]/) - 1
    if (leading % 2 == 0) {
        printf "%s:%d: unreachable bullet column: %s\n", FILENAME, FNR, $0
    } else if (!grouping(cmd) && leading != 3) {
        printf "%s:%d: ungrouped bullet column (`anodizer %s` opens no section): %s\n",
            FILENAME, FNR, cmd, $0
    }
}
fenced && info != "toml" && info != "ini" &&
    $0 ~ /^[[:space:]]*\[[a-z][a-z0-9-]*\][[:space:]]+[^[:space:]]/ {
    tail = $0
    sub(/^[[:space:]]*\[[a-z][a-z0-9-]*\][[:space:]]+/, "", tail)
    if (substr(tail, 1, 1) != "#") {
        printf "%s:%d: stage-name prefix: %s\n", FILENAME, FNR, $0
    }
}
AWK
        if [[ -n "$docs_violations" ]]; then
            echo "A DOCS TRANSCRIPT QUOTES A LINE THE RENDERER CANNOT PRODUCE."
            echo
            echo "$docs_violations"
            echo "crates/core/src/log/render.rs is the authority for all three shapes:"
            echo
            echo "  colon-suffixed label  the label is right-aligned in a 12-column"
            echo "                        gutter with NO colon."
            echo "  unreachable bullet    a body line is indent() (two spaces per open"
            echo "                        section) plus the 3-space BODY_INDENT, so it"
            echo "                        sits at 3 columns ungrouped, 5 inside a stage"
            echo "                        section — never an even count."
            echo "  ungrouped bullet      only release / publish / continue / check"
            echo "                        determinism open a log section, so under every"
            echo "                        other subcommand 3 is the only column a body"
            echo "                        line can reach."
            echo "  stage-name prefix     the stage name is carried by the section"
            echo "                        header and the gutter, never repeated as a"
            echo "                        per-line [stage] prefix."
            echo
            echo "Re-render the block at the renderer's geometry, or move the prose"
            echo "outside the fence."
            exit 1
        fi
    fi
fi

echo "audit-log-labels: no open-coded Warning/Error/Note status labels found."
