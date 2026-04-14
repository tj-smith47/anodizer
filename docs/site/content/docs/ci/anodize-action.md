+++
title = "anodize-action (GitHub Action)"
description = "Complete reference for tj-smith47/anodize-action — inputs, outputs, and common patterns"
weight = 2
template = "docs.html"
+++

[`tj-smith47/anodize-action`](https://github.com/tj-smith47/anodize-action) is the recommended way to run anodize in GitHub Actions. It handles installation, dependency setup, key material, split/merge artifact handling, Docker registry login, and workspace resolution — removing most of the GitHub-Actions-specific plumbing from your workflow.

## Basic usage

```yaml
- uses: tj-smith47/anodize-action@v1
  with:
    auto-install: true
    args: release --clean
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

## Inputs

### Installation source

Pick exactly one installation path. Defaults to downloading the latest released anodize binary.

| Input | Default | Description |
|-------|---------|-------------|
| `version` | `latest` | Anodize version to install from GitHub releases (e.g. `v0.1.1`, `latest`). Ignored when `from-artifact` or `from-source` is set. |
| `from-artifact` | | Artifact name to download instead of a release binary (e.g. `anodize-linux`). Pair with `artifact-run-id` for cross-workflow downloads. |
| `artifact-run-id` | | Workflow run ID for the artifact. Use `auto` to automatically resolve the latest successful run of `artifact-workflow` for the current commit. Use a numeric ID for explicit control. Omit to download from the current workflow run. |
| `artifact-workflow` | `ci.yml` | Workflow filename to search when `artifact-run-id` is `auto`. Ignored otherwise. |
| `from-source` | `false` | Build anodize from source in the current workdir (bootstrap mode). Requires a Rust toolchain — combine with `install-rust: true` if needed. Useful when `from-artifact` covers one platform and the current runner needs a platform-native binary. |

### Dependency setup

Anodize shells out to external tools for several stages (nfpm, cosign, zig, etc.). The action can install them for you.

| Input | Default | Description |
|-------|---------|-------------|
| `install` | | Comma-separated list of dependencies to install: `nfpm`, `makeself`, `snapcraft`, `rpmbuild`, `cosign`, `zig`, `cargo-zigbuild`, `upx`. Uses the platform's native package manager (apt on Linux, brew on macOS, choco on Windows). |
| `auto-install` | `false` | Parse `.anodize.yaml` in the workdir and install whatever the configured stages need. Inspects top-level keys like `nfpms:`, `makeselfs:`, `snapcrafts:`, `srpm:`, `binary_signs:`, `docker_signs:`, `upx:`, and `cross: auto\|zigbuild` to derive the dependency list. |
| `install-rust` | `false` | Install the stable Rust toolchain via `dtolnay/rust-toolchain@stable`. Prerequisite for `from-source: true` and for cross-compilation stages that invoke `cargo` or `cargo-zigbuild`. |

### Key material

Signing keys are passed via inputs (never env vars that echo into logs).

| Input | Default | Description |
|-------|---------|-------------|
| `gpg-private-key` | | GPG private key contents. Imported via `gpg --batch --import`. Pair with `GPG_FINGERPRINT` in the env to tell anodize which key to sign with. |
| `cosign-key` | | Cosign private key contents. Written to `cosign.key` in the workdir with mode `0600`. Pair with `COSIGN_PASSWORD` in the env. |

### Docker setup

When the action sees `docker-registry` set, it logs in, sets up QEMU (for emulated platforms), and configures Docker Buildx (for multi-platform builds).

| Input | Default | Description |
|-------|---------|-------------|
| `docker-registry` | | Container registry hostname (e.g. `ghcr.io`, `docker.io`). |
| `docker-username` | `github.actor` | Registry username. Defaults to the GitHub user that triggered the workflow. |
| `docker-password` | | Registry password or token (commonly `secrets.GITHUB_TOKEN` for ghcr.io). |

### Split / merge artifact management

For fan-out cross-platform builds ([Split/Merge](@/docs/advanced/split-merge.md)), the action can upload the `dist/` directory after a split build and download+merge dist artifacts before a merge job.

| Input | Default | Description |
|-------|---------|-------------|
| `upload-dist` | `false` | After running anodize, upload `dist/` as a workflow artifact named `dist-$RUNNER_OS`. Set to `true` in split build jobs. |
| `download-dist` | `false` | Before running anodize, download all artifacts matching `dist-*` and merge them into `dist/`. Set to `true` in merge jobs. Fails if no split context files are found. |

### Workspace resolution (monorepo)

When a tag-triggered workflow runs, the action can resolve the triggering tag to its owning crate so subsequent steps can use crate-scoped paths.

| Input | Default | Description |
|-------|---------|-------------|
| `resolve-workspace` | `false` | Run `anodize resolve-tag $GITHUB_REF_NAME` and expose the result as the `workspace`, `crate-path`, and `has-builds` outputs. Fails the workflow if no crate matches the tag. |

### Execution

| Input | Default | Description |
|-------|---------|-------------|
| `args` | | Arguments to pass to anodize (e.g. `release --snapshot`, `tag --crate my-lib`). Omit with `install-only: true` to install the binary without running it. |
| `workdir` | `.` | Working directory relative to the repository root. Use when your `.anodize.yaml` is not at the root. |
| `install-only` | `false` | Only install anodize (and any requested dependencies/keys). Skip the `Run anodize` step. Useful when you want to invoke anodize yourself in a subsequent step — e.g., a loop over multiple crates. |

## Outputs

| Output | Description |
|--------|-------------|
| `artifacts` | Contents of `dist/artifacts.json` (the full artifact inventory). Multi-line string. |
| `metadata` | Contents of `dist/metadata.json` (release metadata: tag, project name, release URL, etc.). Multi-line string. |
| `release-url` | URL of the created GitHub release, extracted from `metadata.json`. Empty if no release was created. |
| `workspace` | Crate name resolved from the triggering tag. Set only when `resolve-workspace: true`. |
| `crate-path` | Path to the resolved crate directory (e.g. `crates/my-lib`). Set only when `resolve-workspace: true`. |
| `has-builds` | `true` or `false` — whether the resolved crate has binary builds configured. Useful for conditionally skipping archive/docker stages for library-only crates. Set only when `resolve-workspace: true`. |
| `split-matrix` | JSON matrix for `strategy.matrix` covering all configured build targets, derived from `.anodize.yaml` via `anodize targets --json`. Each entry has `os`, `target`, and `artifact` fields. Set only when `install-only: true`. |

## Common patterns

### Simple tag-triggered release

```yaml
name: Release
on:
  push:
    tags: ["v*"]

permissions:
  contents: write

jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodize-action@v1
        with:
          auto-install: true
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: release --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
```

### Auto-tag on push to main

```yaml
name: CI
on:
  push:
    branches: [main]

jobs:
  tag:
    if: "!contains(github.event.head_commit.message, '#none')"
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
          # PAT so the pushed tag triggers downstream release.yml.
          token: ${{ secrets.GH_PAT }}
      - name: Configure git identity
        run: |
          git config user.name  "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"
      - uses: tj-smith47/anodize-action@v1
        with:
          args: tag
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
```

### Workspace-aware auto-tag (monorepo)

Tag each crate in the workspace independently. `install-only: true` gives you the binary on PATH; you drive the loop yourself.

```yaml
      - uses: tj-smith47/anodize-action@v1
        with:
          install-only: true
      - name: Auto-tag all workspaces
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: |
          for crate in my-core my-cli my-operator my-plugin; do
            echo "--- tagging $crate ---"
            if anodize tag --crate "$crate"; then
              echo "::notice::$crate: tagged"
            else
              echo "::warning::$crate: skipped or failed"
            fi
          done
          git push origin HEAD || true
```

### Tag-triggered monorepo release (resolve tag → crate)

```yaml
name: Release
on:
  push:
    tags:
      - "v*"
      - "*-v*"

permissions:
  contents: write
  packages: write

jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodize-action@v1
        id: anodize
        with:
          auto-install: true
          resolve-workspace: true
          docker-registry: ghcr.io
          docker-password: ${{ secrets.GITHUB_TOKEN }}
          args: release --crate ${{ steps.resolve.outputs.crate }} --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
      - if: steps.anodize.outputs.has-builds == 'false'
        run: echo "Library-only crate — no binary artifacts produced."
```

Note: `resolve-workspace: true` runs `anodize resolve-tag` and exposes the crate name via `steps.<id>.outputs.workspace`. You'll typically wire that into the `--crate` arg of `args`.

### Split/merge cross-platform build

The action's built-in `upload-dist` / `download-dist` replaces the manual `actions/upload-artifact` + `actions/download-artifact` pair for split builds.

```yaml
jobs:
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
          install: zig,cargo-zigbuild,upx
          upload-dist: true        # uploads dist/ as dist-$RUNNER_OS
          args: release --split --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

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
          download-dist: true      # downloads and merges dist-* artifacts
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: release --merge
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
          COSIGN_PASSWORD: ${{ secrets.COSIGN_PASSWORD }}
```

### Reuse a CI-built binary across workflows

When your `ci.yml` builds and uploads the anodize binary once per commit, downstream release workflows can reuse it instead of reinstalling.

```yaml
# ci.yml
- uses: actions/checkout@v4
- uses: dtolnay/rust-toolchain@stable
- run: cargo build --release -p anodize
- uses: actions/upload-artifact@v4
  with:
    name: anodize-linux
    path: target/release/anodize

# release.yml — reuses the ci.yml artifact
- uses: tj-smith47/anodize-action@v1
  with:
    from-artifact: anodize-linux
    artifact-run-id: auto
    artifact-workflow: ci.yml
    auto-install: true
    args: release --clean
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

Pinning `artifact-run-id: auto` resolves the latest successful run of `ci.yml` for the current commit SHA, so the release workflow always picks up a binary built from the same source.

### Dynamic build matrix from config

`install-only: true` produces a `split-matrix` output derived from `.anodize.yaml`. You can feed that directly into a `strategy.matrix`.

```yaml
jobs:
  setup:
    runs-on: ubuntu-latest
    outputs:
      matrix: ${{ steps.setup.outputs.split-matrix }}
    steps:
      - uses: actions/checkout@v4
      - uses: tj-smith47/anodize-action@v1
        id: setup
        with:
          install-only: true

  build:
    needs: setup
    strategy:
      matrix: ${{ fromJson(needs.setup.outputs.matrix) }}
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodize-action@v1
        with:
          install-rust: true
          install: zig,cargo-zigbuild
          upload-dist: true
          args: release --split --clean
        env:
          TARGET: ${{ matrix.target }}
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

### Install only, run anodize yourself

```yaml
- uses: tj-smith47/anodize-action@v1
  with:
    install-only: true
- run: anodize check
- run: anodize release --snapshot
- run: anodize resolve-tag v1.2.3 --json
```

## Env vars the action honors

| Variable | Purpose |
|----------|---------|
| `GITHUB_TOKEN` | Required for release uploads, artifact downloads, and the tag resolver's `gh api` calls. |
| `GPG_FINGERPRINT` | The key ID anodize uses when signing artifacts. Required when `gpg-private-key` is set and the config references multiple keys. |
| `COSIGN_PASSWORD` | Password for the cosign key written via `cosign-key`. |
| `CARGO_REGISTRY_TOKEN` | Required by the crates.io publisher. |
| `TARGET`, `ANODIZE_OS`, `ANODIZE_ARCH` | Split job target selection (see [Split/Merge](@/docs/advanced/split-merge.md)). |

## Retry behavior

The `Run anodize` step retries up to 3 times for transient failures (registry rate limits, Docker push auth expiry, network blips). Between retries it prunes generated artifacts from `dist/` while preserving split context files (`dist/*/context.json`) so `--merge` can still find them. Deterministic failures (config errors, compile failures) will fail identically on every attempt, but the cost of two extra 10s waits is low relative to a flaky release run.

To disable retries, wrap `anodize` manually with `install-only: true`.
