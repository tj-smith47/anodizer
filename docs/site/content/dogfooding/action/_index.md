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
