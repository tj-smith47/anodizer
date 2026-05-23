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
  name_template: "{{ .ProjectName }}-nightly"  # optional; release-name template
  tag_name: nightly                            # optional; the rolling tag to replace each night
```

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
