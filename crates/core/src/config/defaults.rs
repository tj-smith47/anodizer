use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::archives::ArchiveConfig;
use super::aur_source::AurSourceConfig;
use super::build::BuildConfig;
use super::hooks::HookEntry;
use super::installers::{
    AppBundleConfig, DmgConfig, FlatpakConfig, MsiConfig, NsisConfig, PkgConfig,
};
use super::nfpm::NfpmConfig;
use super::notarize::NotarizeConfig;
use super::publishers::{
    AurConfig, CargoPublishConfig, ChocolateyConfig, HomebrewCaskConfig, HomebrewConfig,
    KrewConfig, NixConfig, ScoopConfig, WingetConfig,
};
use super::sbom::SbomConfig;
use super::snapcraft::SnapcraftConfig;
use super::source::SourceConfig;
use super::upx::UpxConfig;
use super::version_files::VersionFileEntry;
use super::{ChecksumConfig, CrossStrategy, DockerSignConfig, DockerV2Config, SignConfig};
use crate::packagers::{InstallScriptConfig, MakeselfConfig, SrpmConfig};

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Workspace-level defaults. Each key has the same shape as the matching key
/// under `crates[]` (plus a few top-level keys) and is applied to every crate:
///
/// - A block fills every field the crate left unset; a field the crate sets
///   keeps the crate's value.
/// - A list entry is merged into the crate's entry with the same identity
///   (`id`, `name`, `package_name`, or an archive's first format); an entry
///   with no match is appended after the crate's own entries.
/// - An empty block (`{}`) in a crate inherits every field from defaults.
/// - `skip: true` in a crate's block drops the inherited block for that
///   crate.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    // --- Build axis ---
    /// Default build settings applied to every crate's builds, merged into
    /// each `crates[].builds[]` entry that shares its `id` or `binary`.
    pub builds: Option<BuildConfig>,
    /// Default archive settings applied to all crates.
    pub archives: Option<ArchiveConfig>,
    /// Default source-archive settings applied to all crates.
    pub source: Option<SourceConfig>,
    /// Default UPX compression settings applied to all crates.
    pub upx: Option<UpxConfig>,

    // --- Packaging axis ---
    /// Default nfpm (deb/rpm/apk) settings applied to all crates.
    pub nfpms: Option<NfpmConfig>,
    /// Default snapcraft settings applied to all crates.
    pub snapcrafts: Option<SnapcraftConfig>,
    /// Default flatpak settings applied to all crates.
    pub flatpaks: Option<FlatpakConfig>,
    /// Default app-bundle settings applied to all crates.
    pub app_bundles: Option<AppBundleConfig>,
    /// Default DMG settings applied to all crates.
    pub dmgs: Option<DmgConfig>,
    /// Default macOS PKG settings applied to all crates.
    pub pkgs: Option<PkgConfig>,
    /// Default MSI settings applied to all crates.
    pub msis: Option<MsiConfig>,
    /// Default NSIS settings applied to all crates.
    pub nsis: Option<NsisConfig>,
    /// Default makeself settings applied to all crates.
    pub makeselves: Option<MakeselfConfig>,
    /// Default install-script settings applied to all crates.
    pub install_scripts: Option<InstallScriptConfig>,
    /// Default SRPM settings applied to all crates.
    pub srpms: Option<SrpmConfig>,
    /// Default Docker (V2 API) image settings applied to all crates. The
    /// `docker_v2:` spelling is still accepted via serde alias for back-compat.
    #[serde(alias = "docker_v2")]
    pub dockers_v2: Option<DockerV2Config>,

    // --- Publish axis ---
    /// Default publisher configurations (single-struct per publisher).
    /// Per-crate `publish.*` entries are merged into these by identity.
    pub publish: Option<PublishDefaults>,

    // --- Sign / notarize / sbom ---
    /// Default artifact signing settings.
    pub sign: Option<SignConfig>,
    /// Default binary-signing settings.
    pub binary_signs: Option<SignConfig>,
    /// Default Docker image signing settings.
    pub docker_signs: Option<DockerSignConfig>,
    /// Default macOS notarization settings.
    pub notarize: Option<NotarizeConfig>,
    /// Default SBOM generation settings.
    pub sbom: Option<SbomConfig>,

    // --- Cross-cutting ---
    /// Default build targets (e.g., ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]).
    pub targets: Option<Vec<String>>,
    /// Default environment variables (`KEY=VALUE` strings) hoisted across crates.
    pub env: Option<Vec<String>>,
    /// Default repo-committed files whose embedded release version is rewritten
    /// at `tag` time (repo-root-relative path strings). Used as a crate's
    /// `version_files` when the crate does not set its own list. Takes the
    /// same entries as `crates[].version_files`, including the anchored
    /// `path` + `match` form.
    pub version_files: Option<Vec<VersionFileEntry>>,
    /// Default cross-compilation strategy for every crate: auto, zigbuild,
    /// cross, or cargo. Same values as `crates[].cross`.
    pub cross: Option<CrossStrategy>,
    /// Default checksum settings applied to all crates. Same shape as
    /// `crates[].checksum`.
    pub checksum: Option<ChecksumConfig>,

    // --- Crate-axis vs workspace-axis (mutually exclusive) ---
    /// Crate-axis defaults marker. Only valid when top-level `crates:` is set.
    /// Reserved for per-crate overrides keyed by crate id.
    pub crates: Option<DefaultsCrateBlock>,
    /// Workspace-axis defaults marker. Only valid when top-level `workspaces:` is set.
    /// Reserved for per-workspace overrides keyed by workspace name.
    pub workspaces: Option<DefaultsWorkspaceBlock>,
}

/// Workspace-default publishers. Each publisher is single-struct in
/// defaults; per-crate `publish.*` may be either a single struct or a list,
/// reconciled by the merge engine.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PublishDefaults {
    /// Default Homebrew formula settings.
    pub homebrew: Option<HomebrewConfig>,
    /// Default Homebrew Cask settings, merged into per-crate `publish.homebrew_cask`.
    ///
    /// Single-struct.
    pub homebrew_cask: Option<HomebrewCaskConfig>,
    /// Default crates.io publish settings, merged into per-crate `publish.cargo`.
    ///
    /// Single-struct.
    pub cargo: Option<CargoPublishConfig>,
    /// Default Scoop manifest settings.
    pub scoop: Option<ScoopConfig>,
    /// Default WinGet manifest settings.
    pub winget: Option<WingetConfig>,
    /// Default Chocolatey package settings.
    pub chocolatey: Option<ChocolateyConfig>,
    /// Default Krew (kubectl plugin manager) settings.
    pub krew: Option<KrewConfig>,
    /// Default Nix derivation settings.
    pub nix: Option<NixConfig>,
    /// Default AUR (binary) settings.
    pub aur: Option<AurConfig>,
    /// Default AUR (source) settings.
    pub aur_source: Option<AurSourceConfig>,
    /// Hooks fired once per failed publisher — same surface as
    /// `publish.on_error` on individual crate configs. Merged into
    /// every crate's resolved publish config.
    pub on_error: Option<Vec<HookEntry>>,
    /// Hooks fired once per publisher a triggered rollback reverted — same
    /// surface as `publish.on_rollback` on individual crate configs. Merged
    /// into every crate's resolved publish config.
    pub on_rollback: Option<Vec<HookEntry>>,
}

/// Block under `defaults.crates:` that supplies workspace-wide fallbacks for
/// per-crate fields. Also drives the axis-mismatch validator (present only
/// when top-level `crates:` is non-empty).
///
/// `deny_unknown_fields` so that typing `defaults.crates: { foo: bar }`
/// surfaces as a parse error rather than silently being accepted.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsCrateBlock {
    /// Git tag template applied to any crate that omits its own
    /// `tag_template`. Overridden by a crate's own `tag_template` when set;
    /// falls back to the `<name>-v{{ Version }}` convention when neither is
    /// set.
    pub tag_template: Option<String>,
}

/// Marker block under `defaults.workspaces:` that signals workspace-axis
/// defaults scope. Required to drive the axis-mismatch validator.
/// Currently empty; future per-workspace-name overrides will live here.
///
/// `deny_unknown_fields` per the same rationale as `DefaultsCrateBlock`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsWorkspaceBlock {}
