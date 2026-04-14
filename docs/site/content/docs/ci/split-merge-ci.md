+++
title = "Standalone Pipeline Commands"
description = "Run publish and announce as independent CI jobs with anodize publish and anodize announce"
weight = 32
template = "docs.html"
+++

Anodize provides three commands — `anodize publish`, `anodize announce`, and
`anodize continue --merge` — that let you break the release pipeline into
separate CI jobs. This gives you finer control over retries, secrets access,
and job dependencies.

## Commands

### `anodize publish`

Runs the publish stages (GitHub Release creation, package registry publishing,
blob storage upload) against a `dist/` directory that already contains built
artifacts. It does **not** rebuild binaries.

Use this command when:

- You want to publish without re-running the build.
- Publish requires secrets (e.g. a crates.io API token) that you prefer to
  keep in a separate job from the build.
- A publish job failed and you need to re-run only that job.

**Flags:**

| Flag | Description |
|------|-------------|
| `--dry-run` | Run all stages without side effects |
| `--token` | GitHub token (overrides `GITHUB_TOKEN`) |
| `--dist` | Custom dist directory (overrides config) |

Global flags like `--config` / `-f` and `--verbose` also apply.

### `anodize announce`

Runs only the announce stage against a `dist/` directory. All configured
announcement providers (Slack, Discord, Twitter/X, Mastodon, etc.) are invoked.

Use this command when:

- Announcements should run after publish completes and succeeds.
- You want to gate announcements on a manual approval step.
- Announcement secrets are stored separately from build/publish secrets.

**Flags:**

| Flag | Description |
|------|-------------|
| `--dry-run` | Run without sending any announcements |
| `--token` | GitHub token (overrides `GITHUB_TOKEN`) |
| `--dist` | Custom dist directory (overrides config) |
| `--skip` | Comma-separated list of providers to skip (e.g. `slack,twitter`) |

### `anodize continue --merge`

Merges artifacts produced by parallel split-build jobs and runs all
post-build stages (archive, sign, changelog, release, publish, announce, etc.)
in a single job. See the advanced Split & Merge guide for full details on
setting up a fan-out build.

## Example: four-job GitHub Actions workflow

This workflow separates build, merge, publish, and announce into four jobs
so each can carry its own secrets and retry independently. It uses
[`tj-smith47/anodize-action`](@/docs/ci/anodize-action.md), whose built-in
`upload-dist` / `download-dist` inputs replace the manual
upload-artifact/download-artifact plumbing.

```yaml
name: Release

on:
  push:
    tags: ["v*"]

permissions:
  contents: write

jobs:
  # Job 1: build binaries on each platform in parallel
  build:
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - uses: tj-smith47/anodize-action@v1
        with:
          install-rust: true
          install: zig,cargo-zigbuild
          upload-dist: true           # uploads dist/ as dist-$RUNNER_OS
          args: release --split --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

  # Job 2: merge artifacts and run post-build stages (everything except publish/announce)
  merge:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - uses: tj-smith47/anodize-action@v1
        with:
          auto-install: true
          download-dist: true         # downloads + merges dist-* artifacts
          upload-dist: true           # re-uploads merged dist for downstream jobs
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: continue --merge --skip publish,announce
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}

  # Job 3: publish releases and packages
  publish:
    needs: merge
    runs-on: ubuntu-latest
    environment: production           # optional: require approval before publishing
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - uses: tj-smith47/anodize-action@v1
        with:
          download-dist: true
          args: publish
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          CARGO_REGISTRY_TOKEN: ${{ secrets.CARGO_REGISTRY_TOKEN }}

  # Job 4: send announcements
  announce:
    needs: publish
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - uses: tj-smith47/anodize-action@v1
        with:
          download-dist: true
          args: announce
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          SLACK_WEBHOOK: ${{ secrets.SLACK_WEBHOOK }}
          TWITTER_CONSUMER_KEY: ${{ secrets.TWITTER_CONSUMER_KEY }}
          TWITTER_CONSUMER_SECRET: ${{ secrets.TWITTER_CONSUMER_SECRET }}
          TWITTER_ACCESS_TOKEN: ${{ secrets.TWITTER_ACCESS_TOKEN }}
          TWITTER_ACCESS_TOKEN_SECRET: ${{ secrets.TWITTER_ACCESS_TOKEN_SECRET }}
          MASTODON_CLIENT_ID: ${{ secrets.MASTODON_CLIENT_ID }}
          MASTODON_CLIENT_SECRET: ${{ secrets.MASTODON_CLIENT_SECRET }}
          MASTODON_ACCESS_TOKEN: ${{ secrets.MASTODON_ACCESS_TOKEN }}
```

## Simpler two-job split

If you only need to separate publish from announce (without fan-out builds),
you can skip the split/merge jobs:

```yaml
jobs:
  publish:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodize-action@v1
        with:
          install-rust: true
          auto-install: true
          install: cargo-zigbuild
          upload-dist: true
          args: release --skip announce
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

  announce:
    needs: publish
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodize-action@v1
        with:
          download-dist: true
          args: announce
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          SLACK_WEBHOOK: ${{ secrets.SLACK_WEBHOOK }}
```

## Dry-run testing

Both `anodize publish` and `anodize announce` support `--dry-run`. Use it in
pull request workflows to verify configuration without sending real requests:

```yaml
- run: anodize announce --dry-run
```
