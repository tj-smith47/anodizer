+++
title = "Cross-Compilation"
description = "Build binaries for multiple platforms from a single machine"
weight = 2
template = "docs.html"
+++

Anodizer supports three cross-compilation strategies, configurable via the `cross` field.

## Strategies

| Strategy | Tool | Best for |
|----------|------|----------|
| `auto` | Auto-detects best option | Default — see [decision order](#how-auto-detection-works) below |
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

1. If the target is glibc-linked Linux (`*-linux-gnu*`) and `cargo-zigbuild`
   is installed (with a working `zig` — binary on PATH or the pip `ziglang`
   wheel) → use zigbuild, **even when the target matches the host triple**.
   zig links against its own bundled libc, so the binary's glibc floor stays
   hermetic instead of tracking the build machine — a CI runner image upgrade
   can't silently raise the glibc requirement of your releases. (musl targets
   are exempt: static libc, no glibc floor.)
2. If the target matches the host triple → use native `cargo build`
3. If host and target are both Apple, or both Windows → use native
   `cargo build` (clang / MSVC cross-compile across their own arches)
4. If `cargo-zigbuild` is installed → use it
5. If `cross` is installed → use it
6. Fall back to native `cargo build` (may fail for cross-platform targets)

To opt out of the linux-gnu zigbuild routing and link against the build
host's glibc, set `cross: cargo` (per-crate or under `defaults:`).

Run `anodizer healthcheck` to see which tools are available.

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
anodizer release -p 4        # max 4 parallel builds
anodizer release -p 1        # sequential builds
```

Defaults to the number of logical CPUs.

## Single-target builds

For local testing, build only for your host platform:

```bash
anodizer release --single-target
```
