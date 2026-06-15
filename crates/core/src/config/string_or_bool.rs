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
                reject_stale_typed_compare(s, "skip/enable expression")?;
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

/// Hard-error when a conditional template compares a typed (bool / number)
/// injected variable to a quoted string (`IsSnapshot == "false"`,
/// `eq .IsHarness "true"`, `NightlyBuild != "0"`, …).
///
/// Those variables are real Tera bools/numbers, and Tera does not coerce
/// `Bool`/`Number` ↔ `str`, so the compare evaluates to `false` in *every*
/// mode and the guarded resource silently skips. Failing loud here matches
/// the render-failure contract below: a condition that can never do what it
/// says is a config bug, not a skip.
fn reject_stale_typed_compare(template: &str, label: &str) -> anyhow::Result<()> {
    if let Some(snippet) = crate::template::find_stale_typed_compare(template) {
        anyhow::bail!(
            "{label}: `{snippet}` compares a typed template variable to a quoted string; \
             these variables are real booleans/numbers, so the comparison never matches and \
             the condition would silently evaluate false in every mode. Use the variable \
             directly instead — e.g. `not IsSnapshot` for `IsSnapshot == \"false\"`, \
             `IsHarness` for `IsHarness == \"true\"`, or an unquoted numeric compare for \
             `NightlyBuild`."
        );
    }
    Ok(())
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
    reject_stale_typed_compare(template, label)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // --- as_bool -----------------------------------------------------------

    #[test]
    fn as_bool_bool_arms_passthrough() {
        assert!(StringOrBool::Bool(true).as_bool());
        assert!(!StringOrBool::Bool(false).as_bool());
    }

    #[test]
    fn as_bool_string_truthy_and_trimmed() {
        assert!(StringOrBool::String("true".into()).as_bool());
        assert!(StringOrBool::String("1".into()).as_bool());
        // Leading/trailing whitespace is trimmed before the truthy check.
        assert!(StringOrBool::String("  true  ".into()).as_bool());
    }

    #[test]
    fn as_bool_string_falsy() {
        assert!(!StringOrBool::String("no".into()).as_bool());
        assert!(!StringOrBool::String(String::new()).as_bool());
        assert!(!StringOrBool::String("false".into()).as_bool());
    }

    // --- Default -----------------------------------------------------------

    #[test]
    fn default_is_bool_false() {
        // `skip`/`output`/`sbom` fields default to "off" — a missing key must
        // not silently behave as `true`.
        assert_eq!(StringOrBool::default(), StringOrBool::Bool(false));
        assert!(!StringOrBool::default().as_bool());
    }

    // --- as_str ------------------------------------------------------------

    #[test]
    fn as_str_bool_renders_word() {
        assert_eq!(StringOrBool::Bool(true).as_str(), "true");
        assert_eq!(StringOrBool::Bool(false).as_str(), "false");
    }

    #[test]
    fn as_str_string_passthrough() {
        assert_eq!(StringOrBool::String("{{ x }}".into()).as_str(), "{{ x }}");
    }

    // --- is_template -------------------------------------------------------

    #[test]
    fn is_template_detects_brace_only_in_string() {
        assert!(StringOrBool::String("{{ .IsSnapshot }}".into()).is_template());
        assert!(!StringOrBool::String("plain".into()).is_template());
        // A Bool never carries a template, regardless of value.
        assert!(!StringOrBool::Bool(true).is_template());
    }

    // --- try_evaluates_to_true --------------------------------------------

    #[test]
    fn try_evaluates_bool_short_circuits_without_rendering() {
        let rendered = std::cell::Cell::new(false);
        let res = StringOrBool::Bool(true).try_evaluates_to_true(|s| {
            rendered.set(true);
            Ok(s.to_string())
        });
        assert!(res.unwrap());
        // Bool arm must NOT invoke the render closure.
        assert!(!rendered.get());
    }

    #[test]
    fn try_evaluates_string_renders_and_matches() {
        let truthy = StringOrBool::String("1".into())
            .try_evaluates_to_true(|s| Ok(s.to_string()))
            .unwrap();
        assert!(truthy);
        let falsy = StringOrBool::String("nope".into())
            .try_evaluates_to_true(|s| Ok(s.to_string()))
            .unwrap();
        assert!(!falsy);
    }

    #[test]
    fn try_evaluates_propagates_render_error() {
        let err =
            StringOrBool::String("{{ x }}".into()).try_evaluates_to_true(|_| anyhow::bail!("boom"));
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("boom"));
    }

    #[test]
    fn try_evaluates_rejects_stale_typed_compare() {
        // Render closure should never be reached; the stale-compare guard
        // fires first and returns Err.
        let res = StringOrBool::String("{{ if IsSnapshot == \"false\" }}true{{ endif }}".into())
            .try_evaluates_to_true(|_| panic!("render must not be called for stale compare"));
        assert!(res.is_err());
    }

    // --- evaluate_if_condition --------------------------------------------

    #[test]
    fn if_condition_none_proceeds() {
        let r = evaluate_if_condition(None, "lbl", |s| Ok(s.to_string())).unwrap();
        assert!(r);
    }

    #[test]
    fn if_condition_empty_literal_proceeds() {
        let r = evaluate_if_condition(Some(""), "lbl", |s| Ok(s.to_string())).unwrap();
        assert!(r);
    }

    #[test]
    fn if_condition_falsy_values_skip() {
        for v in ["false", "0", "no", ""] {
            let r = evaluate_if_condition(Some("tmpl"), "lbl", |_| Ok(v.to_string())).unwrap();
            assert!(!r, "rendered {v:?} should skip");
        }
    }

    #[test]
    fn if_condition_truthy_values_proceed() {
        for v in ["yes", "1", "true", "anything"] {
            let r = evaluate_if_condition(Some("tmpl"), "lbl", |_| Ok(v.to_string())).unwrap();
            assert!(r, "rendered {v:?} should proceed");
        }
    }

    #[test]
    fn if_condition_render_error_carries_context() {
        let err = evaluate_if_condition(Some("badtmpl"), "publisher 'x'", |_| {
            anyhow::bail!("render failed")
        });
        let msg = format!("{:#}", err.unwrap_err());
        assert!(msg.contains("publisher 'x'"));
        assert!(msg.contains("badtmpl"));
    }

    #[test]
    fn if_condition_stale_typed_compare_errors() {
        let err = evaluate_if_condition(
            Some("{{ if IsSnapshot == \"false\" }}go{{ endif }}"),
            "blob 's3'",
            |_| panic!("render must not run for stale compare"),
        );
        assert!(err.is_err());
    }

    // --- deserialize_string_or_bool_opt -----------------------------------

    #[derive(Deserialize)]
    struct BoolOptW {
        #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
        v: Option<StringOrBool>,
    }

    #[test]
    fn deserialize_bool_opt_bool_input() {
        let w: BoolOptW = serde_yaml_ng::from_str("v: true").unwrap();
        assert_eq!(w.v, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn deserialize_bool_opt_string_input() {
        let w: BoolOptW = serde_yaml_ng::from_str("v: \"{{ x }}\"").unwrap();
        assert_eq!(w.v, Some(StringOrBool::String("{{ x }}".into())));
    }

    #[test]
    fn deserialize_bool_opt_null_input() {
        let w: BoolOptW = serde_yaml_ng::from_str("v: null").unwrap();
        assert_eq!(w.v, None);
    }

    // --- HumanDuration::as_humantime_string -------------------------------

    #[test]
    fn human_duration_sub_second_falls_back_to_ms() {
        assert_eq!(
            HumanDuration(Duration::from_millis(250)).as_humantime_string(),
            "250ms"
        );
    }

    #[test]
    fn human_duration_whole_seconds() {
        assert_eq!(
            HumanDuration(Duration::from_secs(45)).as_humantime_string(),
            "45s"
        );
    }

    #[test]
    fn human_duration_minutes_and_seconds() {
        assert_eq!(
            HumanDuration(Duration::from_secs(90)).as_humantime_string(),
            "1m30s"
        );
    }

    #[test]
    fn human_duration_hours_component() {
        let s = HumanDuration(Duration::from_secs(3661)).as_humantime_string();
        assert!(s.contains("1h"), "{s} should contain hours");
        assert!(s.contains("1m"));
        assert!(s.contains("1s"));
    }

    #[test]
    fn human_duration_whole_minute_omits_seconds() {
        // secs == 0 and out non-empty: the trailing `s` is suppressed.
        assert_eq!(
            HumanDuration(Duration::from_secs(120)).as_humantime_string(),
            "2m"
        );
    }

    // --- parse_humantime_duration -----------------------------------------

    #[test]
    fn parse_humantime_ok_values() {
        assert_eq!(
            parse_humantime_duration("10m").unwrap(),
            Duration::from_secs(600)
        );
        assert_eq!(
            parse_humantime_duration("15s").unwrap(),
            Duration::from_secs(15)
        );
        assert_eq!(
            parse_humantime_duration("1h30m").unwrap(),
            Duration::from_secs(3600 + 1800)
        );
        assert_eq!(
            parse_humantime_duration("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(
            parse_humantime_duration("1d").unwrap(),
            Duration::from_secs(86_400)
        );
    }

    #[test]
    fn parse_humantime_whitespace_tolerated() {
        assert_eq!(
            parse_humantime_duration("10 m").unwrap(),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn parse_humantime_errors() {
        assert!(parse_humantime_duration("").is_err());
        assert!(parse_humantime_duration("m").is_err());
        assert!(parse_humantime_duration("10x").is_err());
        // Trailing digits with no unit.
        assert!(parse_humantime_duration("10").is_err());
    }

    // --- StringOrU32 / deserialize_u32_from_string_or_int -----------------

    #[test]
    fn u32_from_int_decimal() {
        let v: StringOrU32 = serde_yaml_ng::from_str("18").unwrap();
        assert_eq!(v.value(), 18);
    }

    #[test]
    fn u32_from_prefixed_octal_string() {
        let v: StringOrU32 = serde_yaml_ng::from_str("\"0o022\"").unwrap();
        assert_eq!(v.value(), 18);
    }

    #[test]
    fn u32_from_bare_leading_zero_is_octal() {
        let v: StringOrU32 = serde_yaml_ng::from_str("\"022\"").unwrap();
        assert_eq!(v.value(), 18);
    }

    #[test]
    fn u32_from_plain_decimal_string() {
        let v: StringOrU32 = serde_yaml_ng::from_str("\"18\"").unwrap();
        assert_eq!(v.value(), 18);
    }

    #[test]
    fn u32_invalid_octal_digit_errors() {
        // 9 is not a valid octal digit.
        let r = serde_yaml_ng::from_str::<StringOrU32>("\"0o999\"");
        assert!(r.is_err());
    }

    #[test]
    fn u32_out_of_range_errors() {
        let r = serde_yaml_ng::from_str::<StringOrU32>("5000000000");
        assert!(r.is_err());
    }

    // --- deserialize_string_or_vec_opt ------------------------------------

    #[derive(Deserialize)]
    struct VecOptW {
        #[serde(deserialize_with = "deserialize_string_or_vec_opt", default)]
        v: Option<Vec<String>>,
    }

    #[test]
    fn string_or_vec_single_string_wraps() {
        let w: VecOptW = serde_yaml_ng::from_str("v: max-age=60").unwrap();
        assert_eq!(w.v, Some(vec!["max-age=60".to_string()]));
    }

    #[test]
    fn string_or_vec_list_passthrough() {
        let w: VecOptW = serde_yaml_ng::from_str("v:\n  - a\n  - b").unwrap();
        assert_eq!(w.v, Some(vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn string_or_vec_null_is_none() {
        let w: VecOptW = serde_yaml_ng::from_str("v: null").unwrap();
        assert_eq!(w.v, None);
    }
}
