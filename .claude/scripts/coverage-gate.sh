#!/usr/bin/env bash
# Fail when workspace line coverage drops below the minimum.
# Usage: coverage-gate.sh <cobertura.xml> [minimum]
# Minimum precedence: arg > COVERAGE_MINIMUM env > 92.5 (the single source of
# truth for the minimum; callers must not restate the number). The minimum only
# ever ratchets UP: a run that measures below it is a regression to fix, never
# a reason to move the number down.
# Delegates percent extraction to coverage-percent.sh so the gate and the
# badge can never disagree about the measured value. The comparison uses
# the script's --raw (full-precision) output: the rounded badge form would
# round 91.99 up to 92 and mask a run under the minimum.
set -euo pipefail

XML="${1:?Usage: coverage-gate.sh <cobertura.xml> [minimum]}"
MINIMUM="${2:-${COVERAGE_MINIMUM:-92.5}}"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
RAW="$(bash "$SCRIPT_DIR/coverage-percent.sh" --raw "$XML")"
BADGE="$(bash "$SCRIPT_DIR/coverage-percent.sh" "$XML")"
ROUNDED="${BADGE%\%}"

# Human-readable form of the raw value: trim trailing zeros so the verdict
# reads "92.99", not "92.990000".
DISPLAY="$(awk -v r="$RAW" 'BEGIN {s = sprintf("%.4f", r); sub(/0+$/, "", s); sub(/\.$/, "", s); print s}')"
LABEL="${DISPLAY}%"
# When rounding hides precision that matters (91.99 vs badge 92.0), show both.
if [ "$DISPLAY" != "$ROUNDED" ]; then
  LABEL="${DISPLAY}% (badge ${ROUNDED}%)"
fi

# awk for the comparison: bash arithmetic can't handle floats.
if awk -v p="$RAW" -v f="$MINIMUM" 'BEGIN {exit !(p + 0 >= f + 0)}'; then
  echo "coverage ${LABEL} >= minimum ${MINIMUM}%"
else
  echo "coverage ${LABEL} below minimum ${MINIMUM}%" >&2
  exit 1
fi
