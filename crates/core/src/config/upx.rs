use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// UpxConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UpxConfig {
    /// Unique identifier for this UPX config.
    pub id: Option<String>,
    /// Build IDs filter: only compress binaries from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Whether to compress binaries with UPX.
    /// Accepts a bool or a template string that evaluates to a bool.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub enabled: Option<StringOrBool>,
    /// UPX executable path or name (default: "upx").
    pub binary: String,
    /// Extra arguments passed to UPX (e.g., ["-9", "--brute"]).
    pub args: Vec<String>,
    /// When true, fail the build if UPX is not found.
    pub required: bool,
    /// Target triples to compress binaries for (empty means all targets).
    pub targets: Option<Vec<String>>,
    /// UPX compression level string (e.g., "1"-"9", "best"). Maps to `--compress` flag.
    pub compress: Option<String>,
    /// Use LZMA compression (--lzma flag).
    pub lzma: Option<bool>,
    /// Use brute-force compression (--brute flag). Very slow but produces smallest output.
    pub brute: Option<bool>,
}

impl Default for UpxConfig {
    fn default() -> Self {
        UpxConfig {
            id: None,
            ids: None,
            enabled: None,
            binary: "upx".to_string(),
            args: Vec::new(),
            required: false,
            targets: None,
            compress: None,
            lzma: None,
            brute: None,
        }
    }
}

/// Custom deserializer for the `upx` field.
/// Accepts:
///   - null/missing → empty vec (via serde default)
///   - a single object → vec of one UpxConfig
///   - an array → vec of UpxConfig
pub(super) fn deserialize_upx<'de, D>(deserializer: D) -> Result<Vec<UpxConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct UpxVisitor;

    impl<'de> Visitor<'de> for UpxVisitor {
        type Value = Vec<UpxConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a UPX config object or an array of UPX config objects")
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut configs = Vec::new();
            while let Some(item) = seq.next_element::<UpxConfig>()? {
                configs.push(item);
            }
            Ok(configs)
        }

        fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
            let config = UpxConfig::deserialize(de::value::MapAccessDeserializer::new(map))?;
            Ok(vec![config])
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(UpxVisitor)
}
