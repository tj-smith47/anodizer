+++
title = "Auto-Tagging"
description = "Automatically create version tags from commit messages"
weight = 1
template = "docs.html"
+++

The `anodizer tag` command reads commit messages for bump directives, finds the latest semver tag, bumps the version, and creates a new tag.

## Usage

```bash
anodizer tag                    # create and push the tag (bump commit stays local)
anodizer tag --push             # also push the version-sync bump commit, atomically
anodizer tag --dry-run          # show what would happen
anodizer tag --custom-tag v2.0  # override with specific tag
```

## Pushing the bump commit (`--push`)

By default `anodizer tag` pushes only the **tag** and leaves the
version-sync `chore(release): bump …` commit on the local branch — so you can
inspect the bump before publishing the branch. Pass `--push` to push the bump
commit to the release branch **atomically with the tag** (`git push --atomic`),
so neither an orphan tag nor an orphan commit can ever exist on the remote.

| Flag | Effect |
|------|--------|
| `--push` | Push the bump commit (branch HEAD) atomically with the tag |
| `--no-push` | Push the tag only; leave the bump commit local (the per-crate path's opt-out, since it pushes branch+tags by default) |
| `--push-remote <name>` | Push to `<name>` instead of `origin` |
| `--push-dry-run` | Create the tag + bump commit locally, but only **print** the `git push` commands `--push` would run instead of executing them |
| `--no-changelog` | Skip refreshing `CHANGELOG.md` on this tag (overrides the `changelog:` config) |

`tag.push: true` in config is the persistent equivalent of `--push`; the CLI
flags override it per invocation.

### Enrolled `version_files` ride the bump commit

The same bump commit also rewrites any files enrolled under `version_files` —
a Helm `Chart.yaml`, an install doc, a README badge — from the old release
version to the new one, so files that embed the version outside `Cargo.toml`
are tagged together and never drift from the tag. See
[Version Files](@/docs/general/version-files.md) for enrollment and the
`anodizer check version-files` CI guard.

### The bump commit refreshes `CHANGELOG.md`

When a `changelog:` block is configured, the same bump commit also prepends a
new `## [version] - date` section to your `CHANGELOG.md` — rendered by
anodizer's native [changelog engine](@/docs/more/changelog.md) (the same one
`anodizer bump --commit` uses: conventional commits since the last tag, grouped
and filtered per your `changelog:` config).
The refreshed `CHANGELOG.md` rides the same `chore(release): bump …` commit as
the `Cargo.toml` / `Cargo.lock` bump and any enrolled `version_files`, so the
changelog is tagged atomically with the version and never drifts.

A minimal `changelog:` block is all it takes — no extra flag enables this:

```yaml
changelog:
  sort: asc
  groups:
    - title: Features
      regexp: "^feat"
      order: 0
    - title: Bug Fixes
      regexp: "^fix"
      order: 1
  filters:
    exclude:
      - "^chore"
      - "^docs"
```

Given the latest tag `v0.1.0`, a `minor` bump, and an existing `CHANGELOG.md`
with a `# Changelog` H1 over prior `## [x.y.z]` sections, `anodizer tag`
prepends the new section in the bump commit and leaves the prior ones intact:

```text
$ anodizer tag
...
bundled changelog section for myapp → 0.2.0
new_tag=v0.2.0
old_tag=v0.1.0
```

```markdown
# Changelog

## [0.2.0] - 2026-06-03

### Features

- a1b2c3d feat: add config validation

### Bug Fixes

- e4f5a6b fix: handle empty target list

## [0.1.0] - 2026-05-12
...
```

Pass `--no-changelog` to skip the refresh for a single tag — a hotfix tag you
don't want to touch the changelog, for example. The tag and the
`Cargo.toml` / `version_files` bump still happen; only `CHANGELOG.md` is left
untouched:

```text
$ anodizer tag --no-changelog
...
new_tag=v0.2.1   # CHANGELOG.md unchanged
old_tag=v0.2.0
```

#### When the refresh runs

The refresh is **on by default** whenever a `changelog:` block is present and
not skipped. `tag` and `bump --commit` share one gate, so the same config
governs both:

| Setting | Effect on the bump commit's `CHANGELOG.md` refresh |
|---------|----------------------------------------------------|
| `changelog:` present, no `skip:` | **Refreshes** (default) |
| `changelog: { skip: true }` | Disabled — honored by both `tag` and `bump --commit` |
| `anodizer tag --no-changelog` | Disabled for that invocation only (overrides config) |
| no `changelog:` block at all | Nothing to render; refresh is a no-op |

#### Config modes

The refresh follows the same per-mode file placement as the bump itself:

- **Single-crate** — one root `CHANGELOG.md` at the repo root.
- **Workspace lockstep** — each member crate's own `CHANGELOG.md` gets the
  shared new version's section.
- **Workspace per-crate** — only the crates this tag actually bumps get their
  `CHANGELOG.md` refreshed, each against its own version and commit range.

`--push-dry-run` vs `--dry-run`: `--dry-run` previews the whole run, touching
nothing (no bump commit, no tag, no push). `--push-dry-run` is narrower — it
still creates the tag and the version-sync bump commit **locally**, then prints
the `git push …` commands the push step would run rather than executing them.
Use it to confirm exactly which refs `--push` would publish (and to which
remote) before you commit to the push; combine with `--dry-run` to preview the
tagging too.

A non-fast-forward rejection is the most likely `--push` failure (someone
pushed to the release branch after your checkout). Because the push is atomic,
neither the branch nor the tag lands when it's rejected, and the error names
the stale ref and tells you to pull/rebase and re-run (or drop `--push` to
publish the tag only).

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
| `push` | bool | `false` | Also push the version-sync bump commit atomically with the tag (CLI `--push` / `--no-push` override) |
| `skip_ci_on_bump` | bool | `false` | Append `[skip ci]` to the bump commit subject. Only safe with a `workflow_run`-triggered release (see below) |

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

**Push behavior differs by mode.** The per-crate auto-dispatch path (a
multi-crate config with no `--crate`) pushes the single bump commit **and**
every per-crate tag atomically by default — `--no-push` opts out of pushing
the branch (tags still go up). The `--crate <name>` path follows the
single-crate/lockstep default: it pushes the tag only and leaves the bump
commit local unless you pass `--push` (or set `tag.push: true`), at which point
the bump commit and tag push atomically. Use `--push-remote <name>` to target a
remote other than `origin`.

### `[skip ci]` on the bump commit (`skip_ci_on_bump`)

By default the version-sync bump commit's subject does **not** carry
`[skip ci]`. The bump commit becomes the tag's target, and GitHub suppresses
**both** the master-push CI re-run **and** any `on: push: tags:` release
trigger when the tag target's message contains `[skip ci]`. Marking it would
silently skip a tag-push-triggered release.

The trade-off depends on how your release workflow is triggered:

| Release trigger | `skip_ci_on_bump` | Why |
|---|---|---|
| `on: push: tags:` (GoReleaser-style) | **off** (default) | `[skip ci]` would suppress the tag-push trigger and the release never fires |
| `on: workflow_run:` (decoupled) | may be **on** | The release fires off the completed CI run, not the tag push, so `[skip ci]` only skips the redundant master-push CI re-run (which is already crate-gated and harmless) |

```yaml
tag:
  skip_ci_on_bump: true   # only with a workflow_run-triggered release
```

If left off, the bump commit's master push triggers a normal CI re-run; that
run's auto-tag job no-ops because no new release-worthy commits exist since the
freshly created tag (the conventional-commit gate in bump detection). See
[Release workflow patterns](@/docs/ci/release-workflows.md) for the two
trigger styles.

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
      # --push lands each crate's version_sync bump commit atomically with its
      # tag, so tagged commits are never orphaned from master and the manual
      # `git push origin HEAD` below is unnecessary.
      if anodizer tag --crate "$crate" --push; then
        echo "::notice::$crate: tagged"
      else
        echo "::warning::$crate: skipped or failed"
      fi
    done
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
