+++
title = "anodizer-action (GitHub Action)"
description = "Complete reference for tj-smith47/anodizer-action — inputs, outputs, and common patterns"
weight = 2
template = "docs.html"
+++

[`tj-smith47/anodizer-action`](https://github.com/tj-smith47/anodizer-action) is the recommended way to run anodizer in GitHub Actions. It handles installation, dependency setup, key material, split/merge artifact handling, Docker registry login, and workspace resolution — removing most of the GitHub-Actions-specific plumbing from your workflow.

## Basic usage

```yaml
- uses: tj-smith47/anodizer-action@v1
  with:
    auto-install: true
    args: release --clean
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

## Inputs

### Installation source

Pick exactly one installation path. Defaults to downloading the latest released anodizer binary.

| Input | Default | Description |
|-------|---------|-------------|
| `version` | `latest` | Anodizer version to install from GitHub releases (e.g. `v0.1.1`, `latest`). Ignored when `from-artifact`, `from-source`, or `from-branch` is set. |
| `from-artifact` | | Artifact name to download instead of a release binary (e.g. `anodizer-linux`). Pair with `artifact-run-id` for cross-workflow downloads. |
| `artifact-run-id` | | Workflow run ID for the artifact. Use `auto` to automatically resolve the latest successful run of `artifact-workflow` for the current commit. Use a numeric ID for explicit control. Omit to download from the current workflow run. |
| `artifact-workflow` | `ci.yml` | Workflow filename to search when `artifact-run-id` is `auto`. Ignored otherwise. |
| `from-source` | `false` | Build anodizer from source in the current workdir (bootstrap mode). Requires a Rust toolchain — combine with `install-rust: true` if needed. Useful when `from-artifact` covers one platform and the current runner needs a platform-native binary. |
| `from-branch` | | Shallow-clone `tj-smith47/anodizer` at the given branch (e.g. `my-feature`) and build it from source. Accepts a **branch name only** — the repo is hardcoded; there is no `from-branch-repo` or `@ref` syntax. Auto-installs the stable Rust toolchain (no need for `install-rust: true`). Clones to `${RUNNER_TEMP}/anodizer-src`; your `workdir:` is untouched. Cargo's `target/` is cached per branch. Mutually exclusive with `version`, `from-artifact`, and `from-source`. |

### Dependency setup

Anodizer shells out to external tools for several stages (nfpm, cosign, zig, etc.). The action can install them for you.

| Input | Default | Description |
|-------|---------|-------------|
| `install` | | Comma-separated list of dependencies to install: `nfpm`, `makeself`, `snapcraft`, `rpmbuild`, `cosign`, `syft`, `zig`, `node`, `cargo-zigbuild`, `upx`, `nsis`, `create-dmg`, `flatpak`, `alejandra`, `linuxdeploy`, `rcodesign`, `wix`, `pkgbuild` (18 total). Uses the platform's native package manager (apt on Linux, brew on macOS, choco on Windows); some deps fall back to direct downloads when no packaged version exists. |
| `auto-install` | `false` | Parse `.anodizer.yaml` in the workdir and install whatever the configured stages need. Inspects top-level keys like `nfpms:`, `makeselfs:`, `snapcrafts:`, `srpm:`, `binary_signs:`, `docker_signs:`, `upx:`, and `cross: auto\|zigbuild` to derive the dependency list. |
| `install-rust` | `false` | Install the stable Rust toolchain via `dtolnay/rust-toolchain@stable`. Prerequisite for `from-source: true` and for cross-compilation stages that invoke `cargo` or `cargo-zigbuild`. |

### Key material

Signing keys are passed via inputs (never env vars that echo into logs).

| Input | Default | Description |
|-------|---------|-------------|
| `gpg-private-key` | | GPG private key contents. Imported via `gpg --batch --import`. Pair with `GPG_FINGERPRINT` in the env to tell anodizer which key to sign with. |
| `apk-private-key` | `""` | PEM-format RSA private key for signing apk packages produced by nfpm. Required when `.anodizer.yaml` configures `nfpm[].apk.signature` (apk uses RSA-PSS, not OpenPGP — the `gpg-private-key` does NOT work here). The action writes the key to a temp file with mode `0600`, exports the path as `APK_PRIVATE_KEY_PATH`, derives the public key, and copies it into `./dist/` as `<repo>-apk-signing-key.rsa.pub` so consumers can attach it via `release.extra_files`. apk verifiers install that pubkey under `/etc/apk/keys/` before `apk add`-ing a signed package. |
| `cosign-key` | | Cosign private key contents. Written to `cosign.key` in the workdir with mode `0600`. Pair with `COSIGN_PASSWORD` in the env. |

### Docker setup

When the action sees `docker-registry` set, it logs in, sets up QEMU (for emulated platforms), and configures Docker Buildx (for multi-platform builds).

| Input | Default | Description |
|-------|---------|-------------|
| `docker-registry` | | Container registry hostname (e.g. `ghcr.io`, `docker.io`). |
| `docker-username` | | Registry username. When unset, the action falls back to the `GITHUB_ACTOR` env var (the GitHub user that triggered the workflow). |
| `docker-password` | | Registry password or token (commonly `secrets.GITHUB_TOKEN` for ghcr.io). |

### Split / merge artifact management

For fan-out cross-platform builds ([Split/Merge](@/docs/advanced/split-merge.md)), the action can upload the `dist/` directory after a split build and download+merge dist artifacts before a merge job.

| Input | Default | Description |
|-------|---------|-------------|
| `upload-dist` | `false` | After running anodizer, upload `dist/` as a workflow artifact named `dist-$RUNNER_OS`. Set to `true` in split build jobs. |
| `download-dist` | `false` | Before running anodizer, download all artifacts matching `dist-*` and merge them into `dist/`. Set to `true` in merge jobs. Fails if no split context files are found. |

### Workspace resolution (monorepo)

When a tag-triggered workflow runs, the action can resolve the triggering tag to its owning crate so subsequent steps can use crate-scoped paths.

| Input | Default | Description |
|-------|---------|-------------|
| `resolve-workspace` | `false` | Run `anodizer resolve-tag $GITHUB_REF_NAME` and expose the result as the `workspace`, `crate-path`, and `has-builds` outputs. Fails the workflow if no crate matches the tag. |
| `determinism-crate` | | When set alongside `determinism: true`, runs the harness scoped to a single crate (e.g. `core`). Use this as the matrix dimension in Strategy C1/C-hybrid to shard determinism checks per crate rather than running all crates in each shard. |

### Determinism harness

The action can run `anodize check determinism` directly (and preserve its
hermetic dist tree for a downstream `release --publish-only` job) without
the caller needing to know the harness CLI. See
[Determinism](@/docs/advanced/determinism.md) for the harness semantics.

| Input | Default | Description |
|-------|---------|-------------|
| `determinism` | `false` | Run the determinism harness on this shard. When true, the action installs Rust (if missing), builds anodizer from source, installs cross-build deps (zig, cargo-zigbuild, upx on Linux), derives the configured-target CSV for the current `RUNNER_OS`, `rustup target add`s those triples, and invokes `anodizer check determinism`. Intended as the entire body of a per-OS harness shard. **Mutually exclusive with `args`.** |
| `determinism-runs` | `2` | N for `anodizer check determinism --runs=N`. |
| `determinism-stages` | `""` | Stages to validate (CSV). Default: Linux gets `build,source,upx,archive,nfpm,makeself,snapcraft,sbom,sign,checksum`; macOS/Windows get `build,source,upx,archive,sbom,sign,checksum` (no Linux-only formats). Explicit values override. |
| `determinism-targets` | | Explicit target CSV override. Default: filter `anodizer targets --json` to entries matching the current `RUNNER_OS`. Set when your shard runs on a non-standard runner label. The release matrix uses this to pin each shard to one MSVC triple on Windows. |
| `preserve-dist` | `false` | Have the harness preserve its hermetic dist tree to `./preserved-dist/` so a downstream `release --publish-only` job can publish from the byte-stable artifacts (no recompile). Manifests get a `-<shard-label>` suffix (`context-<label>.json`, etc.) so sharded uploads don't collide under `merge-multiple: true`. **Requires `determinism: true` and `shard-label`.** |
| `shard-label` | | Per-shard suffix appended to preserved-dist manifests. Required when `preserve-dist: true`. The caller names each shard explicitly; the action does not derive labels. |

### Execution

| Input | Default | Description |
|-------|---------|-------------|
| `args` | | Arguments to pass to anodizer (e.g. `release --snapshot`, `tag --crate my-lib`). Omit with `install-only: true` to install the binary without running it. |
| `workdir` | `.` | Working directory relative to the repository root. Use when your `.anodizer.yaml` is not at the root. |
| `install-only` | `false` | Only install anodizer (and any requested dependencies/keys). Skip the `Run anodizer` step. Useful when you want to invoke anodizer yourself in a subsequent step — e.g., a loop over multiple crates. |

## Outputs

| Output | Description |
|--------|-------------|
| `artifacts` | Contents of `dist/artifacts.json` (the full artifact inventory). Multi-line string. |
| `metadata` | Contents of `dist/metadata.json` (release metadata: tag, project name, release URL, etc.). Multi-line string. |
| `release-url` | URL of the created GitHub release, extracted from `metadata.json`. Empty if no release was created. |
| `workspace` | Crate name resolved from the triggering tag. Set only when `resolve-workspace: true`. |
| `crate-path` | Path to the resolved crate directory (e.g. `crates/my-lib`). Set only when `resolve-workspace: true`. |
| `has-builds` | `true` or `false` — whether the resolved crate has binary builds configured. Useful for conditionally skipping archive/docker stages for library-only crates. Set only when `resolve-workspace: true`. |
| `split-matrix` | JSON matrix for `strategy.matrix` covering all configured build targets, derived from `.anodizer.yaml` via `anodizer targets --json`. Each entry has `os`, `target`, and `artifact` fields. Set only when `install-only: true`. |
| `crates` | JSON array of crate names that received a new tag (e.g. `["core","bin-a"]`). Set when `args: tag` is used on a **per-crate workspace**. Empty array (`[]`) means nothing changed and downstream jobs should be skipped via `if: needs.<job>.outputs.crates != '[]'`. |
| `versions` | JSON object mapping crate name to its new version string (e.g. `{"core":"1.2.0","bin-a":"0.5.1"}`). Set when `args: tag` is used on a per-crate workspace. |
| `new-tag` | New tag `anodizer tag` created (e.g. `v1.2.3`), for **single-crate and lockstep-workspace** repos. Empty when no tag was cut. |
| `old-tag` | Previous tag `anodizer tag` bumped from. Empty on a first release. |
| `part` | Semver part bumped: `major` / `minor` / `patch` / `none` / `custom`. |
| `tagged` | `'true'` when this run cut a new tag (`new-tag` non-empty and differs from `old-tag`), `'false'` on a no-op. Gate downstream release jobs on `if: needs.<job>.outputs.tagged == 'true'` for single-crate / lockstep repos (the lockstep counterpart to the per-crate `crates != '[]'` gate). |
| `head-sha` | Commit at HEAD after `anodizer tag --push` (the tag target — the version-sync bump commit, or the original HEAD when no bump was needed). Check this out in downstream jobs so the tree matches the tag. |
| `irreversibly_published` | `'true'` when the run summary records a one-way-door publisher (crates.io, chocolatey, winget, snapcraft, ...) whose publish landed — the version is burned. Forensic signal for **custom** recovery steps; the default failure handling is anodizer's in-process `release.on_failure` policy, which already refuses to roll back past one-way doors, so most workflows never read this. Gate any manual destructive step on `steps.<id>.outputs.irreversibly_published != 'true'`. |

## Common patterns

### Simple tag-triggered release

```yaml
name: Release
on:
  push:
    tags: ["v*"]

permissions:
  contents: write
  actions: read    # cross-workflow artifact downloads (if used)

jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        with:
          auto-install: true
          gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
          args: release --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          GPG_FINGERPRINT: ${{ secrets.GPG_FINGERPRINT }}
```

No failure-handling steps are needed: `anodizer release` runs a config-derived [preflight](@/docs/general/preflight.md) before any stage and executes the [`release.on_failure` policy](@/docs/advanced/release-resilience.md#release-on-failure-the-in-process-failure-policy) in-process on a pipeline failure.

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
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
          # PAT so the pushed tag triggers downstream release.yml.
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

### Workspace-aware auto-tag (monorepo)

Tag each crate in the workspace independently. `install-only: true` gives you the binary on PATH; you drive the loop yourself.

```yaml
      - uses: tj-smith47/anodizer-action@v1
        with:
          install-only: true
      - name: Auto-tag all workspaces
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: |
          for crate in my-core my-cli my-operator my-plugin; do
            echo "--- tagging $crate ---"
            if anodizer tag --crate "$crate"; then
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
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        id: anodizer
        with:
          auto-install: true
          resolve-workspace: true
          docker-registry: ghcr.io
          docker-password: ${{ secrets.GITHUB_TOKEN }}
          args: release --crate ${{ steps.resolve.outputs.crate }} --clean
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
      - if: steps.anodizer.outputs.has-builds == 'false'
        run: echo "Library-only crate — no binary artifacts produced."
```

Note: `resolve-workspace: true` runs `anodizer resolve-tag` and exposes the crate name via `steps.<id>.outputs.workspace`. You'll typically wire that into the `--crate` arg of `args`.

### Split/merge cross-platform build

The action's built-in `upload-dist` / `download-dist` replaces the manual `actions/upload-artifact` + `actions/download-artifact` pair for split builds.

> If your release path already runs `anodizer check determinism`, the [`preserve-dist` + `release --publish-only`](@/docs/advanced/determinism.md) pattern is strictly better — it reuses the harness's byte-stable dist instead of compiling everything a second time in a separate matrix.

```yaml
jobs:
  build:
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
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
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
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

When your `ci.yml` builds and uploads the anodizer binary once per commit, downstream release workflows can reuse it instead of reinstalling.

```yaml
# ci.yml
- uses: actions/checkout@v6
- uses: dtolnay/rust-toolchain@stable
- run: cargo build --release -p anodizer
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

Pinning `artifact-run-id: auto` resolves the latest successful run of `ci.yml` for the current commit SHA, so the release workflow always picks up a binary built from the same source.

### Dynamic build matrix from config

`install-only: true` produces a `split-matrix` output derived from `.anodizer.yaml`. You can feed that directly into a `strategy.matrix`.

```yaml
jobs:
  setup:
    runs-on: ubuntu-latest
    outputs:
      matrix: ${{ steps.setup.outputs.split-matrix }}
    steps:
      - uses: actions/checkout@v6
      - uses: tj-smith47/anodizer-action@v1
        id: setup
        with:
          install-only: true

  build:
    needs: setup
    strategy:
      matrix: ${{ fromJson(needs.setup.outputs.matrix) }}
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v6
        with:
          fetch-depth: 0
      - uses: tj-smith47/anodizer-action@v1
        with:
          install-rust: true
          install: zig,cargo-zigbuild
          upload-dist: true
          args: release --split --clean
        env:
          TARGET: ${{ matrix.target }}
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

### Install only, run anodizer yourself

```yaml
- uses: tj-smith47/anodizer-action@v1
  with:
    install-only: true
- run: anodizer check
- run: anodizer release --snapshot
- run: anodizer resolve-tag v1.2.3 --json
```

### Test an un-released branch of anodizer

For integration testing a downstream project against an in-flight anodizer PR — or dogfooding a feature branch before it lands — use `from-branch`. The action shallow-clones `tj-smith47/anodizer` at the branch you name, builds it from source, and puts it on `PATH`:

```yaml
- uses: actions/checkout@v6
- uses: tj-smith47/anodizer-action@v1
  with:
    from-branch: my-feature        # branch on tj-smith47/anodizer
    args: release --snapshot --clean
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

- The input accepts a branch name only — the repository is hardcoded to `tj-smith47/anodizer`.
- The Rust toolchain is auto-installed; you don't need to pass `install-rust: true`.
- The cargo `target/` directory is cached per branch (separate cache key from `from-source` / `determinism` modes), so re-runs of the same branch on a hot runner are fast.
- Clones to `${RUNNER_TEMP}/anodizer-src` — your `workdir:` is never mutated.
- Mutually exclusive with `version`, `from-artifact`, and `from-source`. Setting two install paths at once fails the workflow with an actionable error.

## Env vars the action honors

| Variable | Purpose |
|----------|---------|
| `GITHUB_TOKEN` | Required for release uploads, artifact downloads, and the tag resolver's `gh api` calls. |
| `GPG_FINGERPRINT` | The key ID anodizer uses when signing artifacts. Required when `gpg-private-key` is set and the config references multiple keys. |
| `COSIGN_PASSWORD` | Password for the cosign key written via `cosign-key`. |
| `CARGO_REGISTRY_TOKEN` | Required by the crates.io publisher. |
| `TARGET`, `ANODIZER_OS`, `ANODIZER_ARCH` | Split job target selection (see [Split/Merge](@/docs/advanced/split-merge.md)). |

## Retry behavior

The `Run anodizer` step retries up to 3 times for transient failures (registry rate limits, Docker push auth expiry, network blips). Between retries it prunes generated artifacts from `dist/` while preserving split context files (`dist/*/context.json`) so `--merge` can still find them. Deterministic failures (config errors, compile failures) will fail identically on every attempt, but the cost of two extra 10s waits is low relative to a flaky release run.

**Stateful commands are never retried.** The action detects three modes and
runs them exactly once:

- `release --publish-only` — re-running would re-trigger PR-based publishers
  (homebrew, scoop, nix, krew, MCP) and open DUPLICATE PRs against the
  same tag. Recovery: use `release --rollback-only --from-run=<id>` first.
- `release --rollback-only` — idempotent at the entry level (already-rolled
  back entries no-op), but a retry could mask a real partial failure.
- `tag rollback` — already a recovery primitive; retrying would re-attempt
  remote tag deletes (which 404 the second time) and re-push the revert
  (which would fail with "Everything up-to-date" or a non-fast-forward
  error). The internal failure-mode-per-tag warn-and-continue handles
  transient flakes within a single invocation.

To disable retries for the normal `release` path, wrap `anodizer` manually
with `install-only: true`.
