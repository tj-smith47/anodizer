+++
title = "Introduction"
description = "What is anodize and why use it"
weight = 1
template = "docs.html"
+++

Anodize is a Rust-native release automation tool. It reads a declarative config file (`.anodize.yaml`) and executes a full release pipeline: build, archive, checksum, changelog, GitHub release, package manager publishing, Docker images, signing, and announcements.

If you've used [GoReleaser](https://goreleaser.com/) for Go projects, anodize is the same idea — built for Rust. Same config structure, same CLI verbs, same template vocabulary.

## Why anodize?

**One config, full pipeline.** Instead of stitching together shell scripts, GitHub Actions steps, and manual uploads, you define your release in YAML and run `anodize release`. Everything happens automatically:

1. **Build** — Cross-compile binaries for every target
2. **Archive** — Package them into tar.gz, zip, tar.xz, or tar.zst archives
3. **Checksum** — Generate SHA-256 (or other) checksums
4. **Changelog** — Generate from conventional commits
5. **Release** — Create a GitHub release with all assets uploaded
6. **Publish** — Push to crates.io, Homebrew, Scoop, and more
7. **Docker** — Build and push multi-arch container images
8. **Sign** — GPG or cosign signatures
9. **Announce** — Notify Discord, Slack, or webhooks

**Cargo-native.** Anodize understands Cargo workspaces, target triples, and cross-compilation strategies. It integrates with `cargo-zigbuild` and `cross` for seamless cross-platform builds.

**Familiar to GoReleaser users.** If you're migrating from Go, the config structure and template syntax will feel immediately familiar. Anodize even accepts GoReleaser's `{{ .Field }}` template syntax alongside native Tera `{{ Field }}` syntax.

## Quick overview

```yaml
# .anodize.yaml
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
      crates: true
      homebrew:
        tap:
          owner: myorg
          name: homebrew-tap
```

```bash
anodize release
```

That's it. Binaries are built, archived, checksummed, released on GitHub, published to crates.io, and a Homebrew formula is pushed to your tap.
