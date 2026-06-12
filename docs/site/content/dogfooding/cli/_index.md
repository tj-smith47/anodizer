+++
title = "CLI"
description = "anodizer CLI commands and flags, including the Pro multi-stage release flags."
weight = 50
template = "section.html"
+++

# CLI

Commands and flags exposed by the `anodizer` binary.

## Live invocations

Representative `anodizer-action` `args:` and CLI invocations from
[anodizer's `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml),
[`ci.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/ci.yml),
and [cfgd's `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml)
(snapshot 2026-05-24).

```yaml
# anodizer ci.yml — snapshot dry-run on every master push
args: release --snapshot --single-target --clean --dry-run

# anodizer release.yml — workflow_run tag job auto-tags from commit directives;
# --push lands the version-sync bump commit on master atomically with the tag,
# pushed with GITHUB_TOKEN so it triggers no second CI run.
args: tag --push

# anodizer release.yml — determinism shard runs the build pipeline,
# preserves dist/, then the release job calls release --publish-only.
args: release --check determinism --preserve-dist
args: release --publish-only

# cfgd release.yml — split build per workspace crate, with strict gating.
args: release --verbose --debug --strict --split --clean --crate ${{ needs.resolve.outputs.workspace }}
```

## Commands

| Command | Status | Notes |
|---|---|---|
| `release` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`args: release --publish-only`) |
| `build` | ✅ Verified | [`crates/cli/src/commands/build.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/build.rs) (subcommand handler) |
| `check` | ✅ Verified | [`crates/cli/src/commands/check.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/check.rs) |
| `init` | ✅ Verified | [`crates/cli/src/commands/init.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/init.rs) |
| `completion` | ✅ Verified | [`crates/cli/src/commands/completion.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/completion.rs) |
| `jsonschema` | ✅ Verified | [`docs.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/docs.yml) regenerates [`schema.json`](https://github.com/tj-smith47/anodizer/blob/master/docs/site/static/schema.json) via `anodizer jsonschema` |
| `healthcheck` | ✅ Verified | [`crates/cli/src/commands/healthcheck.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/healthcheck.rs) |
| `tag` | ✅ Verified | anodizer's prior releases (v0.2.0–v0.5.0) were auto-tagged from Conventional Commits; the tag is now cut by [`release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml)'s `workflow_run` tag job |
| `tag rollback` | ⏳ Pending | A failed `anodizer release` executes the same rollback path in-process via the `release.on_failure` policy (default `rollback`); the standalone command remains the manual-recovery entry point. Awaits the next release cycle that hits the failure path |
| `targets --json` | ✅ Verified | Consumed by [anodizer-action](https://github.com/tj-smith47/anodizer-action) as a matrix input |
| `resolve-tag` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`resolve-workspace: 'true'` invokes `anodizer resolve-tag`) |
| `changelog` | ✅ Verified | [`crates/cli/src/commands/changelog.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/changelog.rs) |
| `continue` | ✅ Verified | [`crates/cli/src/commands/continue_cmd.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/continue_cmd.rs) (composite; reachable via `release --merge`) |
| `publish` | ✅ Verified | [`crates/cli/src/commands/publish_cmd.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/publish_cmd.rs) (composite; runs inside `release --publish-only`) |
| `announce` | ✅ Verified | [`crates/cli/src/commands/announce_cmd.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/announce_cmd.rs) (composite; runs inside `release --publish-only`) |
| `man` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`before.hooks` runs `anodizer man > dist/anodizer.1`) |
| `bump` | 🤝 Help wanted | [`crates/cli/src/commands/bump/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/bump/mod.rs) (bump `major`/`minor`/`patch`/`custom` — edits `Cargo.toml` + `Cargo.lock` without tagging; PR-first workflow counterpart to `tag`). No live workflow uses it yet |
| `check determinism` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`determinism: 'true'` per shard on the 3-OS matrix; `args: release --check determinism --preserve-dist` on prior releases) |
| `check version-files` | 🤝 Help wanted | [`crates/cli/src/commands/check/version_files.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/check/version_files.rs) (lints that `version_files` entries contain the current version). No live workflow invocation yet |
| `notify` | 🤝 Help wanted | [`crates/cli/src/commands/notify.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/notify.rs) (fires configured announce integrations with a custom message; `--only` / `--skip` filter integrations). No live invocation yet |

## Flags

| Flag | Status | Notes |
|---|---|---|
| `--single-target` | ✅ Verified | [anodizer `ci.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/ci.yml) (`args: release --snapshot --single-target --clean --dry-run`) |
| `tag --push` | ⏳ Pending | Wired in [`release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml)'s `workflow_run` tag job (`args: tag --push`, pushed via `GITHUB_TOKEN`). Covered by integration tests (bare-remote fixture asserts remote branch HEAD == tag target, no orphan). Awaits the first release off `master` for live proof |
| `--split` | ✅ Verified | [`crates/cli/src/commands/release/split.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/release/split.rs) (cfgd's `release.yml` uses it for per-OS split build) |
| `--merge` | ✅ Verified | [`crates/cli/src/commands/release/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/release/mod.rs) (merge counterpart to `--split`) |
| `--publish-only` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`args: release --publish-only`) |
| `--crate <name>` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`args: release ... --crate ${{ needs.resolve.outputs.workspace }}`) |
| `--auto-snapshot` | ✅ Verified | [anodizer `ci.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/ci.yml) (snapshot dry-run on master) |
| `--prepare` | 🤝 Help wanted | Pro multi-stage. `release --prepare` runs build/archive/sign/checksum/sbom but skips release/publish/announce; e2e test asserts the artifact set matches an explicit `--skip=release,publish,announce`. No live release uses the prepare to publish to announce split yet |
| `--fail-fast` | 🤝 Help wanted | Inverts the publish stage's default collect-then-bail behavior to abort on the first publisher error, matching GoReleaser's `Continuable` trait. Default mode collects errors from every post-release publisher (brew/krew/nix/scoop/winget/aur/...) and reports the aggregate |
| `--nightly` | ✅ Verified | [cfgd `nightly.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/nightly.yml) (`args: release --nightly --no-preflight` on a `0 4 * * *` cron — publishes to all configured publishers from `publisher-required-config` branch) |
| `tag --changelog` | 🤝 Help wanted | [`crates/cli/src/commands/tag/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/tag/mod.rs) (opt-in flag; when set alongside a `changelog:` config block, renders and stages changelogs atomically with the version-sync commit). No live workflow passes this flag yet |
