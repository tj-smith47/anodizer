//! Packaging-axis config types lifted out of the monolithic
//! `crate::config` module.
//!
//! Currently houses [`MakeselfConfig`] + helpers and [`SrpmConfig`].
//! The remaining packaging types (`NfpmConfig`, `SnapcraftConfig`,
//! `FlatpakConfig`, `AppBundleConfig`, `DmgConfig`, `PkgConfig`,
//! `MsiConfig`, `NsisConfig`) still live in `config.rs` and will move
//! here in subsequent extraction passes — each with their dedicated
//! deserializer / `*_schema()` helper alongside.
//!
//! Public API path is preserved by re-exports in `config.rs` so consumers
//! can keep importing from `anodizer_core::config::*`.

use crate::config::{
    NfpmContent, NfpmSignatureConfig, StringOrBool, deserialize_string_or_bool_opt,
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// MakeselfConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MakeselfConfig {
    /// Unique identifier for this makeself config (default: "default").
    pub id: Option<String>,
    /// Build IDs filter: only include artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Output filename template (default includes project, version, os, arch).
    pub filename: Option<String>,
    /// Display name embedded in the self-extracting archive.
    pub name: Option<String>,
    /// Startup script to run when the archive is extracted and executed.
    /// Required — the archive will not be created without this.
    pub script: Option<String>,
    /// Description for LSM metadata.
    pub description: Option<String>,
    /// Maintainer for LSM metadata.
    pub maintainer: Option<String>,
    /// Keywords for LSM metadata.
    pub keywords: Option<Vec<String>>,
    /// Homepage URL for LSM metadata.
    pub homepage: Option<String>,
    /// License for LSM metadata.
    pub license: Option<String>,
    /// Compression algorithm: gzip, bzip2, xz, lzo, compress, or none.
    pub compression: Option<String>,
    /// Extra arguments passed to the makeself command.
    pub extra_args: Option<Vec<String>>,
    /// Additional files to include in the archive.
    pub files: Option<Vec<MakeselfFile>>,
    /// Target OS filter (default: ["linux", "darwin"]).
    pub os: Option<Vec<String>>,
    /// Target architecture filter.
    pub arch: Option<Vec<String>>,
    /// Skip this config. Accepts bool or template string.
    /// Accepts the legacy `disable:` spelling via serde alias for back-compat
    /// with imported configs.
    #[serde(
        alias = "disable",
        deserialize_with = "deserialize_string_or_bool_opt",
        default
    )]
    pub skip: Option<StringOrBool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MakeselfFile {
    /// Source file path (relative to project root).
    /// Accepts the `src:` spelling via serde alias for back-compat
    /// with imported configs.
    #[serde(alias = "src")]
    pub source: String,
    /// Destination path inside the archive.
    /// Accepts the `dst:` spelling via serde alias for back-compat
    /// with imported configs.
    #[serde(alias = "dst")]
    pub destination: Option<String>,
    /// Strip the parent directory from the source path.
    pub strip_parent: Option<bool>,
}

/// Deserialize makeselfs: single object → vec of one, array → vec of many.
pub(crate) fn deserialize_makeselfs<'de, D>(
    deserializer: D,
) -> Result<Vec<MakeselfConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct MakeselfVisitor;

    impl<'de> Visitor<'de> for MakeselfVisitor {
        type Value = Vec<MakeselfConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a makeself config object or an array of makeself config objects")
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut configs = Vec::new();
            while let Some(item) = seq.next_element::<MakeselfConfig>()? {
                configs.push(item);
            }
            Ok(configs)
        }

        fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            let config = MakeselfConfig::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![config])
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(MakeselfVisitor)
}

pub(crate) fn makeselfs_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Vec<MakeselfConfig>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description = Some(
            "Makeself self-extracting archive configurations. Accepts a single object or array."
                .to_owned(),
        );
    }
    schema
}

// ---------------------------------------------------------------------------
// AppImageConfig
// ---------------------------------------------------------------------------

/// AppImage packaging configuration.
///
/// Drives the [AppImage](https://appimage.org/) stage, which bundles a built
/// Linux binary plus its desktop integration (a `.desktop` entry + icon) into
/// a single self-contained, runnable `.AppImage` file via
/// [`linuxdeploy`](https://github.com/linuxdeploy/linuxdeploy)'s `appimage`
/// output plugin. One `.AppImage` is produced per matching Linux target so a
/// multi-arch build yields distinct, non-colliding outputs.
///
/// YAML:
/// ```yaml
/// appimages:
///   - id: helix
///     ids: [helix-bin]
///     desktop: contrib/Helix.desktop
///     icon: contrib/helix.png
///     appdir_extra:
///       - src: runtime/
///         dst: usr/lib/helix/runtime
///     update_information: "gh-releases-zsync|helix-editor|helix|latest|helix-*.AppImage.zsync"
///     runtime_harvest:
///       command: "{{ ArtifactPath }} --populate-runtime {{ HarvestDir }}"
///       dir: runtime/
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AppImageConfig {
    /// Unique identifier for this AppImage config (default: "default").
    pub id: Option<String>,
    /// Build IDs filter: only bundle binaries whose `id` is in this list.
    /// When omitted, every Linux binary in the build matrix is eligible.
    pub ids: Option<Vec<String>>,
    /// Output filename template (default includes project, version, os, arch).
    /// The `.AppImage` extension is appended automatically when absent.
    pub filename: Option<String>,
    /// Application name passed to linuxdeploy via the `APP` env var and used
    /// as the AppDir basename. Defaults to the project name.
    pub name: Option<String>,
    /// Path to the `.desktop` entry file (template). Required — linuxdeploy
    /// will not assemble an AppImage without a desktop file.
    pub desktop: Option<String>,
    /// Path to the application icon (template). Required.
    pub icon: Option<String>,
    /// Extra files / directories copied into the AppDir before linuxdeploy
    /// runs (e.g. a harvested `runtime/` tree). Each entry's `dst` is
    /// interpreted relative to the AppDir root.
    pub appdir_extra: Option<Vec<AppImageExtra>>,
    /// zsync delta-update metadata embedded in the AppImage, passed to
    /// linuxdeploy via the `UPDATE_INFORMATION` env var. When omitted, the
    /// AppImage carries no update information and `UPDATE_INFORMATION` is
    /// left unset (matching linuxdeploy's default).
    pub update_information: Option<String>,
    /// Runtime-asset harvest hook: run the freshly-built binary ONCE on the
    /// host to populate a directory, then bundle that directory into the
    /// AppDir. The harvested data is architecture-independent (grammars,
    /// themes, queries), so it is produced once on the host-native binary and
    /// reused for every target's AppImage.
    pub runtime_harvest: Option<RuntimeHarvest>,
    /// Extra arguments appended to the linuxdeploy command line.
    pub extra_args: Option<Vec<String>>,
    /// Target OS filter (default: ["linux"]). AppImage is a Linux-only format.
    pub os: Option<Vec<String>>,
    /// Target architecture filter. When omitted, every architecture in the
    /// build matrix produces its own `.AppImage`.
    pub arch: Option<Vec<String>>,
    /// Skip this config. Accepts bool or template string.
    #[serde(
        alias = "disable",
        deserialize_with = "deserialize_string_or_bool_opt",
        default
    )]
    pub skip: Option<StringOrBool>,
}

/// A file or directory copied into the AppDir before linuxdeploy assembles
/// the AppImage. Mirrors [`MakeselfFile`]'s `src` / `dst` shape.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AppImageExtra {
    /// Source path (file or directory, relative to project root). A trailing
    /// `/` is not required; directories are copied recursively.
    #[serde(alias = "source")]
    pub src: String,
    /// Destination path inside the AppDir (relative to the AppDir root, e.g.
    /// `usr/lib/helix/runtime`).
    #[serde(alias = "destination")]
    pub dst: String,
}

/// Runtime-asset harvest hook for an AppImage config. The `command` template
/// runs the freshly-built host-native binary to populate `dir`; the resulting
/// directory is then bundled into the AppDir (and staged at a stable dist
/// path so an archive `extra_files` glob can reuse it).
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeHarvest {
    /// Command template run once on the host to populate the harvest dir.
    /// `{{ .ArtifactPath }}` resolves to the host-native binary's path and
    /// `{{ .HarvestDir }}` to the absolute harvest output directory. Run via
    /// `sh -c`.
    pub command: String,
    /// Directory (relative to the AppDir root) the harvested assets are
    /// bundled into. Also the AppDir-relative destination for the staged
    /// host-harvested tree.
    pub dir: String,
}

/// Deserialize appimages: single object → vec of one, array → vec of many.
pub(crate) fn deserialize_appimages<'de, D>(
    deserializer: D,
) -> Result<Vec<AppImageConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct AppImageVisitor;

    impl<'de> Visitor<'de> for AppImageVisitor {
        type Value = Vec<AppImageConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an appimage config object or an array of appimage config objects")
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut configs = Vec::new();
            while let Some(item) = seq.next_element::<AppImageConfig>()? {
                configs.push(item);
            }
            Ok(configs)
        }

        fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            let config = AppImageConfig::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![config])
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(AppImageVisitor)
}

pub(crate) fn appimages_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    let mut schema = generator.subschema_for::<Vec<AppImageConfig>>();
    if let schemars::schema::Schema::Object(ref mut obj) = schema {
        obj.metadata().description =
            Some("AppImage packaging configurations. Accepts a single object or array.".to_owned());
    }
    schema
}

// ---------------------------------------------------------------------------
// SrpmConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SrpmConfig {
    /// Enable source RPM generation. Default: false.
    pub enabled: Option<bool>,
    /// Package name (default: project_name).
    pub package_name: Option<String>,
    /// Output filename template.
    pub file_name_template: Option<String>,
    /// Path to the RPM spec file template.
    pub spec_file: Option<String>,
    /// RPM epoch.
    pub epoch: Option<String>,
    /// RPM section.
    pub section: Option<String>,
    /// Package maintainer.
    pub maintainer: Option<String>,
    /// Package vendor.
    pub vendor: Option<String>,
    /// Summary line.
    pub summary: Option<String>,
    /// RPM group.
    pub group: Option<String>,
    /// Package description.
    pub description: Option<String>,
    /// License identifier.
    pub license: Option<String>,
    /// License file name to include.
    pub license_file_name: Option<String>,
    /// Homepage URL.
    pub url: Option<String>,
    /// RPM packager field.
    pub packager: Option<String>,
    /// Compression algorithm (gzip, xz, zstd, none).
    pub compression: Option<String>,
    /// Documentation files to include.
    pub docs: Option<Vec<String>>,
    /// Additional contents to include in the source RPM. Shares the unified
    /// [`NfpmContent`] type with nFPM contents; SRPM-style `source:` /
    /// `destination:` / `type:` keys are accepted via serde aliases.
    pub contents: Option<Vec<NfpmContent>>,
    /// RPM signature configuration. Shares the unified
    /// [`NfpmSignatureConfig`] type with nFPM.
    pub signature: Option<NfpmSignatureConfig>,
    /// Map of binary name → install path declared in the spec's `%files`
    /// section. Each entry tells the generated
    /// `.spec` which installed file the package owns. When omitted, each
    /// binary produced by the build for this crate defaults to
    /// `%{_bindir}/<name>` (i.e. `/usr/bin/<name>`, the RPM-idiomatic
    /// location for a built binary). Provide this only to override the
    /// install path or to declare extra owned paths. Stored as a
    /// `BTreeMap` so the emitted `%files` section iterates in
    /// deterministic key order.
    pub bins: Option<BTreeMap<String, String>>,
    /// Filesystem prefixes the package may install to (RPM `Prefix:` tag).
    /// Each entry becomes one `Prefix:` directive — relocatable RPMs need
    /// at least one prefix declared.
    pub prefixes: Option<Vec<String>>,
    /// Override the build host recorded in the RPM header. Useful for
    /// reproducible builds where the actual hostname leaks build-env detail.
    pub build_host: Option<String>,
    /// `%pretrans` scriptlet — executed on the package transaction *before*
    /// any package in the transaction is installed. Path to a script file.
    pub pretrans: Option<String>,
    /// `%posttrans` scriptlet — executed *after* all packages in the
    /// transaction have been installed. Path to a script file.
    pub posttrans: Option<String>,
    /// Prerelease suffix appended to the version (e.g. `rc1`, `beta2`).
    /// Prerelease component of the package version.
    pub prerelease: Option<String>,
    /// Build metadata appended to the version (e.g. git commit hash).
    /// Version-metadata component of the package version.
    pub version_metadata: Option<String>,
    /// Skip this config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}

// SRPM signatures share [`NfpmSignatureConfig`]; the SRPM-style
// `passphrase:` key is accepted as a serde alias for `key_passphrase:`.
//
// SRPM contents share [`NfpmContent`]; both the canonical `src` / `dst`
// keys and the SRPM-style `source` / `destination` aliases parse.

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrapper exercising the `deserialize_makeselfs` custom visitor through
    /// the same `deserialize_with` attribute the real config uses.
    #[derive(Deserialize)]
    struct MakeselfWrap {
        #[serde(deserialize_with = "deserialize_makeselfs", default)]
        makeselfs: Vec<MakeselfConfig>,
    }

    #[derive(Deserialize)]
    struct AppImageWrap {
        #[serde(deserialize_with = "deserialize_appimages", default)]
        appimages: Vec<AppImageConfig>,
    }

    fn makeselfs(yaml: &str) -> Vec<MakeselfConfig> {
        serde_yaml_ng::from_str::<MakeselfWrap>(yaml)
            .expect("valid makeselfs YAML")
            .makeselfs
    }

    fn appimages(yaml: &str) -> Vec<AppImageConfig> {
        serde_yaml_ng::from_str::<AppImageWrap>(yaml)
            .expect("valid appimages YAML")
            .appimages
    }

    #[test]
    fn makeself_single_object_becomes_one_element_vec() {
        let v = makeselfs("makeselfs:\n  id: solo\n  script: run.sh\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id.as_deref(), Some("solo"));
        assert_eq!(v[0].script.as_deref(), Some("run.sh"));
    }

    #[test]
    fn makeself_array_preserves_order() {
        let v = makeselfs("makeselfs:\n  - id: a\n  - id: b\n");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id.as_deref(), Some("a"));
        assert_eq!(v[1].id.as_deref(), Some("b"));
    }

    #[test]
    fn makeself_null_is_empty_vec() {
        assert!(makeselfs("makeselfs: null").is_empty());
    }

    #[test]
    fn makeself_missing_is_empty_vec() {
        assert!(makeselfs("other: 1").is_empty());
    }

    #[test]
    fn makeself_skip_accepts_bool_and_template_via_string_or_bool() {
        let v = makeselfs("makeselfs:\n  id: x\n  skip: true\n");
        assert!(v[0].skip.as_ref().unwrap().as_bool());

        let t = makeselfs("makeselfs:\n  id: x\n  skip: \"{{ .IsSnapshot }}\"\n");
        assert!(t[0].skip.as_ref().unwrap().is_template());
    }

    #[test]
    fn makeself_disable_alias_folds_into_skip() {
        let v = makeselfs("makeselfs:\n  id: x\n  disable: true\n");
        assert!(v[0].skip.as_ref().unwrap().as_bool());
    }

    #[test]
    fn makeself_file_src_dst_aliases_map_to_source_destination() {
        let v = makeselfs("makeselfs:\n  id: x\n  files:\n    - src: in.txt\n      dst: out.txt\n");
        let f = &v[0].files.as_ref().unwrap()[0];
        assert_eq!(f.source, "in.txt");
        assert_eq!(f.destination.as_deref(), Some("out.txt"));
    }

    #[test]
    fn makeself_file_canonical_source_destination_keys_also_parse() {
        let v = makeselfs("makeselfs:\n  id: x\n  files:\n    - source: a\n      destination: b\n");
        let f = &v[0].files.as_ref().unwrap()[0];
        assert_eq!(f.source, "a");
        assert_eq!(f.destination.as_deref(), Some("b"));
    }

    #[test]
    fn makeself_rejects_unknown_field() {
        let err = serde_yaml_ng::from_str::<MakeselfWrap>("makeselfs:\n  id: x\n  bogus: 1\n");
        assert!(err.is_err(), "deny_unknown_fields must reject typos");
    }

    #[test]
    fn appimage_single_object_becomes_one_element_vec() {
        let v = appimages("appimages:\n  id: hx\n  desktop: a.desktop\n  icon: a.png\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id.as_deref(), Some("hx"));
        assert_eq!(v[0].desktop.as_deref(), Some("a.desktop"));
    }

    #[test]
    fn appimage_array_and_null_paths() {
        assert_eq!(appimages("appimages:\n  - id: a\n  - id: b").len(), 2);
        assert!(appimages("appimages: null").is_empty());
    }

    #[test]
    fn appimage_extra_source_destination_aliases_map_to_src_dst() {
        let v = appimages(
            "appimages:\n  id: x\n  appdir_extra:\n    - source: runtime/\n      destination: usr/lib/runtime\n",
        );
        let extra = &v[0].appdir_extra.as_ref().unwrap()[0];
        assert_eq!(extra.src, "runtime/");
        assert_eq!(extra.dst, "usr/lib/runtime");
    }

    #[test]
    fn appimage_runtime_harvest_parses_command_and_dir() {
        let v = appimages(
            "appimages:\n  id: x\n  runtime_harvest:\n    command: \"{{ .ArtifactPath }} --populate {{ .HarvestDir }}\"\n    dir: runtime/\n",
        );
        let rh = v[0].runtime_harvest.as_ref().unwrap();
        assert!(rh.command.contains("--populate"));
        assert_eq!(rh.dir, "runtime/");
    }

    #[test]
    fn appimage_disable_alias_folds_into_skip() {
        let v = appimages("appimages:\n  id: x\n  disable: true\n");
        assert!(v[0].skip.as_ref().unwrap().as_bool());
    }

    #[test]
    fn srpm_enabled_defaults_to_none_and_parses_when_set() {
        let off: SrpmConfig = serde_yaml_ng::from_str("package_name: foo").unwrap();
        assert!(off.enabled.is_none());
        let on: SrpmConfig = serde_yaml_ng::from_str("enabled: true").unwrap();
        assert_eq!(on.enabled, Some(true));
    }

    #[test]
    fn srpm_bins_stored_in_deterministic_btreemap_order() {
        let cfg: SrpmConfig =
            serde_yaml_ng::from_str("bins:\n  zeta: /usr/bin/zeta\n  alpha: /usr/bin/alpha\n")
                .unwrap();
        let keys: Vec<&String> = cfg.bins.as_ref().unwrap().keys().collect();
        assert_eq!(keys, vec!["alpha", "zeta"], "BTreeMap iterates sorted");
    }

    #[test]
    fn srpm_skip_template_string_parses() {
        let cfg: SrpmConfig = serde_yaml_ng::from_str("skip: \"{{ .Env.SKIP }}\"").unwrap();
        assert!(cfg.skip.as_ref().unwrap().is_template());
    }

    #[test]
    fn srpm_rejects_unknown_field() {
        assert!(serde_yaml_ng::from_str::<SrpmConfig>("nope: 1").is_err());
    }

    #[test]
    fn makeself_empty_array_is_empty_vec() {
        // visit_seq with zero elements is a distinct branch from visit_unit.
        assert!(makeselfs("makeselfs: []").is_empty());
    }

    #[test]
    fn makeself_full_metadata_block_parses() {
        let v = makeselfs(
            "makeselfs:\n  id: x\n  script: run.sh\n  compression: xz\n  os: [linux, darwin]\n  arch: [amd64]\n  extra_args: [--nox11]\n  keywords: [cli, tool]\n",
        );
        let m = &v[0];
        assert_eq!(m.compression.as_deref(), Some("xz"));
        assert_eq!(
            m.os.as_deref(),
            Some(&["linux".to_string(), "darwin".to_string()][..])
        );
        assert_eq!(m.arch.as_deref(), Some(&["amd64".to_string()][..]));
        assert_eq!(m.extra_args.as_deref(), Some(&["--nox11".to_string()][..]));
        assert_eq!(
            m.keywords.as_deref(),
            Some(&["cli".to_string(), "tool".to_string()][..])
        );
    }

    #[test]
    fn makeself_file_strip_parent_round_trips() {
        let v = makeselfs(
            "makeselfs:\n  id: x\n  files:\n    - source: a/b\n      strip_parent: true\n",
        );
        assert_eq!(v[0].files.as_ref().unwrap()[0].strip_parent, Some(true));
    }

    #[test]
    fn makeself_file_rejects_unknown_field() {
        let err = serde_yaml_ng::from_str::<MakeselfWrap>(
            "makeselfs:\n  id: x\n  files:\n    - source: a\n      bogus: 1\n",
        );
        assert!(err.is_err(), "MakeselfFile must deny unknown fields");
    }

    #[test]
    fn appimage_empty_array_is_empty_vec() {
        assert!(appimages("appimages: []").is_empty());
    }

    #[test]
    fn appimage_update_information_and_os_arch_filters_parse() {
        let v = appimages(
            "appimages:\n  id: x\n  update_information: \"gh-releases-zsync|me|app|latest|app-*.AppImage.zsync\"\n  os: [linux]\n  arch: [amd64, arm64]\n",
        );
        let a = &v[0];
        assert!(
            a.update_information
                .as_deref()
                .unwrap()
                .starts_with("gh-releases-zsync")
        );
        assert_eq!(a.os.as_deref(), Some(&["linux".to_string()][..]));
        assert_eq!(
            a.arch.as_deref(),
            Some(&["amd64".to_string(), "arm64".to_string()][..])
        );
    }

    #[test]
    fn appimage_extra_rejects_unknown_field() {
        let err = serde_yaml_ng::from_str::<AppImageWrap>(
            "appimages:\n  id: x\n  appdir_extra:\n    - src: a\n      dst: b\n      bogus: 1\n",
        );
        assert!(err.is_err(), "AppImageExtra must deny unknown fields");
    }

    #[test]
    fn appimage_runtime_harvest_rejects_unknown_field() {
        let err = serde_yaml_ng::from_str::<AppImageWrap>(
            "appimages:\n  id: x\n  runtime_harvest:\n    command: c\n    dir: d\n    bogus: 1\n",
        );
        assert!(err.is_err(), "RuntimeHarvest must deny unknown fields");
    }

    #[test]
    fn appimage_skip_template_marks_template_not_bool() {
        let v = appimages("appimages:\n  id: x\n  skip: \"{{ .IsSnapshot }}\"\n");
        let skip = v[0].skip.as_ref().unwrap();
        assert!(skip.is_template());
        assert!(
            !skip.as_bool(),
            "an unrendered template is not a literal true"
        );
    }

    #[test]
    fn srpm_contents_and_prefixes_and_scriptlets_parse() {
        let cfg: SrpmConfig = serde_yaml_ng::from_str(
            "enabled: true\nprefixes: [/usr, /etc]\npretrans: scripts/pre.sh\nposttrans: scripts/post.sh\ncontents:\n  - src: a.conf\n    dst: /etc/a.conf\n    type: config\n",
        )
        .unwrap();
        assert_eq!(
            cfg.prefixes.as_deref(),
            Some(&["/usr".to_string(), "/etc".to_string()][..])
        );
        assert_eq!(cfg.pretrans.as_deref(), Some("scripts/pre.sh"));
        assert_eq!(cfg.posttrans.as_deref(), Some("scripts/post.sh"));
        let contents = cfg.contents.as_ref().unwrap();
        assert_eq!(contents[0].src, "a.conf");
        assert_eq!(contents[0].dst, "/etc/a.conf");
        assert_eq!(contents[0].content_type.as_deref(), Some("config"));
    }

    #[test]
    fn srpm_version_components_parse() {
        let cfg: SrpmConfig =
            serde_yaml_ng::from_str("prerelease: rc1\nversion_metadata: gitabc123\n").unwrap();
        assert_eq!(cfg.prerelease.as_deref(), Some("rc1"));
        assert_eq!(cfg.version_metadata.as_deref(), Some("gitabc123"));
    }
}
