+++
title = "GitHub Action"
description = "Inputs and outputs exposed by the anodizer-action GitHub Action."
weight = 60
template = "section.html"
+++

# GitHub Action

The action lives at [tj-smith47/anodizer-action](https://github.com/tj-smith47/anodizer-action).
Each row maps to a single Action input.

## Live configuration

Three excerpts from [cfgd's `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml)
(snapshot 2026-05-24) cover every input in the table below.

```yaml
# 1. Resolve job — picks the affected workspace per tag, uses install-only
#    to install anodizer without running a release.
- uses: tj-smith47/anodizer-action@v1
  id: anodizer
  with:
    from-artifact: anodizer-linux
    artifact-run-id: auto
    artifact-workflow: ci.yml
    resolve-workspace: 'true'
    install-only: 'true'

# 2. Split build job — per-OS matrix, builds artifacts and uploads dist/.
- name: Run anodizer release --split
  uses: tj-smith47/anodizer-action@v1
  with:
    from-artifact: ${{ matrix.anodizer_artifact }}
    artifact-run-id: auto
    artifact-workflow: ci.yml
    install: zig,cargo-zigbuild,upx
    upload-dist: 'true'
    args: release --verbose --debug --strict --split --clean --crate ${{ needs.resolve.outputs.workspace }}

# 3. Final release job — merges split artifacts, publishes everything.
- name: Run anodizer release
  uses: tj-smith47/anodizer-action@v1
  with:
    from-artifact: anodizer-linux
    artifact-run-id: auto
    artifact-workflow: ci.yml
    install: nfpm,makeself,snapcraft,rpmbuild,cosign
    download-dist: ${{ needs.resolve.outputs.has-builds }}
    docker-registry: ghcr.io
    docker-password: ${{ secrets.GITHUB_TOKEN }}
    gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}
```

| Input | Status | Notes |
|---|---|---|
| `from-source` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`from-source: true` in split-build jobs) |
| `install-rust` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`uses: dtolnay/rust-toolchain@stable` in the release job) |
| `args` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`args: release --publish-only`) |
| `preserve-dist` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`preserve-dist: 'true'` in determinism-check shards) |
| `shard-label` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`shard-label: ${{ matrix.shard }}` per matrix entry) |
| `from-artifact` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`from-artifact: anodizer-linux`) |
| `artifact-run-id` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`artifact-run-id: auto`) |
| `artifact-workflow` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`artifact-workflow: ci.yml`) |
| `install` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`install: nfpm,makeself,snapcraft,rpmbuild,cosign` + `zig,cargo-zigbuild,upx`) |
| `gpg-private-key` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}`) |
| `docker-registry` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`docker-registry: ghcr.io`) |
| `docker-password` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`docker-password: ${{ secrets.GITHUB_TOKEN }}`) |
| `upload-dist` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`upload-dist: 'true'` in split build job) |
| `download-dist` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`download-dist: ${{ needs.resolve.outputs.has-builds }}`) |
| `resolve-workspace` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`resolve-workspace: 'true'` in resolve job) |
| `version` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (default `"latest"` used in installations not specifying `from-artifact`; accepts exact tag, `"latest"`, or `"nightly"`) |
| `from-branch` | ✅ Verified | [cfgd `nightly.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/nightly.yml) (`from-branch: publisher-required-config` — builds anodizer from an in-progress branch before the features ship) |
| `auto-install` | ✅ Verified | [cfgd `determinism-shards.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/determinism-shards.yml) and [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`auto-install: 'true'`) |
| `docker-username` | 🤝 Help wanted | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/main/action.yml) (registry username; defaults to `github.actor`). All live workflows rely on the default; explicit override unexercised |
| `apk-private-key` | ✅ Verified | [cfgd `nightly.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/nightly.yml) (`apk-private-key: ${{ secrets.APK_PRIVATE_KEY }}` — signs nfpm apk packages on every nightly build) |
| `cosign-key` | 🤝 Help wanted | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/main/action.yml) (cosign private key for keyful signing; `COSIGN_KEY` / `COSIGN_PASSWORD` env). All live workflows use keyless OIDC signing; keyful path unexercised |
| `workdir` | 🤝 Help wanted | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/main/action.yml) (working directory below repo root; default `.`). All live workflows use the default; non-root workdir unexercised |
| `install-only` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`install-only: 'true'` in the resolve job) |
| `determinism` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) and [cfgd `determinism-shards.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/determinism-shards.yml) (`determinism: 'true'` per shard) |
| `determinism-runs` | ⏳ Pending | Live shards run with the default `"2"`; explicit `--runs=N` override unexercised |
| `determinism-stages` | ⏳ Pending | Live shards use platform-derived stage defaults; explicit CSV override unexercised |
| `determinism-targets` | ✅ Verified | [cfgd `determinism-shards.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/determinism-shards.yml) (`determinism-targets: ${{ matrix.shard.targets }}` — explicit target CSV from the shard matrix) |
| `determinism-crate` | ✅ Verified | [cfgd `determinism-shards.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/determinism-shards.yml) (`determinism-crate: ${{ inputs.crate }}` — scopes each shard to one workspace crate) |

## Outputs

| Output | Status | Notes |
|---|---|---|
| `artifacts` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (contents of `dist/artifacts.json`) |
| `metadata` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (contents of `dist/metadata.json`) |
| `release-url` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (GitHub release URL extracted from metadata) |
| `workspace` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (crate name resolved from triggering tag; requires `resolve-workspace: true`) |
| `crate-path` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (path to resolved crate directory; requires `resolve-workspace: true`) |
| `has-builds` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`download-dist: ${{ needs.resolve.outputs.has-builds }}` — gates the merge job on whether the crate has binary builds) |
| `split-matrix` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (JSON `strategy.matrix` for split build jobs; each entry has `os`, `target`, `artifact`; produced when `install-only: true`) |
| `crates` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (JSON array of crate names tagged this run; drives per-crate downstream matrix strategies) |
| `versions` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (JSON object mapping crate name → bumped version) |
| `new-tag` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (tag cut this run, e.g. `v1.2.3`; empty on no-op) |
| `old-tag` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (previous tag bumped from; empty on first release) |
| `part` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (semver part bumped: `major` \| `minor` \| `patch` \| `none` \| `custom`) |
| `tagged` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`'true'` when a new tag was cut; gate downstream release jobs on this) |
| `head-sha` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (commit SHA at HEAD after `tag --push`; check this out in downstream jobs so the tree matches the tag) |
