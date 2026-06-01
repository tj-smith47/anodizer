+++
title = "Why Anodizer"
description = "The Rust-native release features anodizer is built around."
weight = 5
template = "docs.html"
+++

The pipeline stages — build, archive, sign, release, publish, announce — are table stakes. What follows is what anodizer does *because* it's Rust-first: it understands Cargo workspaces, lockfiles, and crates.io, and it proves its own output is reproducible.

## Workspace-native releases

Single crates and multi-crate workspaces use the same config. Each crate can release on its own cadence with its own tag prefix, and anodizer keeps cross-crate version specs in sync.

```yaml
workspaces:
  - crates:
      - name: my-core
        tag_template: "my-core-v{{ .Version }}"
      - name: my-cli
        tag_template: "my-cli-v{{ .Version }}"
```

See [Monorepo / workspaces](@/docs/advanced/monorepo.md).

## Cargo- and lockfile-aware versioning

`anodizer tag` reads Conventional Commits, bumps `Cargo.toml` **and** `Cargo.lock`, commits, tags, and (with `--push`) pushes the bump commit and tag atomically — so a tag never points at a commit missing from the branch.

```bash
anodizer tag --push          # bump + tag + push, no orphaned commit
anodizer bump minor          # bump versions in a PR-first workflow
```

See [Auto-tagging](@/docs/advanced/auto-tagging.md).

## crates.io, ordered correctly

Workspace crates publish in dependency order, and anodizer waits for each dependency to appear on the sparse index before publishing its dependents — no racing propagation.

```yaml
publish:
  cargo:
    wait_for_workspace_deps: true
```

## Reproducible — and verified

Artifacts are deterministic by default. The determinism harness rebuilds in a hermetic worktree and byte-compares the output, so reproducibility is proven, not assumed.

```bash
anodizer check determinism
```

See [Determinism](@/docs/advanced/determinism.md) and [Reproducible builds](@/docs/advanced/reproducible-builds.md).

## Zero-config cross-compilation

musl, glibc, Windows, and macOS targets build via `cargo-zigbuild` or `cross` without per-target toolchain setup.

```yaml
crates:
  - name: my-cli
    builds:
      - targets:
          - x86_64-unknown-linux-musl
          - aarch64-apple-darwin
          - x86_64-pc-windows-msvc
```

See [Cross-compilation](@/docs/builds/cross-compilation.md).

## Rust-ecosystem niceties

- **Generated crate READMEs** kept in sync with a template.
- **`cargo-binstall` metadata** derived from your build config, so `cargo binstall` resolves your release assets automatically.
- **Build from a branch** in CI via the GitHub Action — useful for dogfooding a release pipeline before tagging.
