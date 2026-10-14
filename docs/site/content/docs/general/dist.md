+++
title = "Dist Directory"
description = "Configure the output directory for build artifacts"
weight = 6
template = "docs.html"
+++

The `dist` directory is where Anodizer places all build outputs, packages, and metadata files during a release.

## Config

```yaml
dist: ./dist
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `dist` | string | `./dist` | Output directory for all artifacts |

## Behavior

- The directory is created automatically if it does not exist
- Build outputs are placed in `dist/{target}/` subdirectories by default
- `metadata.json` and `artifacts.json` are written to the dist root
- All stages write their outputs relative to this directory

## Flat output

By default, binaries are organized by target triple (e.g., `dist/x86_64-unknown-linux-gnu/myapp`). To place all binaries in a flat directory instead:

```yaml
crates:
  - name: myapp
    no_unique_dist_dir: true
```

This can also be set per-build:

```yaml
crates:
  - name: myapp
    builds:
      - targets:
          - x86_64-unknown-linux-gnu
        no_unique_dist_dir: true
```

## `--split` and `--merge`

When using split/merge CI (building on multiple machines), each split job writes to its own dist directory. The merge job reads `context.json` files from all dist subdirectories. If you override `dist`, pass the same value via `--dist` to the merge command:

```bash
anodizer release --merge --dist ./my-custom-dist
```
