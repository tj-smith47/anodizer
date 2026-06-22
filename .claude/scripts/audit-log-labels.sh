#!/usr/bin/env bash
# Guard: status labels come ONLY from log.rs, never open-coded.
#
# Contract (crates/core/src/log.rs): the Warning / Error / Note status labels
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
# anywhere in crate source outside the log.rs authority. That exact shape is the
# canonical open-coded status line; the audit intentionally does NOT chase
# labels assembled dynamically (e.g. `format!("{}: ", lbl)`) or mid-literal,
# which carry no `"<Label>: ` opener.
#
# Fix: call log.warn(msg) / log.error(msg) (or render_warning/render_error/
# render_note for the loggerless tracing path) with the bare message — the
# label and its format are added for you.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# A string literal opening with the label + colon: `"Warning: `, `"Error: `,
# `"Note: `. The leading quote distinguishes a printed label from prose like
# `// Note: …`, which carries no quote.
LABEL_RE='"(Warning|Error|Note): '

violations=""
while IFS= read -r hit; do
    # Drop whole-line comments (// , /// , //! , leading * of a block comment):
    # an example label quoted inside a comment is documentation, not output.
    text="${hit#*:*:}"
    trimmed="${text#"${text%%[![:space:]]*}"}"
    case "$trimmed" in
        //* | '*'*) continue ;;
    esac
    violations+="$hit"$'\n'
done < <(
    grep -rnP "$LABEL_RE" crates/*/src --include='*.rs' 2>/dev/null \
        | grep -v 'crates/core/src/log.rs:' || true
)

if [[ -n "$violations" ]]; then
    echo "OPEN-CODED STATUS LABEL — Warning/Error/Note come only from log.rs."
    echo
    echo "$violations"
    echo "These string literals open-code a status-label prefix instead of going"
    echo "through the single authority in crates/core/src/log.rs. That reintroduces"
    echo "the colon-suffixed, mis-aligned line the format is pinned against."
    echo
    echo "Fix: call log.warn(msg) / log.error(msg) (or render_warning / render_error"
    echo "/ render_note for the loggerless tracing path) with the bare message; the"
    echo "label and gutter format are added for you."
    exit 1
fi

echo "audit-log-labels: no open-coded Warning/Error/Note status labels found."
