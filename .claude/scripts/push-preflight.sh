#!/usr/bin/env bash
# Guard: catch upstream divergence BEFORE the long pre-push gate runs.
#
# The release workflow's auto-tag job pushes `chore(release): bump …`
# commits to master. A local `task push` started before that commit is
# fetched runs the full (~13 min) gate and then fails at `git push`
# (non-fast-forward) — or worse, tempts a hurried --force. This preflight
# fetches first and resolves divergence up front:
#
#   * up to date / ahead only        -> proceed
#   * behind ONLY by release-machinery commits (subjects starting with
#     "chore(release): ", i.e. version-sync bumps and rollback reverts
#     written exclusively by anodizer's own tag machinery — see
#     RELEASE_COMMIT_PREFIX in crates/core/src/git/commits.rs)
#                                    -> rebase them in automatically
#   * behind by anything else        -> fail fast with the divergence
#                                       listed, before the gate spends
#                                       any time
set -euo pipefail

remote="origin"
branch="$(git rev-parse --abbrev-ref HEAD)"
upstream="${remote}/${branch}"

echo "push-preflight: fetching ${remote}..."
git fetch --quiet "${remote}"

if ! git rev-parse --verify --quiet "${upstream}" >/dev/null; then
  echo "push-preflight: ${upstream} does not exist yet — nothing to diverge from."
  exit 0
fi

behind="$(git rev-list --count "HEAD..${upstream}")"
ahead="$(git rev-list --count "${upstream}..HEAD")"

if [ "${behind}" -eq 0 ]; then
  echo "push-preflight: up to date with ${upstream} (ahead by ${ahead})."
  exit 0
fi

# Subjects of every commit the upstream has that HEAD lacks.
missing_subjects="$(git log --format='%s' "HEAD..${upstream}")"

# An exit above 1 means the filter never ran, and an unread subject list must
# not read as "nothing but release commits" and license an auto-rebase.
filter_status=0
non_release="$(printf '%s\n' "${missing_subjects}" \
  | grep -v '^chore(release): ')" || filter_status=$?
if [ "${filter_status}" -gt 1 ]; then
  echo "push-preflight: subject filter exited ${filter_status}; the check did not run." >&2
  exit 2
fi

if [ -n "${non_release}" ]; then
  echo "push-preflight: ${branch} is behind ${upstream} by ${behind} commit(s)," >&2
  echo "and not all of them are anodizer release-machinery commits:" >&2
  git log --format='  %h %s' "HEAD..${upstream}" >&2
  echo >&2
  echo "Refusing to auto-rebase real upstream work. Resolve manually" >&2
  echo "(git pull --rebase ${remote} ${branch}) and re-run task push." >&2
  exit 1
fi

echo "push-preflight: behind ${upstream} by ${behind} release-machinery commit(s):"
git log --format='  %h %s' "HEAD..${upstream}"
echo "push-preflight: rebasing onto ${upstream}..."
if ! git rebase "${upstream}"; then
  git rebase --abort || true
  echo "push-preflight: rebase onto ${upstream} conflicted — aborted." >&2
  echo "Resolve manually (git pull --rebase ${remote} ${branch}) and re-run task push." >&2
  exit 1
fi
echo "push-preflight: rebased cleanly onto ${upstream}."
