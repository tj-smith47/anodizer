+++
title = "Rust-specific extras"
description = "Features anodizer adds because Rust's toolchain and packaging conventions differ from Go's. No GoReleaser equivalent."
weight = 70
template = "section.html"
+++

# Rust-specific extras

These features exist because Rust's toolchain and packaging conventions
differ from Go's. They are dogfooded by anodizer, cfgd, and brontes themselves.

## Live configuration

Excerpt from [`cfgd/.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml)
(snapshot 2026-07-25) — the workspace-shaped features in the table below are wired here.

```yaml
# Tag bumper — requires an explicit signal per commit range
# (Conventional Commits or `#major`/`#minor`/`#patch` tokens).
tag:
  default_bump: none
  branch_history: full
  tag_prefix: "v"
  release_branches: [master]
  initial_version: "0.5.0"

# UPX target-triple globs — Rust triples, not Go GOOS/GOARCH.
upx:
  - id: default
    enabled: true
    args: ["--best", "--lzma"]
    targets:
      - x86_64-unknown-linux-gnu
      - aarch64-unknown-linux-gnu
      - x86_64-apple-darwin
      - x86_64-pc-windows-msvc

# Workspaces — independent release cadences per crate.
workspaces:
  - name: cfgd-core                 # shared library, crates.io only
    skip: [announce]
    crates:
      - name: cfgd-core
        path: crates/cfgd-core
        tag_template: "core-v{{ Version }}"
        version_sync: { enabled: true, mode: cargo }
        # publish.cargo inherited from defaults (index_timeout: 600)

  - name: cfgd                      # cross-platform CLI
    crates:
      - name: cfgd
        path: crates/cfgd
        tag_template: "v{{ Version }}"
        depends_on: [cfgd-core]     # dependency-aware publish ordering
        version_sync: { enabled: true, mode: cargo }
        universal_binaries:
          - { name_template: "{{ ProjectName }}", replace: false }
        binstall:
          enabled: true   # pkg_url + per-target overrides derived from archive.name_template
  # ... cfgd-crd, cfgd-operator, cfgd-csi follow the same shape
```

| Feature | Status | Notes |
|---|---|---|
| `pypis[]` — PyPI wheels from a Rust binary | ✅ Verified | Lockstep. GoReleaser has no PyPI publisher; anodizer wraps the release binaries as platform wheels so `pip install anodizer` / `uvx anodizer` works with no Rust toolchain. Live at [pypi.org/project/anodizer](https://pypi.org/project/anodizer/) 0.23.0 with [9 wheels](https://pypi.org/project/anodizer/#files) (macOS x86_64/arm64/universal2, manylinux + musllinux x86_64/aarch64, Windows amd64/arm64). Wheel tags derive from the configured targets. See [`pypis:` docs](../../../docs/publish/pypi/) |
| `pypis[].auth: oidc` — Trusted Publishing | ✅ Verified | [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) (`auth: oidc`): the Actions id-token is exchanged for a short-lived PyPI upload token, so no API token is stored. Runs from the github-hosted [`publish-oidc.yml`](https://github.com/tj-smith47/anodizer/blob/master/.github/workflows/publish-oidc.yml) — the workflow named in PyPI's Trusted Publisher config, and the same runner class crates.io Trusted Publishing needs |
| `install_scripts` — derived `curl \| sh` installer | ✅ Verified | No GoReleaser equivalent. The [`install.sh`](https://github.com/tj-smith47/anodizer/releases/download/v0.23.0/install.sh) shipped with every release derives its os/arch→asset table, `uname` detection arms, supported-platform list, checksums filename, tag prefix, and repo from the configured targets — the same SSOT that keeps `binstall`'s `pkg_url` from 404ing. [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) supplies only banner metadata and `binaries: [anodizer]` |
| `aur_sources[]` — cargo build-from-source PKGBUILD | ✅ Verified | Rust-specific: the PKGBUILD builds with `cargo` from the source tarball rather than repackaging a binary. anodizer ships both AUR shapes from one release — [`anodizer`](https://aur.archlinux.org/packages/anodizer) via `aur_sources:` and [`anodizer-bin`](https://aur.archlinux.org/packages/anodizer-bin) via `aur:`, both at 0.23.0-1. See [`aur_sources:` docs](../../../docs/publish/aur-sources/) |
| `crates.io publish` | ✅ Verified | Dependency-aware ordering, in all three shapes. [anodizer on crates.io](https://crates.io/crates/anodizer) · [cfgd on crates.io](https://crates.io/crates/cfgd) · [brontes on crates.io](https://crates.io/crates/brontes). cfgd publishes its crates in dependency order on every release; brontes covers the single-crate (non-workspace) path |
| `binstall metadata` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`binstall.enabled: true` — `pkg-url` + per-target `overrides` auto-derived from `archive.name_template`, no hand-written URL) |
| `cargo_workspace` detection | ✅ Verified | Per-crate. [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (5 `workspaces:` entries on independent cadences). Lockstep is the other shape: [anodizer `.anodizer.yaml`](https://github.com/tj-smith47/anodizer/blob/master/.anodizer.yaml) lists every crate under one `crates:` block sharing one version and one tag |
| `version_sync` | ✅ Verified | Both modes live, one per shape. `mode: cargo`: [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (per crate). `mode: tag` (single-crate, tag-derived version written into `Cargo.toml` in the bump commit the tag points at): [brontes `.anodizer.yaml`](https://github.com/tj-smith47/brontes/blob/master/.anodizer.yaml) — the [v0.3.0 tag](https://github.com/tj-smith47/brontes/releases/tag/v0.3.0) points at the auto-authored `chore(release): bump . → 0.3.0` commit |
| `tag_pre_hooks` | ✅ Verified | [`crates/core/src/config/tag.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/config/tag.rs) (`tag_pre_hooks` field) |
| `tag_post_hooks` | ✅ Verified | [`crates/core/src/config/tag.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/config/tag.rs) (`tag_post_hooks` field) |
| `ANODIZER_OS` / `ANODIZER_ARCH` split-shard filters | ✅ Verified | [`crates/core/src/partial.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/partial.rs) — with `partial.by: os` each split shard restricts its build set from these env vars (`GGOOS`/`GGOARCH` accepted as GoReleaser-compatible aliases). Live on every anodizer release: the 4-shard [`determinism.yml`](https://github.com/tj-smith47/anodizer/blob/v0.23.0/.github/workflows/determinism.yml) matrix produces the preserved dist the publish job ships |
| UPX target-triple globs | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`upx.targets:` Rust target triples like `x86_64-unknown-linux-gnu`) |
| `anodizer targets --json` | ✅ Verified | [`crates/cli/src/commands/targets.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/targets.rs) (consumed by [anodizer-action](https://github.com/tj-smith47/anodizer-action) matrix input) |
| `anodizer resolve-tag` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/3467bc973151b2a2344827d279672963c6c91d5a/.github/workflows/release.yml) (`resolve-workspace: 'true'` step) |
| Dual-license SPDX rendering (`MIT OR Apache-2.0`) | ✅ Verified | [anodizer `Cargo.toml`](https://github.com/tj-smith47/anodizer/blob/master/Cargo.toml) (`license = "MIT OR Apache-2.0"`) is parsed once and rendered per publisher: Homebrew `license any_of:`, AUR `license=('MIT' 'Apache-2.0')`, Nix `with lib.licenses; [ mit asl20 ]`, Chocolatey SPDX-aware `licenseUrl`. The live npm metapackage [`anodizer`](https://www.npmjs.com/package/anodizer) carries the compound `license: "MIT OR Apache-2.0"` (`npm view anodizer license`). See [`crates/core/src/license.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/license.rs) |
| `version_files[]` (tag-time rewrite) | ✅ Verified | Per-crate — each workspace rewrites its own files at its own cadence. [cfgd v0.6.1 `docs/installation.md`](https://github.com/tj-smith47/cfgd/blob/v0.6.1/docs/installation.md) carries the rewritten `0.6.1` download URLs, committed atomically with the version bump ([cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) enrols `docs/installation.md`, `docs/bootstrap.md`, `docs/skill.md`, `chart/cfgd/Chart.yaml`). See [`crates/core/src/version_files.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/version_files.rs) (`rewrite_version_in_files` — word-boundary rewrite of bare + `v`-prefixed version) |
| `anodizer changelog` command | ✅ Verified | [cfgd v0.4.0 release body](https://github.com/tj-smith47/cfgd/releases/tag/v0.4.0) (rendered Keep-a-Changelog groups). Formats: `keep-a-changelog` (alias `kac`), `release-notes`, `json`; `--write` updates `CHANGELOG.md`. See [`crates/cli/src/commands/changelog.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/changelog.rs) |
| Generated crate READMEs | ✅ Verified | [anodizer `crates/xtask/src/validate_readme.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/xtask/src/validate_readme.rs) (template-driven READMEs kept in sync, validated in CI) |
| `npms[]` per-package auth (`auto`/`token`/`oidc`) | ✅ Verified | The live [`anodizer`](https://www.npmjs.com/package/anodizer) metapackage (`npm view anodizer optionalDependencies`) declares all 8 per-platform `optionalDependencies` (`@tj-smith47/anodizer-{darwin,linux,win32}-*`), each published with provenance. `auth: auto` probes package existence to pick OIDC Trusted Publishing vs token per package. See [`crates/stage-publish/src/npm/auth.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/stage-publish/src/npm/auth.rs) (`decide_auth`) |
