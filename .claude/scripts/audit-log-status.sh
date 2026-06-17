#!/usr/bin/env bash
# Guard: no subprocess command-echo at default verbosity.
#
# Contract (crates/core/src/log.rs): `log.status(...)` prints a default-visible
# `•` line; `log.verbose(...)` shows only under `-v`. The literal subprocess
# command line (joined argv), rendered temp/config/output paths, and other
# internal execution detail belong at `verbose`. At default the user should see
# a stage header plus a concise per-artifact RESULT line (the stage-srpm idiom:
# `creating source RPM <name>`), never a `status("running <argv>")` echo.
#
# This audit fails (exit 1) when a `.status(` call echoes a subprocess command:
#   - the message text starts with running / invoking / executing followed by a
#     joined argv (`.join(" ")` / `_args` / `cmd_str` / `{program} {args}`), OR
#   - the message interpolates a concrete execution path/flag (a literal temp
#     path, --config / --output / --target).
#
# Legitimate high-level events (rollback / failure banners) are exempt: tag the
# line with a trailing `// status-ok: <why>` marker, or it must not match the
# argv/path shapes below. Demote a real command echo to `log.verbose(...)` and,
# if that leaves the step with no default output, add a concise result line.
set -euo pipefail

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# Command-echo shapes at .status(:
#   running/invoking/executing <argv>  where the argv is a joined command —
#   detected by the verb being followed (in the same format! arg) by a
#   `.join(" ")`, an `_args`/`cmd_args`/`cmd_str` interpolation, or the
#   `{program} {args}` pair the build stage uses.
# The grep is intentionally line-local: a status echo and its argv source sit
# on the same `format!` line in every known case (the contract forbids
# multi-line argv assembly inside a status call).
ARGV_RE='\.status\(&?format!\("[^"]*\b(running|invoking|executing)\b[^"]*"[^)]*(\.join\(|_args|cmd_args|cmd_str|\{program\} \{args\})'

# Rendered temp/config/output path or flag interpolated into a default status
# line. Restricted to concrete execution-detail markers (a literal temp path, a
# CLI flag like --config/--output/--target) — NOT a bare `{path}` variable,
# which legitimately appears in file-result lines (e.g. `+ {path}` scaffolding,
# `built X {name}`) and would false-positive on real default output.
PATH_RE='\.status\(&?format!\("[^"]*(/tmp/|--config\b|--output[= ]|--target[= ]|\.tmp\b)'

violations=""
while IFS= read -r hit; do
    # Skip (dry-run) lines: "show the user what would happen" is correct at
    # default. Skip explicit opt-out markers and #[cfg(test)] helper noise.
    case "$hit" in
        *'(dry-run)'*) continue ;;
        *'status-ok:'*) continue ;;
    esac
    violations+="$hit"$'\n'
done < <(grep -rnP "$ARGV_RE|$PATH_RE" crates/*/src --include='*.rs' 2>/dev/null || true)

if [[ -n "$violations" ]]; then
    echo "LOG STATUS-LEVEL COMMAND ECHO — default output must stay concise."
    echo
    echo "$violations"
    echo "These .status(...) calls echo a subprocess command (joined argv) or a"
    echo "rendered temp/config/output path at DEFAULT verbosity. Per the contract"
    echo "in crates/core/src/log.rs, the literal command belongs at log.verbose(...);"
    echo "at default emit only a concise per-artifact RESULT line (the stage-srpm"
    echo "idiom: \`creating source RPM <name>\`)."
    echo
    echo "Fix: demote the echo to log.verbose(...). If that leaves the step with no"
    echo "default output, add a concise result line (e.g. \`built MSI <name>\`)."
    echo "Legitimate high-level events (rollback/failure banners) tag the line with"
    echo "a trailing  // status-ok: <why>  marker."
    exit 1
fi

echo "audit-log-status: no status()-level command echoes found."
