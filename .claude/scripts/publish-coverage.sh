#!/usr/bin/env bash
# Publish the coverage percentage to the orphan 'badges' branch as a Shields.io
# endpoint payload. Switches branches; intended for CI / release use only.
# Local runs that just need the number should use `task coverage:check`.
# Usage: publish-coverage.sh <cobertura.xml>
set -euo pipefail

XML="${1:?Usage: publish-coverage.sh <cobertura.xml>}"

# Refuse to run outside CI — this script switches branches and mutates local
# git config, both of which surprise local users. Local dev should use
# `task coverage:check` instead.
if [ -z "${CI:-}${GITHUB_ACTIONS:-}" ]; then
  echo "error: publish-coverage.sh is CI-only (CI or GITHUB_ACTIONS env var required)." >&2
  echo "       For local coverage, run \`task coverage:check\` instead." >&2
  exit 2
fi

if [ ! -f "$XML" ]; then
  echo "::error::Coverage XML not found: $XML"
  exit 1
fi

# Reuse the same percentage extraction `task coverage:check` prints, so the
# README badge and local stdout never drift.
COVERAGE=$(bash "$(dirname "$0")/coverage-percent.sh" "$XML")
COVERAGE="${COVERAGE%\%}"

if (( $(echo "$COVERAGE >= 90" | bc -l) )); then COLOR="brightgreen"
elif (( $(echo "$COVERAGE >= 80" | bc -l) )); then COLOR="green"
elif (( $(echo "$COVERAGE >= 70" | bc -l) )); then COLOR="yellowgreen"
elif (( $(echo "$COVERAGE >= 60" | bc -l) )); then COLOR="yellow"
else COLOR="red"; fi

# Restore the original checkout before exit. The coverage job resolves a LOCAL
# composite action (`./.github/actions/setup-rust`), whose files must exist in
# the working tree at the job's post phase (cache-save, composite cleanup) — not
# just the main phase. Leaving the tree on the `badges` branch (which lacks
# `.github/`) makes the post phase fail with "Can't find action.yml". --force
# discards the badge-branch tree state; the badge is already committed+pushed.
ORIG_REF="$(git symbolic-ref --quiet --short HEAD || git rev-parse HEAD)"
restore_ref() { git checkout --force "$ORIG_REF" > /dev/null 2>&1 || true; }
trap restore_ref EXIT

git config user.email "github-actions[bot]@users.noreply.github.com"
git config user.name "github-actions[bot]"
git fetch origin badges:badges 2>/dev/null || true
if git show-ref --verify --quiet refs/heads/badges; then
  git checkout badges
else
  git checkout --orphan badges
  git rm -rf . > /dev/null 2>&1 || true
fi

BADGE="{\"schemaVersion\":1,\"label\":\"coverage\",\"message\":\"${COVERAGE}%\",\"color\":\"${COLOR}\"}"
echo "$BADGE" > coverage.json

git add coverage.json
git diff --cached --quiet || git commit -m "Update coverage to ${COVERAGE}%"
git push origin badges --force
