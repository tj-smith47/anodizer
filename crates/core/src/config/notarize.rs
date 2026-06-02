use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{HumanDuration, StringOrBool, deserialize_string_or_bool_opt, evaluate_if_condition};

// ---------------------------------------------------------------------------
// NotarizeConfig (macOS code signing and notarization)
// ---------------------------------------------------------------------------

/// Top-level notarization configuration supporting both cross-platform
/// (`rcodesign`) and native macOS (`codesign` + `xcrun notarytool`) modes.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NotarizeConfig {
    /// Skip all notarization. Accepts bool or template string.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Cross-platform signing/notarization (rcodesign-based, works on any OS).
    pub macos: Option<Vec<MacOSSignNotarizeConfig>>,
    /// Native signing/notarization (codesign + xcrun, macOS only).
    pub macos_native: Option<Vec<MacOSNativeSignNotarizeConfig>>,
}

/// Cross-platform macOS signing and notarization via `rcodesign`.
#[derive(Debug, Clone, Serialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MacOSSignNotarizeConfig {
    /// Build IDs to filter. Default: project name.
    pub ids: Option<Vec<String>>,
    /// Skip this configuration. Accepts bool or template string.
    /// Replaces the previous `enabled:` toggle with the canonical
    /// `skip:` (inverted semantic) to align with every other publisher /
    /// pipe in anodizer.
    ///
    /// Back-compat: the upstream uses `enabled:` (opt-in, default false).
    /// A YAML carrying `enabled:` is accepted via the wire-level
    /// `enabled:` alias that inverts the bool — `enabled: true` becomes
    /// `skip: false`, `enabled: false` becomes `skip: true`. The
    /// canonical field at runtime is `skip:`.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// `true` when [`Self::skip`] holds a templated `enabled:` value that
    /// must be rendered verbatim and have its truthiness NEGATED at
    /// evaluation (a falsy `enabled` → skip). Bool / literal `enabled:`
    /// values are inverted at parse time and leave this `false`. Not part of
    /// the YAML surface — set only by the `enabled:`-alias deserializer.
    #[serde(skip)]
    pub skip_inverts_enabled: bool,
    /// Signing configuration (P12 certificate).
    pub sign: Option<MacOSSignConfig>,
    /// Notarization configuration (App Store Connect API key). Omit for sign-only.
    pub notarize: Option<MacOSNotarizeApiConfig>,
}

/// Wire-format mirror used to accept the `enabled:` field as a
/// deserialize-time alias for the canonical `skip:`. `enabled: true`
/// inverts to `skip: false` (run); `enabled: false` inverts to
/// `skip: true` (skip).
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MacOSSignNotarizeConfigWire {
    ids: Option<Vec<String>>,
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    skip: Option<StringOrBool>,
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    enabled: Option<StringOrBool>,
    sign: Option<MacOSSignConfig>,
    notarize: Option<MacOSNotarizeApiConfig>,
}

impl<'de> Deserialize<'de> for MacOSSignNotarizeConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = MacOSSignNotarizeConfigWire::deserialize(deserializer)?;
        let ResolvedSkip {
            skip,
            inverts_enabled,
        } = resolve_skip_with_enabled_alias(wire.skip, wire.enabled)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            ids: wire.ids,
            skip,
            skip_inverts_enabled: inverts_enabled,
            sign: wire.sign,
            notarize: wire.notarize,
        })
    }
}

impl MacOSSignNotarizeConfig {
    /// Whether this entry should be SKIPPED (not signed / notarized).
    ///
    /// Renders the [`Self::skip`] template via `render` and applies the
    /// inversion convention recorded in [`Self::skip_inverts_enabled`]:
    /// when the value came from a templated `enabled:`, the rendered
    /// truthiness is negated (a falsy `enabled` → skip). A render failure is
    /// propagated as `Err` so callers FAIL CLOSED rather than silently
    /// enabling a stage the operator meant to disable.
    pub fn should_skip(
        &self,
        render: impl Fn(&str) -> anyhow::Result<String>,
    ) -> anyhow::Result<bool> {
        resolve_notarize_skip(&self.skip, self.skip_inverts_enabled, render)
    }
}

/// P12-certificate signing configuration for `rcodesign sign`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MacOSSignConfig {
    /// Path to .p12 certificate file or base64-encoded contents. Templates allowed.
    pub certificate: Option<String>,
    /// Password for the .p12 certificate. Templates allowed.
    pub password: Option<String>,
    /// Path to entitlements XML file. Templates allowed.
    pub entitlements: Option<String>,
    /// RFC-3161 timestamp service URL passed to `rcodesign sign --timestamp-url`.
    /// Defaults to Apple's public timestamp service. Override when running
    /// behind a corporate proxy or when Apple's service is unreachable.
    pub timestamp_url: Option<String>,
}

impl MacOSSignConfig {
    /// Apple's public RFC-3161 timestamp service. Used so the signature
    /// carries a trusted timestamp rather than the host clock; override via
    /// `notarize.macos[*].sign.timestamp_url` when running behind a corporate
    /// proxy or when Apple's service is unreachable.
    pub const DEFAULT_TIMESTAMP_URL: &'static str = "http://timestamp.apple.com/ts01";

    /// Resolve the timestamp URL, ignoring whitespace-only overrides and
    /// falling back to [`Self::DEFAULT_TIMESTAMP_URL`].
    pub fn resolved_timestamp_url(&self) -> &str {
        self.timestamp_url
            .as_deref()
            .map(|u| u.trim())
            .filter(|u| !u.is_empty())
            .unwrap_or(Self::DEFAULT_TIMESTAMP_URL)
    }
}

/// App Store Connect API key configuration for `rcodesign notary-submit`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MacOSNotarizeApiConfig {
    /// App Store Connect API key issuer UUID. Templates allowed.
    pub issuer_id: Option<String>,
    /// Path to .p8 key file or base64-encoded contents. Templates allowed.
    pub key: Option<String>,
    /// API key ID. Templates allowed.
    pub key_id: Option<String>,
    /// Timeout for notarization status polling. Humantime-style string
    /// (e.g. `"10m"`, `"15s"`, `"1h"`). Default when omitted: `"10m"`.
    pub timeout: Option<HumanDuration>,
    /// Whether to wait for notarization to complete.
    pub wait: Option<bool>,
}

impl MacOSNotarizeApiConfig {
    /// Default notarization wait window (10 minutes).
    pub const DEFAULT_TIMEOUT: &'static str = "10m";

    /// Resolve `wait`, falling back to `false` (don't block on notary).
    pub fn resolved_wait(&self) -> bool {
        self.wait.unwrap_or(false)
    }

    /// Resolve `timeout` as a humantime string, falling back to
    /// [`Self::DEFAULT_TIMEOUT`]. Returns an owned `String` because the
    /// stored representation (`HumanDuration`) needs to be re-serialized
    /// when materializing — there's no zero-cost view into it.
    pub fn resolved_timeout(&self) -> String {
        self.timeout
            .map(|d| d.as_humantime_string())
            .unwrap_or_else(|| Self::DEFAULT_TIMEOUT.to_string())
    }
}

/// Artifact-type selector for native macOS notarization. Constrains the YAML
/// `use:` field on `notarize.macos_native` so an unsupported value fails at
/// parse time. Only `dmg` and `pkg` are valid — `notarytool` (the only
/// supported tool) is implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MacOSNativeArtifactKind {
    Dmg,
    Pkg,
}

/// Native macOS signing and notarization via `codesign` + `xcrun notarytool`.
#[derive(Debug, Clone, Serialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MacOSNativeSignNotarizeConfig {
    /// Build IDs to filter. Default: project name.
    pub ids: Option<Vec<String>>,
    /// Skip this configuration. Accepts bool or template string.
    /// Replaces `enabled:` with the canonical `skip:`. Imported
    /// configs may continue to write `enabled:` — the deserializer
    /// inverts it into `skip:` so both spellings work.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// `true` when [`Self::skip`] holds a templated `enabled:` value to be
    /// rendered verbatim and NEGATED at evaluation. See
    /// [`MacOSSignNotarizeConfig::skip_inverts_enabled`].
    #[serde(skip)]
    pub skip_inverts_enabled: bool,
    /// Artifact type to sign and notarize: `dmg` (default) or `pkg`.
    ///
    /// Anodizer-original (signs
    /// binaries directly via rcodesign). Constrained to a typed enum at
    /// parse time so an unsupported value (`zip`, `app`, etc.) fails fast
    /// instead of producing a silent no-op signing pipe.
    #[serde(rename = "use")]
    pub use_: Option<MacOSNativeArtifactKind>,
    /// Native signing configuration (Keychain).
    pub sign: Option<MacOSNativeSignConfig>,
    /// Native notarization configuration (xcrun notarytool).
    pub notarize: Option<MacOSNativeNotarizeConfig>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MacOSNativeSignNotarizeConfigWire {
    ids: Option<Vec<String>>,
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    skip: Option<StringOrBool>,
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    enabled: Option<StringOrBool>,
    #[serde(rename = "use")]
    use_: Option<MacOSNativeArtifactKind>,
    sign: Option<MacOSNativeSignConfig>,
    notarize: Option<MacOSNativeNotarizeConfig>,
}

impl<'de> Deserialize<'de> for MacOSNativeSignNotarizeConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = MacOSNativeSignNotarizeConfigWire::deserialize(deserializer)?;
        let ResolvedSkip {
            skip,
            inverts_enabled,
        } = resolve_skip_with_enabled_alias(wire.skip, wire.enabled)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            ids: wire.ids,
            skip,
            skip_inverts_enabled: inverts_enabled,
            use_: wire.use_,
            sign: wire.sign,
            notarize: wire.notarize,
        })
    }
}

impl MacOSNativeSignNotarizeConfig {
    /// Whether this native entry should be SKIPPED. Mirrors
    /// [`MacOSSignNotarizeConfig::should_skip`]: renders the [`Self::skip`]
    /// template, negates when it came from a templated `enabled:`, and
    /// FAILS CLOSED (propagates `Err`) on a render failure.
    pub fn should_skip(
        &self,
        render: impl Fn(&str) -> anyhow::Result<String>,
    ) -> anyhow::Result<bool> {
        resolve_notarize_skip(&self.skip, self.skip_inverts_enabled, render)
    }
}

/// Resolved per-config skip plus whether its evaluation must invert (the
/// value originated from a templated `enabled:`).
struct ResolvedSkip {
    skip: Option<StringOrBool>,
    /// `true` only when `skip` carries a templated `enabled:` value that
    /// must be rendered verbatim and have its truthiness negated.
    inverts_enabled: bool,
}

/// Invert the `enabled:` (opt-in, default false) into anodizer's
/// canonical `skip:` (opt-out, default false). Both keys may be present;
/// if they conflict (e.g. `skip: true` AND `enabled: true`), surface a
/// clear error.
///
/// Bool and literal `"true"`/`"false"` `enabled:` values are inverted right
/// here. A *templated* `enabled:` (e.g. `{{ .IsSnapshot }}`) cannot be
/// inverted by rewriting the template string — splicing it into a `{% if %}`
/// head yields malformed Tera. Instead the raw template is kept and the
/// returned [`ResolvedSkip::inverts_enabled`] flag tells the evaluator to
/// render it verbatim and negate the result's truthiness.
fn resolve_skip_with_enabled_alias(
    skip: Option<StringOrBool>,
    enabled: Option<StringOrBool>,
) -> Result<ResolvedSkip, String> {
    match (skip, enabled) {
        (Some(s), None) => Ok(ResolvedSkip {
            skip: Some(s),
            inverts_enabled: false,
        }),
        (None, Some(e)) => Ok(invert_enabled(e)),
        (None, None) => Ok(ResolvedSkip {
            skip: None,
            inverts_enabled: false,
        }),
        (Some(s), Some(e)) => {
            // Both spellings present. Allow the case where they agree on
            // intent ("don't run") to be lenient with imported configs;
            // disagreement is a config error. Only comparable when the
            // `enabled:` value resolves to a plain bool — a templated
            // `enabled:` cannot be statically compared to a `skip:` value.
            let resolved = invert_enabled(e);
            if !resolved.inverts_enabled && string_or_bool_eq(&s, resolved.skip.as_ref()) {
                Ok(ResolvedSkip {
                    skip: Some(s),
                    inverts_enabled: false,
                })
            } else {
                Err(format!(
                    "notarize: both `skip:` and `enabled:` are set and disagree (`skip={:?}` / inverted `enabled={:?}`); use one or the other",
                    s, resolved.skip
                ))
            }
        }
    }
}

/// Invert a single `enabled:` value into a [`ResolvedSkip`].
///
/// `Bool(b)` flips to `Bool(!b)`; literal `"true"`/`"false"` strings map to
/// the inverse bool; any other string is treated as a template, kept verbatim
/// with `inverts_enabled = true` so the evaluator renders it on its own and
/// negates the rendered truthiness (no `{{ }}` is ever spliced into a
/// condition head).
fn invert_enabled(v: StringOrBool) -> ResolvedSkip {
    match v {
        StringOrBool::Bool(b) => ResolvedSkip {
            skip: Some(StringOrBool::Bool(!b)),
            inverts_enabled: false,
        },
        StringOrBool::String(s) => match s.trim() {
            "true" => ResolvedSkip {
                skip: Some(StringOrBool::Bool(false)),
                inverts_enabled: false,
            },
            "false" => ResolvedSkip {
                skip: Some(StringOrBool::Bool(true)),
                inverts_enabled: false,
            },
            _ => ResolvedSkip {
                skip: Some(StringOrBool::String(s)),
                inverts_enabled: true,
            },
        },
    }
}

/// Resolve a per-config notarize `skip` to a "should skip" bool.
///
/// A `Bool` short-circuits without rendering. A template renders first, then:
///
/// - **direct `skip:`** (`inverts_enabled == false`) → skip when the rendered
///   value is truthy under the sibling convention ([`StringOrBool::try_evaluates_to_true`]:
///   `"true"`/`"1"` are truthy). This keeps notarize's `skip:` evaluation
///   identical to `should_skip_upload` and every other publisher gate.
/// - **inverted `enabled:`** (`inverts_enabled == true`) → the value is a
///   templated `enabled:` kept verbatim; skip when its rendered value is
///   FALSY under the shared `if:`-style convention (`""`/`false`/`0`/`no` =
///   falsy, so any other rendered value means "enabled → run"). The wider
///   falsy set is required here so an `enabled: "{{ … }}"` rendering to
///   `yes`/`on`/`1`/etc. correctly enables.
///
/// A render failure propagates as `Err` — callers FAIL CLOSED (treat the
/// entry as skipped) rather than silently signing/notarizing a stage the
/// operator meant to disable.
fn resolve_notarize_skip(
    skip: &Option<StringOrBool>,
    inverts_enabled: bool,
    render: impl Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<bool> {
    let Some(value) = skip else {
        return Ok(false);
    };
    if inverts_enabled {
        // Inverted `enabled:`: skip when the enabled expression is falsy.
        // `evaluate_if_condition` returns `true` (proceed/enabled) for any
        // non-falsy render and `Err` on render failure — negate to get
        // "should skip".
        let enabled = evaluate_if_condition(Some(value.as_str()), "notarize: enabled", render)?;
        Ok(!enabled)
    } else {
        // Direct `skip:`: sibling truthy semantics (`"true"`/`"1"` skip).
        value.try_evaluates_to_true(render)
    }
}

fn string_or_bool_eq(a: &StringOrBool, b: Option<&StringOrBool>) -> bool {
    match (a, b) {
        (StringOrBool::Bool(a), Some(StringOrBool::Bool(b))) => a == b,
        (StringOrBool::String(a), Some(StringOrBool::String(b))) => a == b,
        _ => false,
    }
}

impl MacOSNativeSignNotarizeConfig {
    /// Default `use:` selector. Anodize-original — no native
    /// notarize. DMG is the canonical signed-app distribution format
    /// for macOS releases; PKG opt-in handles installers.
    pub const DEFAULT_USE: MacOSNativeArtifactKind = MacOSNativeArtifactKind::Dmg;

    /// Resolve the `use:` selector, falling back to [`Self::DEFAULT_USE`].
    pub fn resolved_use(&self) -> MacOSNativeArtifactKind {
        self.use_.unwrap_or(Self::DEFAULT_USE)
    }
}

/// Keychain-based signing configuration for native `codesign`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MacOSNativeSignConfig {
    /// Keychain identity (e.g., "Developer ID Application: Name"). Templates allowed.
    pub identity: Option<String>,
    /// Path to Keychain file. Templates allowed.
    pub keychain: Option<String>,
    /// Options to pass to codesign (e.g., ["runtime"]). Only used for DMGs.
    pub options: Option<Vec<String>>,
    /// Path to entitlements XML file. Only used for DMGs. Templates allowed.
    pub entitlements: Option<String>,
}

/// Native notarization configuration for `xcrun notarytool`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct MacOSNativeNotarizeConfig {
    /// Notarytool stored credentials profile name. Templates allowed.
    pub profile_name: Option<String>,
    /// Whether to wait for notarization to complete.
    pub wait: Option<bool>,
    /// Timeout for `xcrun notarytool submit --timeout`. Humantime-style
    /// string (e.g. `"10m"`, `"15s"`, `"1h"`).
    pub timeout: Option<HumanDuration>,
}

impl MacOSNativeNotarizeConfig {
    /// Default notarization wait window. Aligns with the cross-platform
    /// rcodesign path (a 10-minute wait window).
    pub const DEFAULT_TIMEOUT: &'static str = "10m";

    /// Resolve `wait`, falling back to `false`. The native xcrun path
    /// prints a "submit only" message instead of polling when `wait`
    /// is false; the unwrap at this accessor pins that fallback in one
    /// place.
    pub fn resolved_wait(&self) -> bool {
        self.wait.unwrap_or(false)
    }

    /// Resolve `timeout` as a humantime string, falling back to
    /// [`Self::DEFAULT_TIMEOUT`].
    pub fn resolved_timeout(&self) -> String {
        self.timeout
            .map(|d| d.as_humantime_string())
            .unwrap_or_else(|| Self::DEFAULT_TIMEOUT.to_string())
    }
}
