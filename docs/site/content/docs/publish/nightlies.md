+++
title = "Nightlies"
description = "Automated rolling nightly releases"
weight = 10
template = "docs.html"
+++

Nightly mode creates date-based versions and replaces a rolling `nightly` release on GitHub.

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
  name_template: "{{ ProjectName }}-nightly"  # optional; release-name template
  tag_name: nightly                            # optional; the rolling tag to replace each night
  publish_release: true       # default true — create a GitHub Release for each nightly run
  keep_single_release: false  # default false — set true to delete prior release before recreating
  draft: false                # optional — override release.draft for nightly runs only
```

| Field | Type | Default | Description |
|---|---|---|---|
| `nightly.publish_release` | `bool` | `true` | Whether to create a GitHub Release at all. |
| `nightly.keep_single_release` | `bool` | `false` | Delete the prior nightly release before creating a new one, keeping only the latest. |
| `nightly.draft` | `bool` | (inherits `release.draft`) | Override the draft flag for nightly runs only. |

## Publisher skip behavior

Most package-manager publishers skip on nightly runs to avoid polluting stable
indexes. The following skip automatically: `homebrew`, `homebrew_casks`, `scoop`,
`aur`, `aur_source`, `krew`, `nix`, `cargo`, `npm`, `chocolatey`, `winget`.

Docker and private registry publishers (`dockerhub`, `dockers_v2`, `cloudsmith`,
`artifactory`, `blob`, `mcp`) do not skip — they accept clobber by design.

To override, set `skips_on_nightly: false` in the publisher block.

## Authentication

Not applicable as a separate config — nightly publishes use the same release credentials (`GITHUB_TOKEN`) and per-publisher tokens as a normal release.

## Common gotchas

- Distinct from `--snapshot` — nightlies are published, snapshots are not.
- The `nightly` tag is force-pushed every run; existing release assets are replaced (set `release.replace_existing_artifacts: true` to clear before re-upload).
- Date format defaults to `YYYYMMDD`; override via `name_template` if you need higher resolution.

## Behavior

- Version becomes `0.1.0-nightly.20260327` (date-stamped)
- Creates/replaces a `nightly` tag and release on GitHub
- Distinct from `--snapshot` — nightlies are published, snapshots are not
