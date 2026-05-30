+++
title = "Nightly Builds"
description = "Automated rolling nightly releases"
weight = 4
template = "docs.html"
+++

Nightly builds create date-stamped versions and maintain a rolling `nightly` release on GitHub.

## Usage

```bash
anodizer release --nightly
```

## Behavior

- Version becomes `0.1.0-nightly.20260327`
- Creates or replaces the `nightly` tag and GitHub release
- All normal pipeline stages run (build, archive, checksum, release, publish)
- Distinct from `--snapshot` — nightlies publish, snapshots don't

## Config

```yaml
nightly:
  name_template: "{{ Version }}-nightly.{{ Now | date(format='%Y%m%d') }}"
  tag_name: nightly
  publish_release: true       # default true — create a GitHub Release for each nightly run
  keep_single_release: false  # default false — set true for a rolling nightly (deletes prior release before recreating)
  draft: false                # optional — override release.draft for nightly runs only
```

| Field | Type | Default | Description |
|---|---|---|---|
| `nightly.publish_release` | `bool` | `true` | Whether to create a GitHub Release at all. Set `false` to build and publish packages without creating a release entry. |
| `nightly.keep_single_release` | `bool` | `false` | When `true`, the prior nightly release is deleted before the new one is created. Keeps only the latest nightly in the releases list. |
| `nightly.draft` | `bool` | (inherits `release.draft`) | Override the draft flag for nightly runs only. |

## Publisher skip behavior

Some publishers opt out of nightly runs automatically to avoid polluting
stable package manager indexes with date-stamped pre-release versions.

Publishers that **skip on nightly** by default:
- `homebrew`, `homebrew_casks` — formula/cask updates for nightlies break `brew upgrade` for stable users
- `scoop` — bucket manifests for nightlies shadow the stable manifest
- `aur`, `aur_source` — AUR packages are expected to be stable releases
- `krew` — kubectl plugin index is semver-gated
- `nix` — nixpkgs and tap entries track stable releases
- `cargo` — crates.io does not allow pre-release overwrites
- `chocolatey` — community gallery is moderation-gated
- `winget` — PR-based; nightly versions are rejected by automated review

Publishers that **do not skip on nightly** (they accept clobber):
- `dockerhub`, `docker_v2` — image tags like `nightly` or `edge` are conventional
- `cloudsmith`, `artifactory` — private registries; republish is explicit via `republish: true`
- `blob` — object storage; nightly assets overwrite by key
- `mcp` — registry entry is idempotent

To override skip behavior for a specific publisher, set `skips_on_nightly: false` in that publisher's config block.

## CI integration

Run nightly builds on a schedule:

```yaml
# GitHub Actions
on:
  schedule:
    - cron: "0 2 * * *"    # 2 AM UTC daily

jobs:
  nightly:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        with:
          install-rust: true
          auto-install: true
          install: cargo-zigbuild
          args: release --nightly
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```
