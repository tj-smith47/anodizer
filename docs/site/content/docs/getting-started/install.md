+++
title = "Install"
description = "How to install anodize"
weight = 2
template = "docs.html"
+++

## From crates.io

```bash
cargo install anodize
```

## From source

```bash
git clone https://github.com/tj-smith47/anodize.git
cd anodize
cargo install --path crates/cli
```

## In GitHub Actions

Use [`tj-smith47/anodize-action`](https://github.com/tj-smith47/anodize-action) instead of `cargo install` — it caches the binary, auto-installs pipeline dependencies (nfpm, cosign, zig, snapcraft, ...), and imports signing keys in a single step:

```yaml
- uses: tj-smith47/anodize-action@v1
  with:
    auto-install: true
    args: release --clean
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

See [GitHub Actions](@/docs/ci/github-actions.md) for full examples.

## Verify installation

```bash
anodize --version
```

## Required tools

Anodize shells out to external tools for certain stages. Run `anodize healthcheck` to see which are available:

```bash
anodize healthcheck
```

| Tool | Required for | Install |
|------|-------------|---------|
| `cargo` | Building | Comes with Rust |
| `git` | Version detection, changelog | System package manager |
| `docker` | Docker stage | [docker.com](https://docs.docker.com/get-docker/) |
| `nfpm` | Linux packages (.deb, .rpm, .apk) | [nfpm.goreleaser.com](https://nfpm.goreleaser.com/install/) |
| `cargo-zigbuild` | Cross-compilation (zigbuild strategy) | `cargo install cargo-zigbuild` |
| `cross` | Cross-compilation (cross strategy) | `cargo install cross` |
| `gpg` | GPG signing | System package manager |
| `cosign` | Cosign signing | [sigstore.dev](https://docs.sigstore.dev/cosign/installation/) |

Only `cargo` and `git` are required for basic usage. Other tools are only needed if you enable the corresponding stages.
