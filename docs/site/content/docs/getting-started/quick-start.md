+++
title = "Quick Start"
description = "Get your first release running in 5 minutes"
weight = 3
template = "docs.html"
+++

## 1. Install anodize

```bash
cargo install anodize
```

## 2. Generate a config

In your project root (where `Cargo.toml` lives):

```bash
anodize init > .anodize.yaml
```

This reads your `Cargo.toml` and generates a starter config with sensible defaults. It discovers all binary crates in your workspace automatically.

## 3. Validate the config

```bash
anodize check
```

This validates your `.anodize.yaml` against the schema — checks for missing fields, invalid target triples, dependency cycles, and more.

## 4. Do a dry run

```bash
anodize release --dry-run
```

This runs the full pipeline without any side effects — no GitHub release created, no packages published, no images pushed. You'll see exactly what would happen.

## 5. Do a snapshot build

```bash
anodize release --snapshot
```

This builds everything locally but skips all publishing stages. Useful for testing your archive formats and verifying binaries compile for all targets.

## 6. Release for real

```bash
export GITHUB_TOKEN="ghp_..."
anodize release --crate myapp
```

This runs the full pipeline: build, archive, checksum, changelog, GitHub release with asset uploads, and any configured publishers.

## What next?

- [Configuration Reference](@/docs/reference/configuration.md) — all config fields explained
- [Template Reference](@/docs/general/templates.md) — template variables and filters
- [GitHub Actions](@/docs/ci/github-actions.md) — automate releases in CI
