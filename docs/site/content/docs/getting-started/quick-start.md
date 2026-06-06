+++
title = "Quick Start"
description = "Get your first release running in 5 minutes"
weight = 3
template = "docs.html"
+++

## 1. Install anodizer

```bash
cargo install anodizer
```

## 2. Generate a config

In your project root (where `Cargo.toml` lives):

```bash
anodizer init > .anodizer.yaml
```

This reads your `Cargo.toml` and generates a starter config with sensible defaults. It discovers all binary crates in your workspace automatically.

## 3. Validate the config

```bash
anodizer check
```

This validates your `.anodizer.yaml` against the schema — checks for missing fields, invalid target triples, dependency cycles, and more.

Anodizer rejects unknown config keys, so a typo like `dockrs_v2:` or
`fromats:` fails fast at load time instead of being silently ignored. Migrating
from GoReleaser? Paste your config in as-is — anodizer accepts the GoReleaser
field names `disable`, `docker_v2`, `layouts`, and `name_template` (snapshot) as
back-compat aliases for their canonical anodizer spellings (`skip`,
`dockers_v2`, `layout`, `version_template`). `disable` and `name_template` are
deprecation-warned at load so you can migrate at your own pace; `docker_v2` and
`layouts` are accepted silently.

## 4. Do a dry run

```bash
anodizer release --dry-run
```

This runs the full pipeline without any side effects — no GitHub release created, no packages published, no images pushed. You'll see exactly what would happen.

## 5. Do a snapshot build

```bash
anodizer release --snapshot
```

This builds everything locally but skips all publishing stages. Useful for testing your archive formats and verifying binaries compile for all targets.

## 6. Release for real

```bash
export GITHUB_TOKEN="ghp_..."
anodizer release --crate myapp
```

This runs the full pipeline: build, archive, checksum, changelog, GitHub release with asset uploads, and any configured publishers.

## What next?

- [Configuration Reference](@/docs/reference/configuration.md) — all config fields explained
- [Template Reference](@/docs/general/templates.md) — template variables and filters
- [GitHub Actions](@/docs/ci/github-actions.md) — automate releases in CI
