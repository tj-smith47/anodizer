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
#   3. A `[stage]` prefix — the stage name is carried by the section header
#      and the gutter, never repeated per line.
#
# Markdown admonitions (`> **Warning:** …`) live outside a fence and are
# prose, so they are untouched. A TOML section header (`[package]`) carries
# nothing after the `]`, so rule 3 asks for trailing content and leaves it be.
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

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
FNR == 1 { fenced = 0 }
/^[[:space:]]*```/ { fenced = !fenced; next }
fenced && $0 ~ /^[[:space:]]*(Warning|Error|Note): / {
    printf "%s:%d: colon-suffixed label: %s\n", FILENAME, FNR, $0
}
fenced && $0 ~ /^[[:space:]]*• / {
    leading = match($0, /[^ ]/) - 1
    if (leading % 2 == 0) {
        printf "%s:%d: unreachable bullet column: %s\n", FILENAME, FNR, $0
    }
}
fenced && $0 ~ /^[[:space:]]*\[[a-z][a-z0-9-]*\][[:space:]]+[^[:space:]]/ {
    printf "%s:%d: stage-name prefix: %s\n", FILENAME, FNR, $0
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
