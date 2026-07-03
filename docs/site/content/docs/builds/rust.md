+++
title = "Rust Builds"
description = "Configure how anodizer compiles your Rust binaries"
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
| `flags` | list | none | Additional flags passed to `cargo build`, one token per entry (e.g., `["--locked"]`) |
| `env` | map | none | Per-target environment variables |
| `copy_from` | string | none | Copies the built binary from another build's **binary name** in the same crate instead of compiling it |
| `reproducible` | bool | `false` | Enable reproducible build settings |
| `amd64_variant` | string | detected from the build env | Declared x86-64 micro-architecture level (`"v1"`–`"v4"`); overrides detection for artifact metadata and derived asset names |

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

## x86-64 micro-architecture levels

A build tuned for a specific x86-64 level names its assets with that level
(`myapp_1.0.0_linux_amd64v3.tar.gz` for a `v3` build; the `v1` baseline adds
no suffix). anodizer detects the level from the resolved per-target env —
`RUSTFLAGS` or `CARGO_TARGET_<TRIPLE>_RUSTFLAGS` carrying
`-Ctarget-cpu=x86-64-v<N>` (long `--codegen target-cpu=` spelling included) —
both at build time (artifact metadata) and at config time (the derived asset
names behind cargo-binstall `pkg_url` and the `curl | sh` installer):

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        targets: [x86_64-unknown-linux-gnu]
        env:
          x86_64-unknown-linux-gnu:
            RUSTFLAGS: "-Ctarget-cpu=x86-64-v3"   # assets named …_amd64v3.…
```

When the tuning value is only resolvable at build time (so config-time
detection cannot see it), declare the level explicitly — the declaration
overrides detection for both the artifact metadata and every derived-name
consumer:

```yaml
crates:
  - name: myapp
    builds:
      - binary: myapp
        targets: [x86_64-unknown-linux-gnu]
        env:
          x86_64-unknown-linux-gnu:
            RUSTFLAGS: "{{ .Env.CI_TUNE_FLAGS }}"   # not renderable at config time
        amd64_variant: "v3"                          # declares what the flags produce
```

`amd64_variant` also stamps `builder: prebuilt` imports (which otherwise
carry the `v1` baseline — nothing can be detected for an imported binary).
It is ignored for non-x86_64 targets.

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
