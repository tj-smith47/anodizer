use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// StringOrBool — accepts bool or template string in YAML
// ---------------------------------------------------------------------------

/// A value that can be either a bool or a template string.
/// Used by `skip`, `skip_upload`, and similar fields across multiple config
/// structs to support both `skip: true` and template conditionals like
/// `skip: "{{ if .IsSnapshot }}true{{ endif }}"`.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum StringOrBool {
    Bool(bool),
    String(String),
}

impl StringOrBool {
    /// Evaluate this value to a bool. If it's a string, treat "true" / "1" as true,
    /// everything else as false.
    pub fn as_bool(&self) -> bool {
        match self {
            StringOrBool::Bool(b) => *b,
            StringOrBool::String(s) => matches!(s.trim(), "true" | "1"),
        }
    }

    /// Return the raw string value for template rendering, or the bool as a string.
    pub fn as_str(&self) -> &str {
        match self {
            StringOrBool::Bool(true) => "true",
            StringOrBool::Bool(false) => "false",
            StringOrBool::String(s) => s,
        }
    }

    /// Whether this value contains a template expression that needs rendering.
    pub fn is_template(&self) -> bool {
        matches!(self, StringOrBool::String(s) if s.contains('{'))
    }

    /// Evaluate whether this value resolves to `true`.
    ///
    /// The value is always run through `render` (Tera leaves plain literals
    /// unchanged, so this is a no-op for non-templated values). The rendered
    /// result is then compared to `"true"` / `"1"` after trimming. A `Bool`
    /// variant short-circuits without rendering.
    ///
    /// Always-rendering keeps this helper consistent with sibling
    /// `should_skip_upload` (which always renders) — a literal `"{{ broken"`
    /// surfaces as an `Err` instead of being silently treated as a false-y
    /// non-template string.
    ///
    /// Used for both `skip:` evaluation (most callers) and `output:` / `sbom:`
    /// bool-or-template fields — there is no separate alias; call this directly.
    pub fn try_evaluates_to_true(
        &self,
        render: impl Fn(&str) -> anyhow::Result<String>,
    ) -> anyhow::Result<bool> {
        match self {
            StringOrBool::Bool(b) => Ok(*b),
            StringOrBool::String(s) => {
                let rendered = render(s)?;
                Ok(matches!(rendered.trim(), "true" | "1"))
            }
        }
    }
}

impl Default for StringOrBool {
    fn default() -> Self {
        StringOrBool::Bool(false)
    }
}

/// Evaluate an `if:` conditional template.
///
/// Returns `Ok(true)` when the caller should proceed with the resource and
/// `Ok(false)` when the resource must be skipped. Mirrors the contract every
/// existing `if_condition` consumer in anodizer applies:
///
/// - `None` → proceed (no gate set).
/// - `Some("")` → proceed (empty literal is a no-op gate; the
///   "no `if:` = always run" behavior — keeps round-tripping clean for
///   configs that emit empty strings).
/// - Template render failure → hard `Err` (matches every existing
///   `if_condition` site; silent-skip on a typo'd template was the W1
///   release-resilience footgun and is intentionally NOT replicated).
/// - Rendered value (trimmed) equal to `"false"`, `"0"`, `"no"`, or empty
///   → skip.
/// - Any other rendered value → proceed.
///
/// `label` is the resource-identifying string woven into the error context
/// chain (e.g. `"publisher 'upload-artifacts'"` or `"blob 's3-cache'"`).
pub fn evaluate_if_condition(
    condition: Option<&str>,
    label: &str,
    render: impl Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<bool> {
    use anyhow::Context as _;
    let Some(template) = condition else {
        return Ok(true);
    };
    if template.is_empty() {
        return Ok(true);
    }
    let rendered = render(template).with_context(|| {
        format!("{label}: `if` template render failed (expression: {template})")
    })?;
    let trimmed = rendered.trim();
    let falsy = matches!(trimmed, "" | "false" | "0" | "no");
    Ok(!falsy)
}

/// Custom deserializer for `Option<StringOrBool>`.
pub(crate) fn deserialize_string_or_bool_opt<'de, D>(
    deserializer: D,
) -> Result<Option<StringOrBool>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct StringOrBoolVisitor;

    impl<'de> Visitor<'de> for StringOrBoolVisitor {
        type Value = Option<StringOrBool>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a bool, a string, or null")
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Ok(Some(StringOrBool::Bool(v)))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(StringOrBool::String(v.to_owned())))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(StringOrBool::String(v)))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(StringOrBoolVisitor)
}

/// A typed duration value parsed from a humantime-style string in YAML.
///
/// Accepts `"10m"`, `"15s"`, `"1h30m"`, `"500ms"`, etc. Used by notarize
/// timeouts so the schema is typed and validation catches malformed values
/// at config-load time instead of during the notarize stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub struct HumanDuration(
    #[serde(serialize_with = "serialize_human_duration")] pub std::time::Duration,
);

impl HumanDuration {
    /// Get the underlying `Duration` value.
    pub fn duration(&self) -> std::time::Duration {
        self.0
    }

    /// Format the duration back to its canonical string form (`{seconds}s` or
    /// `{minutes}m{seconds}s` depending on whole-minute alignment). Matches
    /// the form `xcrun notarytool --timeout` accepts (a unit-suffixed integer).
    pub fn as_humantime_string(&self) -> String {
        let total_secs = self.0.as_secs();
        if total_secs == 0 {
            // Sub-second; fall back to ms.
            return format!("{}ms", self.0.as_millis());
        }
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        let secs = total_secs % 60;
        let mut out = String::new();
        if hours > 0 {
            out.push_str(&format!("{hours}h"));
        }
        if mins > 0 {
            out.push_str(&format!("{mins}m"));
        }
        if secs > 0 || out.is_empty() {
            out.push_str(&format!("{secs}s"));
        }
        out
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, Visitor};

        struct DurVisitor;

        impl<'de> Visitor<'de> for DurVisitor {
            type Value = HumanDuration;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a duration string with unit suffix (e.g. \"10m\", \"15s\", \"1h30m\", \"500ms\")",
                )
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                parse_humantime_duration(v)
                    .map(HumanDuration)
                    .map_err(E::custom)
            }

            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                self.visit_str(&v)
            }
        }

        deserializer.deserialize_str(DurVisitor)
    }
}

fn serialize_human_duration<S: serde::Serializer>(
    d: &std::time::Duration,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&HumanDuration(*d).as_humantime_string())
}

/// Parse a humantime-style duration string. Recognizes `ms`, `s`, `m`, `h`,
/// `d` units and concatenated forms like `"1h30m"`. Whitespace between
/// components is tolerated.
pub(super) fn parse_humantime_duration(input: &str) -> Result<std::time::Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration string".to_string());
    }
    let mut total = std::time::Duration::ZERO;
    let mut number_buf = String::new();
    let mut had_any = false;
    let mut iter = s.chars().peekable();
    while let Some(&c) = iter.peek() {
        if c.is_whitespace() {
            iter.next();
            continue;
        }
        if c.is_ascii_digit() {
            number_buf.push(c);
            iter.next();
            continue;
        }
        if number_buf.is_empty() {
            return Err(format!("expected digit before unit in '{input}'"));
        }
        // Read unit (1 or 2 chars: ms, s, m, h, d).
        let mut unit = String::new();
        unit.push(c);
        iter.next();
        if let Some(&next) = iter.peek()
            && unit == "m"
            && next == 's'
        {
            unit.push('s');
            iter.next();
        }
        let n: u64 = number_buf
            .parse()
            .map_err(|e| format!("invalid number '{number_buf}' in '{input}': {e}"))?;
        let segment = match unit.as_str() {
            "ms" => std::time::Duration::from_millis(n),
            "s" => std::time::Duration::from_secs(n),
            "m" => std::time::Duration::from_secs(n * 60),
            "h" => std::time::Duration::from_secs(n * 3600),
            "d" => std::time::Duration::from_secs(n * 86_400),
            other => return Err(format!("unknown duration unit '{other}' in '{input}'")),
        };
        total += segment;
        number_buf.clear();
        had_any = true;
    }
    if !number_buf.is_empty() {
        return Err(format!(
            "trailing number '{number_buf}' without a unit in '{input}'"
        ));
    }
    if !had_any {
        return Err(format!("no duration components found in '{input}'"));
    }
    Ok(total)
}

/// A value that can be either a `u32` or a string parsed as octal/decimal.
///
/// Used by `NfpmConfig.umask` (and any future field specified
/// as `int OR string` in YAML — the parser canonicalizes both forms to a
/// `u32`). Accepts: `0o022`, `"0o022"`, `"022"`, `"18"`, `18`. Bare numeric
/// YAML values are interpreted as decimal; YAML-string forms accept the
/// `0o`/`0O` prefix to spell octal explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(transparent)]
pub struct StringOrU32(#[serde(deserialize_with = "deserialize_u32_from_string_or_int")] pub u32);

impl StringOrU32 {
    /// Get the underlying `u32` value.
    pub fn value(&self) -> u32 {
        self.0
    }
}

/// Deserialize a `u32` from either a YAML int or a string in octal/decimal.
fn deserialize_u32_from_string_or_int<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct U32Visitor;

    impl<'de> Visitor<'de> for U32Visitor {
        type Value = u32;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a u32 integer or a string parseable as octal/decimal (e.g. 18, \"0o022\", \"022\")")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            u32::try_from(v).map_err(|_| E::custom(format!("value {v} does not fit in u32")))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            u32::try_from(v).map_err(|_| E::custom(format!("value {v} does not fit in u32")))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let trimmed = v.trim();
            if let Some(rest) = trimmed
                .strip_prefix("0o")
                .or_else(|| trimmed.strip_prefix("0O"))
            {
                return u32::from_str_radix(rest, 8)
                    .map_err(|e| E::custom(format!("invalid octal '{v}': {e}")));
            }
            // Bare leading-zero strings (e.g. "022") are octal — match the
            // typical convention for unix file mode strings.
            if trimmed.starts_with('0') && trimmed.len() > 1 {
                return u32::from_str_radix(trimmed, 8)
                    .map_err(|e| E::custom(format!("invalid octal '{v}': {e}")));
            }
            trimmed
                .parse::<u32>()
                .map_err(|e| E::custom(format!("invalid u32 '{v}': {e}")))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            self.visit_str(&v)
        }
    }

    deserializer.deserialize_any(U32Visitor)
}

/// Custom deserializer for `Option<Vec<String>>` that accepts either a single
/// string or an array of strings. Used by `BlobConfig.cache_control`.
pub(super) fn deserialize_string_or_vec_opt<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct StringOrVecVisitor;

    impl<'de> Visitor<'de> for StringOrVecVisitor {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a string, a list of strings, or null")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(vec![v.to_owned()]))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(vec![v]))
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                items.push(item);
            }
            Ok(Some(items))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(StringOrVecVisitor)
}
