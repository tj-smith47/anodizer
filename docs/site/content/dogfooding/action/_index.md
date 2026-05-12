+++
title = "GitHub Action"
description = "Inputs and outputs exposed by the anodizer-action GitHub Action."
weight = 60
template = "section.html"
+++

# GitHub Action

The action lives at [tj-smith47/anodizer-action](https://github.com/tj-smith47/anodizer-action).
Each row maps to a single Action input.

| Input | Status | Notes |
|---|---|---|
| `from-source` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`from-source: true` in build job) |
| `install-rust` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`install-rust: false` since toolchain installed separately) |
| `args` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/release.yml) (`args: release --split --clean`) |
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
