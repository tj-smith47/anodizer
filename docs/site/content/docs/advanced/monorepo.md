+++
title = "Monorepo Support"
description = "Release multiple crates from a single repository"
weight = 2
template = "docs.html"
+++

Anodizer supports Cargo workspaces with independent release cadences per crate. There are two layers: **crates** (always present) and **workspaces** (for larger monorepos that need per-workspace changelogs, skip lists, and release configs).

## Workspace shapes

anodizer classifies a repo into one of four shapes from its config + Cargo
metadata, and tagging / changelog behavior follows from the shape. You only set
the config signal:

| Shape | Config signal | Tag behavior | Changelog shape |
|-------|---------------|--------------|-----------------|
| **Single** | one crate, or no config | one `v*` tag | one flat section |
| **Lockstep** | `[workspace.package].version` in root `Cargo.toml` | one shared `v*` tag | one flat section |
| **Flat-aggregate** | flat `crates:` list, every `tag_template` resolves to the **same** prefix, per-crate `[package].version` | one shared `v*` tag | one flat section |
| **Multi-track** | flat `crates:` list (or `workspaces:`) with **distinct** tag prefixes (`core-v`, `cli-v`) | per-crate tags | `### <crate>` subsection per track |

Lockstep and flat-aggregate ship the whole workspace under one version; the
multi-track shape lets each crate release on its own cadence. See
[Changelog → Workspace shapes](@/docs/more/changelog.md#workspace-shapes-at-a-glance)
for the changelog side, including the flat-aggregate
[coherence rule](@/docs/more/changelog.md#coherence-members-must-agree-on-package-version)
(all members of a shared-prefix list must agree on `[package].version`).

## Flat crates config

For a single Cargo workspace where the crates release on **distinct** tag
tracks (multi-track), give each a distinct `tag_template` prefix in the
top-level `crates:` list:

```yaml
crates:
  - name: core-lib
    path: crates/core
    tag_template: "core-v{{ Version }}"   # distinct prefix → its own track
    depends_on: []

  - name: cli-tool
    path: crates/cli
    tag_template: "cli-v{{ Version }}"     # distinct prefix → its own track
    depends_on: [core-lib]
```

To release the crates together under **one** shared `v*` tag instead, give every
member the **same** prefix (`tag_template: "v{{ Version }}"`) — the
flat-aggregate shape above. All members must then agree on `[package].version`.

## Workspaces config (multi-root monorepo)

For repos with components that release independently (different cadences, different registries, different announce/signing settings), use the top-level `workspaces:` list. Each workspace has its own `crates:`, `changelog:`, `release:`, and optional `skip:` list:

```yaml
version: 2
project_name: mono

workspaces:
  - name: core
    skip:
      - announce                       # lib-only workspace doesn't announce
    crates:
      - name: my-core
        path: crates/my-core
        tag_template: "core-v{{ Version }}"
        version_sync:
          enabled: true
          mode: cargo
        publish:
          crates:
            enabled: true

  - name: cli
    crates:
      - name: my-cli
        path: crates/my-cli
        tag_template: "v{{ Version }}"
        depends_on: [my-core]
        version_sync:
          enabled: true
          mode: cargo
        # ... builds, archives, release, publish, docker, nfpm ...
```

Each workspace's crates produce their own tags (`core-v0.3.5`, `v0.3.5`) and their own release workflows, so a push to `master` can fan out into one release workflow per workspace.

## Key features

### Per-crate tags

Each crate uses its own `tag_template` for both tag discovery and tag creation. Tags never collide across crates:

```bash
# Release just the core library (uses core-v* tags)
anodizer release --crate my-core

# Release just the CLI (uses v* tags)
anodizer release --crate my-cli
```

### Per-crate vs root changelogs

Each crate can keep its own `crates/<name>/CHANGELOG.md`, contribute to a shared
root `CHANGELOG.md`, or both. A multi-track root keeps a `### <crate>`
subsection per track and promotes only the tagged track's subsection on release.
Commit scoping is derived: a per-crate changelog covers its own crate directory
automatically, and the root aggregate spans all crate directories plus the
workspace manifests — you do not set `paths:`. See
[Changelog destination](@/docs/more/changelog.md#changelog-destination) and
[Commit scoping](@/docs/more/changelog.md#commit-scoping).

### Dependency ordering

Use `depends_on` to ensure crates are released in the right order. Anodizer performs topological sorting — if `my-cli` depends on `my-core`, `my-core` is always released first.

### version_sync

When `version_sync.enabled: true` is set per-crate, the tag command also updates that crate's `Cargo.toml` version, any intra-workspace `path + version` dependency specs that reference it, and `Cargo.lock`. The update is committed with `[skip ci]` and the tag points at that commit.

### Release all changed crates

```bash
anodizer release --all
```

This detects which crates have unreleased changes (commits since their last tag) and releases them in dependency order.

### Per-crate publishers without `--crate` / `--all`

When `release` (or `release --publish-only`) runs without `--crate` or `--all`, per-crate publishers (homebrew, scoop, nix, aur, krew, chocolatey, winget, cargo) iterate over **every crate whose publisher block is configured**. This matches the implicit-all behavior the determinism harness's `release --publish-only` step relies on: a single `release --publish-only` invocation publishes every configured crate to every configured publisher target, without an explicit selection flag. Pass `--crate <name>` (repeatable) to scope a run to a subset.

## Auto-tagging a monorepo

Loop `anodizer tag --crate <name>` for each crate so each workspace gets its own release:

```yaml
- uses: tj-smith47/anodizer-action@v1
  with:
    install-only: true

- name: Auto-tag all workspaces
  env:
    GITHUB_TOKEN: ${{ secrets.GH_PAT }}
  run: |
    for crate in my-core my-cli my-operator; do
      anodizer tag --crate "$crate" || true
    done
    git push origin HEAD || true
```

See [Auto-Tagging](@/docs/advanced/auto-tagging.md) and [GitHub Actions](@/docs/ci/github-actions.md) for full examples.

## Resolving a tag to a crate

Tag-triggered release workflows need to know which crate a given tag belongs to. `anodizer resolve-tag` does that lookup:

```bash
$ anodizer resolve-tag core-v0.3.5 --json
{"crate":"my-core","path":"crates/my-core","has-builds":false}
```

In GitHub Actions, use the action's `resolve-workspace: true` input to populate outputs from the triggering tag (see [GitHub Actions](@/docs/ci/github-actions.md#tag-triggered-monorepo-release)).
