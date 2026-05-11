+++
title = "CLI"
description = "anodizer CLI commands and flags, including the Pro multi-stage release flags."
weight = 50
template = "section.html"
+++

# CLI

Commands and flags exposed by the `anodizer` binary.

## Commands

| Command | Status | Notes |
|---|---|---|
| `release` | ✅ Verified | Used in every release pipeline |
| `build` | ✅ Verified | Used in every release pipeline |
| `check` | ✅ Verified | Used in every release pipeline |
| `init` | ✅ Verified | Used in every release pipeline |
| `completion` | ✅ Verified | Used in every release pipeline |
| `jsonschema` | ✅ Verified | Used in every release pipeline |
| `healthcheck` | ✅ Verified | Used in every release pipeline |
| `tag` | ✅ Verified | anodizer's CI auto-creates `v*` tags from master via conventional commits |
| `targets --json` | ✅ Verified | Consumed by [anodizer-action](https://github.com/tj-smith47/anodizer-action) as a matrix input |
| `resolve-tag` | ✅ Verified | cfgd uses on every tag push (tag to workspace mapping) |
| `changelog` | ✅ Verified | Preview command, wired |
| `continue` | ✅ Verified | Composite Pro command. Used via `release --merge` in cfgd |
| `publish` | ✅ Verified | Composite Pro command. Used via `release --merge` in cfgd |
| `announce` | ✅ Verified | Composite Pro command. Used via `release --merge` in cfgd |
| `man` | 🤝 Help wanted | clap_mangen man-page generation. `anodizer man` emits roff for the full CLI tree; smoke test asserts `.TH anodizer` + a known subcommand. No live release ships `anodizer.1` yet |

## Flags

| Flag | Status | Notes |
|---|---|---|
| `--single-target` | ✅ Verified | Snapshot job on every master push |
| `--split` | ✅ Verified | Per-OS worker. Three split jobs per anodizer release |
| `--merge` | ✅ Verified | cfgd's release workflow merges per-OS dist directories |
| `--crate <name>` | ✅ Verified | cfgd's release workflow filters per workspace crate |
| `--auto-snapshot` | ✅ Verified | Snapshot job on every master push |
| `--prepare` | 🤝 Help wanted | Pro multi-stage. `release --prepare` runs build/archive/sign/checksum/sbom but skips release/publish/announce; e2e test asserts the artifact set matches an explicit `--skip=release,publish,announce`. No live release uses the prepare to publish to announce split yet |
| `--fail-fast` | 🤝 Help wanted | Inverts the publish stage's default collect-then-bail behavior to abort on the first publisher error, matching GoReleaser's `Continuable` trait. Default mode collects errors from every post-release publisher (brew/krew/nix/scoop/winget/aur/...) and reports the aggregate |
| `--nightly` | 🤝 Help wanted | Wired; no scheduled nightly workflow yet |
