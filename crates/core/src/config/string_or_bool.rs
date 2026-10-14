use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// StringOrBool — accepts bool or template string in YAML
// ---------------------------------------------------------------------------

/// A value that can be either a bool or a template string.
/// Used by `skip`, `skip_upload`, and similar fields across multiple config
/// structs to support both `skip: true` and template conditionals like
/// `skip: "{{ if .IsSnapshot }}true{{ endif }}"`. An unquoted `1` or `0`
/// loads as `true` or `false`.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(transform = accept_one_or_zero)]
pub enum StringOrBool {
    Bool(bool),
    String(String),
}

/// The loader reads an unquoted `1` / `0` as a bool, so the schema accepts
/// those two integers beside the derived bool and string.
pub(crate) fn accept_one_or_zero(schema: &mut schemars::Schema) {
    if let Some(any_of) = schema.get_mut("anyOf").and_then(|v| v.as_array_mut()) {
        any_of.push(one_or_zero_schema());
    }
}

/// The schema of an unquoted `1` or `0`, which every bool-or-template field
/// loads as a bool.
pub(crate) fn one_or_zero_schema() -> serde_json::Value {
    serde_json::json!({ "type": "integer", "enum": [0, 1] })
}

/// The bool an unquoted YAML `1` or `0` spells; any other integer is a load
/// error against `expected`.
pub(crate) fn bool_from_u64<E: serde::de::Error>(
    v: u64,
    expected: &dyn serde::de::Expected,
) -> Result<bool, E> {
    match v {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(E::invalid_value(
            serde::de::Unexpected::Unsigned(v),
            expected,
        )),
    }
}

/// [`bool_from_u64`] for the signed integer a negative YAML number reads as.
pub(crate) fn bool_from_i64<E: serde::de::Error>(
    v: i64,
    expected: &dyn serde::de::Expected,
) -> Result<bool, E> {
    match u64::try_from(v) {
        Ok(u) => bool_from_u64(u, expected),
        Err(_) => Err(E::invalid_value(serde::de::Unexpected::Signed(v), expected)),
    }
}

impl StringOrBool {
    /// Evaluate this value to a bool. If it's a string, treat `true` / `1` in
    /// any letter case as true, everything else as false.
    pub fn as_bool(&self) -> bool {
        match self {
            StringOrBool::Bool(b) => *b,
            StringOrBool::String(s) => is_true_spelling(s),
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
    /// result is then compared to `true` / `1` after trimming, in any letter
    /// case. A `Bool` variant short-circuits without rendering.
    ///
    /// Always-rendering keeps this helper consistent with sibling
    /// `should_skip_upload` (which always renders) — a literal `"{{ broken"`
    /// returns an `Err`, so a broken template is never read as a false-y
    /// plain string.
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
                Ok(is_true_spelling(&rendered))
            }
        }
    }
}

impl StringOrBool {
    /// Evaluate this value as a boolean that has to be spelled as one.
    ///
    /// A `Bool` is returned as is. A string is rendered and trimmed, then
    /// `true` / `1` read as true and `false` / `0` as false, in any letter
    /// case. Anything else, an empty render included, is an error naming
    /// `label` and the rendered value, so a template that renders nothing
    /// (an unset variable, a typo) cannot pass for `false`.
    pub fn try_evaluate_strict(
        &self,
        label: &str,
        render: impl Fn(&str) -> anyhow::Result<String>,
    ) -> anyhow::Result<bool> {
        let StringOrBool::String(s) = self else {
            return Ok(self.as_bool());
        };
        reject_stale_typed_compare(s, label)?;
        let rendered =
            render(s).map_err(|e| anyhow::anyhow!("{label}: template render failed: {e:#}"))?;
        if is_true_spelling(&rendered) {
            Ok(true)
        } else if is_false_spelling(&rendered) {
            Ok(false)
        } else {
            anyhow::bail!(
                "{label} rendered {:?}, which is not true, false, 1 or 0",
                rendered.trim()
            )
        }
    }
}

/// Whether `s`, trimmed, spells true: `true` or `1` in any letter case, the
/// spellings [`StringOrBool::try_evaluate_strict`] reads as true.
///
/// This and [`is_false_spelling`] are the one place a rendered value is
/// matched against the words; the "Bool-or-template values" table on the
/// templates docs page lists what each field does with them.
pub fn is_true_spelling(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1")
}

/// Whether `s`, trimmed, spells false: `false` or `0` in any letter case.
/// An empty value is not a spelling; each reader says what it does with one.
pub fn is_false_spelling(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "false" | "0")
}

/// Reads a quoted value of a field that takes `auto` or a bool (`prerelease`,
/// `make_latest`, `skip_push`) at load time: `auto`, a true spelling or a
/// false spelling, trimmed and in any letter case, the same way the value is
/// read after a render. Any other string is `None`.
pub(crate) fn auto_or_bool_word<T>(v: &str, auto: T, bool_variant: fn(bool) -> T) -> Option<T> {
    if v.trim().eq_ignore_ascii_case("auto") {
        Some(auto)
    } else if is_true_spelling(v) {
        Some(bool_variant(true))
    } else if is_false_spelling(v) {
        Some(bool_variant(false))
    } else {
        None
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
/// says is a config bug, and it fails.
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

/// The gate an `if:` value actually imposes, or `None` when it imposes none.
///
/// An absent `if:`, an empty one and a blank one all mean "always run", so
/// anything deciding whether two entries carry DIFFERENT gates has to read
/// them the same way [`evaluate_if_condition`] does. Comparing the raw
/// `Option` calls `if: ""` a gate and pairs it unequally with a real one.
///
/// Blank counts because a rendered gate is judged on its TRIMMED value: a
/// literal `if: "  "` read as a gate renders to `"  "`, trims to empty and
/// skips, so one added space would flip an entry from "always run" to
/// "never run".
pub fn active_if_gate(condition: Option<&str>) -> Option<&str> {
    condition.filter(|template| !template.trim().is_empty())
}

/// Evaluate an `if:` conditional template.
///
/// Returns `Ok(true)` when the caller should proceed with the resource and
/// `Ok(false)` when the resource must be skipped. Mirrors the contract every
/// existing `if_condition` consumer in anodizer applies:
///
/// - `None` → proceed (no gate set).
/// - `Some("")` / `Some("   ")` → proceed (an empty or blank literal is a
///   no-op gate; the "no `if:` = always run" behavior — keeps
///   round-tripping clean for configs that emit empty strings).
/// - Template render failure → hard `Err` (matches every existing
///   `if_condition` site; silent-skip on a typo'd template was the W1
///   release-resilience footgun and is intentionally NOT replicated).
/// - Rendered value (trimmed, any letter case) equal to `"false"`, `"0"`,
///   `"no"`, or empty → skip.
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
    let Some(template) = active_if_gate(condition) else {
        return Ok(true);
    };
    reject_stale_typed_compare(template, label)?;
    let rendered = render(template).with_context(|| {
        format!("{label}: `if` template render failed (expression: {template})")
    })?;
    Ok(!is_if_falsy(&rendered))
}

/// Whether a rendered `if:` value means "skip": empty, or `false`, `0` or
/// `no` in any letter case, after trimming.
pub fn is_if_falsy(rendered: &str) -> bool {
    let trimmed = rendered.trim();
    trimmed.is_empty() || is_false_spelling(trimmed) || trimmed.eq_ignore_ascii_case("no")
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
            f.write_str("a bool, 0, 1, a string, or null")
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Ok(Some(StringOrBool::Bool(v)))
        }

        // An unquoted `1` / `0` is the integer YAML reads, and both spell a
        // boolean the string form already accepts; any other number is not
        // one.
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            bool_from_u64(v, &self).map(|b| Some(StringOrBool::Bool(b)))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            bool_from_i64(v, &self).map(|b| Some(StringOrBool::Bool(b)))
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
/// when the config loads, before any stage runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

/// The schema of a duration is the string the loader parses: one or more
/// whole numbers of up to 19 digits, each followed by a unit (`ms`, `s`,
/// `m`, `h`, `d`), with whitespace allowed between the parts.
impl JsonSchema for HumanDuration {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "HumanDuration".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": HUMAN_DURATION_PATTERN,
            "examples": ["90s", "10m", "1h30m", "500ms"],
        })
    }
}

/// The shape `parse_humantime_duration` accepts, as a JSON Schema pattern:
/// one or more terms, each a number of 1 to [`MAX_DURATION_DIGITS`] digits
/// (whitespace allowed between the digits) followed by a unit. Every string
/// the parser accepts matches it. A string that matches it is accepted by the
/// parser unless one term or the sum is longer than the longest
/// `std::time::Duration` (`18446744073709551615s` and 999999999 nanoseconds),
/// a range a pattern cannot express.
const HUMAN_DURATION_PATTERN: &str = r"^\s*([0-9](\s*[0-9]){0,18}\s*(ms|s|m|h|d)\s*)+$";

/// The longest number a duration term may spell. Every 19-digit number fits
/// a `u64`, and the 20-digit numbers above `u64::MAX` cannot be told apart
/// from the ones below it by a pattern, so the parser and the pattern both
/// stop at 19.
const MAX_DURATION_DIGITS: usize = 19;

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
/// components is tolerated. A number longer than [`MAX_DURATION_DIGITS`]
/// digits, and a term or total longer than the longest `Duration`, is an
/// error naming the value.
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
        if number_buf.len() > MAX_DURATION_DIGITS {
            return Err(format!(
                "number '{number_buf}' in '{input}' has more than {MAX_DURATION_DIGITS} digits"
            ));
        }
        let n: u64 = number_buf
            .parse()
            .map_err(|e| format!("invalid number '{number_buf}' in '{input}': {e}"))?;
        let too_long = || {
            format!(
                "'{number_buf}{unit}' in '{input}' is longer than the longest duration \
                 ({}s)",
                u64::MAX
            )
        };
        let seconds = |per_unit: u64| {
            n.checked_mul(per_unit)
                .map(std::time::Duration::from_secs)
                .ok_or_else(too_long)
        };
        let segment = match unit.as_str() {
            "ms" => std::time::Duration::from_millis(n),
            "s" => std::time::Duration::from_secs(n),
            "m" => seconds(60)?,
            "h" => seconds(3600)?,
            "d" => seconds(86_400)?,
            other => return Err(format!("unknown duration unit '{other}' in '{input}'")),
        };
        total = total.checked_add(segment).ok_or_else(|| {
            format!(
                "the total of '{input}' is longer than the longest duration ({}s)",
                u64::MAX
            )
        })?;
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

    #[test]
    fn an_unquoted_one_or_zero_loads_as_a_bool_and_any_other_number_does_not() {
        let load = |value: &str| {
            serde_yaml_ng::from_str::<crate::config::Config>(&format!(
                "project_name: p\nrelease:\n  preflight:\n    fail_on_error: {value}\n"
            ))
            .map(|c| c.release.unwrap().preflight.unwrap().fail_on_error)
        };
        assert_eq!(load("1").unwrap(), Some(StringOrBool::Bool(true)));
        assert_eq!(load("0").unwrap(), Some(StringOrBool::Bool(false)));
        assert_eq!(
            load("\"1\"").unwrap(),
            Some(StringOrBool::String("1".into()))
        );
        for bad in ["2", "-1"] {
            let err = load(bad).unwrap_err().to_string();
            assert!(
                err.contains("expected a bool, 0, 1, a string, or null"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn strict_evaluation_reads_only_a_boolean_spelling() {
        let render = |s: &str| Ok(s.to_string());
        for (raw, want) in [
            (" TRUE ", true),
            ("1", true),
            ("False", false),
            ("0", false),
        ] {
            assert_eq!(
                StringOrBool::String(raw.into())
                    .try_evaluate_strict("f", render)
                    .unwrap(),
                want,
                "{raw:?}"
            );
        }
        assert!(
            StringOrBool::Bool(false)
                .try_evaluate_strict("f", render)
                .is_ok_and(|b| !b)
        );
        for raw in ["", "  ", "maybe", "yes"] {
            let err = StringOrBool::String(raw.into())
                .try_evaluate_strict("f", render)
                .unwrap_err()
                .to_string();
            assert_eq!(
                err,
                format!(
                    "f rendered {:?}, which is not true, false, 1 or 0",
                    raw.trim()
                )
            );
        }
        let err = StringOrBool::String("{{ x".into())
            .try_evaluate_strict("f", |_| anyhow::bail!("boom"))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "f: template render failed: boom");
    }

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

    /// The strict reader `try_evaluate_strict`, the lenient readers
    /// `as_bool`, `try_evaluates_to_true` and `evaluate_if_condition`, and the
    /// spelling helpers all read `true` / `1` and `false` / `0` in any letter
    /// case.
    #[test]
    fn every_reader_strict_and_lenient_takes_true_false_one_and_zero_in_any_case() {
        for (value, truthy) in [
            ("true", true),
            (" TRUE ", true),
            ("True", true),
            ("1", true),
            ("false", false),
            ("FALSE", false),
            ("0", false),
        ] {
            let v = StringOrBool::String(value.to_string());
            assert_eq!(is_true_spelling(value), truthy, "{value:?}");
            assert_eq!(is_false_spelling(value), !truthy, "{value:?}");
            assert_eq!(is_if_falsy(value), !truthy, "{value:?}");
            assert_eq!(v.as_bool(), truthy, "{value:?}");
            assert_eq!(
                v.try_evaluates_to_true(|s| Ok(s.to_string())).unwrap(),
                truthy,
                "{value:?}"
            );
            assert_eq!(
                v.try_evaluate_strict("lbl", |s| Ok(s.to_string())).unwrap(),
                truthy,
                "{value:?}"
            );
            assert_eq!(
                evaluate_if_condition(Some("tmpl"), "lbl", |_| Ok(value.to_string())).unwrap(),
                truthy,
                "{value:?}"
            );
        }
        assert!(!evaluate_if_condition(Some("tmpl"), "lbl", |_| Ok("NO".to_string())).unwrap());
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

    /// A blank literal is the empty one with padding: it imposes no gate,
    /// so the render is never reached and the entry proceeds. Read as a gate
    /// it would render `"  "`, trim to empty and skip — one space flipping
    /// "always run" into "never run".
    #[test]
    fn if_condition_blank_literal_proceeds() {
        for blank in ["  ", "\t", " \n "] {
            assert!(active_if_gate(Some(blank)).is_none(), "{blank:?}");
            let r = evaluate_if_condition(Some(blank), "lbl", |_| {
                panic!("a blank `if:` imposes no gate and must not render")
            })
            .unwrap();
            assert!(r, "{blank:?}");
        }
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

    /// A number over 19 digits, or a term or a total past the longest
    /// `Duration`, is an error that names the value, in a debug and a release
    /// build alike.
    #[test]
    fn a_duration_past_the_longest_is_refused_with_its_value() {
        for (input, names) in [
            (
                "18446744073709551615s 1s",
                "'18446744073709551615' in '18446744073709551615s 1s' has more than 19 digits",
            ),
            (
                "9999999999999999999s 9999999999999999999s",
                "the total of '9999999999999999999s 9999999999999999999s'",
            ),
            ("307445734561825861m", "'307445734561825861m'"),
            ("5124095576030432h", "'5124095576030432h'"),
            ("213503982334602d", "'213503982334602d'"),
            ("18446744073709551616s", "'18446744073709551616'"),
            ("99999999999999999999ms", "'99999999999999999999'"),
        ] {
            let err = parse_humantime_duration(input).unwrap_err();
            assert!(err.contains(names), "{input:?}: {err}");
        }
        assert_eq!(
            parse_humantime_duration("307445734561825860m").unwrap(),
            Duration::from_secs(307_445_734_561_825_860 * 60)
        );
        assert_eq!(
            parse_humantime_duration("9999999999999999999s").unwrap(),
            Duration::from_secs(9_999_999_999_999_999_999)
        );
    }

    /// The schema pattern and the parser agree on every sample: a string the
    /// parser accepts matches the pattern, and a string the pattern matches
    /// is accepted by the parser unless a term or the total is longer than
    /// the longest `Duration`.
    #[test]
    fn the_duration_pattern_and_the_parser_agree() {
        let pattern = regex::Regex::new(super::HUMAN_DURATION_PATTERN).unwrap();
        let numbers = [
            "0",
            "1",
            "1 0",
            "90",
            "007",
            "9999999999999999999",
            "1844674407370955161",
            "18446744073709551615",
            "18446744073709551616",
            "99999999999999999999",
            "1.5",
            "",
        ];
        let units = ["ms", "s", "m", "h", "d", "", "x", " m"];
        let mut terms = Vec::new();
        for n in numbers {
            for u in units {
                terms.push(format!("{n}{u}"));
            }
        }
        let mut samples: Vec<String> = terms.clone();
        for a in &terms {
            for b in ["1h", "30m", "18446744073709551615s", "1 0s"] {
                samples.push(format!("{a}{b}"));
                samples.push(format!(" {b} {a} "));
            }
        }
        let mut accepted_by_both = 0;
        for s in &samples {
            let parsed = super::parse_humantime_duration(s);
            let matched = pattern.is_match(s);
            match &parsed {
                Ok(_) => assert!(matched, "the parser accepts {s:?}, the pattern does not"),
                Err(e) if matched => assert!(
                    e.contains("longer than the longest duration"),
                    "the pattern matches {s:?}, the parser refuses it: {e}"
                ),
                Err(_) => {}
            }
            accepted_by_both += usize::from(parsed.is_ok());
        }
        assert!(
            accepted_by_both > 100,
            "only {accepted_by_both} samples parsed"
        );
        for v in ["90s", "1h30m", "0s", "1 0s"] {
            assert!(
                super::parse_humantime_duration(v).is_ok() && pattern.is_match(v),
                "{v}"
            );
        }
        for v in ["1.5s", "0", "", "90"] {
            assert!(
                super::parse_humantime_duration(v).is_err() && !pattern.is_match(v),
                "{v}"
            );
        }
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

    /// Every production site that asks whether an `if:` imposes a gate reads
    /// it through `active_if_gate` (or hands the raw `Option` to
    /// `evaluate_if_condition`, which folds the empty case itself).
    ///
    /// A bare `Option` test calls `if: ""` a gate: it wins an `.or()`
    /// fallback and shadows the parent's real condition, so the parent's
    /// gate is silently dropped and the child publishes on a run the
    /// operator gated out.
    ///
    /// The shapes the walk reads as a presence test are `.or(` on either
    /// side of the read, `.is_some()`, `.is_none()` and `.is_some_and(`.
    /// `.map(` is deliberately not one: every `.map(` near an
    /// `if_condition` in this workspace is the iterator after a gate call,
    /// not a read of the field.
    #[test]
    fn every_if_condition_presence_test_reads_the_active_gate() {
        use crate::test_helpers::test_sources::{
            function_bodies, production_half, workspace_production_sources,
        };

        let this_file = std::path::Path::new(file!())
            .file_name()
            .expect("string_or_bool.rs");
        let mut strays: Vec<String> = Vec::new();
        let mut population = 0usize;
        for source in workspace_production_sources() {
            if source.file_name() == Some(this_file) {
                continue;
            }
            let text = std::fs::read_to_string(&source).expect("read source");
            for body in function_bodies(production_half(&text)) {
                if !body.contains("if_condition") {
                    continue;
                }
                // rustfmt wraps a fallback chain across lines, so the
                // operators are read with the whitespace removed, and only
                // where they hang off the `if_condition` itself — an
                // unrelated `.or(` elsewhere in the body says nothing.
                let flat: String = body.split_whitespace().collect();
                // A comment in the body can carry a multi-byte character, so
                // a window is widened to the nearest character boundary.
                let window = |from: usize, to: usize| -> &str {
                    let (mut lo, mut hi) = (from.min(flat.len()), to.min(flat.len()));
                    while !flat.is_char_boundary(lo) {
                        lo += 1;
                    }
                    while !flat.is_char_boundary(hi) {
                        hi -= 1;
                    }
                    &flat[lo..hi]
                };
                let mut asks_presence = false;
                let mut builds_fallback = false;
                for (at, _) in flat.match_indices("if_condition") {
                    let tail = window(at, at + 60);
                    // A fallback chain can also be assembled across two
                    // statements, which puts the `.or(` BEFORE the read it
                    // falls back to.
                    let lead = window(at.saturating_sub(60), at);
                    builds_fallback |= tail.contains(".or(") || lead.contains(".or(");
                    asks_presence |= builds_fallback
                        || tail.contains(".is_some()")
                        || tail.contains(".is_none()")
                        || tail.contains(".is_some_and(");
                }
                if !asks_presence {
                    continue;
                }
                population += 1;
                // A fallback chain has to fold the empty case BEFORE the
                // `.or()` picks a winner, so handing the result to
                // `evaluate_if_condition` afterwards is too late: the empty
                // child condition has already shadowed the parent's.
                let folds = if builds_fallback {
                    body.contains("active_if_gate")
                } else {
                    body.contains("active_if_gate") || body.contains("evaluate_if_condition")
                };
                if !folds {
                    strays.push(source.display().to_string());
                }
            }
        }
        assert!(
            strays.is_empty(),
            "read an `if:` through `config::active_if_gate` (or pass the \
             raw Option to `evaluate_if_condition`) — an `if: \"\"` imposes \
             no gate and must not win a fallback: {strays:#?}"
        );
        assert_eq!(
            population, 4,
            "the walk must still find the sites that test an `if:` for \
             presence; a rename that empties it would pass vacuously"
        );
    }

    /// Every documented `if:` field describes the same three spellings.
    ///
    /// A field's description generates straight into
    /// `docs/site/static/schema.json` and the configuration reference, so
    /// one left saying an empty gate SKIPS tells an operator the opposite of
    /// what the engine does — and, where a hand-written page was updated and
    /// its generated sibling was not, the two surfaces contradict each
    /// other. A field with no doc comment at all (the private deserialize
    /// helper in `archives.rs`) generates nothing and is outside the
    /// question.
    #[test]
    fn every_documented_if_field_says_an_empty_gate_runs() {
        use crate::test_helpers::test_sources::{production_half, workspace_production_sources};

        const SENTENCE: &str = "An absent, empty or blank `if:` imposes no gate and \
                                always runs; the falsy test applies to what a \
                                non-blank gate renders.";

        let mut documented = 0usize;
        let mut silent: Vec<String> = Vec::new();
        for path in workspace_production_sources() {
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let lines: Vec<&str> = production_half(&src).lines().collect();
            for (at, line) in lines.iter().enumerate() {
                if line.trim() != r#"#[serde(rename = "if")]"# {
                    continue;
                }
                // The attribute and doc run directly above the field.
                let mut from = at;
                while from > 0 && {
                    let above = lines[from - 1].trim_start();
                    above.starts_with("///") || above.starts_with("#[")
                } {
                    from -= 1;
                }
                let doc = lines[from..at]
                    .iter()
                    .map(|l| l.trim_start())
                    .filter(|l| l.starts_with("///"))
                    .map(|l| l.trim_start_matches('/').trim())
                    .collect::<Vec<_>>()
                    .join(" ");
                if doc.is_empty() {
                    continue;
                }
                documented += 1;
                if !doc.contains(SENTENCE) {
                    silent.push(format!("{}:{}", path.display(), at + 1));
                }
            }
        }
        assert!(
            silent.is_empty(),
            "these `if:` descriptions still leave out what an absent, empty or \
             blank gate does, and each one generates into the schema and the \
             configuration reference: {silent:?}"
        );
        assert_eq!(
            documented, 35,
            "the walk found a different number of documented `if:` fields than \
             it expects; a new one must carry the sentence and be counted here"
        );
    }

    /// Whether an enum body line declares a `Bool(bool)` variant: with or
    /// without the trailing comma, with an attribute written before it on the
    /// same line, and with any spacing inside the parentheses.
    fn is_bool_variant_line(line: &str) -> bool {
        let mut rest = line.trim();
        while let Some(attr) = rest.strip_prefix("#[") {
            let Some(close) = attr.find(']') else {
                return false;
            };
            rest = attr[close + 1..].trim_start();
        }
        let compact: String = rest.chars().filter(|c| !c.is_whitespace()).collect();
        compact.trim_end_matches(',') == "Bool(bool)"
    }

    #[test]
    fn the_bool_variant_is_found_in_every_shape() {
        let enum_with = |variant_lines: &[&str]| {
            let mut body = vec!["pub enum E {", "    Auto,"];
            body.extend_from_slice(variant_lines);
            body.push("}");
            body.iter().any(|l| is_bool_variant_line(l))
        };
        assert!(enum_with(&["    Bool(bool),"]), "trailing comma");
        assert!(enum_with(&["    Bool(bool)"]), "last variant, no comma");
        assert!(
            enum_with(&["    #[serde(rename = \"b\")]", "    Bool(bool),"]),
            "attribute line above"
        );
        assert!(
            enum_with(&["    #[serde(rename = \"b\")] Bool(bool)"]),
            "attribute on the same line"
        );
        assert!(enum_with(&["    Bool( bool ),"]), "spacing");
        assert!(!enum_with(&["    Bool(String),"]));
        assert!(!enum_with(&["    MaybeBool(bool),"]));
    }

    /// Every enum in the config with a `Bool(bool)` variant is a value that
    /// spells a bool, so it loads an unquoted `1` / `0` through the shared
    /// helpers and its schema accepts them. Its deserializer names
    /// `bool_from_u64` and `bool_from_i64`, and its schema either carries the
    /// `accept_one_or_zero` transform or, on every field of that type, a
    /// `schema_with` function that reaches `one_or_zero_schema`. Rule:
    /// `.claude/rules/bool-or-template-values.md`.
    #[test]
    fn every_bool_or_template_enum_reads_one_and_zero() {
        use crate::test_helpers::test_sources::{
            function_bodies, production_half, workspace_production_sources,
        };

        /// Enums with a `Bool(bool)` variant that do not spell a bool, each
        /// with the reason.
        const NOT_A_BOOL_SPELLING: &[(&str, &str)] = &[];

        let all: String = workspace_production_sources()
            .into_iter()
            .filter(|p| {
                p.to_string_lossy()
                    .replace('\\', "/")
                    .contains("crates/core/src/config/")
            })
            .map(|p| production_half(&std::fs::read_to_string(&p).unwrap()).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let lines: Vec<&str> = all.lines().collect();
        let bodies = function_bodies(&all);
        let header_names = |b: &str, name: &str| {
            b.lines().next().is_some_and(|h| {
                h.contains(&format!("fn {name}(")) || h.contains(&format!("fn {name}<"))
            })
        };
        let ident = |s: &str| {
            s.trim_start()
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default()
                .to_string()
        };
        // The attribute and doc lines directly above line `i`.
        let attrs_above = |i: usize| {
            lines[..i]
                .iter()
                .rev()
                .take_while(|l| {
                    let t = l.trim_start();
                    t.starts_with('#') || t.starts_with("//")
                })
                .copied()
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut enums: Vec<(String, usize)> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim_start();
            let Some(rest) = ["pub enum ", "pub(crate) enum ", "enum "]
                .iter()
                .find_map(|p| t.strip_prefix(p))
            else {
                continue;
            };
            let body: Vec<&str> = lines[i..]
                .iter()
                .take_while(|l| **l != "}")
                .copied()
                .collect();
            if body.iter().any(|l| is_bool_variant_line(l)) {
                enums.push((ident(rest), i));
            }
        }

        let macro_types: Vec<String> = all
            .split("impl_auto_or_bool_serde!(")
            .skip(1)
            .map(ident)
            .collect();
        let macro_body = all
            .find("macro_rules! impl_auto_or_bool_serde")
            .map(|at| &all[at..])
            .map(|rest| &rest[..rest.find("\n}\n").unwrap_or(rest.len())])
            .unwrap_or_default();
        let uses_helpers = |s: &str| s.contains("bool_from_u64") && s.contains("bool_from_i64");

        // Functions whose body reaches `one_or_zero_schema`, directly or
        // through one call.
        let direct: Vec<String> = bodies
            .iter()
            .filter(|b| b.contains("one_or_zero_schema()"))
            .map(|b| {
                ident(
                    b.lines()
                        .next()
                        .unwrap()
                        .split("fn ")
                        .nth(1)
                        .unwrap_or_default(),
                )
            })
            .collect();
        let reaches_schema = |name: &str| {
            bodies.iter().filter(|b| header_names(b, name)).any(|b| {
                b.contains("one_or_zero_schema()")
                    || direct.iter().any(|d| b.contains(&format!("{d}()")))
            })
        };

        let mut failures = Vec::new();
        for (name, at) in &enums {
            if NOT_A_BOOL_SPELLING.iter().any(|(n, _)| n == name) {
                continue;
            }
            let impl_region = all
                .find(&format!("Deserialize<'de> for {name} {{"))
                .map(|i| &all[i..])
                .map(|rest| &rest[..rest.find("\nimpl").unwrap_or(rest.len())]);
            let deserializes = match impl_region {
                Some(region) => uses_helpers(region),
                None if macro_types.contains(name) => uses_helpers(macro_body),
                None => bodies.iter().any(|b| {
                    b.lines()
                        .next()
                        .is_some_and(|h| h.contains("fn deserialize_"))
                        && b.contains(&format!("Option<{name}>"))
                        && uses_helpers(b)
                }),
            };
            if !deserializes {
                failures.push(format!(
                    "{name}: its deserializer does not use bool_from_u64 / bool_from_i64"
                ));
            }

            let schema_ok = if attrs_above(*at).contains("accept_one_or_zero") {
                true
            } else {
                let fields: Vec<usize> = lines
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.trim_end().ends_with(&format!(": Option<{name}>,")))
                    .map(|(i, _)| i)
                    .collect();
                !fields.is_empty()
                    && fields.iter().all(|&i| {
                        let attrs = attrs_above(i);
                        attrs
                            .split("schema_with = \"")
                            .nth(1)
                            .and_then(|r| r.split('"').next())
                            .map(|path| path.rsplit("::").next().unwrap_or(path))
                            .is_some_and(reaches_schema)
                    })
            };
            if !schema_ok {
                failures.push(format!("{name}: its schema does not accept 1 / 0"));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");

        for (name, _) in NOT_A_BOOL_SPELLING {
            assert!(
                enums.iter().any(|(n, _)| n == name),
                "allow-list names no enum {name}"
            );
        }
        let mut names: Vec<&str> = enums.iter().map(|(n, _)| n.as_str()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "MakeLatestConfig",
                "PrereleaseConfig",
                "SkipPushConfig",
                "StringOrBool",
                "WrapInDirectory"
            ],
            "a new enum with a Bool(bool) variant must read 1 / 0 and be counted here"
        );
    }

    fn published_schema_validator() -> jsonschema::Validator {
        let schema: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../docs/site/static/schema.json"),
            )
            .unwrap(),
        )
        .unwrap();
        jsonschema::validator_for(&schema).unwrap()
    }

    /// The published schema accepts an unquoted `1` / `0` on one field of
    /// each of the five bool-or-template types (`prerelease`, `make_latest`,
    /// `fail_on_error` for `StringOrBool`, `wrap_in_directory`, `skip_push`),
    /// as the loader does, and refuses `2`. Every other field of a type
    /// shares that type's schema, which
    /// `every_bool_or_template_enum_reads_one_and_zero` holds.
    #[test]
    fn the_published_schema_accepts_one_and_zero_on_a_field_of_each_bool_or_template_type() {
        let validator = published_schema_validator();
        // `values` = prerelease, make_latest, fail_on_error,
        // wrap_in_directory, skip_push.
        let config = |v: [&str; 5]| {
            format!(
                "project_name: p\nrelease:\n  prerelease: {}\n  make_latest: {}\n  \
                 preflight:\n    fail_on_error: {}\ncrates:\n  - name: p\n    path: .\n    \
                 archives:\n      - wrap_in_directory: {}\n    docker_manifests:\n      \
                 - name_template: x\n        image_templates: [x]\n        skip_push: {}\n",
                v[0], v[1], v[2], v[3], v[4]
            )
        };
        let valid = |yaml: &str| {
            let instance: serde_json::Value = serde_yaml_ng::from_str(yaml).unwrap();
            validator.is_valid(&instance)
        };

        let yaml = config(["1", "1", "0", "1", "0"]);
        serde_yaml_ng::from_str::<crate::config::Config>(&yaml)
            .unwrap_or_else(|e| panic!("{e}\n{yaml}"));
        let instance: serde_json::Value = serde_yaml_ng::from_str(&yaml).unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{errors:#?}");

        for field in 0..5 {
            let mut v = ["1", "1", "0", "1", "0"];
            v[field] = "2";
            let yaml = config(v);
            assert!(!valid(&yaml), "the schema accepted 2:\n{yaml}");
            assert!(
                serde_yaml_ng::from_str::<crate::config::Config>(&yaml).is_err(),
                "{yaml}"
            );
        }
    }

    /// A crate's `archives` takes `false`, a list or null in both the
    /// published schema and the loader, and `true` in neither.
    #[test]
    fn the_published_schema_accepts_archives_false_and_refuses_true() {
        let validator = published_schema_validator();
        let config = |v: &str| {
            format!("project_name: p\ncrates:\n  - name: p\n    path: .\n    archives: {v}\n")
        };
        for v in ["false", "[]", "null", "[{formats: [tar.gz]}]"] {
            let yaml = config(v);
            let instance: serde_json::Value = serde_yaml_ng::from_str(&yaml).unwrap();
            let errors: Vec<String> = validator
                .iter_errors(&instance)
                .map(|e| e.to_string())
                .collect();
            assert!(errors.is_empty(), "{errors:#?}\n{yaml}");
            serde_yaml_ng::from_str::<crate::config::Config>(&yaml)
                .unwrap_or_else(|e| panic!("{e}\n{yaml}"));
        }
        let yaml = config("true");
        let instance: serde_json::Value = serde_yaml_ng::from_str(&yaml).unwrap();
        assert!(!validator.is_valid(&instance), "{yaml}");
        assert!(serde_yaml_ng::from_str::<crate::config::Config>(&yaml).is_err());
    }

    /// The published schema takes a duration string such as `"90s"` on a
    /// duration field, as the loader does, and refuses a value the loader
    /// refuses. The schema's pattern and the parser agree on every sample.
    #[test]
    fn the_published_schema_accepts_a_duration_string_on_a_duration_field() {
        let validator = published_schema_validator();
        let pattern = jsonschema::validator_for(
            &serde_json::json!({ "type": "string", "pattern": super::HUMAN_DURATION_PATTERN }),
        )
        .unwrap();
        for (v, ok) in [
            ("90s", true),
            ("10m", true),
            ("1h30m", true),
            ("500ms", true),
            ("2d", true),
            (" 1h 30m ", true),
            ("1 0s", true),
            ("90", false),
            ("s", false),
            ("10x", false),
            ("", false),
        ] {
            assert_eq!(
                super::parse_humantime_duration(v).is_ok(),
                ok,
                "parser on {v:?}"
            );
            assert_eq!(
                pattern.is_valid(&serde_json::json!(v)),
                ok,
                "pattern on {v:?}"
            );
            let yaml = format!("project_name: p\nannounce:\n  deadline: {v:?}\n");
            let instance: serde_json::Value = serde_yaml_ng::from_str(&yaml).unwrap();
            let errors: Vec<String> = validator
                .iter_errors(&instance)
                .map(|e| e.to_string())
                .collect();
            assert_eq!(errors.is_empty(), ok, "schema on {v:?}: {errors:#?}");
            assert_eq!(
                serde_yaml_ng::from_str::<crate::config::Config>(&yaml).is_ok(),
                ok,
                "loader on {v:?}"
            );
        }
    }

    /// The text of `line` up to a `//` comment that sits outside a string.
    fn before_comment(line: &str) -> &str {
        let mut in_string = false;
        let mut escaped = false;
        let bytes = line.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if in_string {
                match (escaped, b) {
                    (false, b'\\') => escaped = true,
                    (false, b'"') => in_string = false,
                    _ => escaped = false,
                }
            } else if b == b'"' {
                in_string = true;
            } else if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
                return &line[..i];
            }
        }
        line
    }

    /// Whether `code` compares something against the literal `"true"` or
    /// `"false"`: an `==` / `!=` / match-arm `|` before it, a `=>` / `|` /
    /// `==` / `!=` after it, an `eq_ignore_ascii_case` argument, or a
    /// `matches!` pattern. A closure `||` or a `"true".into()` is not one.
    fn compares_a_bool_word(code: &str) -> bool {
        for word in ["\"true\"", "\"false\""] {
            let mut from = 0;
            while let Some(at) = code[from..].find(word) {
                let start = from + at;
                let end = start + word.len();
                let before = code[..start].trim_end();
                let after = code[end..].trim_start();
                let single_bar_before = before.ends_with('|') && !before.ends_with("||");
                let single_bar_after = after.starts_with('|') && !after.starts_with("||");
                if before.ends_with("==")
                    || before.ends_with("!=")
                    || single_bar_before
                    || before.ends_with("eq_ignore_ascii_case(")
                    || after.starts_with("=>")
                    || single_bar_after
                    || after.starts_with("==")
                    || after.starts_with("!=")
                    || (before.contains("matches!(") && after.starts_with(')'))
                {
                    return true;
                }
                from = end;
            }
        }
        false
    }

    #[test]
    fn compares_a_bool_word_reads_the_comparison_shapes() {
        for code in [
            r#"if v == "true" {"#,
            r#"v != "false""#,
            r#""true" | "1" => x,"#,
            r#"| "false""#,
            r#""false" => y,"#,
            r#"v.eq_ignore_ascii_case("true")"#,
            r#"matches!(v, "true")"#,
        ] {
            assert!(compares_a_bool_word(code), "{code}");
        }
        for code in [
            r#"env.entry(k).or_insert_with(|| "true".into());"#,
            r#"let s = "true";"#,
            r#"out.push_str("false");"#,
            r#"x.starts_with("true")"#,
        ] {
            assert!(!compares_a_bool_word(code), "{code}");
        }
        assert_eq!(before_comment(r#"a == "x" // == "true""#), r#"a == "x" "#);
        assert_eq!(before_comment(r#"u = "http://h""#), r#"u = "http://h""#);
    }

    /// One allowed comparison: the file, the function that owns it, the
    /// comparing line as written (trimmed), and why it is not a reading of a
    /// bool-or-template value.
    type AllowedComparison = (&'static str, &'static str, &'static str, &'static str);

    /// Comparisons keyed by (file, function, trimmed line), each with the
    /// number of times the line occurs in that function.
    type ComparisonHits = std::collections::BTreeMap<(String, String, String), usize>;

    /// Every line of `text` (the production half of `rel`) that compares
    /// against `true` / `false`, counted under the innermost function that
    /// holds it. Returns the number of function bodies read.
    fn bool_word_comparisons(rel: &str, text: &str, hits: &mut ComparisonHits) -> usize {
        use crate::test_helpers::test_sources::{function_bodies, production_half};
        fn fn_name(body: &str) -> String {
            let first = body.lines().next().unwrap_or_default();
            let after = first.split("fn ").nth(1).unwrap_or_default();
            after
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .next()
                .unwrap_or_default()
                .to_string()
        }
        let bodies = function_bodies(production_half(text));
        for (i, body) in bodies.iter().enumerate() {
            // `function_bodies` yields a nested fn both inside its parent and
            // on its own; cutting it out of the parent counts each line once,
            // under the function that holds it.
            let mut own = body.clone();
            for nested in &bodies[i + 1..] {
                if own.contains(nested.as_str()) {
                    own = own.replacen(nested.as_str(), "", 1);
                }
            }
            let name = fn_name(body);
            for line in own.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                if compares_a_bool_word(before_comment(line)) {
                    *hits
                        .entry((rel.to_string(), name.clone(), trimmed.to_string()))
                        .or_default() += 1;
                }
            }
        }
        bodies.len()
    }

    /// The comparisons no helper and no `allowed` entry accounts for, and the
    /// `allowed` entries that match no comparison. An entry names one line of
    /// one function and covers one occurrence of it, so a second comparison
    /// added to an allowed function, or the same line copied into another
    /// function, is reported.
    fn unaccounted_comparisons<'a>(
        hits: &ComparisonHits,
        helpers: &[&str],
        allowed: &'a [AllowedComparison],
    ) -> (Vec<String>, Vec<&'a AllowedComparison>) {
        let mut used = std::collections::BTreeSet::new();
        let mut failures = Vec::new();
        for ((file, owner, line), &count) in hits {
            if helpers.contains(&owner.as_str()) {
                continue;
            }
            let entry = allowed
                .iter()
                .position(|(f, n, l, _)| f == file && n == owner && l == line);
            match entry {
                Some(i) => {
                    used.insert(i);
                    if count > 1 {
                        failures.push(format!(
                            "{file} ({owner}): {line} occurs {count} times, and its entry covers one"
                        ));
                    }
                }
                None => failures.push(format!("{file} ({owner}): {line}")),
            }
        }
        let stale = allowed
            .iter()
            .enumerate()
            .filter(|(i, _)| !used.contains(i))
            .map(|(_, e)| e)
            .collect();
        (failures, stale)
    }

    /// A function on the allowed list that gains a second comparison is
    /// reported for the new line, and an entry whose line is gone is stale.
    #[test]
    fn a_new_comparison_inside_an_allowed_function_is_reported() {
        let source = r#"
            fn reads_git(answer: &str) -> bool {
                if answer == "true" {
                    return true;
                }
                answer == "false"
            }
        "#;
        let mut hits = std::collections::BTreeMap::new();
        assert_eq!(bool_word_comparisons("x.rs", source, &mut hits), 1);
        let allowed: &[AllowedComparison] = &[
            (
                "x.rs",
                "reads_git",
                r#"if answer == "true" {"#,
                "git output",
            ),
            (
                "x.rs",
                "reads_git",
                r#"answer == "0""#,
                "a line that is gone",
            ),
        ];
        let (failures, stale) = unaccounted_comparisons(&hits, &[], allowed);
        assert_eq!(failures, [r#"x.rs (reads_git): answer == "false""#]);
        assert_eq!(stale, [&allowed[1]]);
        let (failures, _) = unaccounted_comparisons(&hits, &["reads_git"], allowed);
        assert!(
            failures.is_empty(),
            "a helper owns every line: {failures:?}"
        );
    }

    /// An allowed line copied into a second function of the same file, or a
    /// second time into the same function, is reported; an allowed line
    /// moved to another function is reported there and its entry is stale.
    /// A line in a nested fn belongs to that fn alone.
    #[test]
    fn a_copied_or_moved_allowed_comparison_is_reported() {
        let allowed: &[AllowedComparison] =
            &[("x.rs", "reads_git", r#"answer == "true""#, "git output")];
        let report = |source: &str| {
            let mut hits = ComparisonHits::new();
            bool_word_comparisons("x.rs", source, &mut hits);
            let (failures, stale) = unaccounted_comparisons(&hits, &[], allowed);
            (failures, stale.len())
        };

        let copied = r#"
        fn reads_git(answer: &str) -> bool {
            answer == "true"
        }
        fn reads_flag(answer: &str) -> bool {
            answer == "true"
        }
"#;
        assert_eq!(
            report(copied),
            (
                vec![r#"x.rs (reads_flag): answer == "true""#.to_string()],
                0
            )
        );

        let twice = r#"
        fn reads_git(answer: &str) -> bool {
            if answer.is_empty() {
                answer == "true"
            } else {
                answer == "true"
            }
        }
"#;
        let (failures, stale) = report(twice);
        assert_eq!(stale, 0);
        assert_eq!(
            failures,
            [r#"x.rs (reads_git): answer == "true" occurs 2 times, and its entry covers one"#]
        );

        let moved = r#"
        fn reads_git(answer: &str) -> bool {
            answer.is_empty()
        }
        fn reads_flag(answer: &str) -> bool {
            answer == "true"
        }
"#;
        assert_eq!(
            report(moved),
            (
                vec![r#"x.rs (reads_flag): answer == "true""#.to_string()],
                1
            )
        );

        let nested = r#"
        fn outer(answer: &str) -> bool {
            fn reads_git(answer: &str) -> bool {
                answer == "true"
            }
            reads_git(answer)
        }
"#;
        assert_eq!(report(nested), (Vec::new(), 0));
    }

    /// Production code matches a rendered value against `true` / `false`
    /// only through `is_true_spelling` / `is_false_spelling`, so the
    /// accepted spellings live in one place (the templates page's
    /// "Bool-or-template values" table documents them). The lines below
    /// compare against those words for a reason other than reading a
    /// bool-or-template value; each entry must still match its line.
    #[test]
    fn no_production_function_matches_a_rendered_value_against_true_or_false() {
        use crate::test_helpers::test_sources::workspace_production_sources;

        const HELPERS: &[&str] = &["is_true_spelling", "is_false_spelling"];
        const ALLOWED: &[AllowedComparison] = &[
            (
                "crates/cli/src/commands/tag/bump_detect.rs",
                "detect_bump_from_tokens",
                r#""none" | "false" => BumpKind::None,"#,
                "default_bump is a bump keyword, `false` being its alias for none",
            ),
            (
                "crates/core/src/artifact/filter.rs",
                "is_binary_sign_output",
                r#".is_some_and(|v| v == "true")"#,
                "artifact metadata the build writes as exactly `true`",
            ),
            (
                "crates/core/src/artifact/registry.rs",
                "only_replacing_unibins",
                r#"self.metadata.get("replaces").is_none_or(|v| v != "false")"#,
                "artifact metadata the build writes as exactly `false`",
            ),
            (
                "crates/core/src/git/status.rs",
                "shallow_from_answer",
                r#""false" => false,"#,
                "git rev-parse output",
            ),
            (
                "crates/core/src/git/status.rs",
                "shallow_from_answer",
                r#""true" => true,"#,
                "git rev-parse output",
            ),
            (
                "crates/core/src/redact.rs",
                "is_boolean_shape",
                r#""true" | "false" | "yes" | "no" | "on" | "off" | "0" | "1""#,
                "classifies an env value's shape for redaction",
            ),
            (
                "crates/core/src/template/render.rs",
                "build_tera_context",
                r#""false" => ctx.insert(k.clone(), &false),"#,
                "types a template variable as a bool for the template engine",
            ),
            (
                "crates/core/src/template/render.rs",
                "build_tera_context",
                r#""true" => ctx.insert(k.clone(), &true),"#,
                "types a template variable as a bool for the template engine",
            ),
            (
                "crates/core/src/test_helpers/bool_spellings.rs",
                "from_cell",
                r#""false" => Self::False,"#,
                "reads the outcome words of the docs table, a test helper",
            ),
            (
                "crates/core/src/test_helpers/bool_spellings.rs",
                "from_cell",
                r#""true" => Self::True,"#,
                "reads the outcome words of the docs table, a test helper",
            ),
            (
                "crates/stage-archive/src/archive_config.rs",
                "archive_metadata",
                r#"&& d == "true""#,
                "artifact metadata the build writes as exactly `true`",
            ),
            (
                "crates/stage-publish/src/homebrew/cask_custom_block/lexer.rs",
                "names_a_call",
                r#"| "false""#,
                "Ruby keywords in the cask lexer",
            ),
            (
                "crates/stage-publish/src/homebrew/cask_custom_block/lexer.rs",
                "names_a_call",
                r#"| "true""#,
                "Ruby keywords in the cask lexer",
            ),
            (
                "crates/stage-publish/src/homebrew/cask_custom_block/names.rs",
                "pattern_names",
                r#""nil" | "true" | "false" | "self" | "in" | "and" | "or" | "not""#,
                "Ruby keywords in the cask lexer",
            ),
            (
                "crates/stage-publish/src/homebrew/cask_custom_block/words.rs",
                "position",
                r#""self" | "nil" | "true" | "false" | "end" | "class" | "def""#,
                "Ruby keywords in the cask lexer",
            ),
            (
                "crates/stage-publish/src/nix/publish/build.rs",
                "detect_dynamically_linked",
                r#"if v == "true" {"#,
                "artifact metadata the build writes as exactly `true`",
            ),
            (
                "crates/stage-publish/src/schema_validation/chocolatey.rs",
                "validate_nuspec_structural",
                r#"if !matches!(value, "true" | "false" | "1" | "0") {"#,
                "the nuspec XML schema's own boolean spellings",
            ),
            (
                "crates/stage-publish/src/util/git_revert.rs",
                "check_out_branch_with_history",
                r#"if git_stdout(repo_path, &["rev-parse", "--is-shallow-repository"])?.trim() == "true" {"#,
                "git rev-parse output",
            ),
            (
                "crates/stage-sign/src/verify.rs",
                "go_parse_bool",
                r#""0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),"#,
                "Go's strconv.ParseBool spellings, which cosign reads its bool flags with",
            ),
            (
                "crates/stage-sign/src/verify.rs",
                "go_parse_bool",
                r#""1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),"#,
                "Go's strconv.ParseBool spellings, which cosign reads its bool flags with",
            ),
            (
                "crates/stage-sign/src/verify.rs",
                "resolve_keyless_identity",
                r#"let on_github_actions = env.var("GITHUB_ACTIONS").is_some_and(|v| v == "true");"#,
                "the GITHUB_ACTIONS env var, which the runner sets to `true`",
            ),
        ];

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let root = root.canonicalize().unwrap();
        let mut hits = std::collections::BTreeMap::new();
        let mut bodies_read = 0;
        for path in workspace_production_sources() {
            let rel = path
                .canonicalize()
                .unwrap()
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap();
            bodies_read += bool_word_comparisons(&rel, &text, &mut hits);
        }
        assert!(
            bodies_read > 5000,
            "read only {bodies_read} function bodies"
        );

        let (failures, stale) = unaccounted_comparisons(&hits, HELPERS, ALLOWED);
        assert!(
            failures.is_empty(),
            "match a rendered value through is_true_spelling / is_false_spelling, \
             or add the line to ALLOWED with its reason:\n{}",
            failures.join("\n")
        );
        assert!(
            stale.is_empty(),
            "ALLOWED entries that match nothing: {stale:#?}"
        );
        for helper in HELPERS {
            assert!(
                hits.keys().any(|(f, owner, _)| {
                    f == "crates/core/src/config/string_or_bool.rs" && owner == helper
                }),
                "{helper} no longer matches the words"
            );
        }
    }
}
