+++
title = "Config Includes"
description = "Split configuration across multiple files"
weight = 3
template = "docs.html"
+++

{% coming_soon() %}
Config includes are planned for a future release. This feature will allow splitting your `.anodize.yaml` across multiple files for better organization and reuse.
{% end %}

## Planned config

```yaml
includes:
  - configs/base.yaml
  - "configs/{{ Os }}.yaml"    # template expansion in paths
```

## Planned merge strategy

- Deep merge: nested objects are merged recursively
- Arrays concatenate
- Later values override earlier ones
