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
[anodizer's `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml),
[`ci.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/ci.yml),
and [cfgd's `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml)
(snapshots 2026-05-24 / 2026-07-10).

```yaml
# anodizer ci.yml — snapshot dry-run on every master push
args: release --snapshot --single-target --clean --dry-run

# anodizer release.yml — preflight job validates every publish secret
# BEFORE a tag exists (blob creds are ambient on the self-hosted runner).
args: release --preflight-secrets --skip=blob
args: preflight --publish-only --publishers blob,uploads --skip sign,verify-release

# anodizer release.yml — tag job auto-tags from commit directives and pushes
# the tag(s) with GITHUB_TOKEN; the version-sync bump commit stays coupled.
args: tag --changelog --push-tags-only

# anodizer release.yml — determinism shards preserve dist/, then the release
# job publishes the preserved dist without rebuilding.
args: release --publish-only --skip=${{ env.HOSTED_PUBLISHERS }}
args: release --publish-only --publishers ${{ env.HOSTED_PUBLISHERS }}

# cfgd release.yml — split build per workspace crate, with strict gating.
args: release --verbose --debug --strict --split --clean --crate ${{ needs.resolve.outputs.workspace }}
```

## Commands

| Command | Status | Notes |
|---|---|---|
| `release` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (`args: release --publish-only --skip=…` / `--publishers …` in the publish jobs) |
| `build` | ✅ Verified | [`crates/cli/src/commands/build.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/build.rs) (subcommand handler) |
| `check` | ✅ Verified | [`crates/cli/src/commands/check/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/check/mod.rs) |
| `init` | ✅ Verified | [`crates/cli/src/commands/init.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/init.rs) |
| `completion` | ✅ Verified | [`crates/cli/src/commands/completion.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/completion.rs) |
| `jsonschema` | ✅ Verified | [`docs.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/docs.yml) regenerates [`schema.json`](https://github.com/tj-smith47/anodizer/blob/master/docs/site/static/schema.json) via `anodizer jsonschema` |
| `healthcheck` | ✅ Verified | [`crates/cli/src/commands/healthcheck.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/healthcheck.rs) |
| `tag` | ✅ Verified | anodizer's prior releases (v0.2.0–v0.5.0) were auto-tagged from Conventional Commits; the tag is now cut by [`release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml)'s `workflow_run` tag job |
| `tag rollback` | ⏳ Pending | A failed `anodizer release` executes the same rollback path in-process via the `release.on_failure` policy (default `rollback`); the standalone command remains the manual-recovery entry point. Awaits the next release cycle that hits the failure path |
| `targets --json` | ✅ Verified | Consumed by [anodizer-action](https://github.com/tj-smith47/anodizer-action) as a matrix input |
| `resolve-tag` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`resolve-workspace: 'true'` invokes `anodizer resolve-tag`) |
| `changelog` | ✅ Verified | [`crates/cli/src/commands/changelog.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/changelog.rs) |
| `continue` | ✅ Verified | [`crates/cli/src/commands/continue_cmd.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/continue_cmd.rs) (composite; reachable via `release --merge`) |
| `publish` | ✅ Verified | [`crates/cli/src/commands/publish_cmd.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/publish_cmd.rs) (composite; runs inside `release --publish-only`) |
| `announce` | ✅ Verified | [`crates/cli/src/commands/announce_cmd.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/announce_cmd.rs) (composite; runs inside `release --publish-only`) |
| `man` | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`before.hooks` runs `anodizer man > dist/anodizer.1`) |
| `bump` | 🤝 Help wanted | [`crates/cli/src/commands/bump/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/bump/mod.rs) (bump `major`/`minor`/`patch`/`custom` — edits `Cargo.toml` + `Cargo.lock` without tagging; PR-first workflow counterpart to `tag`). No live workflow uses it yet |
| `check determinism` | ✅ Verified | [anodizer `determinism.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/determinism.yml) (reusable workflow called by `release.yml`'s `determinism-check:` job; `determinism: 'true'` per shard on a 4-shard matrix — ubuntu, macos, windows x86_64 + aarch64) |
| `check version-files` | 🤝 Help wanted | [`crates/cli/src/commands/check/version_files.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/check/version_files.rs) (lints that `version_files` entries contain the current version). No live workflow invocation yet |
| `preflight` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (`args: preflight --publish-only --publishers blob,uploads --skip sign,verify-release` on the self-hosted publish runner) — collect-all environment preflight (tools, secrets presence, endpoints, docker, key material) derived from each stage's / publisher's own `requirements` SSOT |
| `tools` | ✅ Verified (tests) | [`crates/cli/src/commands/tools.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/tools.rs) — self-reports the external CLI tools (incl. the cross toolchain) the resolved config's pipeline will invoke, from the same requirements SSOT as `preflight`; consumed by anodizer-action instead of re-deriving the config→tool mapping in shell |
| `vocabulary` | ✅ Verified (tests) | [`crates/cli/src/commands/vocabulary.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/vocabulary.rs) — emits the token vocabulary + config tool set (`--json`) for the action's input validation |
| `notify` | 🤝 Help wanted | [`crates/cli/src/commands/notify.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/notify.rs) (fires configured announce integrations with a custom message; `--only` / `--skip` filter integrations). No live invocation yet |

## Flags

| Flag | Status | Notes |
|---|---|---|
| `--single-target` | ✅ Verified | [anodizer `ci.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/ci.yml) (`args: release --snapshot --single-target --clean --dry-run`) |
| `tag --push` | ✅ Verified | Tags `v0.12.0`–`v0.12.3` were all cut by the tag job's `args: tag --push --changelog` (`gh release list`); anodizer's workflow has since moved to `tag --push-tags-only` (see below), but `--push` stays live: [brontes `ci.yml`](https://github.com/tj-smith47/brontes/blob/master/.github/workflows/ci.yml) runs `args: tag --push --crate brontes` on every master push and cut [v0.2.1](https://github.com/tj-smith47/brontes/releases/tag/v0.2.1) (bump commit + tag pushed atomically). Also covered by integration tests (bare-remote fixture asserts remote branch HEAD == tag target, no orphan). See [`crates/cli/src/commands/tag/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/tag/mod.rs) |
| `--split` | ✅ Verified | [`crates/cli/src/commands/release/split.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/release/split.rs) (cfgd's `release.yml` uses it for per-OS split build) |
| `--merge` | ✅ Verified | [`crates/cli/src/commands/release/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/release/mod.rs) (merge counterpart to `--split`) |
| `--publish-only` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (`args: release --publish-only --skip=…` — publishes the determinism shards' preserved dist without rebuilding) |
| `--crate <name>` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`args: release ... --crate ${{ needs.resolve.outputs.workspace }}`); on the `tag` command, [brontes `ci.yml`](https://github.com/tj-smith47/brontes/blob/master/.github/workflows/ci.yml) (`args: tag --push --crate brontes` — routes the single-crate version-sync path) |
| `--auto-snapshot` | ✅ Verified | [anodizer `ci.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/ci.yml) (snapshot dry-run on master) |
| `--prepare` | 🤝 Help wanted | Pro multi-stage. `release --prepare` runs build/archive/sign/checksum/sbom and skips every upstream-reaching stage (release, docker, docker-sign, blob, publish, snapcraft-publish, announce, verify-release); e2e test asserts the artifact set matches an explicit `--skip` built from `UPSTREAM_STAGES`. No live release uses the prepare to publish to announce split yet |
| `--fail-fast` | 🤝 Help wanted | Inverts the publish stage's default collect-then-bail behavior to abort on the first publisher error, matching GoReleaser's `Continuable` trait. Default mode collects errors from every post-release publisher (brew/krew/nix/scoop/winget/aur/...) and reports the aggregate |
| `--nightly` | ✅ Verified | [cfgd `nightly.yml`](https://github.com/tj-smith47/cfgd/blob/v0.4.0/.github/workflows/nightly.yml) (`args: release --nightly --no-preflight` on a `0 4 * * *` cron — publishes to all configured publishers) |
| `--preflight-secrets` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (`args: release --preflight-secrets --skip=blob` — the preflight job validates every publish secret before a tag exists, so a missing credential aborts before anything irreversible) |
| `tag --push-tags-only` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (`args: tag --changelog --push-tags-only` in the auto-tag job) |
| `--publishers` / `--skip` (publisher routing) | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) — the self-hosted publish job runs `release --publish-only --skip=<hosted set>` and the GitHub-hosted job runs `release --publish-only --publishers <hosted set>` (npm provenance needs GH-hosted OIDC), splitting one release across two runner classes |
| `tag --changelog` | ✅ Verified | [`release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml)'s tag job passes `args: tag --changelog --push-tags-only` on every auto-tag; each [release body](https://github.com/tj-smith47/anodizer/releases) carries the rendered `## Changelog` groups. See [`crates/cli/src/commands/tag/mod.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/tag/mod.rs) (renders and stages changelogs atomically with the version-sync commit) |
