+++
title = "Rust-specific extras"
description = "Features anodizer adds because Rust's toolchain and packaging conventions differ from Go's. No GoReleaser equivalent."
weight = 70
template = "section.html"
+++

# Rust-specific extras

These features exist because Rust's toolchain and packaging conventions
differ from Go's. They are dogfooded by anodizer and cfgd themselves.

## Live configuration

Excerpt from [`cfgd/.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml)
(snapshot 2026-05-24) — every feature in the table below is wired here.

```yaml
# Tag bumper — requires an explicit signal per commit range
# (Conventional Commits or `#major`/`#minor`/`#patch` tokens).
tag:
  default_bump: none
  branch_history: full
  tag_prefix: "v"
  release_branches: [master]
  initial_version: "0.3.5"

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
          enabled: true
          pkg_url: "https://github.com/tj-smith47/cfgd/releases/download/v{{ Version }}/cfgd-{{ Version }}-{ target }.tar.gz"
          pkg_fmt: tgz
  # ... cfgd-operator, cfgd-csi follow the same shape
```

| Feature | Status | Notes |
|---|---|---|
| `crates.io publish` | ✅ Verified | Dependency-aware ordering. [anodizer on crates.io](https://crates.io/crates/anodizer) · [cfgd on crates.io](https://crates.io/crates/cfgd). cfgd publishes 4 crates in dependency order on every release |
| `binstall metadata` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`binstall.pkg_url` + `binstall.pkg_fmt: tgz`) |
| `cargo_workspace` detection | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (4 `workspaces:` entries) |
| `version_sync` | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`version_sync.enabled: true` + `mode: cargo` per crate) |
| `tag_pre_hooks` | ✅ Verified | [`crates/core/src/config/tag.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/config/tag.rs) (`tag_pre_hooks` field) |
| `tag_post_hooks` | ✅ Verified | [`crates/core/src/config/tag.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/config/tag.rs) (`tag_post_hooks` field) |
| `ANODIZER_SPLIT_TARGET` | ✅ Verified | [`crates/core/src/partial.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/core/src/partial.rs) (`ANODIZER_OS` / `ANODIZER_ARCH` env vars; accepts `GGOOS`/`GGOARCH` aliases) |
| UPX target-triple globs | ✅ Verified | [cfgd `.anodizer.yaml`](https://github.com/tj-smith47/cfgd/blob/master/.anodizer.yaml) (`upx.targets:` Rust target triples like `x86_64-unknown-linux-gnu`) |
| `anodizer targets --json` | ✅ Verified | [`crates/cli/src/commands/targets.rs`](https://github.com/tj-smith47/anodizer/blob/master/crates/cli/src/commands/targets.rs) (consumed by [anodizer-action](https://github.com/tj-smith47/anodizer-action) matrix input) |
| `anodizer resolve-tag` | ✅ Verified | [cfgd `release.yml`](https://github.com/tj-smith47/cfgd/blob/master/.github/workflows/release.yml) (`resolve-workspace: 'true'` step) |
