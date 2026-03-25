# Anodize

A Rust-native release automation tool. The same declarative, config-driven release pipeline that [GoReleaser](https://goreleaser.com/) provides for Go — built for Rust.

Anodize reads a declarative config file and executes a full release pipeline: build, archive, checksum, changelog, GitHub release, Docker images, package manager publishing, and more.

Written by [Claude](https://claude.ai); maintained by us.

## Features

- **Cross-platform builds** via `cargo-zigbuild`, `cross`, or native `cargo build`
- **Archives** with tar.gz/zip and OS-specific format overrides
- **Checksums** (SHA256/SHA512) with combined checksums file
- **Changelog** generation from conventional commits
- **GitHub Releases** with asset uploads, draft/prerelease detection
- **crates.io** publishing with dependency-aware ordering and index polling
- **Homebrew** formula generation and tap updates
- **Scoop** manifest generation and bucket updates
- **Docker** multi-arch image builds via `docker buildx`
- **Linux packages** (.deb, .rpm, .apk) via nFPM
- **Signing** with GPG and cosign
- **Announcements** via Discord, Slack, and generic webhooks
- **Workspace support** with per-crate independent release cadences

## Installation

```bash
cargo install anodize
```

## Quick Start

```bash
# Generate a starter config from your Cargo workspace
anodize init > .anodize.yaml

# Validate your config
anodize check

# Build a snapshot (no publishing)
anodize release --snapshot

# Release a specific crate
anodize release --crate my-crate

# Release all crates with unreleased changes
anodize release --all

# Dry run (full pipeline, no side effects)
anodize release --dry-run
```

## Configuration

Anodize uses `.anodize.yaml` (or `.anodize.toml`) in your project root.

```yaml
project_name: myapp

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
    tag_template: "v{{ Version }}"
    builds:
      - binary: myapp
    archives:
      - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
        files: [LICENSE, README.md]
    release:
      github:
        owner: myorg
        name: myapp
    publish:
      crates: true
      homebrew:
        tap:
          owner: myorg
          name: homebrew-tap
```

See the [full configuration reference](docs/configuration.md) for all available fields.

## GitHub Actions

```yaml
name: Release

on:
  push:
    tags: ["v*"]

permissions:
  contents: write

jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - uses: dtolnay/rust-toolchain@stable

      - uses: tj-smith47/anodize@v1
        with:
          args: release
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

> A full-featured JavaScript-based action (with binary caching and structured outputs) is planned at [tj-smith47/anodize-action](https://github.com/tj-smith47/anodize-action).

## CLI Reference

```
anodize release                    Full release pipeline
anodize release --crate <name>     Release a specific crate
anodize release --all              Release all changed crates
anodize release --snapshot         Build without publishing
anodize release --dry-run          Full pipeline, no side effects
anodize release --skip=<stages>    Skip stages (comma-separated)
anodize release --clean            Remove dist/ first
anodize build                      Build only
anodize check                      Validate config
anodize init                       Generate starter config
anodize changelog                  Generate changelog only
```

## Coming Soon

- Monorepo support (multiple independent workspaces)
- Nightly builds
- Config includes/templates
- Split/merge CI builds
- Native OS installers (dmg, msi, pkg)
- Chocolatey and Winget
- Full-featured GitHub Action with caching

## License

MIT
