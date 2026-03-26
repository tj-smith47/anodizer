# Anodize

A Rust-native release automation tool. The same declarative, config-driven release pipeline that [GoReleaser](https://goreleaser.com/) provides for Go — built for Rust.

Anodize reads a declarative config file and executes a full release pipeline: build, archive, checksum, changelog, GitHub release, Docker images, package manager publishing, and more.

Written by [Claude](https://claude.ai); maintained by us.

## Features

- **Cross-platform builds** via `cargo-zigbuild`, `cross`, or native `cargo build`
- **Archives** in tar.gz, tar.xz, tar.zst, zip, or raw binary format with OS-specific overrides
- **Checksums** with SHA-1, SHA-224, SHA-256, SHA-384, SHA-512, BLAKE2b, and BLAKE2s
- **Changelog** generation from conventional commits or GitHub-native release notes
- **GitHub Releases** with asset uploads, draft/prerelease detection, header/footer templates
- **crates.io** publishing with dependency-aware ordering and index polling
- **Homebrew** formula generation and tap updates
- **Scoop** manifest generation and bucket updates
- **Docker** multi-arch image builds via `docker buildx` with extra files and push control
- **Linux packages** (.deb, .rpm, .apk) via nFPM with full lifecycle scripts
- **Signing** with GPG and cosign (multiple signing configs supported)
- **Custom publishers** for generic post-release artifact publishing
- **Announcements** via Discord, Slack, and generic webhooks
- **Tera templates** (Jinja2-like) with GoReleaser-compatible `{{ .Field }}` syntax
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

See the [full configuration reference](docs/configuration.md) for all available fields, the [template reference](docs/templates.md) for template variables and filters, and the [migration guide](docs/migration-from-goreleaser.md) if you are coming from GoReleaser.

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

### Commands

```
anodize release                    Full release pipeline
anodize build                      Build binaries only
anodize check                      Validate configuration
anodize init                       Generate starter config
anodize changelog                  Generate changelog only
anodize completion <shell>         Generate shell completions (bash/zsh/fish/powershell)
anodize healthcheck                Check availability of required external tools
```

### Global Flags

```
-f, --config <path>                Path to config file (overrides auto-detection)
    --verbose                      Enable verbose output
    --debug                        Enable debug output
```

### Release Flags

```
    --crate <name>                 Release a specific crate (repeatable)
    --all                          Release all crates with unreleased changes
    --force                        Force release even without changes
    --snapshot                     Build without publishing
    --dry-run                      Full pipeline, no side effects
    --clean                        Remove dist/ directory first
    --skip=<stages>                Skip stages (comma-separated)
    --token <token>                GitHub token (overrides GITHUB_TOKEN env)
    --timeout <duration>           Pipeline timeout (default: 30m)
-p, --parallelism <n>              Max parallel build jobs (default: CPU count)
    --auto-snapshot                Auto-enable snapshot if repo is dirty
    --single-target                Build only for the host target triple
    --release-notes <path>         Custom release notes file (overrides changelog)
```

### Build Flags

```
    --crate <name>                 Build a specific crate (repeatable)
    --timeout <duration>           Pipeline timeout (default: 30m)
-p, --parallelism <n>              Max parallel build jobs (default: CPU count)
    --single-target                Build only for the host target triple
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
