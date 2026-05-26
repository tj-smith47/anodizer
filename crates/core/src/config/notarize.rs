use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{HumanDuration, StringOrBool, deserialize_string_or_bool_opt};

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
    /// Back-compat: GoReleaser uses `enabled:` (opt-in, default false).
    /// A YAML written against GR docs is accepted via the wire-level
    /// `enabled:` alias that inverts the bool — `enabled: true` becomes
    /// `skip: false`, `enabled: false` becomes `skip: true`. The
    /// canonical field at runtime is `skip:`.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Signing configuration (P12 certificate).
    pub sign: Option<MacOSSignConfig>,
    /// Notarization configuration (App Store Connect API key). Omit for sign-only.
    pub notarize: Option<MacOSNotarizeApiConfig>,
}

/// Wire-format mirror used to accept GoReleaser's `enabled:` field as a
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
        let skip = resolve_skip_with_enabled_alias(wire.skip, wire.enabled)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            ids: wire.ids,
            skip,
            sign: wire.sign,
            notarize: wire.notarize,
        })
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
    /// Default notarization wait window. Mirrors GoReleaser
    /// `internal/pipe/notary/macos.go` (`n.Notarize.Timeout = 10 * time.Minute`).
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
    /// Replaces `enabled:` with the canonical `skip:`. Imported GR
    /// configs may continue to write `enabled:` — the deserializer
    /// inverts it into `skip:` so both spellings work.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Artifact type to sign and notarize: `dmg` (default) or `pkg`.
    ///
    /// Anodizer-original. GR's notarize.macos has no equivalent (signs
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
        let skip = resolve_skip_with_enabled_alias(wire.skip, wire.enabled)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            ids: wire.ids,
            skip,
            use_: wire.use_,
            sign: wire.sign,
            notarize: wire.notarize,
        })
    }
}

/// Invert GR's `enabled:` (opt-in, default false) into anodizer's
/// canonical `skip:` (opt-out, default false). Both keys may be present;
/// if they conflict (e.g. `skip: true` AND `enabled: true`), surface a
/// clear error.
fn resolve_skip_with_enabled_alias(
    skip: Option<StringOrBool>,
    enabled: Option<StringOrBool>,
) -> Result<Option<StringOrBool>, String> {
    match (skip, enabled) {
        (Some(s), None) => Ok(Some(s)),
        (None, Some(e)) => Ok(Some(invert_string_or_bool(e))),
        (None, None) => Ok(None),
        (Some(s), Some(e)) => {
            // Both spellings present. Allow the case where they agree on
            // intent ("don't run") to be lenient with imported configs;
            // disagreement is a config error.
            let inverted = invert_string_or_bool(e);
            if string_or_bool_eq(&s, &inverted) {
                Ok(Some(s))
            } else {
                Err(format!(
                    "notarize: both `skip:` and `enabled:` are set and disagree (`skip={:?}` / `enabled={:?}`); use one or the other",
                    s, inverted
                ))
            }
        }
    }
}

/// Invert a [`StringOrBool`]. `Bool(b)` flips; `String(tmpl)` wraps in a
/// Tera negation so the original template still drives the decision.
fn invert_string_or_bool(v: StringOrBool) -> StringOrBool {
    match v {
        StringOrBool::Bool(b) => StringOrBool::Bool(!b),
        StringOrBool::String(s) => {
            let trimmed = s.trim();
            // Map a literal `"true"` / `"false"` to its inverse without
            // wrapping in a template — keeps round-tripping simple.
            match trimmed {
                "true" => StringOrBool::Bool(false),
                "false" => StringOrBool::Bool(true),
                _ => StringOrBool::String(format!(
                    "{{% if {} %}}false{{% else %}}true{{% endif %}}",
                    trimmed
                )),
            }
        }
    }
}

fn string_or_bool_eq(a: &StringOrBool, b: &StringOrBool) -> bool {
    match (a, b) {
        (StringOrBool::Bool(a), StringOrBool::Bool(b)) => a == b,
        (StringOrBool::String(a), StringOrBool::String(b)) => a == b,
        _ => false,
    }
}

impl MacOSNativeSignNotarizeConfig {
    /// Default `use:` selector. Anodize-original — GR has no native
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
    /// rcodesign path (and GoReleaser `macos.go`'s `10 * time.Minute`).
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
