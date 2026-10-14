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
/// under `crates[]` or at the top level, and fills what the config left
/// unset. A field the config sets always keeps its own value.
///
/// | Key | Applied to | How |
/// |---|---|---|
/// | `builds` | every build of every crate | merged into each `builds[]` entry; a crate with no builds gets one build made from it |
/// | `archives` | every crate | merged into the crate's archive whose first format matches; appended when none matches; a crate with `archives: false` takes nothing |
/// | `nfpms`, `snapcrafts`, `flatpaks`, `app_bundles`, `dmgs`, `pkgs`, `msis`, `nsis`, `dockers_v2` | every crate | merged into the crate's entry with the same identity (`id`; `id` then `package_name` for nfpm; `name` for snapcraft); appended when none matches |
/// | `checksum` | every crate | merged into the crate's `checksum` block |
/// | `cross` | every crate | used when the crate sets no `cross` |
/// | `version_files` | every crate | used whole when the crate sets no `version_files`; a crate list is never merged with it |
/// | `crates.tag_template` | every crate | used when the crate's `tag_template` is unset or empty |
/// | `publish.<publisher>` | every crate | merged into the crate's `publish.<publisher>` block |
/// | `publish.on_error`, `publish.on_rollback` | every crate | appended after the crate's own hooks |
/// | `source`, `notarize`, `srpms` | the top level | merged into the top-level block |
/// | `upx`, `sign`, `binary_signs`, `docker_signs`, `sbom`, `makeselves`, `install_scripts` | the top level | used as the one entry of the top-level list (`upx`, `signs`, `binary_signs`, `docker_signs`, `sboms`, `makeselfs`, `install_scripts`) when that list is empty |
/// | `targets` | every build | used by every build that sets no `targets` |
/// | `env` | the top level | put before the top-level `env`, so a top-level entry with the same key wins |
///
/// "Merged" means the default fills every field the entry left unset,
/// nested blocks field by field; a list field the entry sets keeps its own
/// list whole. An empty block (`{}`) in a crate inherits every field. A
/// block or list entry with `skip: true` (or a `skip` string reading `true`
/// or `1`) takes nothing from defaults, and a list default that shares a
/// skipped entry's identity key is not appended; a build's `skip` does not
/// stop the `builds` merge. A list default that matches no entry, including
/// one with no identity key, is appended to the crate's list; an empty or
/// absent list gets it as its one entry.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    // --- Build axis ---
    /// Default build settings merged into every `crates[].builds[]` entry;
    /// a field the build sets keeps its value. A crate with no `builds`
    /// gets one build made from this block.
    pub builds: Option<BuildConfig>,
    /// Default archive settings, merged into each crate's archive whose
    /// first format matches this block's first format, or appended when
    /// none matches. A crate with `archives: false` takes nothing.
    pub archives: Option<ArchiveConfig>,
    /// Default source-archive settings, merged into the top-level `source`
    /// block.
    pub source: Option<SourceConfig>,
    /// Default UPX settings, used as the one top-level `upx` entry when the
    /// top-level `upx` list is empty.
    pub upx: Option<UpxConfig>,

    // --- Packaging axis ---
    /// Default nfpm (deb/rpm/apk) settings, merged into each crate's nfpm
    /// entry with the same `id` (or, with no `id`, `package_name`), or
    /// appended when none matches.
    pub nfpms: Option<NfpmConfig>,
    /// Default snapcraft settings, merged into each crate's snapcraft entry
    /// with the same `name`, or appended when none matches.
    pub snapcrafts: Option<SnapcraftConfig>,
    /// Default flatpak settings, merged into each crate's flatpak entry with
    /// the same `id`, or appended when none matches.
    pub flatpaks: Option<FlatpakConfig>,
    /// Default app-bundle settings, merged into each crate's app-bundle
    /// entry with the same `id`, or appended when none matches.
    pub app_bundles: Option<AppBundleConfig>,
    /// Default DMG settings, merged into each crate's DMG entry with the
    /// same `id`, or appended when none matches.
    pub dmgs: Option<DmgConfig>,
    /// Default macOS PKG settings, merged into each crate's PKG entry with
    /// the same `id`, or appended when none matches.
    pub pkgs: Option<PkgConfig>,
    /// Default MSI settings, merged into each crate's MSI entry with the
    /// same `id`, or appended when none matches.
    pub msis: Option<MsiConfig>,
    /// Default NSIS settings, merged into each crate's NSIS entry with the
    /// same `id`, or appended when none matches.
    pub nsis: Option<NsisConfig>,
    /// Default makeself settings, used as the one top-level `makeselfs`
    /// entry when that list is empty.
    pub makeselves: Option<MakeselfConfig>,
    /// Default install-script settings, used as the one top-level
    /// `install_scripts` entry when that list is empty.
    pub install_scripts: Option<InstallScriptConfig>,
    /// Default SRPM settings, merged into the top-level `srpms` block.
    pub srpms: Option<SrpmConfig>,
    /// Default Docker (V2 API) image settings, merged into each crate's
    /// `dockers_v2` entry with the same `id`, or appended when none matches.
    /// The `docker_v2:` spelling is still accepted.
    #[serde(alias = "docker_v2")]
    pub dockers_v2: Option<DockerV2Config>,

    // --- Publish axis ---
    /// Default publisher settings, one block per publisher, each merged into
    /// every crate's matching `publish.*` block.
    pub publish: Option<PublishDefaults>,

    // --- Sign / notarize / sbom ---
    /// Default artifact signing settings, used as the one top-level `signs`
    /// entry when that list is empty.
    pub sign: Option<SignConfig>,
    /// Default binary-signing settings, used as the one top-level
    /// `binary_signs` entry when that list is empty.
    pub binary_signs: Option<SignConfig>,
    /// Default Docker image signing settings, used as the one top-level
    /// `docker_signs` entry when that list is unset or empty.
    pub docker_signs: Option<DockerSignConfig>,
    /// Default macOS notarization settings, merged into the top-level
    /// `notarize` block.
    pub notarize: Option<NotarizeConfig>,
    /// Default SBOM settings, used as the one top-level `sboms` entry when
    /// that list is empty.
    pub sbom: Option<SbomConfig>,

    // --- Cross-cutting ---
    /// Build targets for every build that sets no `targets` of its own
    /// (e.g. `["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]`). When
    /// unset or empty, anodizer's built-in target list applies.
    pub targets: Option<Vec<String>>,
    /// Environment variables (`KEY=VALUE` strings) put before the top-level
    /// `env`, so a top-level entry with the same key wins.
    pub env: Option<Vec<String>>,
    /// Repo-committed files whose embedded release version is rewritten at
    /// `tag` time (repo-root-relative path strings). Used whole as the
    /// `version_files` of every crate that sets none; a crate's own list
    /// replaces it entirely. Takes the same entries as
    /// `crates[].version_files`, including the anchored `path` + `match`
    /// form.
    pub version_files: Option<Vec<VersionFileEntry>>,
    /// Cross-compilation strategy for every crate that sets no `cross`:
    /// auto, zigbuild, cross, or cargo. Same values as `crates[].cross`.
    pub cross: Option<CrossStrategy>,
    /// Default checksum settings, merged into every crate's `checksum`
    /// block. Same shape as `crates[].checksum`.
    pub checksum: Option<ChecksumConfig>,

    // --- Crate-axis vs workspace-axis (mutually exclusive) ---
    /// Per-crate fallbacks for a config with a top-level `crates:` list:
    /// `tag_template` is used by every crate whose own `tag_template` is
    /// unset or empty. Setting this block without a top-level `crates:`
    /// list, or together with `defaults.workspaces`, fails when the config
    /// loads.
    pub crates: Option<DefaultsCrateBlock>,
    /// Marker for a config with a top-level `workspaces:` list. It holds no
    /// fields. Setting it without a top-level `workspaces:` list, or
    /// together with `defaults.crates`, fails when the config loads.
    pub workspaces: Option<DefaultsWorkspaceBlock>,
}

/// Workspace-default publishers: one block per publisher, merged into the
/// matching `publish.*` block of every crate (the crate's fields win).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PublishDefaults {
    /// Default Homebrew formula settings, merged into every crate's
    /// `publish.homebrew`.
    pub homebrew: Option<HomebrewConfig>,
    /// Default Homebrew Cask settings, merged into every
    /// crate's `publish.homebrew_cask`.
    pub homebrew_cask: Option<HomebrewCaskConfig>,
    /// Default crates.io publish settings, merged into every
    /// crate's `publish.cargo`.
    pub cargo: Option<CargoPublishConfig>,
    /// Default Scoop manifest settings, merged into every crate's
    /// `publish.scoop`.
    pub scoop: Option<ScoopConfig>,
    /// Default WinGet manifest settings, merged into every crate's
    /// `publish.winget`.
    pub winget: Option<WingetConfig>,
    /// Default Chocolatey package settings, merged into every crate's
    /// `publish.chocolatey`.
    pub chocolatey: Option<ChocolateyConfig>,
    /// Default Krew (kubectl plugin manager) settings, merged into every crate's
    /// `publish.krew`.
    pub krew: Option<KrewConfig>,
    /// Default Nix derivation settings, merged into every crate's
    /// `publish.nix`.
    pub nix: Option<NixConfig>,
    /// Default AUR (binary) settings, merged into every crate's
    /// `publish.aur`.
    pub aur: Option<AurConfig>,
    /// Default AUR (source) settings, merged into every crate's
    /// `publish.aur_source`.
    pub aur_source: Option<AurSourceConfig>,
    /// Hooks fired once per failed publisher, the same shape as a crate's
    /// `publish.on_error`. Appended after every crate's own `on_error`
    /// hooks.
    pub on_error: Option<Vec<HookEntry>>,
    /// Hooks fired once per publisher a triggered rollback reverted, the
    /// same shape as a crate's `publish.on_rollback`. Appended after every
    /// crate's own `on_rollback` hooks.
    pub on_rollback: Option<Vec<HookEntry>>,
}

/// Block under `defaults.crates:` that supplies workspace-wide fallbacks for
/// per-crate fields. Also drives the axis-mismatch validator (present only
/// when top-level `crates:` is non-empty).
///
/// `deny_unknown_fields` so that typing `defaults.crates: { foo: bar }`
/// fails to parse.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsCrateBlock {
    /// Git tag template used by every crate whose own `tag_template` is
    /// unset or empty. A crate's own non-empty `tag_template` wins. When
    /// neither is set, the tag template falls through the rest of the
    /// order documented on `crates[].tag_template`.
    pub tag_template: Option<String>,
}

/// Marker block under `defaults.workspaces:` that signals workspace-axis
/// defaults scope. It holds no fields; the axis-mismatch validator reads
/// its presence.
///
/// `deny_unknown_fields` per the same rationale as `DefaultsCrateBlock`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsWorkspaceBlock {}
