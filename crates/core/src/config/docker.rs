use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::build::BuildHooksConfig;
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
#[serde(default, deny_unknown_fields)]
pub struct DockerRetryConfig {
    /// Number of retry attempts for failed docker push operations
    /// (default: 10, set in `crates/stage-docker/src/lib.rs::resolve_retry_settings`).
    pub attempts: Option<u32>,
    /// Duration string for the initial retry delay (default: `"10s"`), in
    /// the duration format every config field uses, at most one year. A bare
    /// number is read as seconds. Examples: `"1s"`, `"500ms"`, `"10"`.
    pub delay: Option<String>,
    /// Maximum delay between retries (default: `"5m"`), in the same format
    /// as `delay`. Caps the exponential backoff so attempt-9 with a 10s base
    /// does not stretch to ~42 min. Example: `"30s"`.
    pub max_delay: Option<String>,
}

// ---------------------------------------------------------------------------
// DockerV2Config
// ---------------------------------------------------------------------------

/// Docker V2 configuration — the canonical Docker build API.
///
/// Notable fields:
/// - `images` + `tags`, two lists that together name every pushed reference
/// - `annotations` map for OCI annotations (`--annotation`)
/// - `build_args` map for build-time variables
/// - `skip` as a bool or a template for conditional opt-out
/// - `sbom` as a bool or a template; when `true` or `1`, adds `--attest=type=sbom` to buildx
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
    /// Tag suffixes (e.g., ["latest", "{{ Version }}"]). Each image is tagged with each tag.
    pub tags: Vec<String>,
    /// OCI labels to apply to the image via `--label key=value` flags.
    pub labels: Option<HashMap<String, String>>,
    /// Auto-inject the standard predefined `org.opencontainers.image.*` labels
    /// (created, source, revision, version, title, description, licenses, url,
    /// documentation, vendor), derived from project/git/Cargo context. Default
    /// ON; set `oci_labels: false` to opt out. Each auto-label is emitted only
    /// when its value is derivable, and any key the user also supplies in
    /// `labels:` wins (the auto value never clobbers an explicit user label).
    /// `created` is derived from `SOURCE_DATE_EPOCH` for byte-reproducibility
    /// (never wall-clock) and is omitted when no reproducible source date is
    /// resolvable.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub oci_labels: Option<StringOrBool>,
    /// OCI annotations to apply via `--annotation key=value` flags.
    ///
    /// On a multi-platform build a key is written to the image index
    /// (`index:key=value`) unless it already names a buildx scope: a
    /// comma-separated list of `index`, `manifest`, `index-descriptor` and
    /// `manifest-descriptor`, each optionally platform-qualified, as in
    /// `manifest:key`, `index,manifest:key` or `manifest[linux/amd64]:key`.
    /// A prefix holding any other entry is part of the key and is scoped to
    /// the index too. A single-platform build passes every key as written.
    pub annotations: Option<HashMap<String, String>>,
    /// Extra files to copy into the Docker build context.
    pub extra_files: Option<Vec<String>>,
    /// Target platforms for multi-arch builds (e.g., ["linux/amd64", "linux/arm64"]).
    pub platforms: Option<Vec<String>>,
    /// Build arguments passed as `--build-arg KEY=VALUE`.
    ///
    /// Each value is template-expanded and forwarded verbatim to buildx
    /// (one argv token per pair, no shell tokenization). Prefer
    /// `{{ Env.VAR }}` over raw user-config strings for secrets — buildx
    /// records build-args in image history by default, so plaintext values
    /// here propagate into the image metadata.
    ///
    /// A key naming a global `ARG` (one declared before the first `FROM`)
    /// also replaces that `ARG`'s default when resolving the base image
    /// behind `{{ BaseImage }}` and `{{ BaseImageDigest }}`, when its value
    /// renders non-empty. Build args render before the base image resolves,
    /// so they cannot reference `{{ BaseImage }}` themselves.
    pub build_args: Option<HashMap<String, String>>,
    /// Retry configuration for docker push operations.
    pub retry: Option<DockerRetryConfig>,
    /// Arbitrary extra flags passed to the docker build command.
    pub flags: Option<Vec<String>>,
    /// Skip this docker build entirely when the value is, or a template
    /// renders, `true` or `1` (any letter case).
    /// Accepts the legacy `disable:` spelling via serde alias for back-compat.
    #[serde(
        default,
        alias = "disable",
        deserialize_with = "deserialize_string_or_bool_opt"
    )]
    pub skip: Option<StringOrBool>,
    /// Adds `--attest=type=sbom` to the buildx build when the value is, or a
    /// template renders, `true` or `1` (any letter case). Default: `true`.
    /// A snapshot run never adds it.
    #[serde(deserialize_with = "deserialize_string_or_bool_opt", default)]
    pub sbom: Option<StringOrBool>,
    /// Pre/post hooks for this dockers_v2 config. Each hook accepts the same
    /// `cmd`/`dir`/`env`/`output` shape as build/archive hooks. `pre` hooks
    /// run after the staging directory is prepared but before `docker buildx
    /// build`; `post` hooks run after the image digest is captured. Hook
    /// commands, working directories, and env values are template-expanded;
    /// in addition to the standard template variables, hooks see:
    ///
    /// - `{{ Images }}` — list of `image:tag` references for this build.
    ///   Iterate via `{% for img in Images %}{{ img }}{% endfor %}` to mirror
    ///   a list exposure of the same field; `{{ Images | join(sep=",") }}`
    ///   reproduces a flat comma-separated string for legacy templates.
    /// - `{{ Dockerfile }}` — path to the rendered Dockerfile
    /// - `{{ ContextDir }}` — path to the buildx context staging directory
    /// - `{{ Digest }}` — the digest the registry stores for the build (post
    ///   hooks only): the image manifest single-platform, the image index
    ///   multi-platform. Read from buildx's `--metadata-file`
    ///   (`containerimage.digest`), or from `podman push --digestfile` under
    ///   `use: podman`. A build that exports no image has no digest, and a
    ///   post hook that needs one then fails the build
    /// - `{{ BaseImage }}` / `{{ BaseImageDigest }}` — final-stage base image
    ///   (the `BaseImage` / `BaseImageDigest` overlay)
    pub hooks: Option<BuildHooksConfig>,
    /// Docker backend for build commands: `"buildx"` (default) or `"podman"`.
    ///
    /// The default `"buildx"` invokes `docker buildx build` with the full set
    /// of BuildKit features (multi-platform, attestations, `--rewrite-timestamp`,
    /// SBOM, OCI exporter). Setting `use: podman` swaps the binary to
    /// `podman build` and drops every buildx-only flag. The docker stage
    /// fails an entry that combines `use: podman` with `sbom` reading true
    /// outside a snapshot (`sbom` defaults to `true`, so set `sbom: false`),
    /// or with a `--rewrite-timestamp`, `--sbom`, `--provenance`, `--attest`,
    /// `--output`, `--cache-from` or `--cache-to` flag, because plain podman
    /// does not recognise them. Any value other than `buildx` or `podman`
    /// fails the same way.
    ///
    /// **Linux-only.** On a macOS or Windows host the docker stage fails an
    /// entry with `use: podman` with an error naming the entry, before any
    /// `podman` command runs. `check config` does not check the host.
    #[serde(rename = "use")]
    pub use_backend: Option<String>,
    // No `skip_push` field — use the canonical `skip:` to suppress
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
#[serde(default, deny_unknown_fields)]
pub struct DockerDigestConfig {
    /// Disable docker digest artifact creation when the value is, or a
    /// template renders, `true` or `1` (any letter case).
    /// Accepts the legacy `disable:` spelling via serde alias for back-compat.
    #[serde(
        alias = "disable",
        deserialize_with = "deserialize_string_or_bool_opt",
        default
    )]
    pub skip: Option<StringOrBool>,
    /// Template for the digest artifact filename.
    /// Default: tag-based naming (e.g., "ghcr.io_owner_app_v1.0.0.digest").
    pub name_template: Option<String>,
}

// ---------------------------------------------------------------------------
// DockerManifestConfig
// ---------------------------------------------------------------------------

/// Deprecated: prefer `dockers_v2`, which produces multi-arch manifests from
/// its `platforms:` field. `docker_manifests` is kept so imported configs
/// still load, and for stitching together manifest lists from images that
/// were not built by `dockers_v2` in the same run.
// This doc comment is where the deprecation reaches config authors: it
// generates the JSON schema editors read and the configuration reference.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DockerManifestConfig {
    /// Template for the manifest name, e.g. "ghcr.io/owner/app:{{ Version }}".
    pub name_template: String,
    /// Image references to include in the manifest.
    pub image_templates: Vec<String>,
    /// Extra flags for `docker manifest create`.
    pub create_flags: Option<Vec<String>>,
    /// Extra flags for `docker manifest push`.
    pub push_flags: Option<Vec<String>>,
    /// Skip push: `true`, `false`, `"auto"` (skip for prereleases), or a
    /// template rendered at run time. The words `true`, `false`, `1`, `0`
    /// and `auto` load in any letter case, quoted or unquoted; any other
    /// string is a template.
    ///
    /// ```yaml
    /// skip_push: "{{ IsSnapshot }}"
    /// ```
    #[schemars(schema_with = "skip_push_schema")]
    pub skip_push: Option<SkipPushConfig>,
    /// Unique identifier for this manifest config.
    pub id: Option<String>,
    /// Docker backend for manifest commands: `"docker"` (default) or
    /// `"podman"`; any other value (including `"buildx"`) fails the docker
    /// stage. The `"podman"` backend is **Linux-only**: on a macOS or
    /// Windows host the docker stage fails before any `podman` command runs.
    /// `check config` does not check the host.
    #[serde(rename = "use")]
    pub use_backend: Option<String>,
    /// Retry configuration for manifest push (handles transient registry errors).
    pub retry: Option<DockerRetryConfig>,
}
