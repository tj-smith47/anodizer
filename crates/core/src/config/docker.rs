use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::release::{SkipPushConfig, skip_push_schema};
use super::{StringOrBool, deserialize_string_or_bool_opt};

// Use `DockerV2Config` (canonical) for docker image builds.

/// Per-pipe retry configuration for `docker.retry` / `docker_manifest.retry`.
///
/// **Deprecated**: prefer the top-level `retry:` block ([`super::RetryConfig`])
/// which applies to docker pipes (and every other network-bound stage) via
/// `Project.Retry`. When a per-pipe block is present alongside the top-level
/// block, the per-pipe values win for back-compat, but
/// `stage-docker::resolve_retry_params` emits a one-shot deprecation warning.
/// New configs should leave this field unset.
//
// Note: `#[deprecated]` on the type cascades through derive-generated impls
// (Default, Serialize, JsonSchema, ...) and is hard to silence cleanly, so the
// deprecation lives in (a) this rustdoc prose, (b) the runtime `tracing::warn!`
// fired once per process by `stage-docker::resolve_retry_params`, and (c) the
// schemars-generated JSON-schema description carries the same prose for
// editor / IDE consumers.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerRetryConfig {
    /// Number of retry attempts for failed docker push operations
    /// (default: 10, set in `crates/stage-docker/src/lib.rs::resolve_retry_settings`).
    pub attempts: Option<u32>,
    /// Duration string for the initial retry delay (default: `"10s"`).
    /// Examples: `"1s"`, `"500ms"`.
    pub delay: Option<String>,
    /// Maximum delay between retries (default: `"5m"`). Caps the exponential
    /// backoff so attempt-9 with a 10s base does not stretch to ~42 min.
    /// Example: `"30s"`.
    pub max_delay: Option<String>,
}

// ---------------------------------------------------------------------------
// DockerV2Config
// ---------------------------------------------------------------------------

/// Docker V2 configuration — the canonical Docker build API.
///
/// Notable surface:
/// - `images` + `tags` (cleaner separation than a single `image_templates` list)
/// - `annotations` map for OCI annotations (`--annotation`)
/// - `build_args` map for build-time variables
/// - `skip` as a [`StringOrBool`] template for conditional opt-out
/// - `sbom` as a [`StringOrBool`] — when truthy, adds `--sbom=true` to buildx
/// - `flags` for arbitrary extra `docker build` flags
/// - `platforms` is the only target selector — no per-arch field overrides
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DockerV2Config {
    /// Unique identifier for this Docker V2 config.
    pub id: Option<String>,
    /// Build IDs filter: only include binary artifacts whose metadata `id` is in this list.
    pub ids: Option<Vec<String>>,
    /// Path to the Dockerfile relative to the project root.
    pub dockerfile: String,
    /// Base image names (e.g., ["ghcr.io/owner/app"]). Combined with `tags` to form full references.
    pub images: Vec<String>,
    /// Tag suffixes (e.g., ["latest", "{{ .Version }}"]). Each image is tagged with each tag.
    pub tags: Vec<String>,
    /// OCI labels to apply to the image via `--label key=value` flags.
    pub labels: Option<HashMap<String, String>>,
    /// OCI annotations to apply via `--annotation key=value` flags.
    pub annotations: Option<HashMap<String, String>>,
    /// Extra files to copy into the Docker build context.
    pub extra_files: Option<Vec<String>>,
    /// Target platforms for multi-arch builds (e.g., ["linux/amd64", "linux/arm64"]).
    pub platforms: Option<Vec<String>>,
    /// Build arguments passed as `--build-arg KEY=VALUE`.
    pub build_args: Option<HashMap<String, String>>,
    /// Retry configuration for docker push operations.
    pub retry: Option<DockerRetryConfig>,
    /// Arbitrary extra flags passed to the docker build command.
    pub flags: Option<Vec<String>>,
    /// When truthy, skip this docker build entirely. Supports templates.
    /// Accepts the legacy `disable:` spelling via serde alias for back-compat
    /// with imported GoReleaser configs (GR's docker config field is
    /// `pkg/config/config.go:1149` `Disable string`).
    #[serde(
        default,
        alias = "disable",
        deserialize_with = "deserialize_string_or_bool_opt"
    )]
    pub skip: Option<StringOrBool>,
    /// When truthy, adds `--sbom=true` to buildx. Supports templates.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub sbom: Option<StringOrBool>,
    // No `skip_push` field — use the canonical `skip:` (DEC-6) to suppress
    // the publish step (matches every other publisher / pipe in anodizer).
}

// ---------------------------------------------------------------------------
// DockerDigestConfig
// ---------------------------------------------------------------------------

/// Controls docker image digest file creation.
///
/// After each docker image push, a digest file (containing the sha256 digest)
/// is written to the dist directory. This config controls whether that happens
/// and how the files are named.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerDigestConfig {
    /// When truthy, disable docker digest artifact creation.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub skip: Option<StringOrBool>,
    /// Template for the digest artifact filename.
    /// Default: tag-based naming (e.g., "ghcr.io_owner_app_v1.0.0.digest").
    pub name_template: Option<String>,
}

// ---------------------------------------------------------------------------
// DockerManifestConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct DockerManifestConfig {
    /// Template for the manifest name, e.g. "ghcr.io/owner/app:{{ .Version }}".
    pub name_template: String,
    /// Image references to include in the manifest.
    pub image_templates: Vec<String>,
    /// Extra flags for `docker manifest create`.
    pub create_flags: Option<Vec<String>>,
    /// Extra flags for `docker manifest push`.
    pub push_flags: Option<Vec<String>>,
    /// Skip push: true, false, or "auto" (skip for prereleases).
    #[schemars(schema_with = "skip_push_schema")]
    pub skip_push: Option<SkipPushConfig>,
    /// Unique identifier for this manifest config.
    pub id: Option<String>,
    /// Docker backend for manifest commands: "docker" (default) or "podman".
    #[serde(rename = "use")]
    pub use_backend: Option<String>,
    /// Retry configuration for manifest push (handles transient registry errors).
    pub retry: Option<DockerRetryConfig>,
}
