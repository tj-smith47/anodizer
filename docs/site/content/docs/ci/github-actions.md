+++
title = "GitHub Actions"
description = "Automate releases with GitHub Actions"
weight = 1
template = "docs.html"
+++

The [`tj-smith47/anodizer-action`](https://github.com/tj-smith47/anodizer-action) composite action is the recommended way to run anodizer in GitHub Actions. It installs anodizer (cached per version), parses `.anodizer.yaml` to auto-install pipeline dependencies (nfpm, cosign, zig, cargo-zigbuild, upx, snapcraft, rpmbuild, ...), imports signing keys, logs in to container registries, and handles split/merge artifact plumbing — all in one step.

For the complete list of inputs and outputs, see [anodizer-action reference](@/docs/ci/anodizer-action.md).

## Basic release

```yaml
name: Release

on:
  push:
    tags: ["v*"]

permissions:
  contents: write
  actions: read    # required for the cross-workflow artifact download path

jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0    # full history for changelog generation

      - uses: tj-smith47/anodizer-action@v1
        with:
          auto-install: true
          args: release --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

That one step is the whole release job. Before any stage runs, anodizer verifies the environment with a config-derived [preflight](@/docs/general/preflight.md) (required tools, secrets, key material, endpoints), and on a pipeline failure it executes the [`release.on_failure` policy](@/docs/advanced/release-resilience.md#release-on-failure-the-in-process-failure-policy) in-process — rolling back the tag and version bump by default, auto-degrading to `hold` once any one-way-door publisher (crates.io, chocolatey, ...) has landed. Failure policy lives in `.anodizer.yaml`, not in workflow steps:

```yaml
# .anodizer.yaml
release:
  on_failure: rollback   # rollback | hold; default rollback
```

`auto-install: true` reads `.anodizer.yaml` and installs whatever the configured stages need. To pin dependencies explicitly, replace it with `install: nfpm,cosign,zig,...`.

## With signing keys

```yaml
- uses: tj-smith47/anodizer-action@v1
  with:
    auto-install: true
    gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
    cosign-key: ${{ secrets.COSIGN_KEY }}
    args: release --clean
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
    GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
    COSIGN_PASSWORD: ${{ secrets.COSIGN_PASSWORD }}
```

The preflight parses every configured key (GPG, cosign, SSH) before any stage runs, so a truncated or newline-mangled CI secret aborts the release instead of failing a publisher halfway through.

## Auto-tag on push to main

Run `anodizer tag` on every push to the default branch. Use a PAT (not `GITHUB_TOKEN`) so the pushed tag triggers downstream tag-scoped workflows like `release.yml`:

```yaml
name: CI

on:
  push:
    branches: [main]

jobs:
  tag:
    # Skip when the commit message contains #none
    if: "!contains(github.event.head_commit.message, '#none')"
    runs-on: ubuntu-latest
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          # PAT (not GITHUB_TOKEN) so the tag push triggers release.yml.
          token: ${{ secrets.GH_PAT }}

      - name: Configure git identity
        run: |
          git config user.name  "github-actions[bot]"
          git config user.email "github-actions[bot]@users.noreply.github.com"

      - uses: tj-smith47/anodizer-action@v1
        with:
          args: tag
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
```

The tag command reads commit messages for `#major` / `#minor` / `#patch` / `#none` directives, finds the latest semver tag for the crate, bumps accordingly, and pushes the new tag. See [Auto-Tagging](@/docs/advanced/auto-tagging.md) for details.

## Workspace-aware auto-tag (monorepo)

For multi-crate workspaces, tag each crate independently so each gets its own `release.yml` run. `install-only: true` installs the binary to `PATH` without running anodizer — you drive the loop yourself:

```yaml
      - uses: tj-smith47/anodizer-action@v1
        with:
          install-only: true

      - name: Auto-tag all workspaces
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: |
          for crate in my-core my-cli my-operator; do
            echo "--- tagging $crate ---"
            if anodizer tag --crate "$crate"; then
              echo "::notice::$crate: tagged"
            else
              echo "::warning::$crate: skipped or failed"
            fi
          done
          # Push any version_sync commits created by the tag step.
          git push origin HEAD || true
```

Each crate uses its own `tag_template` (e.g., `my-core-v{{ Version }}`) for both lookup and creation, so tags never collide across workspaces.

## Tag-triggered monorepo release

When a tag lands, resolve it to its owning crate and release only that crate. `resolve-workspace: true` populates the `workspace`, `crate-path`, and `has-builds` outputs from the triggering tag:

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
  resolve:
    runs-on: ubuntu-latest
    outputs:
      crate: ${{ steps.a.outputs.workspace }}
      has-builds: ${{ steps.a.outputs.has-builds }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        id: a
        with:
          resolve-workspace: true
          install-only: true

  release:
    needs: resolve
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        with:
          auto-install: true
          docker-registry: ghcr.io
          docker-password: ${{ secrets.GITHUB_TOKEN }}
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: release --crate ${{ needs.resolve.outputs.crate }} --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
```

## Reuse a CI-built anodizer binary across workflows

When you have a separate `ci.yml` that builds and uploads an anodizer binary per commit, downstream release jobs can reuse it instead of reinstalling. Set `artifact-run-id: auto` to resolve the run from the current commit SHA:

```yaml
# ci.yml — builds and uploads anodizer once per commit
- uses: actions/upload-artifact@v4
  with:
    name: anodizer-linux
    path: target/release/anodizer

# release.yml — reuses the ci.yml artifact
- uses: tj-smith47/anodizer-action@v1
  with:
    from-artifact: anodizer-linux
    artifact-run-id: auto
    artifact-workflow: ci.yml
    auto-install: true
    args: release --clean
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

This avoids a cold `cargo install` on every release run.

## Manual install (no action)

If you can't use the action (e.g., a self-hosted environment that can't pull from the Marketplace), install anodizer directly. You'll need to handle dependencies and key imports yourself.

```yaml
- uses: dtolnay/rust-toolchain@stable
- uses: Swatinem/rust-cache@v2
- run: cargo install anodizer
- run: anodizer release --clean
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

## See also

- [Release Workflow Strategies](@/docs/ci/release-workflows.md) — decision tree + canonical YAML for single-crate, lockstep workspace, per-crate workspace, and split-CI governance shapes
- [anodizer-action reference](@/docs/ci/anodizer-action.md) — every input and output
- [Split/Merge](@/docs/advanced/split-merge.md) — fan-out cross-platform builds
- [Auto-Tagging](@/docs/advanced/auto-tagging.md) — commit-message-driven version bumps
- [Standalone pipeline commands](@/docs/ci/split-merge-ci.md) — separate `anodizer publish` and `anodizer announce` jobs
