+++
title = "Cross-Compilation"
description = "Build binaries for multiple platforms from a single machine"
weight = 2
template = "docs.html"
+++

Anodize supports three cross-compilation strategies, configurable via the `cross` field.

## Strategies

| Strategy | Tool | Best for |
|----------|------|----------|
| `auto` | Auto-detects best option | Default — tries zigbuild, then cross, then native cargo |
| `zigbuild` | `cargo-zigbuild` | Fast, minimal setup, works for most targets |
| `cross` | `cross` | Targets requiring system libraries (e.g., OpenSSL) |
| `cargo` | Native `cargo build` | Same-platform targets only |

## Config

```yaml
defaults:
  cross: auto    # auto | zigbuild | cross | cargo
```

Per-crate override:

```yaml
crates:
  - name: myapp
    cross: zigbuild    # override default for this crate
```

## How auto-detection works

When `cross: auto` (the default):

1. If the target matches the host triple → use native `cargo build`
2. If `cargo-zigbuild` is installed → use it
3. If `cross` is installed → use it
4. Fall back to native `cargo build` (may fail for cross-platform targets)

Run `anodize healthcheck` to see which tools are available.

## Installing cross-compilation tools

```bash
# cargo-zigbuild (recommended for most cases)
cargo install cargo-zigbuild

# cross (for targets needing system libraries)
cargo install cross
```

## Parallelism

Control the number of parallel build jobs with `--parallelism` / `-p`:

```bash
anodize release -p 4        # max 4 parallel builds
anodize release -p 1        # sequential builds
```

Defaults to the number of logical CPUs.

## Single-target builds

For local testing, build only for your host platform:

```bash
anodize release --single-target
```
