+++
title = "Single-Target Builds"
description = "Build only for a specific target instead of the full matrix"
weight = 3
template = "docs.html"
+++

Two flags let you run a subset of the configured target matrix — useful for
local iteration, CI escape hatches, and determinism harness sharding.

## `--single-target`

Build only for the host triple. Nothing else changes: UPX, archive, checksum,
and all downstream stages run exactly as for a full release, just for one
platform.

```bash
anodizer release --single-target
```

Typical use: tight iteration loop where you only need the local binary.

```yaml
# No config changes needed — --single-target is a runtime flag.
# Your existing targets list is ignored for the build step.
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
    - x86_64-apple-darwin
    - aarch64-apple-darwin
    - x86_64-pc-windows-msvc
    - aarch64-pc-windows-msvc
```

```bash
# Builds only x86_64-unknown-linux-gnu on a Linux host.
anodizer release --snapshot --single-target
```

## `--targets=<csv>`

Build for an explicit comma-separated subset of the configured triples.
Useful when you know exactly which platforms you need — for example, the
determinism harness shards the matrix across runners and passes `--targets=`
per shard so each runner only cross-compiles the platforms it is responsible
for.

```bash
anodizer release --targets=x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu
```

Config is not required, but the targets you pass must be a subset of the
triples declared in your `defaults.targets` (or the per-crate `builds[].targets`
override); unknown triples are rejected at validation time.

```yaml
# CI shard config — build step is scoped at runtime with --targets=
defaults:
  targets:
    - x86_64-pc-windows-msvc
    - aarch64-pc-windows-msvc
  cross: auto
```

```bash
# On a Windows runner: build only the x86_64 Windows target.
anodizer release --targets=x86_64-pc-windows-msvc
```

## `--output / -o`

Copy the resulting binary to a specific path after the build step. Handy when
you want the binary in a well-known location without parsing `dist/`.

```bash
anodizer build -o ./bin/anodizer
```

```yaml
# No config equivalent — --output is a runtime-only flag.
```

The flag copies the binary for the host target (or the sole target when only
one is built); it is not defined when building multiple targets simultaneously.

## Related

- [Cross-Compilation](cross-compilation.md) — strategy selection (`auto`, `zigbuild`, `cross`, `cargo`)
- [Split/Merge builds](@/docs/advanced/split-merge.md) — fan-out across runners and rejoin for publish
