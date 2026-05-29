+++
title = "Auto-Tagging"
description = "Automatically create version tags from commit messages"
weight = 1
template = "docs.html"
+++

The `anodizer tag` command reads commit messages for bump directives, finds the latest semver tag, bumps the version, and creates a new tag.

## Usage

```bash
anodizer tag                    # create and push tag
anodizer tag --dry-run          # show what would happen
anodizer tag --custom-tag v2.0  # override with specific tag
```

## Commit message directives

Include these tokens in your commit messages to control version bumps:

| Token | Effect |
|-------|--------|
| `#major` | Major version bump (1.0.0 → 2.0.0) |
| `#minor` | Minor version bump (1.0.0 → 1.1.0) |
| `#patch` | Patch version bump (1.0.0 → 1.0.1) |
| `#none` | Skip tagging |

If no directive is found, the `default_bump` config (default: `minor`) is used.

## Config

```yaml
tag:
  default_bump: patch
  tag_prefix: "v"
  initial_version: "0.1.0"
  release_branches:
    - "main"
    - "release/.*"
  branch_history: last       # last | full
  tag_context: repo          # repo | branch
```

## Tag config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default_bump` | string | `minor` | Default bump when no directive found |
| `tag_prefix` | string | `v` | Prefix added to tags |
| `initial_version` | string | `0.1.0` | Starting version when no tags exist |
| `release_branches` | list | `["master", "main"]` | Branch patterns that trigger tags |
| `custom_tag` | string | none | Override all bump logic |
| `tag_context` | string | `repo` | Scope: `repo` or `branch` |
| `branch_history` | string | `last` | How many commits to scan: `last`, `full` |
| `prerelease` | bool | `false` | Enable prerelease mode |
| `prerelease_suffix` | string | `beta` | Prerelease suffix |
| `force_without_changes` | bool | `false` | Tag even without new commits |
| `major_string_token` | string | `#major` | Custom major bump trigger |
| `minor_string_token` | string | `#minor` | Custom minor bump trigger |
| `patch_string_token` | string | `#patch` | Custom patch bump trigger |
| `none_string_token` | string | `#none` | Custom skip trigger |
| `git_api_tagging` | string | none (disabled) | Use GitHub API (`github`) or git CLI (`git`) to create tags |

## Version source of truth

The bumped version comes from the latest git tag, not `Cargo.toml`. Given a
`patch` bump and the latest tag `v0.3.4`, the result is `v0.3.5` — regardless
of what `Cargo.toml` currently says.

`Cargo.toml` only enters the picture when `version_sync` is enabled and its
version is strictly greater than the bumped version. In that case the higher
`Cargo.toml` wins and no further bump is applied — this protects manual
pre-bumps (e.g., `version = "2.0.0"` committed in advance of a major release)
from being downgraded to `v1.1.0`.

## Workspace-aware tagging

Tag individual crates in a workspace:

```bash
anodizer tag --crate my-crate
```

Each crate has its own `tag_template` (e.g., `my-crate-v{{ Version }}`) used
for both tag discovery (finding the latest `my-crate-v*` tag) and tag
creation. This keeps workspaces independent — `my-core-v0.5.0` and
`my-cli-v1.2.0` can coexist without collision.

When `version_sync.enabled: true` is set per-crate, the tag command also
updates that crate's `Cargo.toml` version (and any intra-workspace `path +
version` dependency specs that reference it), commits the change, and tags
that commit so `cargo publish` reads the right version.

The bump commit's marker matters: the **primary** bump commit deliberately
**omits** `[skip ci]` so the tag-push trigger fires downstream release
workflows. (GitHub suppresses tag-push triggers when the tag's target
commit message contains `[skip ci]`.) Only the **side-effect** commits —
the per-crate `version_sync` propagations of an intra-workspace dependency
pin bump — carry the `[skip ci]` marker, since those don't have their own
tag and shouldn't re-trigger CI on their own.

## GitHub Actions: single-crate repo

```yaml
- uses: tj-smith47/anodizer-action@v1
  with:
    args: tag
  env:
    GITHUB_TOKEN: ${{ secrets.GH_PAT }}     # PAT, not GITHUB_TOKEN
```

Use a PAT (not `GITHUB_TOKEN`) when pushing tags, so tag-scoped workflows
like `release.yml` fire on the resulting push. `GITHUB_TOKEN`-authored pushes
never trigger downstream workflows.

## GitHub Actions: monorepo loop

For multi-crate workspaces, tag each crate independently so each gets its
own `release.yml` run:

```yaml
- uses: tj-smith47/anodizer-action@v1
  with:
    install-only: true

- name: Auto-tag all workspaces
  env:
    GITHUB_TOKEN: ${{ secrets.GH_PAT }}
  run: |
    for crate in my-core my-cli my-operator my-plugin; do
      echo "--- tagging $crate ---"
      if anodizer tag --crate "$crate"; then
        echo "::notice::$crate: tagged"
      else
        echo "::warning::$crate: skipped or failed"
      fi
    done
    # Push any version_sync commits created by the tag step so tagged commits
    # aren't orphaned from master — without this, tags point to commits that
    # only exist as tag targets and CI never runs on them.
    git push origin HEAD || true
```

See [GitHub Actions](@/docs/ci/github-actions.md) for the surrounding workflow.

## Dry run

Preview what would happen without actually tagging:

```bash
anodizer tag --dry-run                      # single-crate repo
anodizer tag --crate my-core --dry-run      # specific crate in a workspace
```

## Override the bump

```bash
anodizer tag --default-bump minor           # override config default
anodizer tag --custom-tag v2.0.0            # skip bump logic entirely
```

## Roll back a poisoned tag

When a downstream release fails on a freshly-tagged commit, the operator is
left with a tag pointing at a bumped-but-broken commit. The reverse direction
of `anodize tag` is `anodize tag rollback`:

```bash
anodizer tag rollback "$GITHUB_SHA"       # delete tag(s) at SHA + revert the bump
anodizer tag rollback --dry-run HEAD       # preview without mutation
```

See [Release resilience — Recovering a poisoned tag](./release-resilience.md#recovering-a-poisoned-tag-with-tag-rollback)
for the full flag matrix (`--scope`, `--mode`, `--branch`, `--no-push`) and
the recommended `if: failure()` workflow integration.
