use super::*;

// ---------------------------------------------------------------------------
// Artifactory publisher
// ---------------------------------------------------------------------------

/// Artifactory upload configuration.
/// Uploads artifacts to JFrog Artifactory repositories.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct ArtifactoryConfig {
    /// Human-readable name for this publisher (used in logs).
    pub name: Option<String>,
    /// Target URL template for uploads (supports template variables).
    pub target: Option<String>,
    /// Upload mode: "archive" (upload archives) or "binary" (upload binaries).
    pub mode: Option<String>,
    /// Artifactory username for authentication.
    pub username: Option<String>,
    /// Artifactory password or API key (or env var reference).
    pub password: Option<String>,
    /// Build IDs filter: only upload artifacts from builds whose `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// File extension filter: only upload artifacts matching these extensions.
    pub exts: Option<Vec<String>>,
    /// Path to client X.509 certificate for mTLS authentication.
    pub client_x509_cert: Option<String>,
    /// Path to client X.509 private key for mTLS authentication.
    pub client_x509_key: Option<String>,
    /// Custom HTTP headers sent with each upload request.
    pub custom_headers: Option<HashMap<String, String>>,
    /// Header name used for checksum verification (e.g. `X-Checksum-Sha256`).
    pub checksum_header: Option<String>,
    /// Extra files to upload alongside build artifacts.
    pub extra_files: Option<Vec<ExtraFileSpec>>,
    /// Include checksums in uploaded artifacts.
    pub checksum: Option<bool>,
    /// Include signatures in uploaded artifacts.
    pub signature: Option<bool>,
    /// Include metadata artifacts in uploaded artifacts.
    pub meta: Option<bool>,
    /// Use custom artifact naming instead of default.
    pub custom_artifact_name: Option<bool>,
    /// When true, upload only extra_files (skip normal artifacts).
    pub extra_files_only: Option<bool>,
    /// HTTP method to use for uploads (default: "PUT").
    pub method: Option<String>,
    /// PEM-encoded trusted CA certificates for TLS verification.
    /// Appended to the system certificate pool.
    pub trusted_certificates: Option<String>,
    /// Template-conditional skip: if rendered result is `"true"`, skip this publisher.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
}
