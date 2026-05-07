use anyhow::{Context as _, Result};

use anodizer_core::config::{DockerDigestConfig, DockerV2Config, SkipPushConfig, StringOrBool};
use anodizer_core::context::Context;

use super::detect::docker_supports_provenance;

// ---------------------------------------------------------------------------
// resolve_backend
// ---------------------------------------------------------------------------

/// Resolve the docker backend binary and subcommand for build operations.
///
/// Returns `(binary, subcommands)`:
/// - `"docker"` backend  → `("docker", ["build"])`
/// - `"buildx"` backend  → `("docker", ["buildx", "build"])`
/// - `"podman"` backend  → `("podman", ["build"])`
///
/// When `use_backend` is `None`, the default is `"buildx"` if there are
/// multiple platforms, otherwise `"docker"`.
pub fn resolve_backend(
    use_backend: Option<&str>,
    _multi_platform: bool,
) -> Result<(&str, Vec<&str>)> {
    match use_backend {
        Some("docker") => Ok(("docker", vec!["build"])),
        Some("podman") => Ok(("podman", vec!["build"])),
        Some("buildx") => Ok(("docker", vec!["buildx", "build"])),
        Some(other) => {
            anyhow::bail!(
                "unknown docker backend '{}'; expected 'docker', 'buildx', or 'podman'",
                other
            );
        }
        // Default to plain docker (matching GoReleaser).
        // Users must explicitly set `use: buildx` for buildx features
        // including multi-platform builds.
        None => Ok(("docker", vec!["build"])),
    }
}

// ---------------------------------------------------------------------------
// Shared command-construction helpers
// ---------------------------------------------------------------------------
//
// `build_docker_command` (V1) and `build_docker_v2_command` (V2) share the
// same skeleton: backend prefix → `--progress=plain` → `--platform=…` →
// `--tag …` (× n) → `--label …` (× n) → trailing positional context dir.
// Centralise that common header so the two paths can never drift apart on
// argv ordering or quoting rules.

/// Append the backend command prefix (binary + sub-commands).
fn push_backend_prefix(cmd: &mut Vec<String>, binary: &str, subcommands: &[&str]) {
    cmd.push(binary.to_string());
    for sub in subcommands {
        cmd.push((*sub).to_string());
    }
}

/// Append `--platform=<comma-joined>` when `platforms` is non-empty.
fn push_platforms(cmd: &mut Vec<String>, platforms: &[&str]) {
    if !platforms.is_empty() {
        cmd.push(format!("--platform={}", platforms.join(",")));
    }
}

/// Append `--tag <tag>` pairs for every entry.
fn push_tags<S: AsRef<str>>(cmd: &mut Vec<String>, tags: &[S]) {
    for tag in tags {
        cmd.push("--tag".to_string());
        cmd.push(tag.as_ref().to_string());
    }
}

/// Append `--label key=value` pairs for every entry.
fn push_labels(cmd: &mut Vec<String>, labels: &[(String, String)]) {
    for (key, value) in labels {
        cmd.push("--label".to_string());
        cmd.push(format!("{}={}", key, value));
    }
}

// ---------------------------------------------------------------------------
// V1 spec + builder
// ---------------------------------------------------------------------------

/// Spec for the legacy (V1) `docker build` invocation.
///
/// Bundles every parameter previously taken positionally by
/// `build_docker_command`. Fields are borrowed; the struct is short-lived.
#[derive(Clone, Copy)]
pub struct DockerV1Spec<'a> {
    /// Path to the directory that acts as the Docker build context
    /// (already contains the Dockerfile and binaries).
    pub staging_dir: &'a str,
    /// Docker platform strings, e.g. `["linux/amd64", "linux/arm64"]`.
    pub platforms: &'a [&'a str],
    /// Fully-qualified image tags.
    pub tags: &'a [&'a str],
    /// Rendered `build_flag_templates`.
    pub extra_flags: &'a [String],
    /// When `true`, adds `--push` to the command (buildx only).
    pub push: bool,
    /// Additional flags appended after `--push` (buildx only).
    pub push_flags: &'a [String],
    /// OCI labels emitted as `--label key=value` flags.
    pub labels: &'a [(String, String)],
    /// Backend selection: `"docker"`, `"buildx"`, or `"podman"`.
    pub use_backend: Option<&'a str>,
}

/// Construct the docker build command arguments (V1 path).
pub fn build_docker_command(spec: &DockerV1Spec<'_>) -> Result<Vec<String>> {
    let DockerV1Spec {
        staging_dir,
        platforms,
        tags,
        extra_flags,
        push,
        push_flags,
        labels,
        use_backend,
    } = *spec;

    let multi_platform = platforms.len() > 1;
    let (binary, subcommands) = resolve_backend(use_backend, multi_platform)?;

    let mut cmd: Vec<String> = Vec::new();
    push_backend_prefix(&mut cmd, binary, &subcommands);
    // Always use plain progress for CI-friendly, verbose output.
    cmd.push("--progress=plain".to_string());
    push_platforms(&mut cmd, platforms);
    push_tags(&mut cmd, tags);
    push_labels(&mut cmd, labels);

    // Extra build flags (rendered build_flag_templates)
    for flag in extra_flags {
        cmd.push(flag.clone());
    }

    // Determine the effective backend for --load/--push logic.
    // Default is "docker" (matching GoReleaser); users must explicitly set
    // `use: buildx` for buildx features including multi-platform builds.
    let effective_backend = use_backend.unwrap_or("docker");

    // --push in live mode (unless skip_push).  The --push flag is only valid
    // for buildx; plain `docker build` and `podman build` do NOT support it.
    // For non-buildx backends, push is handled separately after the build via
    // `docker push` / `podman push` per tag.
    //
    // When using buildx without --push, add --load so the image is available
    // locally (otherwise the built image vanishes).  --load is incompatible
    // with multi-platform builds, so only add it for single-platform buildx.
    if push && effective_backend == "buildx" {
        cmd.push("--push".to_string());
        // Additional push flags (buildx only — these are build-time flags
        // like --provenance that only make sense with buildx --push)
        for flag in push_flags {
            cmd.push(flag.clone());
        }
    } else if effective_backend == "buildx" && !multi_platform {
        cmd.push("--load".to_string());
    }

    // Auto-add --provenance=false and --sbom=false for buildx builds, but
    // only when Docker actually supports these flags (probed once and cached).
    // Buildx defaults can inject unwanted attestation manifests and slow down
    // CI — GoReleaser probes `docker build --help` before adding them.
    if effective_backend == "buildx" && docker_supports_provenance() {
        let flags_str = extra_flags.join(" ");
        if !flags_str.contains("--provenance") {
            cmd.push("--provenance=false".to_string());
        }
        if !flags_str.contains("--sbom") {
            cmd.push("--sbom=false".to_string());
        }
    }

    // Build context directory (positional, last argument)
    cmd.push(staging_dir.to_string());

    Ok(cmd)
}

// ---------------------------------------------------------------------------
// build_docker_v2_command
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// V2 spec + builder
// ---------------------------------------------------------------------------

/// Spec for the V2 `docker buildx build` invocation.
///
/// Bundles every parameter previously taken positionally by
/// `build_docker_v2_command`. Fields are borrowed; the struct is short-lived.
#[derive(Clone, Copy)]
pub struct DockerV2Spec<'a> {
    /// Path to the directory that acts as the Docker build context.
    pub staging_dir: &'a str,
    /// Docker platform strings, e.g. `["linux/amd64", "linux/arm64"]`.
    pub platforms: &'a [&'a str],
    /// Fully-qualified `image:tag` references (pre-computed from images × tags).
    pub image_tags: &'a [String],
    /// `--build-arg KEY=VALUE` pairs.
    pub build_args: &'a [(String, String)],
    /// `--annotation KEY=VALUE` pairs.
    pub annotations: &'a [(String, String)],
    /// `--label KEY=VALUE` pairs.
    pub labels: &'a [(String, String)],
    /// Arbitrary extra flags passed directly to buildx.
    pub flags: &'a [String],
    /// When true, adds `--attest=type=sbom`.
    pub sbom: bool,
    /// When `true`, adds `--push` to the command.
    pub push: bool,
    /// When `true`, adds `--load` for single-platform non-push builds
    /// (requires a running Docker daemon).
    pub load: bool,
}

/// Construct the docker build command arguments for a Docker V2 config.
pub fn build_docker_v2_command(spec: &DockerV2Spec<'_>) -> Result<Vec<String>> {
    let DockerV2Spec {
        staging_dir,
        platforms,
        image_tags,
        build_args,
        annotations,
        labels,
        flags,
        sbom,
        push,
        load,
    } = *spec;

    // V2 always uses buildx when platforms are specified
    let multi_platform = platforms.len() > 1;
    let (binary, subcommands) = resolve_backend(Some("buildx"), multi_platform)?;

    let mut cmd: Vec<String> = Vec::new();
    push_backend_prefix(&mut cmd, binary, &subcommands);
    // Always use plain progress for CI-friendly, verbose output that helps
    // diagnose buildx errors (e.g., COPY failures, attestation issues).
    cmd.push("--progress=plain".to_string());
    push_platforms(&mut cmd, platforms);
    push_tags(&mut cmd, image_tags);

    // --build-arg KEY=VALUE
    for (key, value) in build_args {
        cmd.push("--build-arg".to_string());
        cmd.push(format!("{}={}", key, value));
    }

    // --annotation KEY=VALUE
    // For multi-platform builds, GoReleaser v2 prefixes annotation values with
    // "index:" so they target the manifest index rather than individual platform images.
    for (key, value) in annotations {
        cmd.push("--annotation".to_string());
        if multi_platform {
            // Add "index:" prefix, but avoid double-prefixing if already present
            let prefixed_key = if key.starts_with("index:") {
                key.clone()
            } else {
                format!("index:{}", key)
            };
            cmd.push(format!("{}={}", prefixed_key, value));
        } else {
            cmd.push(format!("{}={}", key, value));
        }
    }

    push_labels(&mut cmd, labels);

    // Arbitrary extra flags
    for flag in flags {
        cmd.push(flag.clone());
    }

    // Use --attest=type=sbom for proper OCI attestation (matching GoReleaser v2)
    // rather than the older --sbom=true flag.
    if sbom {
        cmd.push("--attest=type=sbom".to_string());
    }

    // --push / --load logic
    if push {
        cmd.push("--push".to_string());
    } else if load && !multi_platform {
        cmd.push("--load".to_string());
    }
    // When neither push nor load: buildx builds to cache only (no daemon needed)

    // NOTE: GoReleaser V2 does NOT auto-add --provenance=false or --sbom=false.
    // Only the legacy docker pipe does that. V2 relies on explicit user flags
    // or the --attest=type=sbom flag set above.

    // Write image digest to file for capture (GoReleaser V2 behavior).
    // This works even without --push (no daemon needed for digest capture).
    cmd.push(format!("--iidfile={}/id.txt", staging_dir));

    // Build context directory (positional, last argument)
    cmd.push(staging_dir.to_string());

    Ok(cmd)
}

/// Evaluate whether a Docker V2 config is skipped.
///
/// Checks the `skip` field: if it's a template, renders it and checks for "true".
/// Returns `true` when the config should be skipped. Surfaces template render
/// errors instead of silently treating them as "not skipped".
pub fn is_docker_v2_skipped(skip: &Option<StringOrBool>, ctx: &Context) -> Result<bool> {
    match skip {
        None => Ok(false),
        Some(d) => d
            .try_evaluates_to_true(|s| ctx.render_template(s))
            .with_context(|| "docker_v2: render skip template"),
    }
}

/// Resolve the docker digest configuration for a crate into job-level fields.
///
/// Returns `(skip_digest, digest_name_template)`.
/// Errors if the `name_template` contains a template expression that fails to render.
pub(crate) fn resolve_digest_config(
    cfg: Option<&DockerDigestConfig>,
    ctx: &Context,
) -> Result<(bool, Option<String>)> {
    let Some(dc) = cfg else {
        return Ok((false, None));
    };
    let skip_digest = match &dc.skip {
        None => false,
        Some(d) => d
            .try_evaluates_to_true(|s| ctx.render_template(s))
            .with_context(|| "docker: render digest skip template")?,
    };
    let name_template = match dc.name_template.as_ref() {
        Some(tmpl) => {
            let rendered = ctx.render_template(tmpl).with_context(|| {
                format!("docker: failed to render digest name_template '{}'", tmpl)
            })?;
            if rendered.is_empty() {
                None
            } else {
                Some(rendered)
            }
        }
        None => None,
    };
    Ok((skip_digest, name_template))
}

/// Apply GoReleaser-compatible defaults to a single Docker V2 config.
///
/// Mirrors `internal/pipe/docker/v2/docker.go::Default()`:
///   - `id`         defaults to the project name
///   - `dockerfile` defaults to `"Dockerfile"`
///   - `tags`       defaults to `["{{ .Tag }}"]`
///   - `platforms`  defaults to `["linux/amd64", "linux/arm64"]`
///   - `sbom`       defaults to `Some(StringOrBool::Bool(true))`
///
/// Retry defaults are applied later by `resolve_retry_params` (10 attempts,
/// 10s base, 5m max) so they aren't repeated here.
pub fn apply_docker_v2_defaults(mut cfg: DockerV2Config, project_name: &str) -> DockerV2Config {
    if cfg.id.is_none() {
        cfg.id = Some(project_name.to_string());
    }
    if cfg.dockerfile.is_empty() {
        cfg.dockerfile = "Dockerfile".to_string();
    }
    if cfg.tags.is_empty() {
        cfg.tags = vec!["{{ .Tag }}".to_string()];
    }
    if cfg.platforms.is_none() {
        cfg.platforms = Some(vec!["linux/amd64".to_string(), "linux/arm64".to_string()]);
    }
    if cfg.sbom.is_none() {
        cfg.sbom = Some(StringOrBool::Bool(true));
    }
    cfg
}

/// Evaluate whether the sbom flag should be added for a Docker V2 config.
///
/// The `sbom` field is a [`StringOrBool`]. When it evaluates to true, the
/// `--attest=type=sbom` flag is added to the buildx command. Surfaces template
/// render errors instead of silently treating them as "not enabled".
///
/// Default-on: matches GoReleaser `internal/pipe/docker/v2/docker.go:85-87`,
/// which sets `SBOM = "true"` at `Default()` time. Users opt out with
/// `sbom: false` (or a templated string evaluating to `"false"`).
///
/// The default-on policy is enforced in two complementary places:
/// 1. The `Default()`-apply block populates `cfg.sbom = Some(Bool(true))` so
///    the resolved config written to `dist/config.yaml` round-trips faithfully
///    (matches the `resolved_*()` lazy-defaults pattern used elsewhere).
/// 2. This helper's `None` branch returns `Ok(true)` so any caller that
///    bypasses the `Default()` apply (tests, hypothetical alternate entry
///    points) still observes the canonical default.
pub fn is_docker_v2_sbom_enabled(sbom: &Option<StringOrBool>, ctx: &Context) -> Result<bool> {
    match sbom {
        None => Ok(true),
        Some(s) => s
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| "docker_v2: render sbom template"),
    }
}

/// Generate fully-qualified image references by combining each image with each tag.
///
/// For example, images=["ghcr.io/owner/app", "docker.io/owner/app"] and
/// tags=["latest", "v1.0.0"] produces:
/// - ghcr.io/owner/app:latest
/// - ghcr.io/owner/app:v1.0.0
/// - docker.io/owner/app:latest
/// - docker.io/owner/app:v1.0.0
pub fn generate_v2_image_tags(images: &[String], tags: &[String]) -> Vec<String> {
    let mut result = Vec::with_capacity(images.len() * tags.len());
    for image in images {
        for tag in tags {
            result.push(format!("{}:{}", image, tag));
        }
    }
    result.sort();
    result.dedup();
    result
}

/// Resolve whether to skip push based on `SkipPushConfig` and prerelease status.
pub fn resolve_skip_push(skip_push: &Option<SkipPushConfig>, ctx: &Context) -> bool {
    match skip_push {
        Some(SkipPushConfig::Bool(b)) => *b,
        Some(SkipPushConfig::Auto) => {
            // Skip push for prereleases
            ctx.template_vars()
                .get("Prerelease")
                .map(|p| !p.is_empty())
                .unwrap_or(false)
        }
        Some(SkipPushConfig::Template(tmpl)) => {
            // GoReleaser docker.go:343,346: exact `"true"` / `"auto"`
            // string match on the trimmed render, case-sensitive. `"TRUE"` or
            // `"True"` must NOT skip push.
            ctx.render_template(tmpl)
                .map(|rendered| {
                    let trimmed = rendered.trim();
                    trimmed == "true"
                        || (trimmed == "auto"
                            && ctx
                                .template_vars()
                                .get("Prerelease")
                                .map(|p| !p.is_empty())
                                .unwrap_or(false))
                })
                .unwrap_or(false)
        }
        None => false,
    }
}
