use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// GemFury publisher config
// ---------------------------------------------------------------------------
//
// The `gemfury:` publisher block.
// Each entry pushes deb / rpm / apk artifacts to `https://push.fury.io/<account>`.
//
// The legacy `furies:` spelling is accepted via a top-level
// `#[serde(alias = "furies")]` on [`crate::config::Config::gemfury`]; a
// one-time deprecation warning is emitted by
// [`crate::config::warn_on_legacy_furies_alias`].

/// GemFury package registry publisher configuration.
///
/// Pushes deb / rpm / apk artifacts to `https://push.fury.io/<account>`.
/// Authenticates via HTTP Basic auth using the push token as the username
/// (empty password) — the conventional Fury push surface.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GemFuryConfig {
    /// Unique identifier for selecting this entry from the CLI (`--id=...`).
    pub id: Option<String>,

    /// Build IDs filter: only include artifacts whose archive `id` is in this list.
    pub ids: Option<Vec<String>>,

    /// GemFury account name. Required; rendered through the template engine
    /// so `account: "{{ .Env.MY_FURY_ACCOUNT }}"` works.
    pub account: Option<String>,

    /// Push token used as the HTTP Basic auth username (empty password).
    /// When unset, the env var named by `secret_name` (default `FURY_TOKEN`)
    /// is consulted at publish time. NEVER logged.
    pub token: Option<String>,

    /// Environment variable name carrying the push token. Default
    /// `FURY_TOKEN`. The actual token VALUE is read from this env var at
    /// publish/rollback time.
    pub secret_name: Option<String>,

    /// Optional API token used by rollback to issue `DELETE
    /// /<account>/packages/<name>/versions/<version>`. When unset,
    /// the env var named by `api_secret_name` (default `FURY_API_TOKEN`)
    /// is consulted at rollback time. If both are absent at rollback time,
    /// the publisher falls back to a manual-cleanup warn.
    pub api_token: Option<String>,

    /// Environment variable name carrying the API (delete) token. Default
    /// `FURY_API_TOKEN`.
    pub api_secret_name: Option<String>,

    /// Package format filter: only push artifacts matching these formats.
    /// Defaults to `["apk", "deb", "rpm"]`.
    pub formats: Option<Vec<String>>,

    /// Template-conditional skip: if rendered result is `"true"`, skip this
    /// publisher entry. Accepts bool or template string.
    /// Accepts the legacy `disable:` spelling via serde alias for back-compat
    /// with imported `gemfury[].disable:` configs.
    #[serde(
        deserialize_with = "deserialize_string_or_bool_opt",
        alias = "disable",
        default
    )]
    pub skip: Option<StringOrBool>,

    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `true` — GemFury is a Manager-group publisher (mutable but
    /// reversible via the delete API), so a failed publish aborts by default
    /// to avoid surprising the operator with a half-released version. Set to
    /// `false` to log failures but continue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,

    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the GemFury publisher entry is
    /// skipped. Render failure hard-errors. The
    /// `gemfury[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}
