use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// BinstallConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BinstallConfig {
    /// When true, generate a .cargo/config.toml binstall section for cargo-binstall.
    pub enabled: Option<bool>,
    /// Custom download URL template for cargo-binstall (supports templates).
    pub pkg_url: Option<String>,
    /// Directory within the archive where binaries are located.
    pub bin_dir: Option<String>,
    /// Package format hint for cargo-binstall: tgz, tar.gz, tar.xz, zip, bin, etc.
    pub pkg_fmt: Option<String>,
    /// Per-target overrides keyed by Rust target triple
    /// (e.g. `x86_64-unknown-linux-gnu`). Use this when a single `pkg_url`
    /// cannot match all assets — for example when release archives use
    /// GoReleaser-style `<os>-<goarch>` names (`linux-amd64`, `darwin-arm64`)
    /// that cargo-binstall's own tokens never produce. Each entry overrides
    /// `pkg_url`/`pkg_fmt`/`bin_dir` for the matching triple, emitted as a
    /// `[package.metadata.binstall.overrides.<triple>]` sub-table.
    pub overrides: Option<BTreeMap<String, BinstallOverride>>,
}

// ---------------------------------------------------------------------------
// BinstallOverride
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BinstallOverride {
    /// Custom download URL template for this target triple (supports templates).
    /// Lets you point cargo-binstall at a per-target asset name such as
    /// `myapp-{{ .Version }}-linux-amd64.tar.gz`.
    pub pkg_url: Option<String>,
    /// Package format hint for this target triple: tgz, tar.gz, tar.xz, zip, bin, etc.
    pub pkg_fmt: Option<String>,
    /// Directory within the archive where binaries are located for this target triple.
    pub bin_dir: Option<String>,
}
