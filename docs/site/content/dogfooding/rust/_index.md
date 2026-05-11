+++
title = "Rust-specific extras"
description = "Features anodizer adds because Rust's toolchain and packaging conventions differ from Go's. No GoReleaser equivalent."
weight = 70
template = "section.html"
+++

# Rust-specific extras

These features exist because Rust's toolchain and packaging conventions
differ from Go's. They are dogfooded by anodizer and cfgd themselves.

| Feature | Status | Notes |
|---|---|---|
| `crates.io publish` | ✅ Verified | Dependency-aware ordering. [anodizer on crates.io](https://crates.io/crates/anodizer) · [cfgd on crates.io](https://crates.io/crates/cfgd). cfgd publishes 4 crates in dependency order on every release |
| `binstall metadata` | ✅ Verified | `cargo-binstall` compatibility. `cargo binstall cfgd` works because cfgd ships the `pkg_url`/`pkg_fmt` metadata |
| `cargo_workspace` detection | ✅ Verified | Multi-crate monorepo. cfgd's 4-workspace setup |
| `version_sync` | ✅ Verified | Cargo.toml to git tag sync. Runs on every release |
| `tag_pre_hooks` | ✅ Verified | Templated. anodizer's auto-tag flow |
| `tag_post_hooks` | ✅ Verified | Templated. anodizer's auto-tag flow |
| `ANODIZER_SPLIT_TARGET` | ✅ Verified | Env var (replaces GoReleaser's `GGOOS`/`GGOARCH`). Consumed by every split job |
| UPX target-triple globs | ✅ Verified | v0.1.1 binaries are UPX-packed using Rust target triples |
| `anodizer targets --json` | ✅ Verified | The action uses it |
| `anodizer resolve-tag` | ✅ Verified | cfgd's release workflow |
