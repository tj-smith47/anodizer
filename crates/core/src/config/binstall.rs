use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// BinstallConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BinstallConfig {
    /// When true, write `[package.metadata.binstall]` into the crate's
    /// Cargo.toml so `cargo binstall` can install prebuilt release archives.
    ///
    /// # Auto-derivation
    ///
    /// With `enabled: true` and **no** `pkg_url` and **no** `overrides`,
    /// anodize fills in correct per-target metadata automatically: for every
    /// configured build target it emits an
    /// `overrides.<rust-triple>` whose `pkg_url` is the GitHub release download
    /// URL for that target's archive, with the asset name rendered through the
    /// *same* `archive.name_template` the archive stage uses (so the URL can
    /// never drift from the asset the release uploads) and the version
    /// positions expressed as cargo-binstall's own `{ version }` token. The
    /// matching `pkg_fmt` (`tar.gz`→`tgz`, `zip`→`zip`, …) is set per target.
    ///
    /// ```yaml
    /// binstall:
    ///   enabled: true   # nothing else required
    /// ```
    pub enabled: Option<bool>,
    /// Custom download URL template for cargo-binstall (supports templates).
    ///
    /// Setting this (or any [`overrides`](Self::overrides) entry) **disables
    /// auto-derivation** — anodize writes your value verbatim and computes
    /// nothing. Use it only when the auto-derived per-target URLs don't fit
    /// (manual values always win).
    pub pkg_url: Option<String>,
    /// Directory within the archive where binaries are located.
    pub bin_dir: Option<String>,
    /// Package format hint for cargo-binstall: tgz, tar.gz, tar.xz, zip, bin, etc.
    pub pkg_fmt: Option<String>,
    /// Per-target overrides keyed by Rust target triple
    /// (e.g. `x86_64-unknown-linux-gnu`). Each entry overrides
    /// `pkg_url`/`pkg_fmt`/`bin_dir` for the matching triple, emitted as a
    /// `[package.metadata.binstall.overrides.<triple>]` sub-table.
    ///
    /// You rarely need to set this by hand: with `enabled: true` and no
    /// `pkg_url`/`overrides`, anodize auto-derives a correct per-target
    /// override for every build target (see [`enabled`](Self::enabled)).
    /// Supplying any override here (like supplying [`pkg_url`](Self::pkg_url))
    /// **disables auto-derivation** and takes full manual control of the table.
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
    /// `myapp-{{ Version }}-linux-amd64.tar.gz`.
    pub pkg_url: Option<String>,
    /// Package format hint for this target triple: tgz, tar.gz, tar.xz, zip, bin, etc.
    pub pkg_fmt: Option<String>,
    /// Directory within the archive where binaries are located for this target triple.
    pub bin_dir: Option<String>,
}
