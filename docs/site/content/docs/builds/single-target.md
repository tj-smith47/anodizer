+++
title = "Single-Target Builds"
description = "Build only one target of the full matrix"
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

## `--host-targets` {#host-targets}

Build **every** configured target this host can build, automatically skipping
only the ones that need a cross-toolchain the host lacks. In practice the only
real blocker is Apple: `*-apple-darwin` (and any `*-apple-*`) targets require a
macOS SDK that exists only on a real Mac, so they are skipped on a non-macOS
host. Linux and Windows targets cross-link from any host via cargo-zigbuild and
are always kept.

Unlike `--single-target` (one platform) and `--targets=<csv>` (a hand-picked
subset), `--host-targets` is computed from the host: "build the maximal subset
this machine can actually produce." The skipped triples are logged loudly so
you always know what was dropped:

```bash
$ anodizer release --snapshot --host-targets
skipped 2 targets — not buildable on this linux host (--host-targets): \
  aarch64-apple-darwin, x86_64-apple-darwin (apple targets require a macOS host)
```

It is mutually exclusive with `--single-target` and `--targets`, and is only
valid together with `--snapshot` or `--dry-run` — silently dropping configured
targets in a **real** release would ship an incomplete artifact set, so a
non-snapshot run with `--host-targets` hard-errors at startup. If the host can
build *none* of the configured targets (e.g. an apple-darwin-only config on
Linux), it hard-errors too, telling you to run on a macOS host.

```bash
# `task prepush` uses this for a real host-scoped build of everything,
# minus publish/announce (auto-skipped in snapshot mode).
anodizer release --snapshot --clean --host-targets
```

## `--output / -o`

Copy the resulting binary to a specific path after the build step. Handy when
you want the binary in a well-known location without parsing `dist/`.

```bash
anodizer build --single-target -o ./bin/anodizer
```

```yaml
# No config equivalent — --output is a runtime-only flag.
```

The flag copies ONE binary, so it requires `--single-target` and a selection
that builds exactly one. Both are checked before cargo is spawned: a workspace
whose selected crates carry two producing `builds[]` entries is refused
naming them, so nothing is compiled only to be left in `dist/`:

```text
Error --output requires a single build: 2 builds across 1 crate (myapp) would produce a binary; narrow the selection with --crate, or keep one builds[] entry
```

The count covers only the entries the build stage would compile in this run.
An entry whose `skip:` is true, including a template such as
`{{ IsSnapshot }}` that renders true for this run, and an entry with no
targets are left out. `anodizer build` always runs in snapshot mode, so this
config builds only the `snapshot` entry and `-o` accepts it:

```yaml
crates:
  - name: myapp
    builds:
      - id: release-only
        skip: "{{ IsSnapshot }}"
      - id: snapshot
```

```bash
anodizer build --single-target -o ./bin/myapp
```

`-o` without `--single-target` is refused with
`--output requires --single-target (only one binary can be copied)`.

A crate whose single build entry yields several binaries is only known after
the build, and is refused there the same way.

## Related

- [Cross-Compilation](cross-compilation.md) — strategy selection (`auto`, `zigbuild`, `cross`, `cargo`)
- [Split/Merge builds](@/docs/advanced/split-merge.md) — fan-out across runners and rejoin for publish
