use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    Amd64Variant, AppBundleConfig, ArchiveConfig, ArchivesConfig, BinstallConfig, BlobConfig,
    ChecksumConfig, DmgConfig, DockerDigestConfig, DockerManifestConfig, DockerV2Config,
    FlatpakConfig, HookEntry, HooksConfig, MsiConfig, NfpmConfig, NsisConfig, PkgConfig,
    PublishConfig, ReleaseConfig, SnapcraftConfig, StringOrBool, VersionSyncConfig,
    deserialize_archives_config, deserialize_string_or_bool_opt,
};

// ---------------------------------------------------------------------------
// BuildIgnore — exclude specific os/arch combos from builds
// ---------------------------------------------------------------------------

/// Exclude a specific os/arch combination from the build matrix.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
// BuilderKind — `cargo` (compile from source) vs `prebuilt` (import binary)
// ---------------------------------------------------------------------------

/// Selects which builder a `builds[]` entry uses. `Cargo` (the default) runs
/// `cargo build` and discovers the resulting binary under
/// `target/<triple>/<profile>/`. `Prebuilt` skips compilation entirely and
/// imports a binary the operator already placed on disk via the per-build
/// `prebuilt:` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BuilderKind {
    /// Build the binary by invoking `cargo build` (or `cross` / `zigbuild`
    /// per the `cross:` strategy). Default when no `builder:` is set.
    #[default]
    Cargo,
    /// Import a binary already staged on disk instead of compiling. Pairs
    /// with the `prebuilt:` block on the same build entry.
    Prebuilt,
}

// ---------------------------------------------------------------------------
// PrebuiltConfig — path template for the `prebuilt` builder
// ---------------------------------------------------------------------------

/// Per-build options for `builder: prebuilt`. Required when `builder: prebuilt`
/// is set on the same `builds[]` entry.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PrebuiltConfig {
    /// Template path to the imported binary on disk. Rendered once per
    /// target with these template variables available in addition to the
    /// project-wide globals (`Version`, `ProjectName`, ...):
    ///
    /// - `{{ Target }}` — the full Rust target triple
    ///   (e.g. `x86_64-unknown-linux-gnu`).
    /// - `{{ Os }}` — the OS slug (`linux`, `darwin`,
    ///   `windows`, ...).
    /// - `{{ Arch }}` — the architecture slug (`amd64`,
    ///   `arm64`, `armv7`, ...).
    /// - `{{ Amd64 }}` — AMD64 micro-architecture variant
    ///   (`v1` / `v2` / `v3` / `v4`); set for `x86_64-*` triples.
    /// - `{{ Arm64 }}` — ARM64 micro-architecture variant (`v8`); set for
    ///   `aarch64-*` triples.
    /// - `{{ Arm }}` — ARM micro-architecture variant (`6` / `7`); set for
    ///   `armv6*` / `armv7*` triples.
    /// - `{{ I386 }}` — i386 micro-architecture variant (`sse2`); set for
    ///   `i686-*` / `i386-*` / `i586-*` triples.
    /// - `{{ ArtifactExt }}` — `.exe` on Windows targets, empty elsewhere.
    /// - `{{ ArtifactID }}` — the build entry's `id:` (empty when unset).
    ///
    /// The rendered path is `stat()`-ed before the import, unless
    /// `--dry-run` is active, in which case the stat is skipped and the
    /// path is accepted as given. A missing file, a permission error, or
    /// any other I/O failure aborts the build with a message that names
    /// both the rendered path and the originating target triple, matching
    /// the "build will fail" contract.
    ///
    /// Recommendation: place the staged binaries OUTSIDE `dist/`. The
    /// release pipeline removes `dist/` on every run; pointing `path:` at
    /// `dist/...` will resolve against an empty directory.
    pub path: String,
}

// ---------------------------------------------------------------------------
// CrateConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
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
    /// `docker:` block was removed; this is the only docker surface. The
    /// `docker_v2:` spelling is still accepted via serde alias for back-compat.
    #[serde(alias = "docker_v2")]
    pub dockers_v2: Option<Vec<DockerV2Config>>,
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
    /// Repo-committed files that embed this crate's release version outside
    /// `Cargo.toml` (repo-root-relative path strings). At `tag` time each file
    /// has its occurrences of the old version rewritten to the new version —
    /// both bare and `v`-prefixed forms, word-boundary anchored — and is staged
    /// into the same bump commit as this crate's `Cargo.toml`. Overrides the
    /// workspace-level `defaults.version_files`.
    pub version_files: Option<Vec<String>>,
    /// macOS universal binary (fat binary) configurations for this crate.
    pub universal_binaries: Option<Vec<UniversalBinaryConfig>>,
    /// When true (or template evaluating to "true"), all build outputs are
    /// placed in a flat `dist/` directory instead of `dist/{target}/`.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub no_unique_dist_dir: Option<StringOrBool>,
    /// Hooks that run inside THIS crate's scope at the start of the release,
    /// before the build. Distinct from the top-level `before:`, which fires
    /// ONCE around the whole release; these fire once per crate with that
    /// crate's version/tag template vars anchored, so `cmd` / `dir` / `env` /
    /// `if` render against the crate's own `Version` / `Tag` / `ProjectName`.
    /// A non-zero exit aborts the release.
    ///
    /// Fires once per crate in EVERY multi-crate mode — workspace per-crate
    /// AND workspace lockstep with multiple publisher crates — in both a full
    /// `anodizer release` and `anodizer release --publish-only`, matching the
    /// per-crate iteration of `before_publish:` and the publishers. With an
    /// explicit `--crate` subset only the selected crates' hooks fire. No-op
    /// in a single-crate config with no `crates:` block (use the top-level
    /// `before:` there).
    pub before: Option<HooksConfig>,
    /// Hooks that run inside THIS crate's scope at the end of the release,
    /// after the crate's publish dispatch (and post-publish verification)
    /// completes. Per-crate counterpart of the top-level `after:` (which fires
    /// once around the whole release). Same per-crate firing semantics across
    /// all modes, template surface, and abort semantics as the per-crate
    /// `before:`.
    pub after: Option<HooksConfig>,
    /// Hooks that run immediately before THIS crate's publishers dispatch,
    /// once per matching artifact (the same per-artifact semantics as the
    /// top-level `before_publish:`), scoped to the crate's own artifacts and
    /// template vars. Honors the per-entry `ids:` / `artifacts:` filters. A
    /// non-zero exit aborts the release before that crate publishes to any
    /// registry. The top-level `before_publish:` still fires once over the
    /// full artifact set; this one targets a single crate's artifacts.
    pub before_publish: Option<HooksConfig>,
}

/// Helper schema function for archives (accepts false or array).
pub(super) fn archives_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = generator.subschema_for::<Option<Vec<ArchiveConfig>>>();
    schema.ensure_object().insert(
        "description".to_owned(),
        "Archive configurations for this crate. Set to false to disable archiving, or provide an array of archive configs.".into(),
    );
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
            dockers_v2: None,
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
            version_files: None,
            universal_binaries: None,
            no_unique_dist_dir: None,
            before: None,
            after: None,
            before_publish: None,
        }
    }
}

// ---------------------------------------------------------------------------
// UniversalBinaryConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UniversalBinaryConfig {
    /// Unique identifier for this universal binary, propagated into the
    /// artifact's metadata as `id`.
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
#[serde(default, deny_unknown_fields)]
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
    /// value as separate entries (`["--target-dir", "/tmp/{{ Version }}"]`)
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
    /// Template string (e.g. `"{{ CommitTimestamp }}"`) or unix timestamp.
    pub mod_timestamp: Option<String>,
    /// Override the cargo subcommand (default: auto-detected "build" or "zigbuild").
    /// Enables e.g. `cargo auditable build` by setting `command: "auditable build"`.
    pub command: Option<String>,
    /// When true (or template evaluating to "true"), place binaries in flat dist/
    /// instead of dist/{target}/. Overrides the crate-level setting.
    #[serde(default, deserialize_with = "deserialize_string_or_bool_opt")]
    pub no_unique_dist_dir: Option<StringOrBool>,
    /// Declared x86-64 micro-architecture level for this build's artifacts:
    /// `"v2"`, `"v3"`, `"v4"`, or the `"v1"` baseline. When set, it overrides
    /// the level anodizer detects from the resolved build env — the
    /// config-map and inherited process environment merged under cargo's own
    /// mutually-exclusive source order (`CARGO_ENCODED_RUSTFLAGS`, then
    /// `RUSTFLAGS`, then `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, first present
    /// wins) carrying `-Ctarget-cpu=x86-64-v<N>` —
    /// for BOTH the artifact's `amd64_variant` metadata — which names a
    /// v2/v3-tuned group's archives (`…_amd64v3.tar.gz`) — and the config-time
    /// asset-name derivation feeding cargo-binstall `pkg_url` and the
    /// `curl | sh` installer's case table. Declare it when the tuning value is
    /// only resolvable at build time (e.g. `RUSTFLAGS: "{{ .Env.CI_FLAGS }}"`)
    /// or when importing a tuned binary via `builder: prebuilt`. Ignored for
    /// non-x86_64 targets.
    ///
    /// Typed as [`Amd64Variant`], so any value outside `"v1"`..`"v4"` is
    /// rejected when the config is parsed — on every axis the field can be
    /// set from (`crates[]`, `workspaces[].crates[]`, and `defaults.builds`).
    pub amd64_variant: Option<Amd64Variant>,
    /// Builder to use for this entry. `cargo` (the default when omitted)
    /// runs `cargo build`. `prebuilt` skips compilation and imports a
    /// binary the operator already produced via the `prebuilt:` block.
    ///
    /// When `builder: prebuilt`, `targets:` MUST be set explicitly — no
    /// `defaults.targets` fallback — and the `prebuilt.path` template is
    /// rendered per target then stat()-ed before the import.
    pub builder: Option<BuilderKind>,
    /// Options for the `prebuilt` builder. Required when
    /// `builder: prebuilt`; ignored (with a config-load warning) otherwise.
    pub prebuilt: Option<PrebuiltConfig>,
    /// Accepted-but-ignored legacy key. anodizer always drives the cargo
    /// toolchain (or `cross` / `zigbuild`) directly, so there is no
    /// pluggable build binary to point at. The field is parsed only so that
    /// configs imported from a Go-style tool keep loading instead of
    /// hard-failing under `deny_unknown_fields`; its value is never read.
    /// Use `command:` to override the cargo subcommand instead.
    #[doc(hidden)]
    #[serde(default, skip_serializing)]
    pub gobinary: Option<String>,
}

/// Pre/post hook configuration shared across multiple stages. Despite the
/// `Build` prefix in the name, this type is used by both the **build** stage
/// (pre/post compilation hooks) and the **archive** stage (pre/post archiving
/// hooks). The name is kept for backward compatibility with existing configs.
/// **Not** to be confused with the top-level `HooksConfig` (which carries a
/// flat `hooks: Vec<String>` list for `before`/`after` lifecycle hooks).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BuildHooksConfig {
    /// Commands to run before the build step.
    pub pre: Option<Vec<HookEntry>>,
    /// Commands to run after the build step.
    pub post: Option<Vec<HookEntry>>,
}

/// Pre/post archive hook configuration.
///
/// Archive hooks use `before`/`after`; build hooks use `pre`/`post`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveHooksConfig {
    /// Commands to run before the archive step.
    pub before: Option<Vec<HookEntry>>,
    /// Commands to run after the archive step.
    pub after: Option<Vec<HookEntry>>,
}

#[cfg(test)]
mod legacy_field_tests {
    use super::*;

    /// An imported config carrying the removed Go-style `gobinary:` key must
    /// still parse (accept-and-ignore) rather than hard-fail under
    /// `deny_unknown_fields`. The value is captured but never read.
    #[test]
    fn build_gobinary_is_accepted_and_ignored() {
        let build: BuildConfig =
            serde_yaml_ng::from_str("binary: myapp\ngobinary: /usr/local/bin/go")
                .expect("gobinary: must parse as an accepted-but-ignored legacy field");
        assert_eq!(build.binary.as_deref(), Some("myapp"));
        assert_eq!(build.gobinary.as_deref(), Some("/usr/local/bin/go"));
    }

    /// `gobinary` is parse-only and must not round-trip into serialized output
    /// (it carries no behavior).
    #[test]
    fn build_gobinary_is_not_serialized() {
        let build = BuildConfig {
            gobinary: Some("/usr/local/bin/go".to_string()),
            ..Default::default()
        };
        let yaml = serde_yaml_ng::to_string(&build).unwrap();
        assert!(
            !yaml.contains("gobinary"),
            "gobinary must be skipped on serialize: {yaml}"
        );
    }
}
