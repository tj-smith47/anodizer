+++
title = "Reproducible Builds"
description = "Produce bit-for-bit reproducible release artifacts"
weight = 5
template = "docs.html"
+++

{% coming_soon() %}
Reproducible build support is planned for a future release. When enabled, anodize will set `SOURCE_DATE_EPOCH`, strip non-deterministic metadata from archives, and pass `--remap-path-prefix` to rustc.
{% end %}

## Planned config

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        reproducible: true
```

## Planned behavior

- `SOURCE_DATE_EPOCH` set from commit timestamp
- Archive file timestamps normalized to commit date
- `--remap-path-prefix` strips local paths from binaries
