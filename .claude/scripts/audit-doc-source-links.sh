#!/usr/bin/env bash
# Guard: every `crates/**.rs` path cited in the docsite resolves on disk.
#
# The docsite's dogfooding + reference pages cite implementation files by path,
# both as link text and inside github.com/.../blob/master/<path> URLs. Those
# paths are plain strings: nothing compiles them, so a file move leaves a link
# that 404s for the reader while every test still passes.
#
# The recurring cause is the god-file split (a `foo.rs` becoming `foo/` with
# mod.rs + siblings). Each split silently invalidates every doc citation of the
# old path at once, and the breakage is invisible until a reader clicks.
#
# This audit fails (exit 1) when a cited path does not exist as a file. It
# checks existence only — whether the path still holds the SYMBOL named beside
# it is beyond a path check, so a split that preserves the filename but moves a
# function will pass here and must be caught in review.
#
# Fix: repoint the citation at the file that now holds the cited code. Prefer
# the specific sibling (`preflight/checkers.rs`) over the module root when the
# citation names a symbol; use `mod.rs` when it references the subsystem
# broadly.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

DOCS_DIR="docs/site/content"

if [[ ! -d "$DOCS_DIR" ]]; then
    echo "audit-doc-source-links: no $DOCS_DIR directory; nothing to check."
    exit 0
fi

# Cited paths are repo-relative and always start at `crates/`. Collect the
# unique set across every page, then test each for existence.
broken=""
while IFS= read -r path; do
    [[ -z "$path" ]] && continue
    if [[ ! -f "$path" ]]; then
        # Report every page that cites it so the fix is one pass, not a hunt.
        cites=$(grep -rln -- "$path" "$DOCS_DIR" 2>/dev/null | tr '\n' ' ')
        broken+="  $path"$'\n'"      cited by: $cites"$'\n'
    fi
done < <(
    grep -rhoP 'crates/[A-Za-z0-9_./-]+\.rs' "$DOCS_DIR" 2>/dev/null | sort -u || true
)

if [[ -n "$broken" ]]; then
    echo "BROKEN DOC SOURCE LINK — a cited crates/**.rs path does not exist."
    echo
    echo "$broken"
    echo "These docsite citations point at files that are gone. The usual cause is"
    echo "a god-file split: \`foo.rs\` became \`foo/\` with mod.rs plus siblings, and"
    echo "every citation of the old path broke at once."
    echo
    echo "Fix: repoint each citation at the file that now holds the cited code —"
    echo "the specific sibling when a symbol is named, mod.rs for a subsystem-wide"
    echo "reference."
    exit 1
fi

echo "audit-doc-source-links: every cited crates/**.rs doc path resolves."
