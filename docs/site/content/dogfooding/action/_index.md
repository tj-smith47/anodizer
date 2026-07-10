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

Three excerpts from [cfgd's `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml)
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
| `from-source` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`from-source: true` in split-build jobs) |
| `install-rust` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (SHA-pinned `uses: dtolnay/rust-toolchain@…` — stable branch — in the release job) |
| `args` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (`args: release --publish-only --skip=…` / `--publishers …`) |
| `preserve-dist` | ✅ Verified | [anodizer `determinism.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/determinism.yml) (`preserve-dist: 'true'` in every determinism shard) |
| `shard-label` | ✅ Verified | [anodizer `determinism.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/determinism.yml) (`shard-label: ${{ matrix.shard }}` per matrix entry) |
| `from-artifact` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`from-artifact: anodizer-linux`) |
| `artifact-run-id` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`artifact-run-id: auto`) |
| `artifact-workflow` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`artifact-workflow: ci.yml`) |
| `install` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`install: nfpm,makeself,snapcraft,rpmbuild,cosign` + `zig,cargo-zigbuild,upx`) |
| `gpg-private-key` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`gpg-private-key: ${{ secrets.GPG_PRIVATE_KEY }}`) |
| `docker-registry` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`docker-registry: ghcr.io`) |
| `docker-password` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`docker-password: ${{ secrets.GITHUB_TOKEN }}`) |
| `upload-dist` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`upload-dist: 'true'` in split build job) |
| `download-dist` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`download-dist: ${{ needs.resolve.outputs.has-builds }}`) |
| `resolve-workspace` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`resolve-workspace: 'true'` in resolve job) |
| `version` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (`version: ${{ needs.tag.outputs.publish_version }}` in both publish jobs; accepts exact tag, `"latest"`, or `"nightly"`) |
| `from-branch` | ✅ Verified | cfgd dogfooded `from-branch: publisher-required-config` in [`ci.yml` at v0.4.0](https://github.com/tj-smith47/cfgd/blob/v0.4.0/.github/workflows/ci.yml) and [`determinism-shards.yml` at v0.4.0](https://github.com/tj-smith47/cfgd/blob/v0.4.0/.github/workflows/determinism-shards.yml) — the workflows that built and cut [cfgd v0.4.0](https://github.com/tj-smith47/cfgd/releases/tag/v0.4.0) (dogfooded through v0.4.0; cfgd switched to a pinned release version on 2026-06-11). anodizer's own [`nightly.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/nightly.yml) also wires `from-branch: master` |
| `auto-install` | ✅ Verified | [cfgd `determinism-shards.yml`](https://github.com/tj-smith47/cfgd/blob/v0.4.0/.github/workflows/determinism-shards.yml) and [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`auto-install: 'true'`); [brontes `release.yml`](https://github.com/tj-smith47/brontes/blob/master/.github/workflows/release.yml) relies on it to provision cosign + syft for the sign/SBOM stages on a clean runner |
| `reclaim-disk` | 🤝 Help wanted | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (reclaims large, build-irrelevant runner caches before heavy packaging on disk-tight runners). anodizer's own [determinism shards](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/determinism.yml) reclaim via an inline step, so the input itself is unexercised |
| `docker-username` | 🤝 Help wanted | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (registry username; defaults to `github.actor`). All live workflows rely on the default; explicit override unexercised |
| `apk-private-key` | ✅ Verified | [cfgd `nightly.yml`](https://github.com/tj-smith47/cfgd/blob/v0.4.0/.github/workflows/nightly.yml) (`apk-private-key: ${{ secrets.APK_PRIVATE_KEY }}` — signs nfpm apk packages on every nightly build) |
| `cosign-key` | 🤝 Help wanted | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (cosign private key for keyful signing; `COSIGN_KEY` / `COSIGN_PASSWORD` env). All live workflows use keyless OIDC signing; keyful path unexercised |
| `workdir` | 🤝 Help wanted | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (working directory below repo root; default `.`). All live workflows use the default; non-root workdir unexercised |
| `install-only` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`install-only: 'true'` in the resolve job) |
| `determinism` | ✅ Verified | [anodizer `determinism.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/determinism.yml) and [cfgd `determinism-shards.yml`](https://github.com/tj-smith47/cfgd/blob/v0.4.0/.github/workflows/determinism-shards.yml) (`determinism: 'true'` per shard) |
| `determinism-runs` | ⏳ Pending | Live shards run with the default `"2"`; explicit `--runs=N` override unexercised |
| `determinism-stages` | ⏳ Pending | Live shards use platform-derived stage defaults; explicit CSV override unexercised |
| `determinism-targets` | ✅ Verified | [cfgd `determinism-shards.yml`](https://github.com/tj-smith47/cfgd/blob/v0.4.0/.github/workflows/determinism-shards.yml) (`determinism-targets: ${{ matrix.shard.targets }}` — explicit target CSV from the shard matrix) |
| `determinism-crate` | ✅ Verified | [cfgd `determinism-shards.yml`](https://github.com/tj-smith47/cfgd/blob/v0.4.0/.github/workflows/determinism-shards.yml) (`determinism-crate: ${{ inputs.crate }}` — scopes each shard to one workspace crate) |

## Outputs

| Output | Status | Notes |
|---|---|---|
| `artifacts` | ⏳ Pending | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (contents of `dist/artifacts.json`, populated on every release run). No live workflow consumes the output yet |
| `metadata` | ⏳ Pending | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (contents of `dist/metadata.json`, populated on every release run). No live workflow consumes the output yet |
| `release-url` | ⏳ Pending | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (GitHub release URL extracted from metadata). No live workflow consumes the output yet |
| `workspace` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (crate name resolved from triggering tag; requires `resolve-workspace: true`) |
| `crate-path` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (path to resolved crate directory; requires `resolve-workspace: true`) |
| `has-builds` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`download-dist: ${{ needs.resolve.outputs.has-builds }}` — gates the merge job on whether the crate has binary builds) |
| `split-matrix` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (JSON `strategy.matrix` for split build jobs; each entry has `os`, `target`, `artifact`; produced when `install-only: true`) |
| `crates` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/v0.5.0/.github/workflows/release.yml) (`contains(fromJson(needs.tag.outputs.crates), 'cfgd-core')` gates every per-crate publish job) |
| `versions` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/v0.5.0/.github/workflows/release.yml) (`fromJson(needs.tag.outputs.versions).cfgd` feeds downstream version pins) |
| `new-tag` | ⏳ Pending | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (tag cut this run, e.g. `v1.2.3`; empty on no-op). Live workflows gate on `tagged` instead; the output itself is unconsumed |
| `old-tag` | ⏳ Pending | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (previous tag bumped from; empty on first release). No live workflow consumes it yet |
| `part` | ⏳ Pending | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (semver part bumped: `major` \| `minor` \| `patch` \| `none` \| `custom`). No live workflow consumes it yet |
| `tagged` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (`autotag-tagged: ${{ steps.autotag.outputs.tagged }}` → `needs.tag.outputs.tagged == 'true'` gates every downstream release job) |
| `head-sha` | ✅ Verified | [anodizer `release.yml`](https://github.com/tj-smith47/anodizer/blob/v0.16.0/.github/workflows/release.yml) (`autotag-sha: ${{ steps.autotag.outputs.head-sha }}` — downstream jobs check out this SHA so the tree matches the tag) |
| `irreversibly-published` | ✅ Verified (tests) | [`action.yml`](https://github.com/tj-smith47/anodizer-action/blob/master/action.yml) (`'true'` when the run's `summary.json` records a one-way-door publisher — crates.io, Chocolatey, winget, Snapcraft — landing; gate any custom destructive recovery step on it. A deprecated `irreversibly_published` snake_case alias is retained). No live workflow consumes it yet |
