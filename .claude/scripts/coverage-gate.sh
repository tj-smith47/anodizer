#!/usr/bin/env bash
# Fail when workspace line coverage drops below the floor.
# Usage: coverage-gate.sh <cobertura.xml> [floor]
# Floor precedence: arg > COVERAGE_FLOOR env > 92.5 (the single source of
# truth for the floor — callers must not restate the number).
# Delegates percent extraction to coverage-percent.sh so the gate and the
# badge can never disagree about the measured value. The comparison uses
# the script's --raw (full-precision) output: the rounded badge form would
# round 92.49 up to 92.5 and mask a floor breach.
set -euo pipefail

XML="${1:?Usage: coverage-gate.sh <cobertura.xml> [floor]}"
FLOOR="${2:-${COVERAGE_FLOOR:-92.5}}"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
RAW="$(bash "$SCRIPT_DIR/coverage-percent.sh" --raw "$XML")"
BADGE="$(bash "$SCRIPT_DIR/coverage-percent.sh" "$XML")"
ROUNDED="${BADGE%\%}"

# Human-readable form of the raw value: trim trailing zeros so the verdict
# reads "92.99", not "92.990000".
DISPLAY="$(awk -v r="$RAW" 'BEGIN {s = sprintf("%.4f", r); sub(/0+$/, "", s); sub(/\.$/, "", s); print s}')"
LABEL="${DISPLAY}%"
# When rounding hides precision that matters (92.49 vs badge 92.5), show both.
if [ "$DISPLAY" != "$ROUNDED" ]; then
  LABEL="${DISPLAY}% (badge ${ROUNDED}%)"
fi

# awk for the comparison: bash arithmetic can't handle floats.
if awk -v p="$RAW" -v f="$FLOOR" 'BEGIN {exit !(p + 0 >= f + 0)}'; then
  echo "coverage ${LABEL} >= floor ${FLOOR}%"
else
  echo "coverage ${LABEL} below floor ${FLOOR}%" >&2
  exit 1
fi
