//! Sign / docker-sign config types.
//!
//! Lifted out of the monolithic `crate::config` module per the WAVE 5
//! split (see `.claude/known-bugs.md`'s "WAVE 5 deferred" entry). The
//! historical `anodizer_core::config::{SignConfig, DockerSignConfig}`
//! import path is preserved by re-exports at the bottom of `config.rs`.

use crate::config::{StringOrBool, deserialize_string_or_bool_opt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SignConfig {
    /// Unique identifier for this sign config.
    pub id: Option<String>,
    /// Artifact types to sign: "all", "archive", "binary", "checksum", "package", "sbom" (default: "none").
    pub artifacts: Option<String>,
    /// Signing command to invoke (default: "cosign" or "gpg").
    pub cmd: Option<String>,
    /// Arguments passed to the signing command (supports templates with ${artifact} and ${signature}).
    pub args: Option<Vec<String>>,
    /// Signature output filename template (supports templates).
    pub signature: Option<String>,
    /// Content written to the signing command's stdin.
    pub stdin: Option<String>,
    /// Path to a file whose content is written to the signing command's stdin.
    pub stdin_file: Option<String>,
    /// Build IDs filter: only sign artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Environment variables passed to the signing command.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Certificate file to embed in the signature (Cosign bundle signing).
    pub certificate: Option<String>,
    /// Capture and log stdout/stderr of the signing command.
    /// Accepts bool or template string (e.g., "{{ .IsSnapshot }}").
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub output: Option<StringOrBool>,
    /// Template-conditional: skip this sign config if rendered result is "false" or empty.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerSignConfig {
    /// Unique identifier for this docker sign config.
    pub id: Option<String>,
    /// Docker artifact types to sign: "all", "image", or "manifest" (default: "none").
    pub artifacts: Option<String>,
    /// Signing command to invoke (default: "cosign").
    pub cmd: Option<String>,
    /// Arguments passed to the signing command (supports templates).
    pub args: Option<Vec<String>>,
    /// Signature output filename template (supports templates).
    pub signature: Option<String>,
    /// Certificate file to embed in the signature (Cosign bundle signing).
    pub certificate: Option<String>,
    /// Docker config IDs filter: only sign images from configs whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Content written to the signing command's stdin.
    pub stdin: Option<String>,
    /// Path to a file whose content is written to the signing command's stdin.
    pub stdin_file: Option<String>,
    /// Environment variables passed to the signing command.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    /// Capture and log stdout/stderr of the docker signing command.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub output: Option<StringOrBool>,
    /// Template-conditional: skip this docker sign config if rendered result is "false" or empty.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
}
