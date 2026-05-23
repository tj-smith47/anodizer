+++
title = "Recovery flags"
description = "Per-publisher flags that let anodizer recover from a botched or interrupted prior release"
weight = 50
template = "docs.html"
+++

When a release fails partway through, several publishers leave durable state behind that blocks a clean re-run. Each publisher exposes a flag that lets anodizer take the appropriate recovery action — delete-and-recreate, force-push, or re-submit — instead of failing with a conflict.

These flags are all **defaults-off**. Set them when you want anodizer to overwrite prior state on conflict; leave them off when you want the conflict surfaced as an error so an operator can review.

## Per-publisher flags

| Publisher | Flag | Mechanism on conflict | Set when |
|---|---|---|---|
| GitHub Release (draft) | `release.replace_existing_draft` | DELETE the existing draft → create fresh draft | re-cutting a tag after a failed first attempt left a draft behind |
| GitHub / Gitea / GitLab release assets | `release.replace_existing_artifacts` | DELETE the conflicting asset → re-upload | partial asset upload left stale bytes |
| Chocolatey | `chocolatey.republish_in_moderation` | re-`choco push` over a version still in the community moderation queue | a prior version is stuck in moderation |
| Winget | `winget.update_existing_pr` | `git push --force-with-lease` over the stale PR branch | a prior release attempt left an open PR on `microsoft/winget-pkgs` |
| Krew | `krew.update_existing_pr` | `git push --force-with-lease` over the stale PR branch | prior release left an open PR on `kubernetes-sigs/krew-index` |
| Homebrew Cask | `homebrew.cask.update_existing_pr` | `git push --force-with-lease` over the stale PR branch | prior release left an open PR on the cask tap |
| Cloudsmith | `cloudsmith.republish` | Cloudsmith API "replace prior version" | re-cutting any version (Cloudsmith versions are otherwise immutable) |

## Behavior detail

### `release.replace_existing_draft`

When publishing a draft release and a draft with the same NAME already exists in the repo, anodizer deletes it via the GitHub API (`DELETE /repos/:owner/:repo/releases/:id`) and creates a fresh draft. The deletion is unconditional — if you only want to *reuse* the existing draft rather than replace it, use `release.use_existing_draft` instead (the two flags are mutually exclusive; setting both is a config error).

### `release.replace_existing_artifacts`

When uploading a release asset and the server returns `422 already_exists`, anodizer compares the existing asset's size to the local file. If they match (likely an outer-retry recovery), the upload is treated as a no-op. If they differ (real conflict), anodizer deletes the existing asset and re-uploads. Applies to GitHub, Gitea, and GitLab. Without this flag, the size-mismatch case bails with a real error.

### `chocolatey.republish_in_moderation`

Anodizer queries the Chocolatey OData feed for the version being published. If `<d:PackageStatus>` is `Submitted` (in moderation queue), the default is to skip with a warning. With this flag set, anodizer falls through to `choco push` anyway. Chocolatey's [moderation policy](https://github.com/chocolatey/choco-wiki/blob/master/Moderation.md) documents same-version resubmission during review ("make the required changes and resubmit the **exact** same version") — but the community-feed API may still reject the push with a 409 Conflict depending on queue state. If the push fails, the warning + dispatch summary surfaces it.

### `*.update_existing_pr` (winget, krew, homebrew cask)

When `gh pr create` reports a PR for the same head branch already exists, the default is to skip with a warning. With this flag set, anodizer runs `git push --force-with-lease origin <branch>` to overwrite the existing PR's branch — the open PR auto-picks up the new content without creating a duplicate. The `--force-with-lease` guard refuses to push if someone else has committed to the branch since you last fetched.

### `cloudsmith.republish`

Cloudsmith treats versions as immutable by default. Setting `republish: true` opts into the Cloudsmith API's explicit replace-prior-version path. This is the only flag in the recovery set that maps to a single upstream API operation rather than a delete-then-create or force-push.

## Operational guidance

- **Recommended posture for production releases:** all of these flags should be `true` unless you have a specific reason to leave a conflict in place. They are no-ops when there's no conflict, so setting them `true` carries no downside outside the conflict path.
- **Leaving them off:** the failure mode is a stuck release pipeline — anodizer skips the publisher and emits a warning, but the publisher target is unchanged. Operators have to clean up manually.
- **Rollback interaction:** these flags only affect the *publish* path. The rollback path (see [Release resilience](release-resilience.md)) operates independently and always force-cleans publisher state regardless of how these flags are set.

## See also

- [Release resilience](release-resilience.md) — classification (Submitter / Manager / Publisher) and rollback semantics
- Individual publisher docs (linked from the [publish index](../publish/_index.md)) for the rest of each publisher's config
