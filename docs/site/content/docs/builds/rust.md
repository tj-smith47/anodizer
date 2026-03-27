+++
title = "Rust Builds"
description = "Configure how anodize compiles your Rust binaries"
weight = 1
template = "docs.html"
+++

The build stage compiles your Rust crate for each configured target triple.

## Minimal config

```yaml
crates:
  - name: myapp
    path: "."
    builds:
      - binary: myapp
```

This builds a single binary for the default targets.

## Build config fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `binary` | string | — | Name of the binary to build (must match a `[[bin]]` in Cargo.toml) |
| `targets` | list | inherited from `defaults.targets` | Target triples to compile for |
| `features` | list | none | Cargo features to enable |
| `no_default_features` | bool | `false` | Disable default features |
| `flags` | string | none | Additional flags passed to `cargo build` |
| `env` | map | none | Per-target environment variables |
| `copy_from` | string | none | Copy build config from another crate |
| `reproducible` | bool | `false` | Enable reproducible build settings |

## Multiple binaries

If your crate produces multiple binaries:

```yaml
crates:
  - name: myapp
    path: "."
    builds:
      - binary: myapp
      - binary: myapp-cli
        features: ["cli-extras"]
```

## Custom targets

```yaml
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
    - x86_64-apple-darwin
    - aarch64-apple-darwin
    - x86_64-pc-windows-msvc

crates:
  - name: myapp
    builds:
      - binary: myapp
        targets:            # override defaults for this binary
          - x86_64-unknown-linux-gnu
          - aarch64-apple-darwin
```

## Build features

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        features: ["tls", "compression"]
        no_default_features: true
```

## Full example

```yaml
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
    - x86_64-apple-darwin
    - aarch64-apple-darwin
    - x86_64-pc-windows-msvc
  cross: auto

crates:
  - name: myapp
    path: "."
    builds:
      - binary: myapp
        features: ["production"]
        env:
          x86_64-unknown-linux-gnu:
            CC: "gcc"
```
