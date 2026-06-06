use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// SbomConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SbomConfig {
    /// Unique identifier for this SBOM config (default: "default").
    pub id: Option<String>,
    /// Command to run for SBOM generation (default: "syft").
    pub cmd: Option<String>,
    /// Environment variables to pass to the command, as `KEY=VALUE` strings.
    /// Order is preserved. Values are template-rendered before being set.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Command-line arguments (supports templates and $artifact, $document vars).
    pub args: Option<Vec<String>>,
    /// Output document path templates (supports templates).
    pub documents: Option<Vec<String>>,
    /// Which artifacts to catalog: "source", "archive", "binary", "package", "diskimage", "installer", "any" (default: "archive").
    pub artifacts: Option<String>,
    /// Filter by artifact IDs (ignored if artifacts="source").
    pub ids: Option<Vec<String>>,
    /// Skip this SBOM config. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}

impl SbomConfig {
    /// Default `id` when an SBOM config has none (`"default"`).
    pub const DEFAULT_ID: &'static str = "default";

    /// Default SBOM-generation command
    /// (`cfg.Cmd = "syft"`).
    pub const DEFAULT_CMD: &'static str = "syft";

    /// Default `artifacts` filter
    /// (`cfg.Artifacts = "archive"`).
    pub const DEFAULT_ARTIFACTS: &'static str = "archive";

    /// Default document-path template when `artifacts: binary`. Includes
    /// per-target Os/Arch suffix so per-arch SBOMs don't collide.
    /// Default value.
    pub const DEFAULT_DOCUMENT_BINARY: &'static str =
        "{{ .Binary }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}.sbom.json";

    /// Default document-path template for any non-binary, non-any
    /// `artifacts:` filter.
    pub const DEFAULT_DOCUMENT_OTHER: &'static str = "{{ .ArtifactName }}.sbom.json";

    /// Default `args` for the syft command, using shell-style `$artifact` /
    /// `$document` placeholders verbatim — the arg-renderer rewrites
    /// these to per-artifact values at execution time.
    pub const DEFAULT_SYFT_ARGS: &[&'static str] = &[
        "$artifact",
        "--output",
        "spdx-json=$document",
        "--enrich",
        "all",
    ];

    /// Env entry that syft requires to emit file paths in the SBOM
    /// when cataloging archives or source.
    pub const DEFAULT_SYFT_ENV_KEY: &'static str = "SYFT_FILE_METADATA_CATALOGER_ENABLED";
    pub const DEFAULT_SYFT_ENV_VAL: &'static str = "true";

    /// Resolve the SBOM-config id, falling back to `"default"`.
    pub fn resolved_id(&self) -> &str {
        self.id.as_deref().unwrap_or(Self::DEFAULT_ID)
    }

    /// Resolve the SBOM command, falling back to `"syft"`.
    pub fn resolved_cmd(&self) -> &str {
        self.cmd.as_deref().unwrap_or(Self::DEFAULT_CMD)
    }

    /// Resolve the `artifacts:` filter, falling back to `"archive"`.
    pub fn resolved_artifacts(&self) -> &str {
        self.artifacts.as_deref().unwrap_or(Self::DEFAULT_ARTIFACTS)
    }

    /// Resolve `documents`, falling back to the artifact-type-specific
    /// default when unset. Caller should pass the result of
    /// [`Self::resolved_artifacts`] for `artifacts`.
    pub fn resolved_documents(&self, artifacts: &str) -> Vec<String> {
        self.documents.clone().unwrap_or_else(|| match artifacts {
            "binary" => vec![Self::DEFAULT_DOCUMENT_BINARY.to_string()],
            "any" => vec![],
            _ => vec![Self::DEFAULT_DOCUMENT_OTHER.to_string()],
        })
    }

    /// Resolve `args`, falling back to [`Self::DEFAULT_SYFT_ARGS`] when
    /// `cmd` is `"syft"`; empty vec otherwise (args are only initialized
    /// when cmd is syft, and left
    /// args empty for other cmds).
    pub fn resolved_args(&self, cmd: &str) -> Vec<String> {
        self.args.clone().unwrap_or_else(|| {
            if cmd == Self::DEFAULT_CMD {
                Self::DEFAULT_SYFT_ARGS
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            } else {
                Vec::new()
            }
        })
    }

    /// Default env additions for the syft sub-process. Empty unless cmd
    /// is syft AND artifacts is source/archive — in which case syft
    /// needs the file-metadata cataloger enabled to produce file paths
    /// in the SBOM.
    pub fn default_syft_env_for(cmd: &str, artifacts: &str) -> Vec<(String, String)> {
        if cmd == Self::DEFAULT_CMD && matches!(artifacts, "source" | "archive") {
            vec![(
                Self::DEFAULT_SYFT_ENV_KEY.to_string(),
                Self::DEFAULT_SYFT_ENV_VAL.to_string(),
            )]
        } else {
            Vec::new()
        }
    }
}

/// Custom deserializer for the `sboms` / `sbom` field.
/// Accepts:
///   - null/missing → empty vec (via serde default)
///   - a single object → vec of one SbomConfig
///   - an array → vec of SbomConfig
pub(super) fn deserialize_sboms<'de, D>(deserializer: D) -> Result<Vec<SbomConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct SbomsVisitor;

    impl<'de> Visitor<'de> for SbomsVisitor {
        type Value = Vec<SbomConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an SBOM config object or an array of SBOM config objects")
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut configs = Vec::new();
            while let Some(item) = seq.next_element::<SbomConfig>()? {
                configs.push(item);
            }
            Ok(configs)
        }

        fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            let config = SbomConfig::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![config])
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(SbomsVisitor)
}
