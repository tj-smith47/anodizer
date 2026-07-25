#!/usr/bin/env bash
# Guard: every `crates/**.rs` citation in the docsite resolves — path AND symbol.
#
# The docsite's dogfooding + reference pages cite implementation files by path,
# both as link text and inside github.com/.../blob/master/<path> URLs, and most
# citations name the symbol that carries the claim beside the path
# (`(`gate_required_failures`)`, "covered by `foo_test_name`"). Both halves are
# plain strings: nothing compiles them, so a file move leaves a link that 404s
# for the reader, and a rename — or a symbol that never existed — leaves a claim
# that is simply false while every test still passes.
#
# Two recurring causes, one shared blast radius:
#   * the god-file split (a `foo.rs` becoming `foo/` with mod.rs + siblings)
#     invalidates every citation of the old path at once;
#   * moving a `#[cfg(test)]` body into a sibling `tests.rs` keeps the path
#     valid while every test name cited against it becomes a phantom.
#
# Both checks fail the audit (exit 1).
#
# Symbol extraction is deliberately narrow, so that a hit is a real defect:
#   * candidates are read ONLY from the table cell (or prose line) that carries
#     the citation, with `[text](url)` markup stripped first. The cited path and
#     its URL therefore never masquerade as a symbol, and a config key sitting
#     in the row's *first* column is out of scope: that column documents the
#     YAML surface, not the Rust surface.
#   * only three shapes count as a Rust symbol: snake_case
#     (`status_table_rows`), UpperCamelCase with two or more humps
#     (`ReconcileState`), and SCREAMING_SNAKE (`ANODIZER_ARCH`). A single
#     lowercase word, a dotted path, and YAML syntax (`nfpms[].scripts`) are
#     prose or config, never checked.
#   * anything that is a key in the generated JSON schema is dropped before the
#     shape test. A prose sentence naming `dockers_v2` or `retain_on_rollback`
#     beside an unrelated source path is discussing the config surface, not
#     claiming that file defines a Rust item — and those two vocabularies
#     collide on shape often enough that shape alone cannot separate them. The
#     schema is the generated, authoritative list of config keys, so the
#     exclusion never drifts from the config surface it protects.
#   * a symbol passes when it word-matches ANY path cited in the same cell —
#     the impl-plus-tests pair on one row is the common case and must not
#     force the author to split the row.
#
# When the schema is absent the config-key vocabulary is unknowable, so the
# symbol check is skipped with a notice rather than run without its filter: a
# storm of config-key false positives would train readers to ignore the audit.
#
# Fix: repoint the citation at the file that now holds the cited code (prefer
# the specific sibling — `preflight/tests.rs` — over the module root when a
# symbol is named; `mod.rs` only for a subsystem-wide reference), or correct the
# symbol to the one that exists.
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

SCHEMA="docs/site/static/schema.json"
if [[ ! -f "$SCHEMA" ]]; then
    echo "audit-doc-source-links: every cited crates/**.rs doc path resolves."
    echo "audit-doc-source-links: $SCHEMA is missing, so config keys cannot be told"
    echo "  apart from Rust symbols; the cited-symbol check is skipped this run."
    exit 0
fi

phantoms=$(
    find "$DOCS_DIR" -name '*.md' -print0 | xargs -0 perl -e '
use strict;
use warnings;

my $schema = shift @ARGV;
my %config_key;
if (open my $sfh, "<", $schema) {
    local $/;
    my $body = <$sfh>;
    close $sfh;
    while ($body =~ /"([A-Za-z_][A-Za-z0-9_]*)"\s*:/g) { $config_key{$1} = 1 }
}

my %slurped;
sub file_body {
    my ($path) = @_;
    return $slurped{$path} if exists $slurped{$path};
    my $body = "";
    if (open my $fh, "<", $path) {
        local $/;
        $body = <$fh>;
        close $fh;
    }
    $slurped{$path} = $body;
    return $body;
}

for my $doc (@ARGV) {
    open my $fh, "<", $doc or next;
    my $lineno = 0;
    while (my $line = <$fh>) {
        $lineno++;
        next unless $line =~ m{crates/[A-Za-z0-9_./-]+\.rs};

        # A markdown table row scopes each citation to its own cell; anything
        # else (prose, list item) is treated as one cell.
        my @cells = ($line =~ /^\s*\|/) ? split(/\|/, $line) : ($line);
        for my $cell (@cells) {
            next unless $cell =~ m{crates/[A-Za-z0-9_./-]+\.rs};

            my @paths = ($cell =~ m{(crates/[A-Za-z0-9_./-]+\.rs)}g);
            my %seen_path;
            @paths = grep { !$seen_path{$_}++ } @paths;

            my $prose = $cell;
            $prose =~ s/\[[^\]]*\]\([^)]*\)//g;
            $prose =~ s{https?://\S+}{}g;

            for my $sym ($prose =~ /`([^`]+)`/g) {
                next unless $sym =~ /^[A-Za-z_][A-Za-z0-9_]*$/;
                next if $config_key{$sym};
                next unless $sym =~ /^[a-z][a-z0-9]*(?:_[a-z0-9]+)+$/
                         || $sym =~ /^[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+$/
                         || $sym =~ /^[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+)+$/;

                my $hit = 0;
                for my $path (@paths) {
                    if (file_body($path) =~ /\b\Q$sym\E\b/) { $hit = 1; last }
                }
                next if $hit;

                printf "  %s:%d\n      symbol `%s` is not in %s\n",
                    $doc, $lineno, $sym, join(", ", @paths);
            }
        }
    }
    close $fh;
}
' "$SCHEMA"
)

if [[ -n "$phantoms" ]]; then
    echo "PHANTOM DOC SYMBOL — a docsite claim names a symbol its cited file does not contain."
    echo
    echo "$phantoms"
    echo
    echo "Each line above is a claim the reader cannot verify: the path resolves but"
    echo "the function, struct, or test named beside it is not in that file. Either"
    echo "the symbol was renamed or moved (commonly into a sibling \`tests.rs\`), or"
    echo "it never existed."
    echo
    echo "Fix: repoint the citation at the file that holds the symbol, or replace"
    echo "the symbol with the real one. Never delete the name to silence this —"
    echo "an unnamed claim is an unverifiable claim."
    exit 1
fi

echo "audit-doc-source-links: every cited crates/**.rs doc path resolves, and every symbol cited beside one exists in it."
