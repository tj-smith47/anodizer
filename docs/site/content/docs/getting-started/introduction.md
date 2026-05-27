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

**Familiar to GoReleaser users.** If you're migrating from Go, the config structure and template syntax will feel immediately familiar. Anodizer even accepts GoReleaser's `{{ .Field }}` template syntax alongside native Tera `{{ Field }}` syntax.

**AI-enhanced release notes.** The changelog stage can summarize commits using an LLM — Anthropic Claude, OpenAI, or a local Ollama model. Configure the provider in `changelog.ai` and get release notes that read like prose instead of a raw commit list.

**Distributed builds and rootless containers.** The `--split` / `--merge` flags fan builds out across CI runners and rejoin artifacts for publishing, eliminating the single-runner bottleneck. Docker image builds support Podman as a drop-in buildx alternative — no daemon, no root required.

## Quick overview

```yaml
# .anodizer.yaml
project_name: myapp

crates:
  - name: myapp
    path: "."
    builds:
      - binary: myapp
    archives:
      - name_template: "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"
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

```bash
anodizer release
```

That's it. Binaries are built, archived, checksummed, released on GitHub, published to crates.io, and a Homebrew formula is pushed to your tap.
