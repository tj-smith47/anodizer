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
| **Lockstep, `crates:` with no `tag_template`** | `[workspace.package].version` plus a `crates:` list whose members omit `tag_template` | one shared `v*` tag — the family is [derived](#crate-level-defaults) from the lockstep signal, so `anodizer tag` cuts `v1.2.3`, not one `<name>-v1.2.3` per crate | one flat section |
| **Flat-aggregate** | flat `crates:` list, every `tag_template` resolves to the **same** prefix, per-crate `[package].version` | one shared `v*` tag | one flat section |
| **Multi-track** | flat `crates:` list (or `workspaces:`) with **distinct** tag prefixes (`core-v`, `cli-v`) | per-crate tags | `### <crate>` subsection per track |

Lockstep and flat-aggregate ship the whole workspace under one version; the
multi-track shape lets each crate release on its own cadence. See
[Changelog → Workspace shapes](@/docs/more/changelog.md#workspace-shapes-at-a-glance)
for the changelog side, including the flat-aggregate
[coherence rule](@/docs/more/changelog.md#coherence-members-must-agree-on-package-version)
(all members of a shared-prefix list must agree on `[package].version`).

## Crate-level defaults

`CrateConfig.tag_template` is optional. Anodizer resolves each crate's
effective tag family — the template `anodizer tag` mints under and every
other command (`release`, `bump`, `changelog`, `docker promote`,
`tag rollback`, `resolve-tag`) scans under — with this precedence, first
rung that applies wins:

```
1. crate's own tag_template
2. defaults.crates.tag_template
3. derived from tag.tag_prefix            →  "<tag_prefix>{{ Version }}"
4. derived from a Cargo lockstep workspace →  "v{{ Version }}"
5. built-in fallback                       →  "<name>-v{{ Version }}"
```

Rung 1 — the crate says so:

```yaml
crates:
  - { name: core, path: crates/core, tag_template: "core-v{{ Version }}" }   # → core-v1.2.3
```

Rung 2 — one default for the whole list:

```yaml
defaults:
  crates:
    tag_template: "v{{ Version }}"

crates:
  - { name: core, path: crates/core }   # → v1.2.3
  - { name: cli, path: crates/cli }     # → v1.2.3
```

Rung 3 — a repo-level `tag.tag_prefix` names the family every crate that
omits `tag_template` releases under:

```yaml
tag:
  tag_prefix: "rel-"

crates:
  - { name: core, path: crates/core }   # → rel-1.2.3
  - { name: cli, path: crates/cli }     # → rel-1.2.3
```

Rung 4 — a Cargo lockstep workspace (root `Cargo.toml` declares
`[workspace.package].version`) releases under one `v*` tag, so its crates
need no template at all:

```toml
# Cargo.toml
[workspace.package]
version = "1.2.3"
```

```yaml
crates:
  - { name: core, path: crates/core }   # → v1.2.3
  - { name: cli, path: crates/cli }     # → v1.2.3
```

Rung 5 — nothing above applies (independent `[package].version`s, no
`tag.tag_prefix`): each crate is its own track under `<name>-v`:

```yaml
crates:
  - { name: core, path: crates/core }   # → core-v1.2.3
  - { name: cli, path: crates/cli }     # → cli-v0.4.0
```

Rungs 3 and 4 never fire for a config with a `workspaces:` list — that block
declares several release tracks, and each track's crates fall through to
their own `tag_template` or the `<name>-v` convention. Under `-v`,
`anodizer` prints `derived tag family '<template>' for crates omitting
tag_template` whenever rung 3 or 4 filled a crate.

A crate's own `tag_template` always wins when set — the lower rungs only
fill the gap for crates that omit it.

**Correctness note — this touches repo-shape detection.** Shape detection
(the table above) groups crates by the tag prefix their `tag_template`
resolves to — the [Flat-aggregate](#workspace-shapes) shape requires the
*whole* `crates:` list to share one prefix. Because rungs 3 and 4 fill the
field at config load, deleting a workspace's repeated
`tag_template: "v{{ Version }}"` lines is safe for a **lockstep** workspace
or one with an explicit `tag.tag_prefix`: the derived value keeps shape
detection seeing one group, and `anodizer tag` keeps cutting one shared tag.
The note applies only to a workspace with **independent** `[package].version`s
and **no** `tag.tag_prefix`: there every crate falls to rung 5, its own
`<name>-v` track, and shape detection sees independent singleton tracks
instead of one shared tag. To release such a workspace under one tag,
set `defaults.crates.tag_template` (rung 2) or `tag.tag_prefix` (rung 3) — and
bring the members' `[package].version`s into agreement, which the
[coherence rule](@/docs/more/changelog.md#coherence-members-must-agree-on-package-version)
enforces.

`defaults.crates:` (and this precedence) only applies when the top-level
`crates:` list is non-empty — a single-crate config with no `crates:` block
has nothing to default.

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

Grouping is **by prefix**, not all-or-nothing: a shared prefix on a SUBSET of
the list aggregates just that subset (those members bump and tag as one unit,
under one shared tag) while every crate with a unique prefix keeps its own
track. The `[package].version` coherence rule applies per prefix group.

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

`depends_on` is optional. When a crate omits it, anodizer derives it at
config-load time straight from that crate's `Cargo.toml` — reading
`[dependencies]`, `[build-dependencies]`, and every
`[target.'cfg(...)'.dependencies]` table (`[dev-dependencies]` is excluded,
since it's never resolved for a `cargo publish` upload) and keeping the
intra-workspace subset:

```yaml
crates:
  - name: my-core
    path: crates/my-core
    tag_template: "core-v{{ Version }}"
    # depends_on omitted — derived from crates/my-core/Cargo.toml

  - name: my-cli
    path: crates/my-cli
    tag_template: "v{{ Version }}"
    # depends_on omitted — derived as [my-core] because
    # crates/my-cli/Cargo.toml has `my-core = { path = "../my-core" }`
```

An explicit `depends_on:` (as in the earlier examples on this page) is
still honored and always wins — the derivation only fills crates that omit
the field entirely. `anodizer init` scaffolds the same complete
`depends_on:` set explicitly (it writes what it derives, rather than
omitting it), so a freshly generated config and a hand-trimmed one resolve
identically. This only matters once a project has more than one crate under
`crates:` — a single-crate config has no intra-workspace dependency to
order.

### Workspace-membership guard

`anodizer check config` fails when a crate in `crates:` has an active cargo
publisher (`publish.cargo` configured and not skipped) and its `Cargo.toml`
names an intra-workspace dependency — via the same `[dependencies]` /
`[build-dependencies]` / `[target.'cfg(...)'.dependencies]` scan the
`depends_on` derivation above uses — that is itself **absent from
`crates:`**. This catches the class of failure where a crate is a real
publish-order requirement on disk but missing from the config entirely,
which would otherwise only surface late, mid-`cargo publish`, as a
registry-side "no matching package named ... found":

```text
$ anodizer check config
Error: crate 'anodizer-stage-install-script' is a workspace member and an
intra-workspace dependency of published crate 'anodizer', but is absent
from `crates:` (cargo will fail publishing 'anodizer')
```

The check also fires when the dependency crate IS listed in `crates:` but
has no active cargo publisher itself (skipped, or never configured for
crates.io) — cargo still fails the dependent's publish because the
dependency is never uploaded to the registry first.

It applies across every multi-crate shape on this page (lockstep-with-a-
`crates:`-list, flat-aggregate, and multi-track/per-crate) since all of them
funnel through the same crate universe; a crate with no active cargo
publisher is never checked, since a missing workspace dependency can't break
a publish that never runs. See [Release Resilience → workspace-membership
guard](@/docs/advanced/release-resilience.md#static-check-workspace-membership-guard)
for how this fits alongside `check config`'s other static lints.

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

### Mixed configs: top-level `crates:` alongside `workspaces:`

Top-level crates that live next to `workspaces:` groups are their own release
tracks. A bare `anodizer tag` treats them like the flat-`crates:` shapes:
those sharing one `tag_template` prefix bump as **one aggregate group under
one shared tag** (and must agree on `[package].version` — a divergence errors
before tagging); those with distinct prefixes stay independent singleton
tracks. See
[Mixed configs](@/docs/advanced/auto-tagging.md#mixed-configs-crates-alongside-workspaces)
for the grouping rules.

**Upgrading an existing repo to a mixed config:** every track without a tag
matching its `tag_template` counts as changed, so the first release-worthy
push after the upgrade cuts a first-ever tag per new track — one release
workflow can fan out per track. Pre-create tags at the current versions for
tracks that should not release immediately.

## Resolving a tag to a crate

Tag-triggered release workflows need to know which crate a given tag belongs to. `anodizer resolve-tag` does that lookup:

```bash
$ anodizer resolve-tag core-v0.3.5 --json
{"crate":"my-core","path":"crates/my-core","has-builds":false}
```

In GitHub Actions, use the action's `resolve-workspace: true` input to populate outputs from the triggering tag (see [GitHub Actions](@/docs/ci/github-actions.md#tag-triggered-monorepo-release)).
