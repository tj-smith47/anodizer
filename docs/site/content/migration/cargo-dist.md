+++
title = "From cargo-dist"
description = "Migrate from cargo-dist to anodize"
weight = 2
template = "docs.html"
+++

[cargo-dist](https://opensource.axo.dev/cargo-dist/) focuses on binary distribution — building and packaging pre-built binaries with generated installers. Anodize is a full release pipeline that includes distribution but also covers changelog, GitHub releases, package manager publishing, Docker, signing, and announcements.

## When to switch

Consider anodize if you need:
- Publishing to Homebrew, Scoop, crates.io, or AUR
- Docker image builds as part of the release
- Changelog generation from conventional commits
- GPG or cosign signing
- Announcements (Discord, Slack, webhooks)
- More control over archive formats and naming

## Migration steps

1. Install anodize: `cargo install anodize`
2. Run `anodize init` to generate a config from your workspace
3. Add your desired publishing targets (Homebrew, Scoop, etc.)
4. Remove cargo-dist config from your `Cargo.toml` (`[workspace.metadata.dist]`)
5. Replace the cargo-dist GitHub Actions workflow with an anodize workflow
6. Run `anodize release --dry-run` to verify
