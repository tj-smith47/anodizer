use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ExtraFileSpec, StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// UploadConfig (generic HTTP upload)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UploadConfig {
    /// Human-readable name for this upload config.
    pub name: Option<String>,
    /// Build IDs filter: only upload artifacts whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// File extension filter: only upload artifacts with these extensions.
    pub exts: Option<Vec<String>>,
    /// Target URL template (supports template variables like {{ ProjectName }}, {{ Version }}).
    pub target: String,
    /// Username for HTTP basic auth.
    /// Resolution order: rendered `username` template → env `UPLOAD_{NAME}_USERNAME`.
    /// Set this to a literal value or a `{{ Env.X }}` template.
    pub username: Option<String>,
    /// Password for HTTP basic auth.
    ///
    /// Strongly prefer `{{ Env.UPLOAD_PASSWORD }}` (or any other env-var
    /// template) over an in-config literal — plaintext values here are NOT
    /// redacted from dry-run output and will land in `dist/config.yaml`
    /// when the pipeline runs with `--dry-run` / `--snapshot`. Resolution
    /// order: rendered `password` template → env `UPLOAD_{NAME}_SECRET`.
    /// Password-resolution cascade.
    pub password: Option<String>,
    /// HTTP method: PUT or POST (default: PUT).
    pub method: Option<String>,
    /// Upload mode: "archive" (default) or "binary".
    pub mode: Option<String>,
    /// Header name for the SHA256 checksum of the artifact.
    pub checksum_header: Option<String>,
    /// Path to PEM-encoded trusted CA certificates.
    pub trusted_certificates: Option<String>,
    /// Path to PEM-encoded client X.509 certificate for mTLS.
    pub client_x509_cert: Option<String>,
    /// Path to PEM-encoded client X.509 key for mTLS.
    pub client_x509_key: Option<String>,
    /// Include checksums in uploaded artifacts.
    pub checksum: Option<bool>,
    /// Include signatures in uploaded artifacts.
    pub signature: Option<bool>,
    /// Include metadata artifacts in uploaded artifacts.
    pub meta: Option<bool>,
    /// Custom HTTP headers (each value is template-expanded).
    pub custom_headers: Option<HashMap<String, String>>,
    /// When true, use the artifact name as-is (don't append to target URL).
    pub custom_artifact_name: Option<bool>,
    /// Extra files to include in uploading.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Upload only extra files, skip normal artifacts.
    pub extra_files_only: Option<bool>,
    /// Skip condition template (if rendered to "true", skip this upload).
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the upload is skipped.
    /// Render failure hard-errors. The `uploads[].if:` conditional gate.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// Re-upload an artifact even when an identical one already exists at the
    /// target path (default: `false`).
    ///
    /// With the default, a re-run that finds the same version's artifact
    /// already uploaded with a matching SHA-256 records an idempotent SKIP
    /// rather than re-PUTting it — so re-running a partially-failed release is
    /// safe. A path that already holds a *different* artifact for the same
    /// version still hard-errors (immutable-version drift) unless `overwrite`
    /// is set. With `overwrite: true`, every artifact is PUT unconditionally.
    pub overwrite: Option<bool>,
    /// Override whether this upload failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the
    /// release. Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// When `true`, a triggered rollback leaves this upload's artifacts in
    /// place rather than issuing a server-side DELETE. Default `false`.
    pub retain_on_rollback: Option<bool>,
}
