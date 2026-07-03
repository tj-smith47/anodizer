+++
title = "Import Pre-Built Binaries"
description = "Skip cargo build and import binaries you've already produced (CGO, external toolchains, sharded CI)"
weight = 90
template = "docs.html"
+++

`builder: prebuilt` skips `cargo build` entirely and imports binaries you've
already produced elsewhere into the anodizer release pipeline. Once imported,
the binary flows through every downstream stage (archive, sbom, sign,
checksum, publish) exactly as if anodizer had compiled it.

Reasons to use this builder:

- **CGO / external toolchains.** You build with a tool anodizer can't drive
  in-process (a custom `Makefile`, Bazel, a vendored cross-compiler).
- **Sharded CI.** You build each platform on its own runner with
  `anodizer build --single-target` and want a second config to assemble the
  release from the artifacts.
- **Pre-compiled vendor binaries.** You're shipping a binary you didn't
  build yourself (vendor SDK, statically linked third-party tool).

## Minimal config

```yaml
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ Version }}"
    builds:
      - binary: myapp
        builder: prebuilt
        prebuilt:
          path: "output/myapp_{{ Target }}"
        targets:
          - x86_64-unknown-linux-gnu
          - aarch64-unknown-linux-gnu
          - x86_64-apple-darwin
          - aarch64-apple-darwin
          - x86_64-pc-windows-msvc
```

With those binaries staged before the build runs:

```text
output/myapp_x86_64-unknown-linux-gnu
output/myapp_aarch64-unknown-linux-gnu
output/myapp_x86_64-apple-darwin
output/myapp_aarch64-apple-darwin
output/myapp_x86_64-pc-windows-msvc.exe
```

…`anodizer build` imports each one, registers it as an `ArtifactKind::Binary`
artifact tagged with the matching target triple, and lets the rest of the
pipeline (archive, sbom, sign, checksum, publish) run unchanged.

> **Warning.** Stage your binaries OUTSIDE `dist/`. anodizer removes
> `dist/` at the start of every release run, so a `prebuilt.path` that
> points into `dist/` would resolve against an empty directory and fail
> with a stat error.

## Path template

`prebuilt.path` is rendered through anodizer's Tera template engine once per
target with the following variables (in addition to the project-wide
globals like `{{ Version }}` and `{{ ProjectName }}`):

| Variable | Example | Notes |
|---|---|---|
| `{{ Target }}` | `x86_64-unknown-linux-gnu` | Full Rust target triple. |
| `{{ Os }}` | `linux` | GoReleaser-style OS slug (`linux`, `darwin`, `windows`, …). |
| `{{ Arch }}` | `amd64` | GoReleaser-style arch slug (`amd64`, `arm64`, `armv7`, …). |
| `{{ Amd64 }}` | `v1` | AMD64 micro-arch variant; set for `x86_64-*` triples. Imports default to the `v1` baseline; declare `amd64_variant: "v3"` on the build entry when importing a tuned binary so its metadata and asset names carry the real level. |
| `{{ Arm64 }}` | `v8` | ARM64 micro-arch variant; set for `aarch64-*` triples. |
| `{{ Arm }}` | `7` | ARM micro-arch variant; set for `armv6*` / `armv7*` triples. |
| `{{ I386 }}` | `sse2` | i386 micro-arch variant; set for `i686-*` / `i386-*` / `i586-*` triples. |
| `{{ ArtifactExt }}` | `.exe` | `.exe` on Windows targets, empty elsewhere. |
| `{{ ArtifactID }}` | `mybuild` | The build entry's `id:` value (empty when unset). |

Examples:

```yaml
# Mirror cargo's per-target directory layout
prebuilt:
  path: "target/{{ Target }}/release/myapp"

# Match GoReleaser's documented (Os, Arch) shape
prebuilt:
  path: "output/myapp_{{ Os }}_{{ Arch }}"

# Per-architecture amd64 variant suffix
prebuilt:
  path: "output/myapp_{{ Os }}_{{ Arch }}{{ if Amd64 }}_{{ Amd64 }}{{ end }}"
```

The rendered path is `stat()`-ed before the import. A missing file, a
permission error, or any other I/O failure aborts the build with a message
that names both the rendered path and the originating target triple — anodizer
will never silently fall through on a missing prebuilt binary, matching
GoReleaser's "GoReleaser will fail" contract.

## Required-explicit `targets:`

When `builder: prebuilt` is set, the build entry MUST declare its own
`targets:` list. The `defaults.targets:` fallback does NOT apply.

```yaml
# OK
builds:
  - binary: myapp
    builder: prebuilt
    prebuilt:
      path: "output/myapp_{{ Target }}"
    targets:
      - x86_64-unknown-linux-gnu

# REJECTED at config-load
defaults:
  targets: [x86_64-unknown-linux-gnu]
builds:
  - binary: myapp
    builder: prebuilt
    prebuilt:
      path: "output/myapp_{{ Target }}"
    # targets: missing — `defaults.targets` does not propagate to prebuilt builds
```

Rationale: a prebuilt import has no concept of a "default target matrix" —
the only reason to use `builder: prebuilt` is because you already know which
binaries you've staged on disk, so you should also know exactly which
targets to register.

## Cargo-only knobs are rejected

The following fields are mutually exclusive with `builder: prebuilt` and
fail at config-load time when set together:

| Field | Why it's rejected |
|---|---|
| `cross_tool` | Selects the cross-compile binary cargo invokes. No cargo invocation under prebuilt. |
| `command` | Replaces the cargo subcommand (e.g. `auditable build`). No cargo invocation under prebuilt. |
| `features` | Cargo features are evaluated at compile time. Prebuilt skips compilation. |
| `no_default_features` | Same reasoning as `features`. |
| crate-level `cross:` strategy | The strategy controls how cargo cross-compiles. Set on a crate whose builds include any `builder: prebuilt`, it's rejected (no compilation to direct). |

Each rejection error names the offending field and points at the remediation
(either drop the field or switch to `builder: cargo`).

## CGO / external-toolchain workflow

A common pattern is to build each platform on its own CI runner with a
platform-specific config, then assemble the release from the artifacts in a
single coordinator job.

### Stage 1 — per-platform build

Each runner uses a minimal config that builds for exactly one target with
the platform-specific toolchain:

```yaml
# .anodizer.build.yaml
crates:
  - name: myapp
    path: "."
    builds:
      - binary: myapp
        # Cargo features, cross_tool, env, etc. are all fair game here
        # since this is a normal `builder: cargo` (the default) build.
        env:
          x86_64-unknown-linux-gnu:
            CC: "x86_64-linux-musl-gcc"
```

Each runner invokes:

```bash
anodizer build -f .anodizer.build.yaml --single-target
```

See [Single-Target Builds](single-target/) for the `--single-target` flag
that restricts the matrix to one triple. Each runner uploads its binary as
a CI artifact.

### Stage 2 — release coordinator

A single job downloads every per-platform artifact into `output/` and runs
a release config that imports them via `builder: prebuilt`:

```yaml
# .anodizer.release.yaml
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ Version }}"
    builds:
      - binary: myapp
        builder: prebuilt
        prebuilt:
          path: "output/myapp_{{ Target }}"
        targets:
          - x86_64-unknown-linux-gnu
          - aarch64-unknown-linux-gnu
          - x86_64-apple-darwin
          - aarch64-apple-darwin
          - x86_64-pc-windows-msvc
    archives: [...]
    publish: {...}
```

```bash
anodizer release -f .anodizer.release.yaml
```

This pattern keeps build environments isolated (each runner only needs its
platform's toolchain), parallelises the slow step (compilation), and lets
a single coordinator handle the orchestrated parts (archive, sign,
checksum, publish).

## Determinism Harness behaviour

`anodizer check determinism` runs N from-clean rebuilds of your project in
hermetic git worktrees and diffs the emitted artifacts byte-for-byte. With
`builder: prebuilt`:

- **All builds are prebuilt** — the harness short-circuits with a status
  line (`determinism harness skipped: no buildable targets (all builds use
  \`builder: prebuilt\`)`) and exits zero. There is nothing to rebuild;
  re-stat()-ing the same staged file twice would just produce two identical
  hashes.
- **Mixed config (some cargo, some prebuilt)** — the harness still runs.
  The cargo targets get the normal hermetic-rebuild treatment; the prebuilt
  artifacts appear in both runs at their staged path with identical bytes
  and pass through the diff cleanly.

## Sign stage compatibility

Prebuilt artifacts register with `ArtifactKind::Binary` — the same kind
`cargo build` outputs use — so anodizer's sign stage picks them up via the
existing `artifacts: binary` selector without a separate code path. The
imported bytes flow through `cosign sign-blob` (or your configured signer)
identically to a `cargo build` output.

## cargo-binstall interaction

`cargo-binstall` derives package URLs from the Cargo registry metadata
(name + version) and a download-URL template. The `builder: prebuilt`
import path does NOT regenerate Cargo registry metadata for the imported
binary — anodizer skips `cargo publish` derivation entirely when the
binary itself wasn't built from a crate.

In practice, if your release ships a prebuilt binary but you also want
`cargo binstall` support, disable binstall for the prebuilt build:

```yaml
crates:
  - name: myapp
    path: "."
    builds:
      - binary: myapp
        builder: prebuilt
        prebuilt: { path: "output/myapp_{{ Target }}" }
        targets: [x86_64-unknown-linux-gnu]
    binstall:
      enabled: false  # cargo-binstall is incompatible with prebuilt imports
```

anodizer does not derive `cargo publish` metadata for prebuilt imports;
disable binstall per-crate via `binstall: { enabled: false }` if you do not
want a placeholder metadata block written into the crate's `Cargo.toml`.

### binstall metadata is auto-derived — `enabled: true` is the norm

You do **not** hand-write `pkg_url` or per-target `overrides`. When `binstall`
is enabled and you supply neither, anodizer derives a per-target
`overrides.<rust-triple>` for every configured build target. Each derived
`pkg-url` is the asset's GitHub release download URL, built from the **same**
`archive.name_template` the archive stage uses — so the URL can never name an
asset the release doesn't actually upload. The "binstall installs the wrong
URL and 404s" class is eliminated by construction.

```yaml
crates:
  - name: cfgd
    path: "."
    binstall:
      enabled: true   # that's it — overrides are derived from archive.name_template
```

In snapshot/dry-run the [emission-validate stage](@/docs/publish/snapshots.md#emission-validate)
cross-checks each emitted binstall URL against the artifacts the run produced
and fails loud if any reference drifts, so a broken binstall block is caught
locally before a release.

### Escape hatch: explicit per-target `pkg_url` overrides

Auto-derivation covers the GoReleaser-style asset-name case automatically, so
you rarely need this. Reach for it only when your assets are named in a way
the archive template can't express — supplying a top-level `pkg_url` **or** any
`overrides` entry suppresses derivation entirely (manual values always win).

`cargo-binstall`'s own tokens only ever expand to its target words —
`{ target }` yields the Rust triple and the OS/arch tokens resolve to
`macos`/`x86_64`/`aarch64`, never `darwin`/`amd64`/`arm64`. To pin each Rust
target triple to a specific asset name, map it under `binstall.overrides`.
Each entry overrides `pkg_url`/`pkg_fmt`/`bin_dir` for that triple and is
emitted as a `[package.metadata.binstall.overrides.<triple>]` sub-table.
anodize templates (`{{ Version }}`) are rendered; cargo-binstall's own
`{ ... }` tokens are left intact.

```yaml
crates:
  - name: cfgd
    path: "."
    binstall:
      enabled: true
      overrides:
        x86_64-unknown-linux-gnu:
          pkg_url: "https://github.com/myorg/cfgd/releases/download/v{{ Version }}/cfgd-{{ Version }}-linux-amd64.tar.gz"
          pkg_fmt: tgz
        aarch64-unknown-linux-gnu:
          pkg_url: "https://github.com/myorg/cfgd/releases/download/v{{ Version }}/cfgd-{{ Version }}-linux-arm64.tar.gz"
          pkg_fmt: tgz
        x86_64-apple-darwin:
          pkg_url: "https://github.com/myorg/cfgd/releases/download/v{{ Version }}/cfgd-{{ Version }}-darwin-amd64.tar.gz"
          pkg_fmt: tgz
        aarch64-apple-darwin:
          pkg_url: "https://github.com/myorg/cfgd/releases/download/v{{ Version }}/cfgd-{{ Version }}-darwin-arm64.tar.gz"
          pkg_fmt: tgz
        x86_64-pc-windows-msvc:
          pkg_url: "https://github.com/myorg/cfgd/releases/download/v{{ Version }}/cfgd-{{ Version }}-windows-amd64.zip"
          pkg_fmt: zip
        aarch64-pc-windows-msvc:
          pkg_url: "https://github.com/myorg/cfgd/releases/download/v{{ Version }}/cfgd-{{ Version }}-windows-arm64.zip"
          pkg_fmt: zip
```

This renders into the crate's `Cargo.toml` as:

```toml
[package.metadata.binstall.overrides.x86_64-unknown-linux-gnu]
pkg-url = "https://github.com/myorg/cfgd/releases/download/v1.2.3/cfgd-1.2.3-linux-amd64.tar.gz"
pkg-fmt = "tgz"
```

so `cargo binstall cfgd` resolves the real per-target asset for every
platform.
