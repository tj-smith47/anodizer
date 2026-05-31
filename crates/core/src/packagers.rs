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
    /// with imported GoReleaser configs (GR makeself uses `disable: string`).
    #[serde(
        alias = "disable",
        deserialize_with = "deserialize_string_or_bool_opt",
        default
    )]
    pub skip: Option<StringOrBool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MakeselfFile {
    /// Source file path (relative to project root).
    /// Accepts the GoReleaser `src:` spelling via serde alias for back-compat
    /// with imported configs (GR `MakeselfFile.Source` is keyed `src`).
    #[serde(alias = "src")]
    pub source: String,
    /// Destination path inside the archive.
    /// Accepts the GoReleaser `dst:` spelling via serde alias for back-compat
    /// with imported configs (GR `MakeselfFile.Destination` is keyed `dst`).
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
// SrpmConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
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
    /// section, mirroring GR `SRPM.Bins`. Each entry tells the generated
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
    /// Mirrors GR `NFPM.Prerelease`.
    pub prerelease: Option<String>,
    /// Build metadata appended to the version (e.g. git commit hash).
    /// Mirrors GR `NFPM.VersionMetadata`.
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
