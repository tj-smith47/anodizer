+++
title = "Split / Merge (Distributed Builds)"
description = "Fan out cross-platform builds across parallel CI jobs and merge them into a single release"
weight = 55
template = "docs.html"
+++

Split/merge lets you build binaries for each platform on native CI runners (Linux, macOS, Windows) in parallel, then collect all the artifacts on one final job that creates the release.

This avoids slow cross-compilation and lets each job run on hardware that matches its target OS.

## How it works

1. **Matrix jobs** — each job runs `anodize release --split` on its native runner. This builds only the binaries for that job's platform and writes a `context.json` (artifacts list + git state) to a `dist/<platform>/` subdirectory.
2. **Artifact handoff** — each job uploads its `dist/<platform>/` directory as a CI artifact.
3. **Merge job** — a final job downloads all platform artifacts, restores them into `dist/`, then runs `anodize continue --merge` (or `anodize release --merge`). This loads the per-platform contexts, merges all artifacts, and runs all post-build stages: archives, checksums, signing, release upload, publishing, announcements.

## Config

```yaml
partial:
  by: goos    # "goos" (default) or "target"
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `by` | string | `goos` | How to group targets into split jobs: `goos` (one job per OS) or `target` (one job per full target triple). |

### `by: goos` (recommended)

All architecture variants for the same OS run in a single job. A project with targets `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` produces one Linux job.

### `by: target`

Each unique target triple gets its own job. Use this when you need each architecture to run on different hardware (e.g., building native arm64 on a Graviton runner).

## Target selection in split jobs

Each split job determines which targets to build using this priority chain:

1. `TARGET` environment variable — exact target triple (e.g. `x86_64-unknown-linux-gnu`).
2. `ANODIZE_OS` + optional `ANODIZE_ARCH` environment variables — filter by OS/arch.
3. Host auto-detection via `rustc -vV`, interpreted according to `partial.by`.

## CLI commands

### `anodize release --split`

Builds only the binaries for the current platform and writes output to `dist/<platform>/context.json`. No release is created; no signing or publishing happens.

```
anodize release --split
```

### `anodize release --merge`

Loads all `context.json` files from `dist/*/` subdirectories, merges the artifact lists, and runs the full post-build pipeline (archives, checksums, signing, release, publish, blob storage, announce).

```
anodize release --merge
```

### `anodize continue --merge`

Equivalent to `anodize release --merge`. Preferred for the merge job to make the intent explicit.

```
anodize continue --merge
```

### Dry run

Both `--split` and `--merge` respect `--dry-run`:

```
anodize release --split --dry-run
anodize continue --merge --dry-run
```

## Artifact handoff

Each `--split` job writes its output to:

```
dist/
  linux/          # or "darwin", "windows", or full triple if by: target
    context.json  # artifact list + git state for this platform
    myapp         # compiled binary
    ...
```

The `context.json` file contains the artifact metadata (paths, kinds, checksums) and git context (tag, commit, branch, template variables). The merge job uses this to reconstruct the artifact registry without rebuilding.

A `dist/matrix.json` file is also written (on the first `--split` run) listing the CI matrix entries with runner suggestions, though it is not required by the merge step.

## GitHub Actions example

Uses [`tj-smith47/anodize-action`](@/docs/ci/anodize-action.md) with built-in
`upload-dist` / `download-dist` to handle artifact handoff:

```yaml
name: Release

on:
  push:
    tags:
      - "v*"

jobs:
  build:
    name: Build (${{ matrix.target }})
    strategy:
      matrix:
        include:
          - target: linux
            runner: ubuntu-latest
          - target: darwin
            runner: macos-latest
          - target: windows
            runner: windows-latest
    runs-on: ${{ matrix.runner }}
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
          ANODIZE_OS: ${{ matrix.target }}
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

  release:
    name: Release (merge)
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
          args: continue --merge
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

`upload-dist: true` uploads the split job's `dist/` directory as an artifact
named `dist-$RUNNER_OS` (`dist-Linux`, `dist-macOS`, `dist-Windows`).
`download-dist: true` in the merge job downloads every `dist-*` artifact and
merges them into `dist/` in the expected layout, fails the job if no split
context files are found.

If you need to manage artifacts manually (e.g., a non-GitHub runner), upload
each split job's `dist/<platform>/` and download them into `dist/` in the
merge job — the subdirectory names must match the `dist/<platform>/` layout
written by `--split`, which depends on your `partial.by` setting.

## Merge pipeline stages

When `--merge` runs, it executes all post-build stages in order:

1. Archives
2. nFPM (Linux packages)
3. Snapcraft
4. DMG
5. MSI
6. PKG
7. Source archive
8. Changelog
9. Checksums
10. Sign
11. Release (GitHub/GitLab)
12. Publish (Homebrew, Scoop, crates.io, etc.)
13. Docker
14. Blob storage
15. Announce

Use `--skip` to skip individual stages during merge:

```
anodize continue --merge --skip docker,announce
```
