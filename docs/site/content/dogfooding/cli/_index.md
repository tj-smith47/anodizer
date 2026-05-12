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
| `release` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`args: release --split --clean`) |
| `build` | ✅ Verified | [`crates/cli/src/commands/build.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/build.rs) (subcommand handler) |
| `check` | ✅ Verified | [`crates/cli/src/commands/check.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/check.rs) |
| `init` | ✅ Verified | [`crates/cli/src/commands/init.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/init.rs) |
| `completion` | ✅ Verified | [`crates/cli/src/commands/completion.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/completion.rs) |
| `jsonschema` | ✅ Verified | [`docs.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/docs.yml) regenerates [`schema.json`](https://github.com/tj-smith47/anodizer/blob/master/docs/site/static/schema.json) via `anodizer jsonschema` |
| `healthcheck` | ✅ Verified | [`crates/cli/src/commands/healthcheck.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/healthcheck.rs) |
| `tag` | ✅ Verified | [anodizer `ci.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/ci.yml) (`args: tag` step on master pushes) |
| `targets --json` | ✅ Verified | Consumed by [anodizer-action](https://github.com/tj-smith47/anodizer-action) as a matrix input |
| `resolve-tag` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`resolve-workspace: 'true'` invokes `anodizer resolve-tag`) |
| `changelog` | ✅ Verified | [`crates/cli/src/commands/changelog.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/changelog.rs) |
| `continue` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`args: release --merge` runs the continue composite) |
| `publish` | ✅ Verified | [`crates/cli/src/commands/publish_cmd.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/publish_cmd.rs) (composite; used via `release --merge`) |
| `announce` | ✅ Verified | [`crates/cli/src/commands/announce_cmd.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/announce_cmd.rs) (composite; used via `release --merge`) |
| `man` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`before.hooks` runs `anodizer man > dist/anodizer.1`) |

## Flags

| Flag | Status | Notes |
|---|---|---|
| `--single-target` | ✅ Verified | [anodizer `ci.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/ci.yml) (`args: release --snapshot --single-target --clean --dry-run`) |
| `--split` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`args: release --split --clean` per OS) |
| `--merge` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`args: release --merge` in merge job) |
| `--crate <name>` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`args: release ... --crate ${{ needs.resolve.outputs.workspace }}`) |
| `--auto-snapshot` | ✅ Verified | [anodizer `ci.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/ci.yml) (snapshot dry-run on master) |
| `--prepare` | 🤝 Help wanted | Pro multi-stage. `release --prepare` runs build/archive/sign/checksum/sbom but skips release/publish/announce; e2e test asserts the artifact set matches an explicit `--skip=release,publish,announce`. No live release uses the prepare to publish to announce split yet |
| `--fail-fast` | 🤝 Help wanted | Inverts the publish stage's default collect-then-bail behavior to abort on the first publisher error, matching GoReleaser's `Continuable` trait. Default mode collects errors from every post-release publisher (brew/krew/nix/scoop/winget/aur/...) and reports the aggregate |
| `--nightly` | 🤝 Help wanted | Wired; no scheduled nightly workflow yet |
