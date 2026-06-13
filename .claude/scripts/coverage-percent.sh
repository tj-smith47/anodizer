#!/usr/bin/env bash
# Print the workspace coverage percentage as it appears in the README badge,
# extracted from cobertura.xml's top-level line-rate. One line of output:
#   <percent>%
# Same formula the badge publisher uses; downstream tools can grep this.
#
# --raw prints the full-precision percent (line-rate*100, no rounding, no %
# sign). Threshold checks must use it: the badge form rounds to one decimal,
# so 92.99 displays as 93.0 and would mask a 93.0 floor breach.
set -euo pipefail

RAW=0
if [ "${1:-}" = "--raw" ]; then
  RAW=1
  shift
fi

XML="${1:?Usage: coverage-percent.sh [--raw] <cobertura.xml>}"

if [ ! -f "$XML" ]; then
  echo "::error::Coverage XML not found: $XML" >&2
  exit 1
fi

LINE_RATE=$(grep -oP -m1 'line-rate="\K[^"]+' "$XML" || true)
# Fail loudly on a missing or non-numeric line-rate: awk would coerce
# garbage to 0, letting a parse failure masquerade as a real measurement.
if ! printf '%s' "$LINE_RATE" | grep -Eq '^[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?$'; then
  echo "::error::No numeric top-level line-rate found in $XML (got: '${LINE_RATE}')" >&2
  exit 1
fi

if [ "$RAW" = 1 ]; then
  awk -v r="$LINE_RATE" 'BEGIN {printf "%.6f\n", r * 100}'
else
  awk -v r="$LINE_RATE" 'BEGIN {printf "%.1f%%\n", r * 100}'
fi
