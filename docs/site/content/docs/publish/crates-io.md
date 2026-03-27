+++
title = "crates.io"
description = "Publish crates to the Rust package registry"
weight = 2
template = "docs.html"
+++

Publish your crate to [crates.io](https://crates.io) with dependency-aware ordering.

## Minimal config

```yaml
crates:
  - name: myapp
    publish:
      crates: true
```

## Config options

```yaml
publish:
  crates:
    enabled: true
    index_timeout: 300    # seconds to wait for crates.io index update
```

## Workspace ordering

When publishing multiple workspace crates, anodize resolves dependency order using topological sorting. If crate `B` depends on crate `A`, `A` is published first and anodize waits for the crates.io index to update before publishing `B`.

## Authentication

Set `CARGO_REGISTRY_TOKEN`:

```bash
export CARGO_REGISTRY_TOKEN="cio_..."
anodize release
```
