#!/usr/bin/env bash
# Retry the anodize Release workflow when crates.io rate-limits new crate
# publication. Designed to be safe to run on a schedule (e.g. cron).
#
# What it does:
#   1. If all 27 anodize crates are already on crates.io, exit (no-op).
#   2. Otherwise, delete the existing GitHub release for the tag (NOT the tag).
#   3. Re-run the failed jobs of the most recent Release workflow run for the
#      tag, picking up from where it left off. The publish stage skips crates
#      already on crates.io, so each retry advances by one new crate (the
#      crates.io new-crate rate limit is ~1/10 min after the burst).
#
# Required env / config:
#   - `gh` CLI installed and authenticated against tj-smith47/anodize.
#   - REPO and TAG can be overridden via env; defaults below.
#
# Exit codes:
#   0 = no-op (all crates published) OR retry triggered successfully
#   1 = unexpected error (gh failure, no run found, etc.)
set -euo pipefail

REPO="${REPO:-tj-smith47/anodize}"
TAG="${TAG:-v0.1.0}"

# All 27 anodize-* crates that need to be published.  Update this list if the
# workspace adds or removes published crates.
CRATES=(
  anodize
  anodize-core
  anodize-stage-announce
  anodize-stage-appbundle
  anodize-stage-archive
  anodize-stage-blob
  anodize-stage-build
  anodize-stage-changelog
  anodize-stage-checksum
  anodize-stage-dmg
  anodize-stage-docker
  anodize-stage-flatpak
  anodize-stage-makeself
  anodize-stage-msi
  anodize-stage-nfpm
  anodize-stage-notarize
  anodize-stage-nsis
  anodize-stage-pkg
  anodize-stage-publish
  anodize-stage-release
  anodize-stage-sbom
  anodize-stage-sign
  anodize-stage-snapcraft
  anodize-stage-source
  anodize-stage-srpm
  anodize-stage-templatefiles
  anodize-stage-upx
)

UA="anodize-retry-release (${REPO})"

log() { printf '[retry-release] %s\n' "$*" >&2; }

# 1. Check whether all crates are already published.
all_published=true
missing=()
for c in "${CRATES[@]}"; do
  status=$(curl -s -o /dev/null -w '%{http_code}' -A "$UA" \
    "https://crates.io/api/v1/crates/${c}")
  if [ "$status" != "200" ]; then
    all_published=false
    missing+=("$c")
  fi
done

if [ "$all_published" = "true" ]; then
  log "all ${#CRATES[@]} crates already published — nothing to do"
  exit 0
fi

log "${#missing[@]} crate(s) still missing: ${missing[*]}"

# 2. Delete the GitHub release for the tag (keep the tag intact).
if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
  log "deleting existing release $TAG (keeping tag)"
  gh release delete "$TAG" --repo "$REPO" --yes
else
  log "no existing release for $TAG"
fi

# 3. Find the latest Release workflow run for this tag and re-run failed jobs.
RUN_ID=$(gh run list \
  --repo "$REPO" \
  --workflow=Release \
  --limit=20 \
  --json databaseId,headBranch,conclusion \
  --jq "[.[] | select(.headBranch==\"${TAG}\")][0].databaseId")

if [ -z "$RUN_ID" ] || [ "$RUN_ID" = "null" ]; then
  log "no Release workflow run found for tag $TAG"
  exit 1
fi

log "re-running failed jobs of Release run $RUN_ID"
gh run rerun "$RUN_ID" --repo "$REPO" --failed
