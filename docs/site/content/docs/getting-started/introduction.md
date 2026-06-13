+++
title = "Introduction"
description = "What is anodizer and why use it"
weight = 1
template = "docs.html"
+++

Anodizer is a Rust-native release automation tool. It reads a declarative config file (`.anodizer.yaml`) and executes a full release pipeline: build, archive, checksum, changelog, GitHub release, package manager publishing, Docker images, signing, and announcements.

If you've used [GoReleaser](https://goreleaser.com/) for Go projects, anodizer is the same idea — built for Rust. Same config structure, same CLI verbs, same template vocabulary.

## Why anodizer?

**One config, full pipeline.** Instead of stitching together shell scripts, GitHub Actions steps, and manual uploads, you define your release in YAML and run `anodizer release`. Everything happens automatically:

1. **Build** — Cross-compile binaries for every target
2. **Archive** — Package them into tar.gz, zip, tar.xz, or tar.zst archives
3. **Checksum** — Generate SHA-256 (or other) checksums
4. **Changelog** — Generate from conventional commits
5. **Release** — Create a GitHub release with all assets uploaded
6. **Publish** — Push to crates.io, Homebrew, Scoop, and more
7. **Docker** — Build and push multi-arch container images
8. **Sign** — GPG or cosign signatures
9. **Announce** — Notify Discord, Slack, or webhooks

**Cargo-native.** Anodizer understands Cargo workspaces, target triples, and cross-compilation strategies. It integrates with `cargo-zigbuild` and `cross` for seamless cross-platform builds.

**Familiar to GoReleaser users.** If you're migrating from Go, the config structure and CLI verbs feel immediately familiar — and so do your templates. Anodizer renders every template on the [Tera](https://keats.github.io/tera/) engine but accepts **two dialects**: the canonical Tera-native no-dot form (`{{ Version }}`) and the dotted Go `text/template` form a `.goreleaser.yaml` is written in (`{{ .Version }}`). The dotted form — along with Go idioms like `{{ if }}`, `eq`, `ne`, `and`, and `or` — is auto-translated before rendering, so a snippet pasted straight from a GoReleaser config works unchanged. See the [Templates reference](@/docs/general/templates.md) for the full variable list and the Go-to-Tera mapping table.

**AI-enhanced release notes.** The changelog stage can summarize commits using an LLM — Anthropic Claude, OpenAI, or a local Ollama model. Configure the provider in `changelog.ai` and get release notes that read like prose instead of a raw commit list.

**Distributed builds and rootless containers.** The `--split` / `--merge` flags fan builds out across CI runners and rejoin artifacts for publishing, eliminating the single-runner bottleneck. Docker image builds support Podman as a drop-in buildx alternative — no daemon, no root required.

## Quick overview

A complete, loadable config has three layers: **what to build** (`crates`), **where to release** (`release`), and **where to publish** (`publish` per crate, plus top-level publishers like `homebrew_casks`). This config builds one binary, archives and checksums it, cuts a GitHub release, publishes to crates.io, and pushes a Homebrew **cask** to your tap:

```yaml
# .anodizer.yaml
project_name: myapp

crates:
  - name: myapp
    path: "."
    builds:
      - binary: myapp        # the [[bin]] target name from Cargo.toml
    archives:
      - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
    release:
      github:
        owner: myorg
        name: myapp
    publish:
      cargo: {}              # empty map opts in; cargo publishes to crates.io

# homebrew_casks is a top-level array — the canonical, warning-free Homebrew
# surface. Each entry opens (or updates) a PR against its tap repo per release.
homebrew_casks:
  - repository:
      owner: myorg
      name: homebrew-tap
    directory: Casks
    description: "A Rust-native release automation tool"
    homepage: "https://github.com/myorg/myapp"
    license: MIT
    binaries:
      - myapp
    commit_author:
      name: anodizer-bot
      email: bot@example.com
```

With that config in place, one command runs the whole pipeline:

```bash
anodizer release
```

Binaries are built, archived, checksummed, released on GitHub, published to crates.io, and a Homebrew cask PR is opened against `myorg/homebrew-tap`. The release stage needs a `GITHUB_TOKEN` with `contents:write` (and `pull_request:write` for the cask PR); `cargo` needs `CARGO_REGISTRY_TOKEN`. Start with `anodizer release --dry-run` to see the plan with no side effects — see the [Quick Start](@/docs/getting-started/quick-start.md) for the full first-release walkthrough.

> **Prerequisite:** the tap repo (`myorg/homebrew-tap` above) must already exist before a release can push to it — anodizer writes the cask into it but does not create it. Create an empty `<owner>/homebrew-tap` repo first, and make sure the token has `contents:write` (direct push) or `pull_request:write` (PR workflow) on it.

> **Why `homebrew_casks`, not `publish.homebrew`?** The cask is the canonical channel for pre-compiled binaries — including CLI tools, which install as a `binary` stub. The older `publish.homebrew` *Formula* path still parses for back-compat but emits a deprecation warning at config-load. New configs should use `homebrew_casks`. See [Homebrew Casks](@/docs/publish/homebrew-casks.md) for the full cask surface; the [deprecated Formula reference](@/docs/publish/homebrew.md) documents both forms, and the [migration guide](@/migration/goreleaser.md) covers the GoReleaser jump.
