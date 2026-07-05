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
use super::{ChecksumConfig, CrossStrategy, DockerSignConfig, DockerV2Config, SignConfig};
use crate::packagers::{MakeselfConfig, SrpmConfig};

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/// Workspace-level defaults that path-mirror the `CrateConfig` (and select
/// top-level `Config`) shape. Each field here is folded into every resolved
/// crate by `defaults_merge::apply_defaults` according to the deep-merge /
/// merge-by-identity semantics documented in `defaults_merge`.
///
/// Multi-publisher fields are single-struct on both sides today: defaults
/// supplies one struct per publisher, and per-crate `publish.*` fields are
/// also single-struct. A future change may introduce list-or-scalar via
/// `OneOrMany<T>` on the per-crate side so a crate can declare multiple
/// homebrew taps / scoop buckets / etc.; the defaults side would stay
/// single-struct and merge into the first per-crate entry by identity.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    // --- Build axis ---
    /// Default build settings applied to every crate's builds (deep-merged
    /// into each `CrateConfig.builds[]` entry by identity on `id`/`binary`).
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
    /// at `tag` time (repo-root-relative path strings). Hoisted across crates;
    /// folded into each crate's `version_files` by `defaults_merge` when the
    /// crate does not set its own list. Mirrors `CrateConfig.version_files`.
    pub version_files: Option<Vec<String>>,
    /// Default cross-compilation strategy: auto, zigbuild, cross, or cargo.
    /// Mirrors `CrateConfig.cross` so the strategy can be hoisted to defaults.
    pub cross: Option<CrossStrategy>,
    /// Default checksum settings applied to all crates.
    /// Mirrors `CrateConfig.checksum` so checksum config can be hoisted to defaults.
    pub checksum: Option<ChecksumConfig>,

    // --- Crate-axis vs workspace-axis (mutually exclusive) ---
    /// Crate-axis defaults marker. Only valid when top-level `crates:` is set.
    /// Reserved for per-crate overrides keyed by crate id (future waves).
    pub crates: Option<DefaultsCrateBlock>,
    /// Workspace-axis defaults marker. Only valid when top-level `workspaces:` is set.
    /// Reserved for per-workspace overrides keyed by workspace name (future waves).
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

/// Marker block under `defaults.crates:` that signals crate-axis defaults
/// scope. Required to drive the axis-mismatch validator. Currently
/// empty; future per-crate-id overrides will live here.
///
/// `deny_unknown_fields` so that typing `defaults.crates: { foo: bar }`
/// surfaces as a parse error rather than silently being accepted — without
/// it, the empty struct is a sink that swallows arbitrary keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsCrateBlock {}

/// Marker block under `defaults.workspaces:` that signals workspace-axis
/// defaults scope. Required to drive the axis-mismatch validator.
/// Currently empty; future per-workspace-name overrides will live here.
///
/// `deny_unknown_fields` per the same rationale as `DefaultsCrateBlock`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsWorkspaceBlock {}
