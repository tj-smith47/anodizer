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
  `anodize build --single-target` and want a second config to assemble the
  release from the artifacts.
- **Pre-compiled vendor binaries.** You're shipping a binary you didn't
  build yourself (vendor SDK, statically linked third-party tool).

## Minimal config

```yaml
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        builder: prebuilt
        prebuilt:
          path: "output/myapp_{{ .Target }}"
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

…`anodize build` imports each one, registers it as an `ArtifactKind::Binary`
artifact tagged with the matching target triple, and lets the rest of the
pipeline (archive, sbom, sign, checksum, publish) run unchanged.

> **Warning.** Stage your binaries OUTSIDE `dist/`. anodizer removes
> `dist/` at the start of every release run, so a `prebuilt.path` that
> points into `dist/` would resolve against an empty directory and fail
> with a stat error.

## Path template

`prebuilt.path` is rendered through anodizer's Tera template engine once per
target with the following variables (in addition to the project-wide
globals like `{{ .Version }}` and `{{ .ProjectName }}`):

| Variable | Example | Notes |
|---|---|---|
| `{{ .Target }}` | `x86_64-unknown-linux-gnu` | Full Rust target triple. |
| `{{ .Os }}` | `linux` | GoReleaser-style OS slug (`linux`, `darwin`, `windows`, …). |
| `{{ .Arch }}` | `amd64` | GoReleaser-style arch slug (`amd64`, `arm64`, `armv7`, …). |
| `{{ .Amd64 }}` | `v1` | AMD64 micro-arch variant, present for `x86_64-*` triples. |
| `{{ .Arm64 }}` | `v8` | ARM64 micro-arch variant, present for `aarch64-*` triples. |

Examples:

```yaml
# Mirror cargo's per-target directory layout
prebuilt:
  path: "target/{{ .Target }}/release/myapp"

# Match GoReleaser's documented (Os, Arch) shape
prebuilt:
  path: "output/myapp_{{ .Os }}_{{ .Arch }}"

# Per-architecture amd64 variant suffix
prebuilt:
  path: "output/myapp_{{ .Os }}_{{ .Arch }}{{ if .Amd64 }}_{{ .Amd64 }}{{ endif }}"
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
      path: "output/myapp_{{ .Target }}"
    targets:
      - x86_64-unknown-linux-gnu

# REJECTED at config-load
defaults:
  targets: [x86_64-unknown-linux-gnu]
builds:
  - binary: myapp
    builder: prebuilt
    prebuilt:
      path: "output/myapp_{{ .Target }}"
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
anodize build -f .anodizer.build.yaml --single-target
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
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        builder: prebuilt
        prebuilt:
          path: "output/myapp_{{ .Target }}"
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
anodize release -f .anodizer.release.yaml
```

This pattern keeps build environments isolated (each runner only needs its
platform's toolchain), parallelises the slow step (compilation), and lets
a single coordinator handle the orchestrated parts (archive, sign,
checksum, publish).

## Determinism harness behaviour

`anodize check determinism` runs N from-clean rebuilds of your project in
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
        prebuilt: { path: "output/myapp_{{ .Target }}" }
        targets: [x86_64-unknown-linux-gnu]
    binstall:
      enabled: false  # cargo-binstall is incompatible with prebuilt imports
```

A future revision may auto-generate the binstall block from the imported
artifacts directly; for now the integration is opt-out per crate.
