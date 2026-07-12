<div align="center">

<img src="assets/logo.svg" width="200" alt="anodizer logo">

# anodizer

The release pipeline built for Rust — workspace-aware, reproducible, and signed by default.

[![CI](https://github.com/tj-smith47/anodizer/actions/workflows/ci.yml/badge.svg)](https://github.com/tj-smith47/anodizer/actions/workflows/ci.yml)
[![Release](https://github.com/tj-smith47/anodizer/actions/workflows/release.yml/badge.svg)](https://github.com/tj-smith47/anodizer/actions/workflows/release.yml)
[![Docs](https://github.com/tj-smith47/anodizer/actions/workflows/docs.yml/badge.svg)](https://github.com/tj-smith47/anodizer/actions/workflows/docs.yml)
[![Coverage](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/tj-smith47/anodizer/badges/coverage.json)](https://github.com/tj-smith47/anodizer/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/anodizer.svg)](https://crates.io/crates/anodizer)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-blue.svg)](#license)

</div>

Anodizer reads a declarative config file and runs your entire release from a single `anodizer release` command: build, archive, checksum, changelog, sign, release, publish, and announce. It's built around the Rust ecosystem — Cargo workspaces, `Cargo.lock`-aware version bumps, crates.io, and byte-reproducible artifacts.

Written by [Claude](https://claude.ai); maintained by us.

See [What works (with proof)](https://tj-smith47.github.io/anodizer/dogfooding/) for a per-feature status — every "live" claim links to a real published artifact you can verify yourself.

## Why anodizer?

Your release is a Cargo workspace — not a bag of loose binaries. anodizer is built that way from the ground up.

- **It speaks Cargo.** Per-crate release cadences, per-crate tags, and a tag resolver let a single crate and a thirty-crate monorepo share one config. `anodizer tag` and `anodizer bump` rewrite `Cargo.toml` *and* `Cargo.lock`, then commit and tag — locally by default, and (on `--push`) push the bump commit and tag atomically — no orphaned bump commit, no hand-rolled `git push`, no lockfile drift.
- **crates.io, published in the right order.** Dependency-aware ordering with sparse-index polling holds each crate until the ones it depends on have propagated — so a workspace publish never races itself into a transient "version not found."
- **Cross-compiles without the toolchain tax.** musl, glibc, Windows, and macOS from one machine via `cargo-zigbuild` or `cross`. No `rustup target add` rituals, no per-target CI shards to babysit.
- **Reproducible — and it proves it.** Deterministic artifacts by default, then `anodizer check determinism` rebuilds them and byte-compares. "Reproducible" becomes a fact your CI enforces, not a claim in your release notes.
- **Signing and attestation are first-class.** cosign + GPG for binaries, archives, checksums, and images, plus SLSA-style build provenance — wired in a few lines, not bolted on after a CVE scare.

Then the long tail that Rust authors actually hit: generated per-crate READMEs, `cargo-binstall` metadata derived straight from your config (no hand-maintained `pkg-url` that 404s), `version_files` to pin your docs and install scripts to the released version, and post-release install smoke tests that catch a broken artifact before your users do.

Already know GoReleaser? anodizer's `{{ .Field }}` template syntax will feel right at home. Moving from cargo-dist, release-plz, or cargo-release? The [migration guides](https://tj-smith47.github.io/anodizer/migration/) map your setup straight over.

## Features

**Build**
- Cross-platform builds via `cargo-zigbuild`, `cross`, or native `cargo build`
- Per-build hooks (pre/post), environment variables, feature flags, and target overrides
- UPX binary compression with per-target filtering
- Workspace support with per-crate independent release cadences

**Package**
- Archives in tar.gz, tar.xz, tar.zst, zip, gz, or raw binary format with OS-specific overrides
- Linux packages (.deb, .rpm, .apk, .archlinux, .ipk) via nFPM with full lifecycle scripts
- Snapcraft snaps with prime-dir architecture
- macOS DMG disk images and PKG installers
- Windows MSI and NSIS installers
- Flatpak bundles
- AppImage portable Linux applications
- Makeself self-extracting archives
- Source RPMs (.src.rpm)
- Source archives with file filtering
- SBOM generation (CycloneDX/SPDX)
- Checksums with SHA-256, SHA-512, SHA3, BLAKE2b, BLAKE2s, BLAKE3, CRC32, MD5, and more

**Sign**
- GPG and cosign signing for binaries, archives, checksums, Docker images, and SBOMs
- Multiple independent signing configurations
- Conditional signing via template expressions
- Build provenance attestations (SLSA-style) for binaries and artifacts

**Publish**
- GitHub/GitLab/Gitea Releases with asset uploads, draft/prerelease detection, header/footer templates
- crates.io with dependency-aware ordering and index polling
- Homebrew formula and cask generation
- Homebrew-core formula bump PRs (bump an existing `homebrew-core` formula)
- Scoop manifest generation
- Chocolatey package generation
- Winget manifest generation
- AUR PKGBUILD and .SRCINFO generation
- Krew plugin manifest generation
- Nix derivation generation
- SchemaStore catalog registration for editor autocomplete of your config files
- MCP registry server-manifest publishing (Model Context Protocol)
- Docker multi-arch images via `docker buildx`
- Blob storage uploads (S3, GCS, Azure)
- NPM per-platform binary packages and PyPI native binary wheels
- Artifactory, Cloudsmith, Fury, Docker Hub
- Custom publisher commands

**Announce**
- Discord, Slack, Telegram, Teams, Mattermost
- Email, Reddit, Twitter/X, Mastodon, Bluesky, LinkedIn
- OpenCollective, Discourse
- Generic webhooks with custom headers and templates

**Advanced**
- Tera templates (Jinja2-like) with GoReleaser-compatible `{{ .Field }}` syntax
- Nightly builds with date-based versioning
- Config includes for shared configuration
- Split/merge CI for fan-out parallel builds
- Monorepo support with independent workspaces
- Auto-tagging from commit message directives
- Reproducible builds with `mod_timestamp` and `builds_info`
- Version-string file syncing (`version_files`) to keep docs, scripts, and manifests in lockstep at tag
- Post-release verification with install smoke tests
- JSON Schema for editor autocomplete and validation

## Installation

### Homebrew (macOS/Linux)

```bash
brew install tj-smith47/tap/anodizer
```

### Cargo

```bash
cargo install anodizer
```

### From source

```bash
git clone https://github.com/tj-smith47/anodizer.git
cd anodizer
cargo install --path crates/cli
```

## Quick Start

```bash
# Generate a starter config from your Cargo workspace
anodizer init > .anodizer.yaml

# Validate your config
anodizer check

# Check that required tools are available
anodizer healthcheck

# Build a snapshot (no publishing)
anodizer release --snapshot

# Dry run (full pipeline, no side effects)
anodizer release --dry-run

# Auto-tag from commit directives
# (Conventional Commits: feat: → minor, fix: → patch, BREAKING CHANGE: → major)
anodizer tag --dry-run   # preview what tag would be created
anodizer tag --push      # create + push the tag, which triggers the release workflow

# Or force a specific tag value:
anodizer tag --custom-tag v0.1.0
```

For CI-based releases, set `GITHUB_TOKEN` (or `ANODIZER_GITHUB_TOKEN`) as a secret — the release pipeline picks it up automatically.

## Configuration

Anodizer uses `.anodizer.yaml` (or `.anodizer.toml`) in your project root. Add a schema comment for editor autocomplete:

```yaml
# yaml-language-server: $schema=https://tj-smith47.github.io/anodizer/schema.json

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
      cargo: {}
      homebrew:
        repository:
          owner: myorg
          name: homebrew-tap
```

See the [full configuration reference](https://tj-smith47.github.io/anodizer/docs/reference/configuration/) and the [template reference](https://tj-smith47.github.io/anodizer/docs/general/templates/) for all available fields, variables, and filters.

## Real-world adoption: cfgd

[`cfgd`](https://github.com/tj-smith47/cfgd) — declarative, GitOps-style machine configuration management — is anodizer's first real-world adopter and dogfoods every shipped publisher. It's a 4-crate workspace (shared lib + CLI + Kubernetes operator + CSI driver) that ships to crates.io (dependency-aware ordering), GitHub Releases, Homebrew, Scoop, Chocolatey, Winget, the Snap Store, Krew, GHCR, and via `cargo binstall` — all from one `.anodizer.yaml` and one tag push.

A condensed slice of [cfgd's `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml):

```yaml
workspaces:
  - name: cfgd-core
    crates:
      - name: cfgd-core
        tag_template: "core-v{{ Version }}"
        version_sync: { enabled: true, mode: cargo }

  - name: cfgd
    crates:
      - name: cfgd
        depends_on: [cfgd-core]
        version_sync: { enabled: true, mode: cargo }
        universal_binaries:
          - name_template: "{{ ProjectName }}"
            replace: false
        binstall:
          enabled: true   # pkg-url + per-target overrides derived from archive.name_template
  # ... cfgd-operator, cfgd-csi
```

Every cell of [What works (with proof)](https://tj-smith47.github.io/anodizer/dogfooding/) links to a real published cfgd artifact for the feature in question — that's the verification surface.

## GitHub Actions

Anodizer ships a first-party action, [`tj-smith47/anodizer-action`](https://github.com/tj-smith47/anodizer-action), which is what this repo dogfoods in its own `release.yml`:

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
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0

      - uses: dtolnay/rust-toolchain@stable

      - name: Release
        uses: tj-smith47/anodizer-action@v1
        with:
          version: latest        # accepts `latest`, `nightly`, or an exact tag (e.g. `v0.5.0`). Pin in production.
          auto-install: true     # auto-detect nfpm/makeself/snapcraft/cosign/etc from .anodizer.yaml
          args: release --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

For split/merge fan-out, GPG key import, registry login, and per-platform variants, see [anodizer-action](https://github.com/tj-smith47/anodizer-action) and the live [`.github/workflows/release.yml`](.github/workflows/release.yml) in this repo.

## CLI Reference

```
anodizer release       Full release pipeline (--snapshot, --dry-run, --split/--merge, --publish-only, --rollback-only)
anodizer tag           Auto-tag from commit directives
anodizer tag rollback  Delete anodize-managed tags at a SHA and revert the bump commit
anodizer check         Validate configuration + run determinism harness
anodizer preflight     Verify the environment can run the configured release (tools, secrets, key material)
anodizer init          Generate starter .anodizer.yaml
anodizer healthcheck   Probe external tools (nfpm, cosign, ...)
```

Failure handling is in-process: a failed `anodizer release` executes the
`release.on_failure` policy itself (`rollback` by default — delete the tag,
revert the bump — auto-degrading to `hold` once a one-way-door publisher like
crates.io has landed), so release workflows need no `if: failure()` recovery
steps. `anodizer tag rollback "$GITHUB_SHA"` is the manual recovery command
for killed or held runs. See
[Release resilience](https://tj-smith47.github.io/anodizer/docs/advanced/release-resilience/)
for the flag matrix and recovery flows.

Full reference: `anodizer --help` or the [docs site](https://tj-smith47.github.io/anodizer/docs/reference/cli/).

## Documentation

Full documentation is available at **[tj-smith47.github.io/anodizer](https://tj-smith47.github.io/anodizer/)**.

Operator guides:

- [Release resilience guide](https://tj-smith47.github.io/anodizer/docs/advanced/release-resilience/) - three-group publisher dispatch, Submitter gate, rollback, replay-from-run
- [Determinism guide](https://tj-smith47.github.io/anodizer/docs/advanced/determinism/) - byte-stability contract, `anodizer check determinism` harness, runtime allow-list

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
