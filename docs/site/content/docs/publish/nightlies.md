+++
title = "Nightlies"
description = "Automated rolling nightly releases"
weight = 10
template = "docs.html"
+++

Nightly mode creates date-based versions and replaces a rolling `nightly` release on GitHub.

## Usage

```bash
anodize release --nightly
```

## Behavior

- Version becomes `0.1.0-nightly.20260327` (date-stamped)
- Creates/replaces a `nightly` tag and release on GitHub
- Distinct from `--snapshot` — nightlies are published, snapshots are not

## Config

```yaml
nightly:
  name_template: "{{ Version }}-nightly.{{ Now | date(format='%Y%m%d') }}"
  tag_name: nightly
```
