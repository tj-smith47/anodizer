+++
title = "Nightly Builds"
description = "Automated rolling nightly releases"
weight = 4
template = "docs.html"
+++

Nightly builds create date-stamped versions and maintain a rolling `nightly` release on GitHub.

## Usage

```bash
anodize release --nightly
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
```

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
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodize-action@v1
        with:
          install-rust: true
          auto-install: true
          install: cargo-zigbuild
          args: release --nightly
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```
