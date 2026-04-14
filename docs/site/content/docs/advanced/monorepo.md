+++
title = "Monorepo Support"
description = "Release multiple crates from a single repository"
weight = 2
template = "docs.html"
+++

Anodize supports Cargo workspaces with independent release cadences per crate. There are two layers: **crates** (always present) and **workspaces** (for larger monorepos that need per-workspace changelogs, skip lists, and release configs).

## Flat crates config

For a single Cargo workspace where every crate shares the same changelog, release config, and publish settings, use the top-level `crates:` list:

```yaml
crates:
  - name: core-lib
    path: crates/core
    tag_template: "core-v{{ Version }}"
    depends_on: []

  - name: cli-tool
    path: crates/cli
    tag_template: "cli-v{{ Version }}"
    depends_on: [core-lib]
```

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
anodize release --crate my-core

# Release just the CLI (uses v* tags)
anodize release --crate my-cli
```

### Dependency ordering

Use `depends_on` to ensure crates are released in the right order. Anodize performs topological sorting — if `my-cli` depends on `my-core`, `my-core` is always released first.

### version_sync

When `version_sync.enabled: true` is set per-crate, the tag command also updates that crate's `Cargo.toml` version, any intra-workspace `path + version` dependency specs that reference it, and `Cargo.lock`. The update is committed with `[skip ci]` and the tag points at that commit.

### Release all changed crates

```bash
anodize release --all
```

This detects which crates have unreleased changes (commits since their last tag) and releases them in dependency order.

## Auto-tagging a monorepo

Loop `anodize tag --crate <name>` for each crate so each workspace gets its own release:

```yaml
- uses: tj-smith47/anodize-action@v1
  with:
    install-only: true

- name: Auto-tag all workspaces
  env:
    GITHUB_TOKEN: ${{ secrets.GH_PAT }}
  run: |
    for crate in my-core my-cli my-operator; do
      anodize tag --crate "$crate" || true
    done
    git push origin HEAD || true
```

See [Auto-Tagging](@/docs/advanced/auto-tagging.md) and [GitHub Actions](@/docs/ci/github-actions.md) for full examples.

## Resolving a tag to a crate

Tag-triggered release workflows need to know which crate a given tag belongs to. `anodize resolve-tag` does that lookup:

```bash
$ anodize resolve-tag core-v0.3.5 --json
{"crate":"my-core","path":"crates/my-core","has-builds":false}
```

In GitHub Actions, use the action's `resolve-workspace: true` input to populate outputs from the triggering tag (see [GitHub Actions](@/docs/ci/github-actions.md#tag-triggered-monorepo-release)).
