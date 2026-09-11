+++
title = "Nightlies"
description = "Automated rolling nightly releases"
weight = 10
template = "docs.html"
+++

Nightly mode creates a commit-immutable prerelease version off the newest release tag and publishes it like any other release.

## Classification

Not applicable — this is a workflow page, not a publisher. Nightly mode is a release-time flag that changes the version string and tag policy; the publishers that fire are the same ones configured for normal releases.

## Minimal config

```bash
anodizer release --nightly
```

No YAML changes required for the default behavior.

## Full config reference

```yaml
nightly:
  version_template: "{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly"  # optional
  name_template: "{{ ProjectName }} nightly"  # optional; names the nightly release
  tag_name: nightly           # optional; pin ONE rolling tag instead of a per-commit tag
  publish_release: true       # default true — create a GitHub Release for each nightly run
  keep_single_release: false  # default false — set true to keep only the newest nightly release
  skip_if_no_changes: false   # default false — set true to no-op on a quiet day
  draft: false                # optional — override release.draft for nightly runs only
```

| Field | Type | Default | Description |
|---|---|---|---|
| `nightly.version_template` | `string` | `"{{ incpatch(v=Version) }}-{{ ShortCommit }}-nightly"` | The rendered `Version` for the run. |
| `nightly.name_template` | `string` | (inherits `release.name_template`) | Names the nightly release, replacing `release.name_template` on nightly runs only. |
| `nightly.tag_name` | `string` | (unset — the crate's `tag_template` creates the tag) | Pins one rolling tag, moved each run. In a workspace creating more than one tag family the value is prefixed with the publishing crate's family (`operator-v{{ Version }}` + `edge` → `operator-vedge`). |
| `nightly.publish_release` | `bool` | `true` | Whether to create a GitHub Release at all. |
| `nightly.keep_single_release` | `bool` | `false` | Keep only the newest nightly release, deleting the older ones. In a workspace that creates more than one tag family the sweep is narrowed to the publishing crate's family, so the tracks do not delete each other. |
| `nightly.skip_if_no_changes` | `bool` | `false` | Skip the release when the run's changelog resolved to zero notable entries. Decided per crate in a per-crate workspace. |
| `nightly.draft` | `bool` | (inherits `release.draft`) | Override the draft flag for nightly runs only. |

## Publisher skip behavior

Most package-manager publishers skip on nightly runs to avoid polluting stable
indexes. The following skip automatically: `homebrew`, `homebrew_casks`, `homebrew-core`,
`scoop`, `aur`, `aur_source`, `krew`, `nix`, `cargo`, `npm`, `pypi`, `chocolatey`, `winget`.

Docker and private registry publishers (`dockerhub`, `dockers_v2`, `cloudsmith`,
`artifactory`, `blob`, `mcp`) do not skip — they accept clobber by design.

To override, set `skips_on_nightly: false` in the publisher block.

## Authentication

Not applicable as a separate config — nightly publishes use the same release credentials (`GITHUB_TOKEN`) and per-publisher tokens as a normal release.

## Common gotchas

- Distinct from `--snapshot` — nightlies are published, snapshots are not.
- Without `tag_name` every run cuts its own tag, so `keep_single_release: true` is what stops the pile accumulating.
- With `tag_name` the pinned tag is moved every run and the assets the previous run uploaded are replaced. Pinning the tag turns on that replacement by itself; `release.replace_existing_artifacts: true` is not needed for it.
- A nightly cuts even when nothing changed since the last release; `skip_if_no_changes: true` turns that into a no-op.
- `skip_if_no_changes` needs a changelog signal to act on: with `--skip=changelog` or `changelog.use: github-native` there is no entry count, so the nightly cuts as usual. A repo with no prior tag is never skipped either — its whole history is unreleased.
- With a multitrack workspace, `tag_name` is prefixed with each crate's own family (`operator-v` + `edge` -> `operator-vedge`) so one literal tag cannot carry three tracks.

## Behavior

- Version becomes `0.1.1-3aece9d-nightly` — the newest release tag's patch, bumped, plus the seven-character short commit
- In a workspace creating several tag families the base is the newest tag across ALL of them, so one lagging track cannot stamp its siblings with a stale version
- Creates a release on the tag the crate's `tag_template` creates, or on `nightly.tag_name` when set
- Distinct from `--snapshot` — nightlies are published, snapshots are not
