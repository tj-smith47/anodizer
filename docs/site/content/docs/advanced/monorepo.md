+++
title = "Monorepo Support"
description = "Release multiple crates from a single repository"
weight = 2
template = "docs.html"
+++

Anodize supports Cargo workspaces with independent release cadences per crate.

## Config

```yaml
crates:
  - name: core-lib
    path: crates/core
    tag_template: "core-v{{ Version }}"
    depends_on: []

  - name: cli-tool
    path: crates/cli
    tag_template: "cli-v{{ Version }}"
    depends_on: [core-lib]
```

## Key features

### Per-crate tags

Each crate uses its own `tag_template` to create independent version tags:

```bash
# Release just the core library
anodize release --crate core-lib

# Release just the CLI
anodize release --crate cli-tool
```

### Dependency ordering

Use `depends_on` to ensure crates are released in the right order. Anodize performs topological sorting — if `cli-tool` depends on `core-lib`, `core-lib` is always released first.

### Release all changed crates

```bash
anodize release --all
```

This detects which crates have unreleased changes (commits since their last tag) and releases them in dependency order.
