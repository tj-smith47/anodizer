use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    AppBundleConfig, ArchiveConfig, ArchivesConfig, BinstallConfig, BlobConfig, ChecksumConfig,
    DmgConfig, DockerDigestConfig, DockerManifestConfig, DockerV2Config, FlatpakConfig, HookEntry,
    MsiConfig, NfpmConfig, NsisConfig, PkgConfig, PublishConfig, ReleaseConfig, SnapcraftConfig,
    StringOrBool, VersionSyncConfig, deserialize_archives_config, deserialize_string_or_bool_opt,
};

// ---------------------------------------------------------------------------
// BuildIgnore — exclude specific os/arch combos from builds
// ---------------------------------------------------------------------------

/// Exclude a specific os/arch combination from the build matrix.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BuildIgnore {
    /// Operating system to exclude (e.g., "linux", "darwin", "windows").
    pub os: String,
    /// Architecture to exclude (e.g., "amd64", "arm64", "386").
    pub arch: String,
}

// ---------------------------------------------------------------------------
// BuildOverride — per-target env, flags, features
// ---------------------------------------------------------------------------

/// Override env, flags, or features for targets matching glob patterns.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BuildOverride {
    /// Glob patterns to match against target triples (e.g., `["x86_64-*", "*-linux-*"]`).
    pub targets: Vec<String>,
    /// Extra environment variables to set for matching targets.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Extra flags to append for matching targets, one per list entry.
    pub flags: Option<Vec<String>>,
    /// Extra features to enable for matching targets.
    pub features: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// CrossStrategy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CrossStrategy {
    Auto,
    Zigbuild,
    Cross,
    Cargo,
}

// ---------------------------------------------------------------------------
// CrateConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CrateConfig {
    /// Crate name as published (must match the Cargo.toml package name).
    pub name: String,
    /// Relative path to the crate directory from the project root.
    pub path: String,
    /// Git tag template used to tag and identify releases (supports templates).
    pub tag_template: String,
    /// Pinned semver version. When set, `anodizer bump --strict` refuses to
    /// edit this crate's `Cargo.toml` to anything other than this value;
    /// without `--strict`, the bump proceeds with a warning. Lets a release
    /// captain freeze a crate's version while still running broad
    /// `--workspace` bumps.
    pub version: Option<String>,
    /// Other crates this crate depends on; ensures release ordering.
    pub depends_on: Option<Vec<String>>,
    /// Build configurations for this crate. One entry per binary by default.
    pub builds: Option<Vec<BuildConfig>>,
    /// Cross-compilation strategy for this crate: auto, zigbuild, cross, or cargo.
    pub cross: Option<CrossStrategy>,
    #[serde(default, deserialize_with = "deserialize_archives_config")]
    #[schemars(schema_with = "archives_schema")]
    pub archives: ArchivesConfig,
    /// Checksum configuration for this crate.
    pub checksum: Option<ChecksumConfig>,
    /// GitHub release configuration for this crate.
    pub release: Option<ReleaseConfig>,
    /// Publishing targets (Homebrew, Scoop, AUR, etc.) for this crate.
    pub publish: Option<PublishConfig>,
    /// Docker V2 image build configurations for this crate (canonical API:
    /// images+tags, annotations, build_args, sbom, disable). The legacy
    /// `docker:` block was removed; this is the only docker surface.
    pub docker_v2: Option<Vec<DockerV2Config>>,
    /// Docker image digest file configuration for this crate.
    pub docker_digest: Option<DockerDigestConfig>,
    /// Docker multi-platform manifest configurations for this crate.
    pub docker_manifests: Option<Vec<DockerManifestConfig>>,
    /// Linux package (deb, rpm, apk) configurations for this crate. Renamed
    /// from `nfpm:` (singular) for spelling parity with `Defaults.nfpms` and
    /// the rest of the plural-name per-crate packaging lists (`dmgs`, `msis`,
    /// `pkgs`, `nsis`, ...). The `nfpm:` spelling is still accepted via serde
    /// alias for back-compat.
    #[serde(alias = "nfpm")]
    pub nfpms: Option<Vec<NfpmConfig>>,
    /// Snapcraft package configurations for this crate.
    pub snapcrafts: Option<Vec<SnapcraftConfig>>,
    /// macOS DMG disk image configurations for this crate.
    pub dmgs: Option<Vec<DmgConfig>>,
    /// Windows MSI installer configurations for this crate.
    pub msis: Option<Vec<MsiConfig>>,
    /// macOS PKG installer configurations for this crate.
    pub pkgs: Option<Vec<PkgConfig>>,
    /// NSIS installer configurations for this crate.
    pub nsis: Option<Vec<NsisConfig>>,
    /// macOS app bundle configurations for this crate.
    pub app_bundles: Option<Vec<AppBundleConfig>>,
    /// Linux Flatpak bundle configurations for this crate.
    pub flatpaks: Option<Vec<FlatpakConfig>>,
    /// Cloud storage (S3/GCS/Azure) upload configurations for this crate.
    pub blobs: Option<Vec<BlobConfig>>,
    /// cargo-binstall metadata configuration for this crate.
    pub binstall: Option<BinstallConfig>,
    /// Automatic version number synchronization configuration for this crate.
    pub version_sync: Option<VersionSyncConfig>,
    /// macOS universal binary (fat binary) configurations for this crate.
    pub universal_binaries: Option<Vec<UniversalBinaryConfig>>,
    /// When true (or template evaluating to "true"), all build outputs are
    /// placed in a flat `dist/` directory instead of `dist/{target}/`.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub no_unique_dist_dir: Option<StringOrBool>,
}

/// Helper schema function for archives (accepts false or array).
pub(super) fn archives_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Option<Vec<ArchiveConfig>>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description = Some("Archive configurations for this crate. Set to false to disable archiving, or provide an array of archive configs.".to_owned());
    }
    schema
}

impl Default for CrateConfig {
    fn default() -> Self {
        CrateConfig {
            name: String::new(),
            path: String::new(),
            tag_template: String::new(),
            version: None,
            depends_on: None,
            builds: None,
            cross: None,
            archives: ArchivesConfig::Configs(vec![]),
            checksum: None,
            release: None,
            publish: None,
            docker_v2: None,
            docker_digest: None,
            docker_manifests: None,
            nfpms: None,
            snapcrafts: None,
            dmgs: None,
            msis: None,
            pkgs: None,
            nsis: None,
            app_bundles: None,
            flatpaks: None,
            blobs: None,
            binstall: None,
            version_sync: None,
            universal_binaries: None,
            no_unique_dist_dir: None,
        }
    }
}

// ---------------------------------------------------------------------------
// UniversalBinaryConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct UniversalBinaryConfig {
    /// Unique identifier for this universal binary, propagated into the
    /// artifact's metadata as `id` (GoReleaser universalbinary.go:42-44).
    #[serde(default)]
    pub id: Option<String>,
    /// Output filename template for the universal binary (supports templates).
    pub name_template: Option<String>,
    /// When true, remove the individual arch binaries after creating the universal binary.
    pub replace: Option<bool>,
    /// Build IDs filter: only combine artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Pre/post hooks around universal binary creation.
    pub hooks: Option<BuildHooksConfig>,
    /// Override the modification timestamp for reproducible universal binaries.
    pub mod_timestamp: Option<String>,
}

// ---------------------------------------------------------------------------
// BuildConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BuildConfig {
    /// Unique identifier for this build, used to reference it from archives and other configs.
    pub id: Option<String>,
    /// Binary name to build (must match a Cargo binary target in the crate).
    ///
    /// Optional so that `defaults.builds` (a path-mirrored template that
    /// applies to every crate) can omit `binary` — the per-crate `builds[]`
    /// entry supplies it. When the binary is absent at the per-crate level
    /// it falls back to the crate's `name` field.
    pub binary: Option<String>,
    /// When true (or template evaluating to "true"), skip this build entirely.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub skip: Option<StringOrBool>,
    /// Target triples to build for. When set, REPLACES `defaults.targets`
    /// for this build (override semantics — the per-build value wins
    /// outright, no concat). When `None`, this build inherits
    /// `defaults.targets` verbatim. Both `cli::commands::helpers::
    /// collect_build_targets` and `stage-build` enforce this rule.
    pub targets: Option<Vec<String>>,
    /// Cargo features to enable for this build.
    pub features: Option<Vec<String>>,
    /// When true, pass --no-default-features to cargo build.
    pub no_default_features: Option<bool>,
    /// Per-target environment variables keyed as {target: {KEY: VALUE}}.
    pub env: Option<HashMap<String, HashMap<String, String>>>,
    /// Copy the binary from another build ID instead of building it.
    pub copy_from: Option<String>,
    /// Extra flags passed to cargo build, one per list entry (e.g., `["--release", "--locked"]`).
    ///
    /// Each entry is template-rendered then passed verbatim as a single argv
    /// token — there is no `sh -c` step and no shell tokenization, so a
    /// rendered value containing spaces stays one argv entry (it is NOT
    /// re-split). Use one list entry per flag, including the flag and its
    /// value as separate entries (`["--target-dir", "/tmp/{{ .Version }}"]`)
    /// when the value itself may contain spaces.
    pub flags: Option<Vec<String>>,
    /// When true, enable reproducible builds by stripping timestamps.
    pub reproducible: Option<bool>,
    /// Per-build hooks executed before and after compilation.
    pub hooks: Option<BuildHooksConfig>,
    /// Exclude specific os/arch combinations from this build's target matrix.
    /// Falls back to `defaults.builds.ignore` when not set.
    pub ignore: Option<Vec<BuildIgnore>>,
    /// Per-target overrides for env, flags, and features for this build.
    /// Falls back to `defaults.builds.overrides` when not set.
    pub overrides: Option<Vec<BuildOverride>>,
    /// Override the cross-compilation tool binary path (e.g., a custom `cross` wrapper).
    /// When set, this binary is used instead of cargo/cross/zigbuild.
    pub cross_tool: Option<String>,
    /// Override the modification timestamp of built binaries for reproducible builds.
    /// Template string (e.g. `"{{ .CommitTimestamp }}"`) or unix timestamp.
    pub mod_timestamp: Option<String>,
    /// Override the cargo subcommand (default: auto-detected "build" or "zigbuild").
    /// Enables e.g. `cargo auditable build` by setting `command: "auditable build"`.
    pub command: Option<String>,
    /// When true (or template evaluating to "true"), place binaries in flat dist/
    /// instead of dist/{target}/. Overrides the crate-level setting.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub no_unique_dist_dir: Option<StringOrBool>,
    /// Deprecated: GoReleaser's `gobinary:` field selects the cargo-like build
    /// command (named after `go build`). Anodizer's tool is always `cargo`,
    /// so the field is captured for back-compat YAML import only and
    /// `apply_build_legacy_aliases` emits a deprecation warning at config-load
    /// time. GR ref: `internal/pipe/build/build.go:93-95`.
    #[serde(default, rename = "gobinary")]
    pub legacy_gobinary: Option<String>,
}

/// Pre/post hook configuration shared across multiple stages. Despite the
/// `Build` prefix in the name, this type is used by both the **build** stage
/// (pre/post compilation hooks) and the **archive** stage (pre/post archiving
/// hooks). The name is kept for backward compatibility with existing configs.
/// **Not** to be confused with the top-level `HooksConfig` (which carries a
/// flat `hooks: Vec<String>` list for `before`/`after` lifecycle hooks).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct BuildHooksConfig {
    /// Commands to run before the build step.
    pub pre: Option<Vec<HookEntry>>,
    /// Commands to run after the build step.
    pub post: Option<Vec<HookEntry>>,
}

/// Pre/post archive hook configuration.
///
/// Archive hooks use `before`/`after` (matching GoReleaser's archive pipe);
/// build hooks use `pre`/`post` (matching GoReleaser's build pipe).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ArchiveHooksConfig {
    /// Commands to run before the archive step.
    pub before: Option<Vec<HookEntry>>,
    /// Commands to run after the archive step.
    pub after: Option<Vec<HookEntry>>,
}
