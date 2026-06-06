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
  version_template: "{{ Base }}-nightly.{{ NightlyBuild }}+{{ ShortCommit }}"
  name_template: "{{ Version }}-nightly.{{ Now | date(format='%Y%m%d') }}"
  tag_name: nightly
  publish_release: true       # default true — create a GitHub Release for each nightly run
  publish_repo: nushell/nightly  # optional — publish to a DIFFERENT repo than the source
  retention:
    keep_last: 10             # keep the 10 newest nightly releases, delete the rest (+ tags)
  draft: false                # optional — override release.draft for nightly runs only
```

| Field | Type | Default | Description |
|---|---|---|---|
| `nightly.version_template` | `string` | `"{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly"` | Template for the rendered nightly version. May reference `{{ NightlyBuild }}` and `{{ Base }}` (see below). |
| `nightly.publish_release` | `bool` | `true` | Whether to create a GitHub Release at all. Set `false` to build and publish packages without creating a release entry. |
| `nightly.publish_repo` | `string` | (source repo) | Publish the nightly release to a different `"owner/repo"` than the one resolved from `release.github` (e.g. a dedicated `org/nightly` repo). The release create, asset upload, and retention deletes all target this repo. The active token must have write access to it. GitHub-only. |
| `nightly.retention.keep_last` | `int` | (none) | Keep the N newest nightly releases (matched by the rendered nightly release name) and delete the older ones, including the git tags anodizer created for them. Operates on `publish_repo` when set. |
| `nightly.keep_single_release` | `bool` | `false` | Back-compat alias for `retention: { keep_last: 1 }` (a single rolling nightly release). When both are set, `retention` wins. |
| `nightly.draft` | `bool` | (inherits `release.draft`) | Override the draft flag for nightly runs only. |

### Build-counter and base-version template vars

Two template vars support nushell-style nightly versioning
(`<base>-nightly.<build>+<sha6>`):

| Var | Source | Resets when |
|---|---|---|
| `{{ Base }}` | The numeric base semver (no prerelease / build metadata), captured before nightly templating overwrites `Version`. | Never within a tag; reflects the current tag's `MAJOR.MINOR.PATCH`. |
| `{{ NightlyBuild }}` | Stateless per-base build counter — `git rev-list --count <last-tag>..HEAD`. | A new version tag lands (the count returns to a small number automatically — no state anodizer persists). |

```yaml
# nushell-style: 0.103.0-nightly.42+a1b2c3
nightly:
  version_template: "{{ Base }}-nightly.{{ NightlyBuild }}+{{ ShortCommit }}"
  publish_repo: myorg/nightly
  retention:
    keep_last: 10
```

`nightly.publish_repo` and `nightly.retention` are configured at the top
level (`nightly:`); they apply across all crates in a workspace. The
`{{ NightlyBuild }}` counter is global (derived from the repo's git
history), so it is identical for every crate in a lockstep or per-crate
workspace release.

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
- `npm` — npm does not allow re-publishing the same version
- `chocolatey` — community gallery is moderation-gated
- `winget` — PR-based; nightly versions are rejected by automated review

Publishers that **do not skip on nightly** (they accept clobber):
- `dockerhub`, `dockers_v2` — image tags like `nightly` or `edge` are conventional
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
