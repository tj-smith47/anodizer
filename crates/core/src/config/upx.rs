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

#[cfg(test)]
mod tests {
    use super::*;

    // `upx:` uses the hand-written `deserialize_upx` (single-object OR array OR
    // null → Vec<UpxConfig>). Each visitor arm is driven through a wrapper
    // struct that mirrors the real field attributes.
    #[derive(Deserialize)]
    struct UpxWrapper {
        #[serde(default, deserialize_with = "deserialize_upx")]
        upx: Vec<UpxConfig>,
    }

    #[test]
    fn default_fills_upx_binary_and_empty_lists() {
        let d = UpxConfig::default();
        assert_eq!(d.binary, "upx");
        assert!(d.args.is_empty());
        assert!(!d.required);
        assert!(d.id.is_none());
        assert!(d.ids.is_none());
        assert!(d.enabled.is_none());
        assert!(d.targets.is_none());
        assert!(d.compress.is_none());
        assert!(d.lzma.is_none());
        assert!(d.brute.is_none());
    }

    #[test]
    fn single_object_becomes_one_element_vec() {
        let w: UpxWrapper = serde_yaml_ng::from_str("upx:\n  compress: best\n").unwrap();
        assert_eq!(w.upx.len(), 1);
        assert_eq!(w.upx[0].compress.as_deref(), Some("best"));
        // Fields omitted from YAML still take their struct defaults.
        assert_eq!(w.upx[0].binary, "upx");
    }

    #[test]
    fn array_collects_every_entry() {
        let w: UpxWrapper =
            serde_yaml_ng::from_str("upx:\n  - id: a\n  - id: b\n  - id: c\n").unwrap();
        assert_eq!(w.upx.len(), 3);
        assert_eq!(w.upx[0].id.as_deref(), Some("a"));
        assert_eq!(w.upx[2].id.as_deref(), Some("c"));
    }

    #[test]
    fn null_and_missing_yield_empty_vec() {
        let null: UpxWrapper = serde_yaml_ng::from_str("upx: null").unwrap();
        assert!(null.upx.is_empty());
        let missing: UpxWrapper = serde_yaml_ng::from_str("{}").unwrap();
        assert!(missing.upx.is_empty());
    }

    #[test]
    fn unknown_field_is_rejected() {
        // `deny_unknown_fields` on UpxConfig must reject typos rather than
        // silently drop them into a no-op.
        let r: Result<UpxWrapper, _> = serde_yaml_ng::from_str("upx:\n  compres: best\n");
        assert!(r.is_err(), "unknown field `compres` must be rejected");
    }

    #[test]
    fn enabled_accepts_bool_and_template_string() {
        let as_bool: UpxWrapper = serde_yaml_ng::from_str("upx:\n  enabled: true\n").unwrap();
        assert_eq!(as_bool.upx[0].enabled, Some(StringOrBool::Bool(true)));

        let as_tmpl: UpxWrapper =
            serde_yaml_ng::from_str("upx:\n  enabled: \"{{ if IsSnapshot }}false{{ endif }}\"\n")
                .unwrap();
        assert_eq!(
            as_tmpl.upx[0].enabled,
            Some(StringOrBool::String(
                "{{ if IsSnapshot }}false{{ endif }}".into()
            ))
        );
    }

    #[test]
    fn full_flag_surface_deserializes() {
        let w: UpxWrapper = serde_yaml_ng::from_str(
            "upx:\n  - id: pack\n    binary: /opt/upx\n    args: [\"-9\", \"--brute\"]\n    required: true\n    targets: [\"x86_64-unknown-linux-gnu\"]\n    compress: \"9\"\n    lzma: true\n    brute: false\n    ids: [\"cli\"]\n",
        )
        .unwrap();
        let c = &w.upx[0];
        assert_eq!(c.binary, "/opt/upx");
        assert_eq!(c.args, vec!["-9", "--brute"]);
        assert!(c.required);
        assert_eq!(c.targets.as_deref().unwrap(), ["x86_64-unknown-linux-gnu"]);
        assert_eq!(c.compress.as_deref(), Some("9"));
        assert_eq!(c.lzma, Some(true));
        assert_eq!(c.brute, Some(false));
        assert_eq!(c.ids.as_deref().unwrap(), ["cli"]);
    }
}
