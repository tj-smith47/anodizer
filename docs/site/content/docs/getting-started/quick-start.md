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

This reads your `Cargo.toml` and generates a starter config with sensible defaults. `init` discovers your `project_name`, every `[[bin]]` target across the workspace, the GitHub `owner`/`name` from your `origin` remote, and a default archive `name_template` — so the generated config builds and releases without further edits. Anything it can derive is left out of the file as an implicit default; you only write config to *override* a default or to add a publisher (a tap, a registry) it can't infer.

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

This runs the full pipeline without any side effects — no GitHub release created, no packages published, no images pushed. `--dry-run` plans and **renders** every stage (templates, manifest contents, the publisher PRs it *would* open) but skips the real build and every network write, so it's fast and offline. Use it to confirm your config resolves before spending a real build.

## 5. Do a snapshot build

```bash
anodizer release --snapshot
```

Unlike `--dry-run`, a snapshot performs the **real** build, archive, checksum, and signing stages — it just skips every stage that writes to a remote (GitHub release, crates.io, taps, registries, images). It also derives a snapshot version (the default appends `-SNAPSHOT`; the template can fold in the commit hash) instead of requiring a release tag, so you can snapshot an untagged working tree. Use it to test that your binaries compile for every target and that archive formats and checksums come out right. Snapshot additionally cross-checks each publisher's *would-be* emission (a binstall URL, a Nix asset map) against the assets the build actually produced, catching a class of "release succeeds but is silently wrong" bugs locally.

## 6. Release for real

```bash
export GITHUB_TOKEN="ghp_..."
anodizer release --crate myapp
```

This runs the full pipeline: build, archive, checksum, changelog, GitHub release with asset uploads, and any configured publishers. Each publisher reads its own credential from the environment — `GITHUB_TOKEN` (needs `contents:write` for the release, plus `pull_request:write` if a Homebrew cask or other PR-based tap is configured), `CARGO_REGISTRY_TOKEN` for crates.io, and so on. A `homebrew_casks` entry opens (or updates) a pull request against its tap repo on every release rather than committing directly, so review and merge happen on your terms.

> **Token scopes are checked in preflight.** anodizer surfaces a missing or under-scoped token *before* it starts publishing, so you find out at the top of the run, not halfway through. Run with `--strict` to turn those warnings into hard failures in CI.

## What next?

- [Configuration Reference](@/docs/reference/configuration.md) — all config fields explained
- [Template Reference](@/docs/general/templates.md) — template variables and filters
- [GitHub Actions](@/docs/ci/github-actions.md) — automate releases in CI
