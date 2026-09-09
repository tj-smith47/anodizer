#!/usr/bin/env bash
# Guard: no subprocess/HTTP command-echo at default verbosity.
#
# Contract (crates/core/src/log/): `log.status(...)` prints a default-visible
# `•` line; `log.verbose(...)` shows only under `-v`. The literal subprocess
# command line (joined argv), the bare HTTP request a publisher fires, rendered
# temp/config/output paths, and other internal execution detail belong at
# `verbose`. At default the user should see a stage header plus a concise
# per-artifact RESULT line (the stage-srpm idiom: `creating source RPM <name>`),
# never a `status("running <argv>")` or `status("DELETE <url>")` echo.
#
# This audit fails (exit 1) when a `.status(` call echoes a command/request:
#   - running / invoking / executing followed by a joined argv (`.join(" ")` /
#     `_args` / `cmd_str` / `{program} {args}`), OR by ANY `--flag` (so a hand-
#     assembled `running npm publish … --registry …` is caught, not only the
#     join helpers), OR
#   - a concrete execution path/flag (a literal temp path, --config/--output/
#     --target), OR
#   - a bare HTTP-verb request: `format!("DELETE {}", url)` and kin (the verb
#     immediately followed by a single `{}` and the closing quote). The outcome
#     variants — `"DELETE {} already absent"`, `"deleted {}"` — are RESULT
#     lines, not echoes, and are intentionally NOT matched, OR
#   - a raw subprocess stdio tee: a `[<stage> stdout]`/`[<stage> stderr]`
#     literal tag or a `stdout_str`/`stderr_str` capture interpolated into the
#     line. Raw child stdout/stderr (the cosign tlog lines, the sigstore consent
#     banner) is verbose-only; demote the tee to `log.verbose(...)`.
#
# The scan is multi-line aware: a `.status(&format!(` whose format string sits
# on the next line (a common rustfmt wrap) is reassembled before matching, so
# the echo cannot hide behind a line break.
#
# Legitimate high-level events (rollback / failure banners) are exempt: tag the
# line (or the line directly above the call) with a `// status-ok: <why>`
# marker, or keep the message off the shapes above. (dry-run) lines are exempt —
# "show the user what would happen" is correct at default. Demote a real echo to
# `log.verbose(...)`; if that leaves the step with no default output, add a
# concise result line.
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"

ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

collect_files FILES -rlE '\.status\(' crates/*/src --include='*.rs' --exclude-dir=target
if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "audit-log-status: no status() calls found."
    exit 0
fi

# A `.status(...)` argument is a violation when, after reassembling a
# next-line-wrapped format string, the text matches a command/request echo
# shape (and carries no (dry-run)/status-ok: exemption). The marker is read
# off the COMMENT half of the line (lib/rust-lex.awk's `comment_part`), so a
# `status-ok:` spelled inside the format string cannot exempt the echo it is
# printed by; `prev_cmt` lets a marker on the line directly above the call
# exempt it (the rustfmt-wrapped form keeps the marker above the
# `.status(&format!(` opener).
run_scanner violations -f "$LIB_DIR/rust-lex.awk" -f - "${FILES[@]}" <<'AWK'
    function trim(s)            { sub(/^[[:space:]]+/, "", s); return s }
    function exempt(b, bc, pc)  { return (b ~ /\(dry-run\)/) || (bc ~ /status-ok:/) || (pc ~ /status-ok:/) }
    function echoes(b,    verb) {
        verb = (b ~ /(running|invoking|executing)/)
        if (verb && b ~ /(\.join\(|_args|cmd_args|cmd_str|[{]program[}] [{]args[}])/) return 1
        if (verb && b ~ /--[a-z]/)                                                    return 1
        if (b ~ /(\/tmp\/|--config|--output[= ]|--target[= ]|\.tmp)/)                 return 1
        if (b ~ /"(GET|POST|PUT|DELETE|PATCH|HEAD) [{][}]"/)                          return 1
        # Raw subprocess stdio tee: a `[<stage> stdout]`/`[<stage> stderr]`
        # literal tag, or a `stdout_str`/`stderr_str` capture interpolated into
        # the line. cosign tlog noise + the sigstore consent banner ride out on
        # these — verbose-only per the contract.
        if (b ~ /(stdout\]|stderr\])/)                                               return 1
        if (b ~ /(stdout_str|stderr_str)/)                                           return 1
        return 0
    }

    FNR == 1 { pending = 0; prev_cmt = ""; reset_lex() }

    { cmt = comment_part($0) }

    # Continuation line carrying the format string for a `.status(&format!(`
    # opener seen on the previous line. Reassemble and judge against the marker
    # that sat above the opener.
    pending {
        buf = pend_prefix " " $0
        if (echoes(buf) && !exempt(buf, pend_cmt " " cmt, pend_prev_cmt))
            printf("%s:%d: %s\n", FILENAME, pend_fnr, trim(pend_prefix))
        pending = 0
        prev_cmt = cmt
        next
    }

    /\.status\(/ {
        # rustfmt wrap: `...log.status(&format!(` with the string on the next line.
        if ($0 ~ /\.status\(&?format!\([[:space:]]*$/) {
            pending = 1; pend_prefix = $0; pend_fnr = FNR
            pend_prev_cmt = prev_cmt; pend_cmt = cmt
            prev_cmt = cmt
            next
        }
        if (echoes($0) && !exempt($0, cmt, prev_cmt))
            printf("%s:%d: %s\n", FILENAME, FNR, trim($0))
        prev_cmt = cmt
        next
    }

    { prev_cmt = cmt }
AWK

if [[ -n "$violations" ]]; then
    echo "LOG STATUS-LEVEL COMMAND ECHO — default output must stay concise."
    echo
    echo "$violations"
    echo
    echo "These .status(...) calls echo a subprocess command (joined argv or a"
    echo "hand-assembled '<verb> … --flag'), a rendered temp/config/output path,"
    echo "or a bare HTTP request ('DELETE {url}') at DEFAULT verbosity. Per the"
    echo "contract in crates/core/src/log/, the literal command/request belongs"
    echo "at log.verbose(...); at default emit only a concise per-artifact RESULT"
    echo "line (the stage-srpm idiom: \`creating source RPM <name>\`)."
    echo
    echo "Fix: demote the echo to log.verbose(...). If that leaves the step with no"
    echo "default output, add a concise result line (e.g. \`published <name>\`,"
    echo "\`deleted <url>\`). Legitimate high-level events (rollback/failure banners)"
    echo "tag the line — or the line directly above it — with  // status-ok: <why>."
    exit 1
fi

echo "audit-log-status: no status()-level command/request echoes found."
