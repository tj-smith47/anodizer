use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ExtraFileSpec, StringOrBool, deserialize_string_or_bool_opt};

// ---------------------------------------------------------------------------
// Artifactory publisher
// ---------------------------------------------------------------------------

/// Artifactory upload configuration.
/// Uploads artifacts to JFrog Artifactory repositories.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
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
    /// Glob patterns matched against each artifact's file name; anodizer drops
    /// any artifact whose name matches at least one glob from THIS Artifactory
    /// target only. Use it to keep heavy sidecars (checksums, signatures,
    /// SBOMs) off a given repository while archives still upload. Composes with
    /// `ids:` and `exts:` (all filters apply). `None`/empty keeps everything.
    ///
    /// ```yaml
    /// artifactories:
    ///   - target: "https://repo.example.com/{{ .ProjectName }}/{{ .Tag }}/{{ .ArtifactName }}"
    ///     exclude: ["*.sha256", "*.sig", "*.cdx.json"]
    /// ```
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    /// File extension filter: only upload artifacts matching these extensions.
    pub exts: Option<Vec<String>>,
    /// Path to client X.509 certificate for mTLS authentication.
    pub client_x509_cert: Option<String>,
    /// Path to client X.509 private key for mTLS authentication.
    pub client_x509_key: Option<String>,
    /// Debian repository distribution(s) for `.deb` uploads, written into the
    /// Artifactory `;deb.distribution=` upload matrix param so apt can index
    /// the package. Defaults to `["stable"]` when unset. A multi-element list
    /// is emitted as Artifactory's comma-separated form
    /// (`deb.distribution=bookworm,bullseye`), publishing the same `.deb` into
    /// several distributions at once. Ignored for non-`.deb` artifacts.
    pub deb_distributions: Option<Vec<String>>,
    /// Debian repository component(s) for `.deb` uploads, written into the
    /// `;deb.component=` matrix param. Defaults to `["main"]` when unset.
    /// Multiple components are emitted comma-separated. Ignored for
    /// non-`.deb` artifacts.
    pub deb_components: Option<Vec<String>>,
    /// Override the Debian architecture for `.deb` uploads
    /// (`;deb.architecture=`). When unset (the default), the architecture is
    /// derived from each artifact's build target (`x86_64` → `amd64`,
    /// `aarch64` → `arm64`, `armv7` → `armhf`, `i686` → `i386`, …), so it
    /// never needs to be set by hand. Set this only to force a value for an
    /// artifact whose target can't be mapped. Ignored for non-`.deb`
    /// artifacts.
    pub deb_architecture: Option<String>,
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
    /// Re-upload an artifact even when an identical one already exists at the
    /// target path (default: `false`).
    ///
    /// With the default, a re-run that finds the same version's artifact
    /// already uploaded with a matching SHA-256 records an idempotent SKIP
    /// rather than re-PUTting it — so re-running a partially-failed release is
    /// safe. A path that already holds a *different* artifact for the same
    /// version still hard-errors (immutable-version drift) unless `overwrite`
    /// is set. With `overwrite: true`, every artifact is PUT unconditionally
    /// (Artifactory replaces the stored copy), restoring blind-overwrite
    /// behaviour for repos configured to allow it.
    pub overwrite: Option<bool>,
    /// PEM-encoded trusted CA certificates for TLS verification.
    /// Appended to the system certificate pool.
    pub trusted_certificates: Option<String>,
    /// Template-conditional skip: if rendered result is `"true"`, skip this publisher.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Override whether this publisher failing should fail the overall release.
    ///
    /// Default: `false` — a failure here is logged but does not abort the release.
    /// Set to `true` to fail the release on any error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// Template-conditional gate: when the rendered result is falsy
    /// (`"false"` / `"0"` / `"no"` / empty), the artifactory publisher is
    /// skipped. Render failure hard-errors. The
    /// `artifactories[].if:`.
    #[serde(rename = "if")]
    pub if_condition: Option<String>,
    /// When `true`, a triggered rollback leaves this publisher's work in
    /// place rather than attempting to undo it. Default `false`.
    pub retain_on_rollback: Option<bool>,
}
