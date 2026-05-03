use super::*;

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
}
