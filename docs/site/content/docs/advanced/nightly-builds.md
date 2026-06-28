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

- Version is rendered from `nightly.version_template`. The default
  (`{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly`) bumps the patch of
  the current tag and appends the short commit — e.g. tag `v0.13.0` →
  `0.13.1-a1b2c3d-nightly` — so two same-day commits yield two distinct,
  commit-immutable nightly versions.
- Creates or replaces the `nightly` tag and GitHub release
- All normal pipeline stages run (build, archive, checksum, release, publish)
- Distinct from `--snapshot` — nightlies publish, snapshots don't
- `--nightly` does **not** skip the env-preflight check. Preflight still runs
  as the first step unless you pass `--no-preflight` (or use `--snapshot` /
  `--dry-run` / `--split` / `--publish-only`, which skip it implicitly). The
  dogfood nightly below pairs `--no-preflight` with the action's
  `auto-install` so the toolchain is provisioned from `anodizer tools` rather
  than gated by a second credential check at release time.

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
| `nightly.tag_name` | `string` | `"nightly"` | Name of the rolling git tag created for nightly releases (moved on each run rather than accumulating semver tags). |
| `nightly.name_template` | `string` | `"{{ ProjectName }}-nightly"` | Template for the nightly release name. Distinct from `version_template`, which renders the version string. |
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
  workflow_dispatch: {}    # also allow on-demand nightly runs

permissions:
  contents: write          # create/replace the rolling `nightly` release + tag
  id-token: write          # keyless cosign signing via Fulcio/OIDC
  attestations: write      # mint SLSA build provenance

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
          # auto-install queries `anodizer tools` for the exact toolchain this
          # config needs and installs it — including the cross-compile tools
          # (cargo-zigbuild + zig, or `cross`) resolved from each build's
          # target and `cross:` strategy. No need to hand-list them.
          auto-install: true
          args: release --nightly
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

anodizer dogfoods this on its own repo with a `workflow_dispatch`-only
[`nightly.yml`](https://github.com/tj-smith47/anodizer/blob/v0.12.3/.github/workflows/nightly.yml)
(`from-branch: master`, schedule withheld pending a target-coverage decision),
while cfgd runs the full scheduled form above.
