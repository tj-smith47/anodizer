use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{DockerRetryConfig, SkipPushConfig, StringOrBool};
use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anodize_core::stage::Stage;
use anodize_core::target::map_target;

// ---------------------------------------------------------------------------
// find_image_digest
// ---------------------------------------------------------------------------

/// Look up the digest for a docker image tag from the list of artifacts.
///
/// Searches for a `DockerImage` artifact whose `tag` metadata matches the given
/// image reference and returns its `digest` metadata value (e.g.,
/// `sha256:abc123...`).  The digest may be stored as the full
/// `registry/repo@sha256:...` string (from `docker inspect`), so we extract
/// just the `sha256:...` portion when present.
fn find_image_digest(artifacts: &[Artifact], image: &str) -> Option<String> {
    for a in artifacts {
        if a.kind != ArtifactKind::DockerImage {
            continue;
        }
        let tag = match a.metadata.get("tag") {
            Some(t) => t,
            None => continue,
        };
        if tag != image {
            continue;
        }
        if let Some(digest) = a.metadata.get("digest") {
            if digest.is_empty() {
                return None;
            }
            // docker inspect returns "registry/repo@sha256:abc..." — extract
            // just the "sha256:..." part for use in manifest references.
            if let Some(at_pos) = digest.find('@') {
                return Some(digest[at_pos + 1..].to_string());
            }
            // Already a bare digest (sha256:...)
            return Some(digest.clone());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// is_retriable_error
// ---------------------------------------------------------------------------

/// Determine whether a docker error message indicates a transient
/// network/registry failure that is worth retrying, as opposed to a build
/// failure (bad Dockerfile, missing files, etc.) that will never succeed.
pub fn is_retriable_error(error_msg: &str) -> bool {
    let retriable_patterns = [
        "dial tcp",
        "connection refused",
        "connection reset",
        "received unexpected HTTP status: 500 Internal Server Error",
        "received unexpected HTTP status: 502 Bad Gateway",
        "received unexpected HTTP status: 503 Service Unavailable",
        "received unexpected HTTP status: 504 Gateway Timeout",
        "EOF",
        "timeout",
        "TLS handshake",
        "i/o timeout",
        "server misbehaving",
        "no such host",
        "REFUSED_STREAM",
        "registry returned status",
        // GoReleaser V2 retries on manifest verification failures
        "manifest verification failed for digest",
    ];
    let lower = error_msg.to_lowercase();
    retriable_patterns
        .iter()
        .any(|p| lower.contains(&p.to_lowercase()))
}

// ---------------------------------------------------------------------------
// docker_supports_provenance  (cached probe)
// ---------------------------------------------------------------------------

/// Cached result of probing `docker buildx build --help` for `--provenance`.
///
/// GoReleaser probes `docker build --help` output before unconditionally
/// adding `--provenance=false` and `--sbom=false`.  We do the same: run the
/// help command once, cache the result, and only add the flags when the
/// installed Docker version actually recognises them.
static DOCKER_SUPPORTS_PROVENANCE: OnceLock<bool> = OnceLock::new();

fn docker_supports_provenance() -> bool {
    *DOCKER_SUPPORTS_PROVENANCE.get_or_init(|| {
        // Try `docker buildx build --help` first (buildx is the common path).
        // Fall back to `docker build --help` for non-buildx installs.
        let output = Command::new("docker")
            .args(["buildx", "build", "--help"])
            .output()
            .or_else(|_| Command::new("docker").args(["build", "--help"]).output());

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.contains("--provenance")
            }
            Err(_) => false, // docker not available — skip the flags
        }
    })
}

// ---------------------------------------------------------------------------
// check_buildx_driver
// ---------------------------------------------------------------------------

/// Check the current buildx driver and warn if it is not one of the standard
/// types ("docker-container" or "docker").
///
/// GoReleaser v2 validates the driver via `docker buildx inspect` and errors
/// on invalid drivers. We warn rather than error to be lenient, but the
/// check ensures users know their setup may not work for multi-platform builds.
fn check_buildx_driver(log: &StageLogger) {
    let output = Command::new("docker")
        .args(["buildx", "inspect"])
        .output();
    match output {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            // Parse the Driver line from `docker buildx inspect` output.
            // Example: "Driver:           docker-container"
            for line in stdout.lines() {
                if let Some(driver) = line.strip_prefix("Driver:") {
                    let driver = driver.trim();
                    if driver != "docker-container" && driver != "docker" {
                        log.warn(&format!(
                            "buildx driver '{}' is not 'docker-container' or 'docker'; \
                             multi-platform builds may not work correctly",
                            driver
                        ));
                    }
                    return;
                }
            }
            // Driver line not found in output — warn about unknown driver
            log.warn("could not determine buildx driver from 'docker buildx inspect' output");
        }
        Err(_) => {
            // docker buildx not available — skip the check
        }
    }
}

// ---------------------------------------------------------------------------
// platform_to_arch
// ---------------------------------------------------------------------------

/// Extract the architecture component from a Docker platform string.
///
/// Handles three-component platform strings like `"linux/arm/v7"` by
/// concatenating the arch and variant (e.g. `"armv7"`), which matches
/// the output of [`map_target`] for armv7/armv6 Rust triples.
///
/// Examples:
/// - `"linux/amd64"` → `"amd64"`
/// - `"linux/arm64"` → `"arm64"`
/// - `"linux/arm/v7"` → `"armv7"`
/// - `"linux/arm/v6"` → `"armv6"`
pub fn platform_to_arch(platform: &str) -> &str {
    let parts: Vec<&str> = platform.split('/').collect();
    match parts.as_slice() {
        [_, arch, variant] => {
            // For "linux/arm/v7" → "armv7", "linux/arm/v6" → "armv6"
            // We need static strings since the return type is &str.
            match (*arch, *variant) {
                ("arm", "v6") => "armv6",
                ("arm", "v7") => "armv7",
                _ => variant,
            }
        }
        [_, arch] => arch,
        _ => platform,
    }
}

// ---------------------------------------------------------------------------
// tag_suffix
// ---------------------------------------------------------------------------

/// Extract the architecture portion of a platform string for use as a tag suffix.
///
/// Delegates to [`platform_to_arch`] since the logic is identical:
/// - `"linux/amd64"` → `"amd64"`
/// - `"linux/arm64"` → `"arm64"`
/// - `"linux/arm/v7"` → `"armv7"`
fn tag_suffix(platform: &str) -> String {
    platform_to_arch(platform).to_string()
}

// ---------------------------------------------------------------------------
// parse_duration_string
// ---------------------------------------------------------------------------

/// Parse a human-readable duration string into a [`Duration`].
///
/// Supported suffixes: `ms` (milliseconds), `s` (seconds), `m` (minutes).
/// Examples: `"500ms"`, `"1s"`, `"30s"`, `"2m"`.
///
/// Returns an error if the string is empty, has an unknown suffix, or contains
/// a non-numeric prefix.
pub fn parse_duration_string(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration string");
    }

    if let Some(n) = s.strip_suffix("ms") {
        let millis: u64 = n
            .parse()
            .with_context(|| format!("invalid milliseconds in duration '{s}'"))?;
        Ok(Duration::from_millis(millis))
    } else if let Some(n) = s.strip_suffix('m') {
        let mins: u64 = n
            .parse()
            .with_context(|| format!("invalid minutes in duration '{s}'"))?;
        Ok(Duration::from_secs(mins * 60))
    } else if let Some(n) = s.strip_suffix('s') {
        let secs: u64 = n
            .parse()
            .with_context(|| format!("invalid seconds in duration '{s}'"))?;
        Ok(Duration::from_secs(secs))
    } else if let Ok(secs) = s.parse::<u64>() {
        // Bare number without suffix — treat as seconds (GoReleaser compat)
        Ok(Duration::from_secs(secs))
    } else {
        anyhow::bail!(
            "unknown duration suffix in '{s}'; expected ms, s, or m (e.g. '500ms', '1s', '2m')"
        );
    }
}

/// Resolve retry parameters from an optional [`DockerRetryConfig`].
///
/// Returns `(attempts, base_delay, max_delay)` with sensible defaults:
/// - attempts defaults to 10 (matching GoReleaser's default)
/// - delay defaults to 10s
/// - max_delay defaults to 5m (caps exponential backoff at a reasonable ceiling)
pub fn resolve_retry_params(
    retry: &Option<DockerRetryConfig>,
) -> Result<(u32, Duration, Option<Duration>)> {
    // Default max_delay of 5 minutes prevents exponential backoff from growing
    // to unreasonably long waits (e.g. 42 minutes at attempt 9 with 10s base).
    let default_max_delay = Some(Duration::from_secs(300));

    match retry {
        None => Ok((10, Duration::from_secs(10), default_max_delay)),
        Some(cfg) => {
            let attempts = cfg.attempts.unwrap_or(10);
            let base_delay = match &cfg.delay {
                Some(d) => parse_duration_string(d)?,
                None => Duration::from_secs(10),
            };
            let max_delay = match &cfg.max_delay {
                Some(d) => Some(parse_duration_string(d)?),
                None => default_max_delay,
            };
            Ok((attempts, base_delay, max_delay))
        }
    }
}

// ---------------------------------------------------------------------------
// build_docker_command
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
    multi_platform: bool,
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
        None => {
            if multi_platform {
                Ok(("docker", vec!["buildx", "build"]))
            } else {
                Ok(("docker", vec!["build"]))
            }
        }
    }
}

/// Construct the docker build command arguments.
///
/// * `staging_dir` – path to the directory that acts as the Docker build
///   context (already contains the Dockerfile and binaries).
/// * `platforms` – Docker platform strings, e.g. `["linux/amd64", "linux/arm64"]`.
/// * `tags` – fully-qualified image tags.
/// * `extra_flags` – rendered `build_flag_templates`.
/// * `push` – when `true`, adds `--push` to the command.
/// * `push_flags` – additional flags added to the command when pushing.
/// * `labels` – OCI labels added as `--label key=value` flags.
/// * `use_backend` – backend selection: `"docker"`, `"buildx"`, or `"podman"`.
#[allow(clippy::too_many_arguments)]
pub fn build_docker_command(
    staging_dir: &str,
    platforms: &[&str],
    tags: &[&str],
    extra_flags: &[String],
    push: bool,
    push_flags: &[String],
    labels: &[(String, String)],
    use_backend: Option<&str>,
) -> Result<Vec<String>> {
    let multi_platform = platforms.len() > 1;
    let (binary, subcommands) = resolve_backend(use_backend, multi_platform)?;

    let mut cmd: Vec<String> = Vec::new();
    cmd.push(binary.to_string());
    for sub in subcommands {
        cmd.push(sub.to_string());
    }

    // --platform=linux/amd64,linux/arm64
    if !platforms.is_empty() {
        let platform_str = platforms.join(",");
        cmd.push(format!("--platform={platform_str}"));
    }

    // --tag <tag> for each image tag
    for tag in tags {
        cmd.push("--tag".to_string());
        cmd.push(tag.to_string());
    }

    // --label key=value for each OCI label
    for (key, value) in labels {
        cmd.push("--label".to_string());
        cmd.push(format!("{}={}", key, value));
    }

    // Extra build flags (rendered build_flag_templates)
    for flag in extra_flags {
        cmd.push(flag.clone());
    }

    // Determine the effective backend for --load/--push logic
    let effective_backend = match use_backend {
        Some(b) => b,
        None => {
            if multi_platform {
                "buildx"
            } else {
                "docker"
            }
        }
    };

    // --push in live mode (unless skip_push); when using buildx without
    // --push, add --load so the image is available locally (otherwise the
    // built image vanishes).  --load is incompatible with multi-platform
    // builds, so only add it for single-platform buildx.
    if push {
        cmd.push("--push".to_string());
        // Additional push flags
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

/// Construct the docker build command arguments for a Docker V2 config.
///
/// V2 uses `images` + `tags` to generate image references, `build_args` map,
/// `annotations` map, `sbom` flag, and arbitrary `flags`.
///
/// * `staging_dir` – path to the directory that acts as the Docker build context.
/// * `platforms` – Docker platform strings, e.g. `["linux/amd64", "linux/arm64"]`.
/// * `image_tags` – fully-qualified image:tag references (pre-computed from images x tags).
/// * `build_args` – `--build-arg KEY=VALUE` pairs.
/// * `annotations` – `--annotation KEY=VALUE` pairs.
/// * `labels` – `--label KEY=VALUE` pairs.
/// * `flags` – arbitrary extra flags passed directly.
/// * `sbom` – when true, adds `--sbom=true`.
/// * `push` – when `true`, adds `--push` to the command.
#[allow(clippy::too_many_arguments)]
pub fn build_docker_v2_command(
    staging_dir: &str,
    platforms: &[&str],
    image_tags: &[String],
    build_args: &[(String, String)],
    annotations: &[(String, String)],
    labels: &[(String, String)],
    flags: &[String],
    sbom: bool,
    push: bool,
) -> Result<Vec<String>> {
    // V2 always uses buildx when platforms are specified
    let multi_platform = platforms.len() > 1;
    let (binary, subcommands) = resolve_backend(Some("buildx"), multi_platform)?;

    let mut cmd: Vec<String> = Vec::new();
    cmd.push(binary.to_string());
    for sub in subcommands {
        cmd.push(sub.to_string());
    }

    // --platform=linux/amd64,linux/arm64
    if !platforms.is_empty() {
        let platform_str = platforms.join(",");
        cmd.push(format!("--platform={platform_str}"));
    }

    // --tag <tag> for each image:tag combination
    for tag in image_tags {
        cmd.push("--tag".to_string());
        cmd.push(tag.clone());
    }

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

    // --label KEY=VALUE
    for (key, value) in labels {
        cmd.push("--label".to_string());
        cmd.push(format!("{}={}", key, value));
    }

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
    } else if !multi_platform {
        cmd.push("--load".to_string());
    }

    // NOTE: GoReleaser V2 does NOT auto-add --provenance=false or --sbom=false.
    // Only the legacy docker pipe does that. V2 relies on explicit user flags
    // or the --attest=type=sbom flag set above.

    // Build context directory (positional, last argument)
    cmd.push(staging_dir.to_string());

    Ok(cmd)
}

/// Evaluate whether a Docker V2 config is disabled.
///
/// Checks the `disable` field: if it's a template, renders it and checks for "true".
/// Returns `true` when the config should be skipped.
pub fn is_docker_v2_disabled(disable: &Option<StringOrBool>, ctx: &Context) -> bool {
    match disable {
        None => false,
        Some(d) => d.is_disabled(|s| ctx.render_template(s)),
    }
}

/// Evaluate whether the sbom flag should be added for a Docker V2 config.
///
/// The `sbom` field is a [`StringOrBool`]. When it evaluates to true, the
/// `--attest=type=sbom` flag is added to the buildx command.
pub fn is_docker_v2_sbom_enabled(sbom: &Option<StringOrBool>, ctx: &Context) -> bool {
    match sbom {
        None => false,
        Some(s) => s.evaluates_to_true(|tmpl| ctx.render_template(tmpl)),
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
            // Render template string and treat truthy result as "skip"
            ctx.render_template(tmpl)
                .map(|rendered| rendered.trim().eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// DockerBuildJob — prepared data for a single docker build
// ---------------------------------------------------------------------------

/// All the information needed to execute a single docker build command.
///
/// The preparation phase (staging files, rendering templates, building the
/// command) is done sequentially because it needs `&mut Context`.  The
/// execution phase (running docker) can then run in parallel.
struct DockerBuildJob {
    /// Pre-built docker command arguments (binary + flags + context dir).
    cmd_args: Vec<String>,
    /// Human-readable backend label for log messages ("buildx", "docker", "podman").
    backend_label: String,
    /// Crate name (for error context).
    crate_name: String,
    /// Docker config index (for error context).
    idx: usize,
    /// Retry parameters.
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Option<Duration>,
    /// Whether to push (and therefore capture digests after build).
    should_push: bool,
    /// Rendered image tags — used for digest capture and artifact registration.
    rendered_tags: Vec<String>,
    /// Docker platforms string (comma-separated, for artifact metadata).
    platforms_str: String,
    /// Staging directory path.
    staging_dir: PathBuf,
    /// Optional docker config id.
    id: Option<String>,
    /// Optional use_backend string.
    use_backend: Option<String>,
    /// Dist directory (for writing digest files).
    dist: PathBuf,
    /// Whether this is a V2 docker build (affects artifact type registration).
    is_v2: bool,
}

/// Result of executing a single docker build job.
struct DockerBuildResult {
    /// Digests captured after a successful push, keyed by tag.
    tag_digests: HashMap<String, String>,
}

/// Execute a single docker build job with retry logic.
///
/// This is a free function (not a method) so it can be called from
/// `std::thread::scope` spawned threads without borrowing `self`.
fn execute_docker_build(job: &DockerBuildJob, log: &StageLogger) -> Result<DockerBuildResult> {
    log.status(&format!("running: {}", job.cmd_args.join(" ")));

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=job.max_attempts {
        if attempt > 1 {
            let multiplier = 2u64.saturating_pow(attempt - 2);
            let delay_ms = job.base_delay.as_millis().saturating_mul(multiplier as u128);
            let mut delay = Duration::from_millis(delay_ms as u64);
            if let Some(cap) = job.max_delay
                && delay > cap
            {
                delay = cap;
            }
            log.warn(&format!(
                "attempt {}/{} failed, retrying in {:?}…",
                attempt - 1,
                job.max_attempts,
                delay,
            ));
            std::thread::sleep(delay);
        }

        let output = Command::new(&job.cmd_args[0])
            .args(&job.cmd_args[1..])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .with_context(|| {
                format!(
                    "docker: execute {} for crate {} index {} (attempt {}/{})",
                    job.backend_label, job.crate_name, job.idx, attempt, job.max_attempts
                )
            })?;

        if !output.stdout.is_empty() {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&output.stdout);
        }
        if !output.stderr.is_empty() {
            use std::io::Write;
            let _ = std::io::stderr().write_all(&output.stderr);
        }

        // Capture stderr for diagnostic hints before output is consumed.
        let stderr_text = String::from_utf8_lossy(&output.stderr).to_string();

        match log.check_output(output, &format!("docker {}", job.backend_label)) {
            Ok(_) => {
                if attempt > 1 {
                    log.status(&format!(
                        "docker {} succeeded on attempt {}/{}",
                        job.backend_label, attempt, job.max_attempts
                    ));
                }
                last_err = None;
                break;
            }
            Err(e) => {
                let err_msg = format!("{:#}", e);
                let is_retriable = is_retriable_error(&err_msg);
                if attempt < job.max_attempts && !is_retriable {
                    // Diagnostic: file-not-found hints for COPY/ADD failures
                    if stderr_text.contains("COPY") || stderr_text.contains("ADD") {
                        log.warn(
                            "the Dockerfile COPY/ADD failed — check that the \
                             files referenced in your Dockerfile exist in the \
                             staging directory; the available files may not match \
                             what the Dockerfile expects",
                        );
                    }
                    // Diagnostic: buildx context / TLS errors
                    if stderr_text.contains("could not read certificates")
                        || stderr_text
                            .contains("server gave HTTP response to HTTPS client")
                    {
                        log.warn(
                            "this may be a Docker context issue — \
                             try running: docker context use default",
                        );
                    }
                    log.warn(&format!(
                        "docker {} failed with non-retriable error, not retrying",
                        job.backend_label
                    ));
                    return Err(e).with_context(|| {
                        format!(
                            "docker: non-retriable failure for crate {} index {}",
                            job.crate_name, job.idx
                        )
                    });
                }
                last_err = Some(e);
            }
        }
    }

    if let Some(e) = last_err {
        return Err(e).with_context(|| {
            format!(
                "docker: all {} attempts failed for crate {} index {}",
                job.max_attempts, job.crate_name, job.idx
            )
        });
    }

    // Capture digests after successful push
    let mut tag_digests = HashMap::new();
    if job.should_push {
        for tag in &job.rendered_tags {
            let inspect_bin = if job.backend_label == "podman" {
                "podman"
            } else {
                "docker"
            };
            let digest_output = Command::new(inspect_bin)
                .args(["inspect", "--format", "{{index .RepoDigests 0}}", tag])
                .output();

            if let Ok(output) = digest_output
                && output.status.success()
            {
                let digest = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !digest.is_empty() {
                    tag_digests.insert(tag.clone(), digest.clone());
                    let safe_name = tag.replace(['/', ':'], "_");
                    let digest_file = job.dist.join(format!("{}.digest", safe_name));
                    if let Err(e) = fs::write(&digest_file, &digest) {
                        log.warn(&format!(
                            "failed to write digest file {}: {}",
                            digest_file.display(),
                            e
                        ));
                    } else {
                        log.status(&format!("saved digest to {}", digest_file.display()));
                    }
                }
            }
        }
    }

    Ok(DockerBuildResult { tag_digests })
}

// ---------------------------------------------------------------------------
// Shared staging helpers (used by both legacy and V2 paths)
// ---------------------------------------------------------------------------

/// Stage binary artifacts into the docker build context.
///
/// For each platform, creates a `binaries/<arch>` directory under `staging_dir`
/// and copies matching binary artifacts into it. Filtering is done by:
/// - `ids_filter`: optional list of artifact metadata IDs to include
/// - `binary_filter`: optional list of binary names to include (legacy only)
fn stage_binaries(
    platforms: &[String],
    staging_dir: &std::path::Path,
    dry_run: bool,
    ids_filter: Option<&Vec<String>>,
    binary_filter: Option<&Vec<String>>,
    crate_name: &str,
    ctx: &Context,
    log: &StageLogger,
    prefix: &str,
) -> Result<()> {
    for platform in platforms {
        let arch = platform_to_arch(platform);
        let binaries_dir = staging_dir.join("binaries").join(arch);
        if !dry_run {
            fs::create_dir_all(&binaries_dir).with_context(|| {
                format!("{}: create binaries dir {}", prefix, binaries_dir.display())
            })?;
        }

        let matching_binaries: Vec<_> = ctx
            .artifacts
            .by_kind_and_crate(ArtifactKind::Binary, crate_name)
            .into_iter()
            .filter(|b| {
                let artifact_arch = b
                    .target
                    .as_deref()
                    .map(|t| map_target(t).1)
                    .unwrap_or_default();
                if artifact_arch != arch {
                    return false;
                }
                if let Some(ids) = ids_filter {
                    let artifact_id = b.metadata.get("id").map(|s| s.as_str()).unwrap_or("");
                    if !ids.iter().any(|id| id == artifact_id) {
                        return false;
                    }
                }
                match binary_filter {
                    None => true,
                    Some(names) => {
                        let bin_name = b.metadata.get("binary").map(|s| s.as_str()).unwrap_or("");
                        names.iter().any(|n| n == bin_name)
                    }
                }
            })
            .collect();

        if matching_binaries.is_empty() {
            log.warn(&format!(
                "no binaries found for platform {} — check ids/binary filters",
                platform
            ));
        }

        for bin_artifact in matching_binaries {
            let bin_name = bin_artifact
                .metadata
                .get("binary")
                .map(|s| s.as_str())
                .unwrap_or_else(|| {
                    bin_artifact
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("binary")
                });

            let dest = binaries_dir.join(bin_name);

            if dry_run {
                log.status(&format!(
                    "(dry-run) would copy {} -> {}",
                    bin_artifact.path.display(),
                    dest.display()
                ));
            } else {
                log.status(&format!(
                    "staging binary {} -> {}",
                    bin_artifact.path.display(),
                    dest.display()
                ));
                fs::copy(&bin_artifact.path, &dest).with_context(|| {
                    format!(
                        "{}: copy binary {} to {}",
                        prefix,
                        bin_artifact.path.display(),
                        dest.display()
                    )
                })?;
            }
        }
    }
    Ok(())
}

/// Stage artifacts into docker build context using GoReleaser V2 layout.
///
/// V2 uses `<os>/<arch>/<name>` directory structure (matching `$TARGETPLATFORM`)
/// and stages Binary, LinuxPackage, CArchive, CShared, and PyWheel artifacts.
/// Artifacts with `goos == "all"` are copied into every platform directory.
fn stage_artifacts_v2(
    platforms: &[String],
    staging_dir: &std::path::Path,
    dry_run: bool,
    ids_filter: Option<&Vec<String>>,
    crate_name: &str,
    ctx: &Context,
    log: &StageLogger,
) -> Result<()> {
    let stageable_kinds = [
        ArtifactKind::Binary,
        ArtifactKind::LinuxPackage,
        ArtifactKind::CArchive,
        ArtifactKind::CShared,
        ArtifactKind::PyWheel,
    ];

    for platform in platforms {
        let parts: Vec<&str> = platform.split('/').collect();
        // Use full platform path (e.g., "linux/amd64") as directory structure
        let platform_dir = staging_dir.join(platform.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !dry_run {
            fs::create_dir_all(&platform_dir).with_context(|| {
                format!("docker_v2: create platform dir {}", platform_dir.display())
            })?;
        }

        let arch = platform_to_arch(platform);
        let os = parts.first().copied().unwrap_or("linux");

        let mut platform_artifact_count = 0usize;
        for kind in &stageable_kinds {
            let artifacts: Vec<_> = ctx
                .artifacts
                .by_kind_and_crate(*kind, crate_name)
                .into_iter()
                .filter(|a| {
                    // Match by architecture, or goos == "all" (cross-platform artifacts)
                    if let Some(target) = a.target.as_deref() {
                        let (a_os, a_arch) = map_target(target);
                        (a_os == os && a_arch == arch) || a_os == "all"
                    } else {
                        // No target = universal artifact, include everywhere
                        true
                    }
                })
                .filter(|a| {
                    if let Some(ids) = ids_filter {
                        let artifact_id = a.metadata.get("id").map(|s| s.as_str()).unwrap_or("");
                        ids.iter().any(|id| id == artifact_id)
                    } else {
                        true
                    }
                })
                .collect();

            platform_artifact_count += artifacts.len();
            for artifact in artifacts {
                let file_name = artifact
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("artifact");
                let dest = platform_dir.join(file_name);

                if dry_run {
                    log.status(&format!(
                        "(dry-run) would copy {} -> {}",
                        artifact.path.display(),
                        dest.display()
                    ));
                } else {
                    log.status(&format!(
                        "staging {} -> {}",
                        artifact.path.display(),
                        dest.display()
                    ));
                    fs::copy(&artifact.path, &dest).with_context(|| {
                        format!(
                            "docker_v2: copy {} to {}",
                            artifact.path.display(),
                            dest.display()
                        )
                    })?;
                }
            }
        }

        if platform_artifact_count == 0 {
            log.warn(&format!(
                "no binaries found for platform {} — check ids/binary filters",
                platform
            ));
        }
    }
    Ok(())
}

/// Copy a Dockerfile into the staging directory.
fn copy_dockerfile(
    dockerfile: &str,
    staging_dir: &std::path::Path,
    dry_run: bool,
    log: &StageLogger,
    prefix: &str,
) -> Result<()> {
    let dockerfile_src = PathBuf::from(dockerfile);
    let dockerfile_dest = staging_dir.join("Dockerfile");

    if dry_run {
        log.status(&format!(
            "(dry-run) would copy Dockerfile {} -> {}",
            dockerfile_src.display(),
            dockerfile_dest.display()
        ));
    } else {
        log.status(&format!(
            "copying Dockerfile {} -> {}",
            dockerfile_src.display(),
            dockerfile_dest.display()
        ));
        fs::copy(&dockerfile_src, &dockerfile_dest).with_context(|| {
            format!(
                "{}: copy Dockerfile from {} to {}",
                prefix,
                dockerfile_src.display(),
                dockerfile_dest.display()
            )
        })?;
    }
    Ok(())
}

/// Copy extra files into the staging directory.
///
/// Preserves relative directory structure for relative paths. For absolute
/// paths, only the filename is used.
fn stage_extra_files(
    extra_files: &[String],
    staging_dir: &std::path::Path,
    dry_run: bool,
    log: &StageLogger,
    prefix: &str,
) -> Result<()> {
    for file_path in extra_files {
        let src = PathBuf::from(file_path);
        if src.is_dir() {
            anyhow::bail!(
                "{}: extra_files entry '{}' is a directory; only files are supported",
                prefix,
                file_path
            );
        }
        let dest = if src.is_absolute() {
            let file_name = src
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new(file_path));
            staging_dir.join(file_name)
        } else {
            staging_dir.join(file_path)
        };

        if dry_run {
            log.status(&format!(
                "(dry-run) would copy extra file {} -> {}",
                src.display(),
                dest.display()
            ));
        } else {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "{}: create parent dirs for extra file {}",
                        prefix,
                        dest.display()
                    )
                })?;
            }
            log.status(&format!(
                "copying extra file {} -> {}",
                src.display(),
                dest.display()
            ));
            fs::copy(&src, &dest).with_context(|| {
                format!(
                    "{}: copy extra file {} to {}",
                    prefix,
                    src.display(),
                    dest.display()
                )
            })?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// DockerStage
// ---------------------------------------------------------------------------

pub struct DockerStage;

impl Stage for DockerStage {
    fn name(&self) -> &str {
        "docker"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("docker");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();
        let parallelism = ctx.options.parallelism.max(1);

        // Collect crates that have docker, docker_v2, or docker_manifests config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| {
                c.docker.is_some() || c.docker_v2.is_some() || c.docker_manifests.is_some()
            })
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // Validate Docker V2 config ID uniqueness — duplicate IDs cause
        // confusing artifact collisions and filtering bugs.
        {
            let mut seen_ids: HashSet<String> = HashSet::new();
            for krate in &crates {
                if let Some(ref v2_cfgs) = krate.docker_v2 {
                    for v2_cfg in v2_cfgs {
                        if let Some(ref id) = v2_cfg.id {
                            if !seen_ids.insert(id.clone()) {
                                log.warn(&format!(
                                    "duplicate docker_v2 config id '{}' — \
                                     each config should have a unique id",
                                    id
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Validate the buildx driver once if any V2 configs exist (V2 always uses buildx).
        if !dry_run && crates.iter().any(|c| c.docker_v2.is_some()) {
            check_buildx_driver(&log);
        }

        let mut new_artifacts: Vec<Artifact> = Vec::new();

        // ==================================================================
        // Phase 1: Prepare all docker build jobs sequentially
        //
        // This phase needs &mut Context for template rendering and artifact
        // lookups.  Each job is fully self-contained after preparation.
        // ==================================================================
        let mut build_jobs: Vec<DockerBuildJob> = Vec::new();

        for krate in &crates {
            let docker_configs = match krate.docker.as_ref() {
                Some(cfgs) => cfgs.clone(),
                None => Vec::new(),
            };

            for (idx, docker_cfg) in docker_configs.iter().enumerate() {
                // Check disable (template-aware) before doing any work.
                if let Some(ref d) = docker_cfg.disable {
                    if d.is_disabled(|tmpl| ctx.render_template(tmpl)) {
                        let fallback = format!("index {}", idx);
                        let label = docker_cfg.id.as_deref().unwrap_or(&fallback);
                        log.status(&format!(
                            "docker: skipping disabled config '{}' for crate {}",
                            label, krate.name
                        ));
                        continue;
                    }
                }

                // Determine platforms (default: empty = use host platform, no --platform flag).
                // GoReleaser omits --platform when unset, letting Docker use the host platform.
                // Setting platforms forces buildx mode and requires QEMU/binfmt for cross-arch.
                let platforms: Vec<String> = docker_cfg.platforms.clone().unwrap_or_default();

                // Validate the backend early — before staging files — so a
                // typo like `use: "dockr"` is caught immediately.
                resolve_backend(docker_cfg.use_backend.as_deref(), platforms.len() > 1)?;

                // Build the staging directory path
                let staging_dir: PathBuf =
                    dist.join("docker").join(&krate.name).join(idx.to_string());

                if !dry_run {
                    fs::create_dir_all(&staging_dir).with_context(|| {
                        format!("docker: create staging dir {}", staging_dir.display())
                    })?;
                }

                // ------------------------------------------------------------------
                // Stage binaries, Dockerfile, and extra files
                // ------------------------------------------------------------------
                stage_binaries(
                    &platforms,
                    &staging_dir,
                    dry_run,
                    docker_cfg.ids.as_ref(),
                    docker_cfg.binaries.as_ref(),
                    &krate.name,
                    ctx,
                    &log,
                    "docker",
                )?;

                // Default dockerfile to "Dockerfile" (matching GoReleaser) and template-render path.
                let dockerfile_raw = if docker_cfg.dockerfile.is_empty() {
                    "Dockerfile"
                } else {
                    &docker_cfg.dockerfile
                };
                let rendered_dockerfile = ctx.render_template(dockerfile_raw)
                    .with_context(|| format!("docker: render dockerfile path '{}'", dockerfile_raw))?;
                copy_dockerfile(&rendered_dockerfile, &staging_dir, dry_run, &log, "docker")?;

                if let Some(ref extra_files) = docker_cfg.extra_files {
                    stage_extra_files(extra_files, &staging_dir, dry_run, &log, "docker")?;
                }

                // Process templated_extra_files: render and copy to staging dir
                if let Some(ref tpl_specs) = docker_cfg.templated_extra_files {
                    if !tpl_specs.is_empty() {
                        anodize_core::templated_files::process_templated_extra_files(
                            tpl_specs, ctx, &staging_dir, "docker",
                        )?;
                    }
                }

                // ------------------------------------------------------------------
                // Render image tag templates
                // ------------------------------------------------------------------
                let mut rendered_tags: Vec<String> = Vec::new();
                for tmpl in &docker_cfg.image_templates {
                    let tag = ctx.render_template(tmpl).with_context(|| {
                        format!(
                            "docker: render image_template '{}' for crate {}",
                            tmpl, krate.name
                        )
                    })?;
                    // Skip empty rendered templates (GoReleaser behavior)
                    if tag.is_empty() {
                        continue;
                    }
                    rendered_tags.push(tag);
                }

                if rendered_tags.is_empty() {
                    log.warn(&format!(
                        "docker[{}]: all image_templates rendered to empty for crate {}; skipping build",
                        idx, krate.name
                    ));
                    continue;
                }

                // ------------------------------------------------------------------
                // Build the docker command arguments
                // ------------------------------------------------------------------
                let platform_refs: Vec<&str> = platforms.iter().map(|s| s.as_str()).collect();
                let tag_refs: Vec<&str> = rendered_tags.iter().map(|s| s.as_str()).collect();
                let staging_str = staging_dir.to_string_lossy().into_owned();

                // Render build_flag_templates
                let mut extra_flags = Vec::new();
                if let Some(ref flag_templates) = docker_cfg.build_flag_templates {
                    for tmpl in flag_templates {
                        let rendered = ctx.render_template(tmpl).with_context(|| {
                            format!("docker: render build_flag_template '{}'", tmpl)
                        })?;
                        extra_flags.push(rendered);
                    }
                }

                // Determine whether to push
                let skip_push = resolve_skip_push(&docker_cfg.skip_push, ctx);
                let should_push = !dry_run && !skip_push;

                // Render push_flags (template-aware, consistent with build_flag_templates)
                let mut push_flags = Vec::new();
                if let Some(ref pf_templates) = docker_cfg.push_flags {
                    for tmpl in pf_templates {
                        let rendered = ctx
                            .render_template(tmpl)
                            .with_context(|| format!("docker: render push_flag '{}'", tmpl))?;
                        push_flags.push(rendered);
                    }
                }

                // Render labels (template-aware)
                let mut rendered_labels: Vec<(String, String)> = Vec::new();
                if let Some(ref label_map) = docker_cfg.labels {
                    for (key, value_tmpl) in label_map {
                        let rendered_value = ctx
                            .render_template(value_tmpl)
                            .with_context(|| format!("docker: render label value for '{}'", key))?;
                        rendered_labels.push((key.clone(), rendered_value));
                    }
                    // Sort for deterministic command output
                    rendered_labels.sort_by(|a, b| a.0.cmp(&b.0));
                }

                let cmd_args = build_docker_command(
                    &staging_str,
                    &platform_refs,
                    &tag_refs,
                    &extra_flags,
                    should_push,
                    &push_flags,
                    &rendered_labels,
                    docker_cfg.use_backend.as_deref(),
                )?;

                // Resolve retry configuration
                let (max_attempts, base_delay, max_delay) = resolve_retry_params(&docker_cfg.retry)
                    .with_context(|| {
                        format!(
                            "docker: invalid retry config for crate {} index {}",
                            krate.name, idx
                        )
                    })?;

                let (_backend_binary, backend_subcmds) =
                    resolve_backend(docker_cfg.use_backend.as_deref(), platforms.len() > 1)?;
                let backend_label = if backend_subcmds.contains(&"buildx") {
                    "buildx"
                } else {
                    _backend_binary
                };

                if dry_run {
                    log.status(&format!("(dry-run) would run: {}", cmd_args.join(" ")));
                    if max_attempts > 1 {
                        log.status(&format!(
                            "(dry-run) retry: up to {} attempts, base delay {:?}{}",
                            max_attempts,
                            base_delay,
                            match max_delay {
                                Some(d) => format!(", max delay {:?}", d),
                                None => String::new(),
                            }
                        ));
                    }
                    // In dry-run, register artifacts directly (no build to execute)
                    for tag in &rendered_tags {
                        let mut meta = HashMap::new();
                        meta.insert("tag".to_string(), tag.clone());
                        meta.insert("platforms".to_string(), platforms.join(","));
                        if let Some(ref id) = docker_cfg.id {
                            meta.insert("id".to_string(), id.clone());
                        }
                        if let Some(ref backend) = docker_cfg.use_backend {
                            meta.insert("use".to_string(), backend.clone());
                        }
                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::DockerImage,
                            name: String::new(),
                            path: staging_dir.clone(),
                            target: None,
                            crate_name: krate.name.clone(),
                            metadata: meta,
                            size: None,
                        });
                    }
                } else {
                    build_jobs.push(DockerBuildJob {
                        cmd_args,
                        backend_label: backend_label.to_string(),
                        crate_name: krate.name.clone(),
                        idx,
                        max_attempts,
                        base_delay,
                        max_delay,
                        should_push,
                        rendered_tags: rendered_tags.clone(),
                        platforms_str: platforms.join(","),
                        staging_dir: staging_dir.clone(),
                        id: docker_cfg.id.clone(),
                        use_backend: docker_cfg.use_backend.clone(),
                        dist: dist.clone(),
                        is_v2: false,
                    });
                }
            }

            // ------------------------------------------------------------------
            // Docker V2 configs
            // ------------------------------------------------------------------
            let docker_v2_configs = match krate.docker_v2.as_ref() {
                Some(cfgs) => cfgs.clone(),
                None => Vec::new(),
            };

            // Apply GoReleaser-compatible defaults to V2 configs.
            let docker_v2_configs: Vec<_> = docker_v2_configs
                .into_iter()
                .map(|mut cfg| {
                    // ID defaults to project name
                    if cfg.id.is_none() {
                        cfg.id = Some(ctx.config.project_name.clone());
                    }
                    // Dockerfile defaults to "Dockerfile"
                    if cfg.dockerfile.is_empty() {
                        cfg.dockerfile = "Dockerfile".to_string();
                    }
                    // Tags default to ["{{ .Tag }}"]
                    if cfg.tags.is_empty() {
                        cfg.tags = vec!["{{ .Tag }}".to_string()];
                    }
                    // Platforms default to ["linux/amd64", "linux/arm64"]
                    if cfg.platforms.is_none() {
                        cfg.platforms =
                            Some(vec!["linux/amd64".to_string(), "linux/arm64".to_string()]);
                    }
                    // SBOM defaults to true
                    if cfg.sbom.is_none() {
                        cfg.sbom = Some(StringOrBool::Bool(true));
                    }
                    // Retry defaults are already handled by resolve_retry_params (10 attempts, 10s, 5m)
                    cfg
                })
                .collect();

            for (idx, v2_cfg) in docker_v2_configs.iter().enumerate() {
                // Check disable — skip when template evaluates to true
                if is_docker_v2_disabled(&v2_cfg.disable, ctx) {
                    log.status(&format!(
                        "docker_v2[{}]: skipping disabled config for crate {}",
                        idx, krate.name
                    ));
                    continue;
                }

                // Template-render platforms and filter empty results (GoReleaser's tpl.ApplySlice)
                let platforms: Vec<String> = v2_cfg
                    .platforms
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|p| {
                        ctx.render_template(&p).ok().filter(|r| !r.is_empty())
                    })
                    .collect();

                // V2 always uses buildx
                resolve_backend(Some("buildx"), platforms.len() > 1)?;

                // Build staging directory — use "docker_v2" subdirectory to avoid
                // collisions with legacy docker configs.
                let staging_dir: PathBuf = dist
                    .join("docker_v2")
                    .join(&krate.name)
                    .join(idx.to_string());

                if !dry_run {
                    fs::create_dir_all(&staging_dir).with_context(|| {
                        format!("docker_v2: create staging dir {}", staging_dir.display())
                    })?;
                }

                // Stage artifacts using V2 layout (os/arch/name, multiple artifact types)
                stage_artifacts_v2(
                    &platforms,
                    &staging_dir,
                    dry_run,
                    v2_cfg.ids.as_ref(),
                    &krate.name,
                    ctx,
                    &log,
                )?;

                // Template-render the Dockerfile path (GoReleaser does this via tmpl.New(ctx).Apply)
                let rendered_dockerfile = ctx.render_template(&v2_cfg.dockerfile)
                    .with_context(|| {
                        format!(
                            "docker_v2: render dockerfile path '{}' for crate {}",
                            v2_cfg.dockerfile, krate.name
                        )
                    })?;
                copy_dockerfile(&rendered_dockerfile, &staging_dir, dry_run, &log, "docker_v2")?;

                if let Some(ref extra_files) = v2_cfg.extra_files {
                    stage_extra_files(extra_files, &staging_dir, dry_run, &log, "docker_v2")?;
                }

                // Render tags through template engine
                let mut rendered_tags: Vec<String> = Vec::new();
                for tag_tmpl in &v2_cfg.tags {
                    let rendered = ctx.render_template(tag_tmpl).with_context(|| {
                        format!(
                            "docker_v2: render tag template '{}' for crate {}",
                            tag_tmpl, krate.name
                        )
                    })?;
                    if rendered.is_empty() {
                        continue;
                    }
                    rendered_tags.push(rendered);
                }

                // Render images through template engine
                let mut rendered_images: Vec<String> = Vec::new();
                for img_tmpl in &v2_cfg.images {
                    let rendered = ctx.render_template(img_tmpl).with_context(|| {
                        format!(
                            "docker_v2: render image template '{}' for crate {}",
                            img_tmpl, krate.name
                        )
                    })?;
                    if rendered.is_empty() {
                        continue;
                    }
                    rendered_images.push(rendered);
                }

                // For snapshot builds, GoReleaser splits multi-platform configs
                // into per-platform builds with --load (no push) and tag suffix.
                // This builds each platform separately so images are available locally.
                let snapshot_platforms: Vec<Vec<String>> = if ctx.is_snapshot() && platforms.len() > 1 {
                    platforms.iter().map(|p| vec![p.clone()]).collect()
                } else {
                    vec![platforms.clone()]
                };

                for snapshot_plats in &snapshot_platforms {
                    let mut per_plat_tags = rendered_tags.clone();

                    // During snapshot, add platform arch suffix to each tag.
                    if ctx.is_snapshot() && snapshot_plats.len() == 1 {
                        let suffix = tag_suffix(&snapshot_plats[0]);
                        for tag in &mut per_plat_tags {
                            tag.push('-');
                            tag.push_str(&suffix);
                        }
                    }

                // Generate image:tag combinations
                let image_tags = generate_v2_image_tags(&rendered_images, &per_plat_tags);

                if image_tags.is_empty() {
                    log.warn(&format!(
                        "docker_v2[{}]: no image tags produced for crate {} (images or tags resolved to empty); skipping",
                        idx, krate.name
                    ));
                    continue;
                }

                // Render build_args (template-aware keys and values, matching GoReleaser's tplMapFlags)
                let mut rendered_build_args: Vec<(String, String)> = Vec::new();
                if let Some(ref args_map) = v2_cfg.build_args {
                    for (key_tmpl, value_tmpl) in args_map {
                        let rendered_key = ctx.render_template(key_tmpl).with_context(|| {
                            format!("docker_v2: render build_arg key '{}'", key_tmpl)
                        })?;
                        let rendered_value = ctx.render_template(value_tmpl).with_context(|| {
                            format!("docker_v2: render build_arg value for '{}'", key_tmpl)
                        })?;
                        // Skip entries where key or value is empty after templating
                        if !rendered_key.is_empty() && !rendered_value.is_empty() {
                            rendered_build_args.push((rendered_key, rendered_value));
                        }
                    }
                    rendered_build_args.sort_by(|a, b| a.0.cmp(&b.0));
                }

                // Render annotations (template-aware keys and values)
                let mut rendered_annotations: Vec<(String, String)> = Vec::new();
                if let Some(ref ann_map) = v2_cfg.annotations {
                    for (key_tmpl, value_tmpl) in ann_map {
                        let rendered_key = ctx.render_template(key_tmpl).with_context(|| {
                            format!("docker_v2: render annotation key '{}'", key_tmpl)
                        })?;
                        let rendered_value = ctx.render_template(value_tmpl).with_context(|| {
                            format!("docker_v2: render annotation value for '{}'", key_tmpl)
                        })?;
                        if !rendered_key.is_empty() && !rendered_value.is_empty() {
                            rendered_annotations.push((rendered_key, rendered_value));
                        }
                    }
                    rendered_annotations.sort_by(|a, b| a.0.cmp(&b.0));
                }

                // Render labels (template-aware keys and values)
                let mut rendered_labels: Vec<(String, String)> = Vec::new();
                if let Some(ref label_map) = v2_cfg.labels {
                    for (key_tmpl, value_tmpl) in label_map {
                        let rendered_key = ctx.render_template(key_tmpl).with_context(|| {
                            format!("docker_v2: render label key '{}'", key_tmpl)
                        })?;
                        let rendered_value = ctx.render_template(value_tmpl).with_context(|| {
                            format!("docker_v2: render label value for '{}'", key_tmpl)
                        })?;
                        if !rendered_key.is_empty() && !rendered_value.is_empty() {
                            rendered_labels.push((rendered_key, rendered_value));
                        }
                    }
                    rendered_labels.sort_by(|a, b| a.0.cmp(&b.0));
                }

                // Render flags (template-aware, filter empty results)
                let mut rendered_flags: Vec<String> = Vec::new();
                if let Some(ref flag_list) = v2_cfg.flags {
                    for flag_tmpl in flag_list {
                        let rendered = ctx.render_template(flag_tmpl).with_context(|| {
                            format!("docker_v2: render flag '{}'", flag_tmpl)
                        })?;
                        if !rendered.is_empty() {
                            rendered_flags.push(rendered);
                        }
                    }
                }

                // Evaluate sbom — GoReleaser only adds SBOM in the Publish path (not snapshot).
                let sbom_enabled = if ctx.is_snapshot() {
                    false
                } else {
                    is_docker_v2_sbom_enabled(&v2_cfg.sbom, ctx)
                };

                let platform_refs: Vec<&str> = snapshot_plats.iter().map(|s| s.as_str()).collect();
                let staging_str = staging_dir.to_string_lossy().into_owned();

                // Snapshot builds never push (GoReleaser uses --load per-platform).
                // Non-snapshot: push unless skip_push is set.
                let should_push = if ctx.is_snapshot() {
                    false
                } else {
                    let v2_skip_push = match &v2_cfg.skip_push {
                        None => false,
                        Some(s) => s.evaluates_to_true(|tmpl| ctx.render_template(tmpl)),
                    };
                    !dry_run && !v2_skip_push
                };

                let cmd_args = build_docker_v2_command(
                    &staging_str,
                    &platform_refs,
                    &image_tags,
                    &rendered_build_args,
                    &rendered_annotations,
                    &rendered_labels,
                    &rendered_flags,
                    sbom_enabled,
                    should_push,
                )?;

                // Resolve retry configuration
                let (max_attempts, base_delay, max_delay) =
                    resolve_retry_params(&v2_cfg.retry).with_context(|| {
                        format!(
                            "docker_v2: invalid retry config for crate {} index {}",
                            krate.name, idx
                        )
                    })?;

                if dry_run {
                    log.status(&format!("(dry-run) would run: {}", cmd_args.join(" ")));
                    if max_attempts > 1 {
                        log.status(&format!(
                            "(dry-run) retry: up to {} attempts, base delay {:?}{}",
                            max_attempts,
                            base_delay,
                            match max_delay {
                                Some(d) => format!(", max delay {:?}", d),
                                None => String::new(),
                            }
                        ));
                    }
                    // Register artifacts in dry-run
                    for tag in &image_tags {
                        let mut meta = HashMap::new();
                        meta.insert("tag".to_string(), tag.clone());
                        meta.insert("platforms".to_string(), snapshot_plats.join(","));
                        meta.insert("api".to_string(), "v2".to_string());
                        meta.insert("use".to_string(), "buildx".to_string());
                        if let Some(ref id) = v2_cfg.id {
                            meta.insert("id".to_string(), id.clone());
                        }
                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::DockerImageV2,
                            name: tag.clone(),
                            path: PathBuf::from(tag),
                            target: None,
                            crate_name: krate.name.clone(),
                            metadata: meta,
                            size: None,
                        });
                    }
                } else {
                    build_jobs.push(DockerBuildJob {
                        cmd_args,
                        backend_label: "buildx".to_string(),
                        crate_name: krate.name.clone(),
                        idx,
                        max_attempts,
                        base_delay,
                        max_delay,
                        should_push,
                        rendered_tags: image_tags,
                        platforms_str: snapshot_plats.join(","),
                        staging_dir: staging_dir.clone(),
                        id: v2_cfg.id.clone(),
                        use_backend: Some("buildx".to_string()),
                        dist: dist.clone(),
                        is_v2: true,
                    });
                }
                } // end for snapshot_plats
            }
        }

        // ==================================================================
        // Phase 2: Execute docker build jobs in parallel
        //
        // Uses std::thread::scope with a simple semaphore pattern (channel-
        // based) bounded by ctx.parallelism, matching GoReleaser's
        // semerrgroup.New(ctx.Parallelism) behavior.
        // ==================================================================
        if !build_jobs.is_empty() {
            use std::sync::mpsc;

            /// Drop guard that returns a semaphore token to the channel when
            /// dropped, ensuring the token is returned even if the thread
            /// panics. Without this, a panic would permanently consume a slot
            /// and eventually deadlock the remaining threads.
            struct SemaphoreGuard<'a> {
                sender: &'a mpsc::SyncSender<()>,
            }
            impl Drop for SemaphoreGuard<'_> {
                fn drop(&mut self) {
                    let _ = self.sender.send(());
                }
            }

            // Channel-based semaphore: pre-fill with `parallelism` tokens.
            // Each thread takes a token before starting and returns it on
            // completion.  This bounds active docker builds to `parallelism`.
            let (sem_tx, sem_rx) = mpsc::sync_channel::<()>(parallelism);
            for _ in 0..parallelism {
                let _ = sem_tx.send(());
            }

            // Collect results in order (indexed by job position).
            let job_count = build_jobs.len();
            let log_ref = &log;
            let results: Vec<Result<DockerBuildResult>> =
                std::thread::scope(|scope| {
                    let mut handles = Vec::with_capacity(job_count);

                    for job in &build_jobs {
                        // Acquire a semaphore token (blocks if all slots are busy).
                        let _ = sem_rx.recv();
                        let sem_tx_ref = &sem_tx;

                        let handle = scope.spawn(move || {
                            // Guard returns the token on drop (including panic).
                            let _guard = SemaphoreGuard { sender: sem_tx_ref };
                            execute_docker_build(job, log_ref)
                        });
                        handles.push(handle);
                    }

                    handles
                        .into_iter()
                        .map(|h| h.join().expect("docker build thread panicked"))
                        .collect()
                });

            // ==================================================================
            // Phase 3: Collect results and register artifacts
            // ==================================================================
            for (job, result) in build_jobs.iter().zip(results.into_iter()) {
                let build_result = result?;
                for tag in &job.rendered_tags {
                    let mut meta = HashMap::new();
                    meta.insert("tag".to_string(), tag.clone());
                    meta.insert("platforms".to_string(), job.platforms_str.clone());
                    if let Some(ref id) = job.id {
                        meta.insert("id".to_string(), id.clone());
                    }
                    if let Some(ref backend) = job.use_backend {
                        meta.insert("use".to_string(), backend.clone());
                    }
                    if let Some(d) = build_result.tag_digests.get(tag) {
                        meta.insert("digest".to_string(), d.clone());
                    }
                    // V2 builds register as DockerImageV2; legacy as DockerImage.
                    new_artifacts.push(Artifact {
                        kind: if job.is_v2 { ArtifactKind::DockerImageV2 } else { ArtifactKind::DockerImage },
                        name: tag.clone(),
                        path: PathBuf::from(tag),
                        target: None,
                        crate_name: job.crate_name.clone(),
                        metadata: meta,
                        size: None,
                    });
                }
            }
        }

        // ==================================================================
        // Docker manifests (must run after all builds complete, since they
        // reference the built image digests)
        // ==================================================================
        for krate in &crates {
            // ------------------------------------------------------------------
            // Docker manifests
            // ------------------------------------------------------------------
            if let Some(ref manifest_configs) = krate.docker_manifests {
                for (midx, manifest_cfg) in manifest_configs.iter().enumerate() {
                    // Check disable (template-aware) before doing any work.
                    if let Some(ref d) = manifest_cfg.disable {
                        if d.is_disabled(|tmpl| ctx.render_template(tmpl)) {
                            let fallback = format!("index {}", midx);
                            let label = manifest_cfg.id.as_deref().unwrap_or(&fallback);
                            log.status(&format!(
                                "docker: skipping disabled manifest '{}' for crate {}",
                                label, krate.name
                            ));
                            continue;
                        }
                    }

                    // Validate: image_templates must not be empty — a manifest
                    // with zero images is always a configuration error.
                    if manifest_cfg.image_templates.is_empty() {
                        let fallback = format!("index {}", midx);
                        let manifest_label = manifest_cfg.id.as_deref().unwrap_or(&fallback);
                        anyhow::bail!(
                            "docker manifest '{}': image_templates must not be empty",
                            manifest_label
                        );
                    }

                    // Render the manifest name template
                    let manifest_name = ctx
                        .render_template(&manifest_cfg.name_template)
                        .with_context(|| {
                            format!(
                                "docker: render manifest name_template '{}' for crate {}",
                                manifest_cfg.name_template, krate.name
                            )
                        })?;

                    // Render image templates, skipping entries that resolve
                    // to empty strings (e.g. conditional templates that
                    // evaluate to nothing for certain configurations).
                    let mut rendered_images: Vec<String> = Vec::new();
                    for tmpl in &manifest_cfg.image_templates {
                        let img = ctx.render_template(tmpl).with_context(|| {
                            format!(
                                "docker: render manifest image_template '{}' for crate {}",
                                tmpl, krate.name
                            )
                        })?;
                        if img.trim().is_empty() {
                            log.warn(&format!(
                                "docker: manifest image_template '{}' rendered to empty string, skipping",
                                tmpl
                            ));
                            continue;
                        }
                        rendered_images.push(img);
                    }

                    // Determine the binary for manifest commands
                    let manifest_bin = match manifest_cfg.use_backend.as_deref() {
                        Some("podman") => "podman",
                        _ => "docker",
                    };

                    // Render create_flags through template engine
                    let rendered_create_flags: Vec<String> = manifest_cfg
                        .create_flags
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .map(|f| ctx.render_template(f).unwrap_or_else(|_| f.clone()))
                        .collect();

                    // Render push_flags through template engine
                    let rendered_push_flags: Vec<String> = manifest_cfg
                        .push_flags
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .map(|f| ctx.render_template(f).unwrap_or_else(|_| f.clone()))
                        .collect();

                    // Build `docker manifest create` command.
                    // Pin image references to their digest (sha256:...) when
                    // available, so the manifest references immutable content
                    // rather than mutable tags.  Digests are captured during the
                    // image push phase and stored in the `new_artifacts` list.
                    let mut create_cmd: Vec<String> = vec![
                        manifest_bin.to_string(),
                        "manifest".to_string(),
                        "create".to_string(),
                        manifest_name.clone(),
                    ];
                    for img in &rendered_images {
                        if let Some(digest) = find_image_digest(&new_artifacts, img) {
                            let pinned = format!("{}@{}", img, digest);
                            log.verbose(&format!("manifest: pinning {} to digest {}", img, digest));
                            create_cmd.push(pinned);
                        } else {
                            log.warn(&format!("no digest found for {}, using tag reference", img));
                            create_cmd.push(img.clone());
                        }
                    }
                    for flag in &rendered_create_flags {
                        create_cmd.push(flag.clone());
                    }

                    // Determine whether to push
                    let manifest_skip_push = resolve_skip_push(&manifest_cfg.skip_push, ctx);
                    let mut manifest_digest: Option<String> = None;

                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would run: {} manifest rm {}",
                            manifest_bin, manifest_name
                        ));
                        log.status(&format!("(dry-run) would run: {}", create_cmd.join(" ")));
                        if !manifest_skip_push {
                            let mut push_cmd: Vec<String> = vec![
                                manifest_bin.to_string(),
                                "manifest".to_string(),
                                "push".to_string(),
                                manifest_name.clone(),
                            ];
                            for flag in &rendered_push_flags {
                                push_cmd.push(flag.clone());
                            }
                            log.status(&format!("(dry-run) would run: {}", push_cmd.join(" ")));
                        }
                    } else {
                        // Remove any existing manifest to prevent stale manifest
                        // failures on re-runs (GoReleaser does this too).
                        // We ignore "no such manifest" errors (manifest didn't
                        // exist yet, which is fine) but propagate other failures.
                        if let Ok(rm_output) = Command::new(manifest_bin)
                            .args(["manifest", "rm", &manifest_name])
                            .output()
                        {
                            if !rm_output.status.success() {
                                let stderr =
                                    String::from_utf8_lossy(&rm_output.stderr).to_lowercase();
                                if !stderr.contains("no such manifest")
                                    && !stderr.contains("not found")
                                {
                                    let stderr_full = String::from_utf8_lossy(&rm_output.stderr);
                                    anyhow::bail!(
                                        "docker manifest rm {} failed: {}",
                                        manifest_name,
                                        stderr_full.trim()
                                    );
                                }
                            }
                        }

                        // Manifest create/push with retry logic — registry
                        // operations can fail transiently. Uses the
                        // manifest's retry config (same as docker build).
                        let (manifest_max_attempts, manifest_base_delay, manifest_max_delay) =
                            resolve_retry_params(&manifest_cfg.retry).with_context(|| {
                                format!(
                                    "docker: invalid retry config for manifest {} crate {}",
                                    midx, krate.name
                                )
                            })?;

                        {
                            let mut last_err: Option<anyhow::Error> = None;
                            for attempt in 1..=manifest_max_attempts {
                                if attempt > 1 {
                                    let multiplier = 2u64.saturating_pow(attempt - 2);
                                    let delay_ms = manifest_base_delay
                                        .as_millis()
                                        .saturating_mul(multiplier as u128);
                                    let mut delay = Duration::from_millis(delay_ms as u64);
                                    if let Some(cap) = manifest_max_delay
                                        && delay > cap
                                    {
                                        delay = cap;
                                    }
                                    log.warn(&format!(
                                        "manifest create attempt {}/{} failed, retrying in {:?}…",
                                        attempt - 1,
                                        manifest_max_attempts,
                                        delay,
                                    ));
                                    std::thread::sleep(delay);
                                }
                                log.status(&format!("running: {}", create_cmd.join(" ")));
                                let output = Command::new(&create_cmd[0])
                                    .args(&create_cmd[1..])
                                    .output()
                                    .with_context(|| {
                                        format!(
                                            "docker: manifest create for crate {} manifest {} (attempt {}/{})",
                                            krate.name, midx, attempt, manifest_max_attempts
                                        )
                                    })?;
                                match log.check_output(output, "docker manifest create") {
                                    Ok(_) => {
                                        if attempt > 1 {
                                            log.status(&format!(
                                                "docker manifest create succeeded on attempt {}/{}",
                                                attempt, manifest_max_attempts
                                            ));
                                        }
                                        last_err = None;
                                        break;
                                    }
                                    Err(e) => {
                                        let err_msg = format!("{:#}", e);
                                        if attempt < manifest_max_attempts
                                            && is_retriable_error(&err_msg)
                                        {
                                            last_err = Some(e);
                                        } else {
                                            return Err(e);
                                        }
                                    }
                                }
                            }
                            if let Some(e) = last_err {
                                return Err(e);
                            }
                        }

                        // Push the manifest (with retry) and capture digest
                        if !manifest_skip_push {
                            let mut push_cmd: Vec<String> = vec![
                                manifest_bin.to_string(),
                                "manifest".to_string(),
                                "push".to_string(),
                                manifest_name.clone(),
                            ];
                            for flag in &rendered_push_flags {
                                push_cmd.push(flag.clone());
                            }

                            let mut last_err: Option<anyhow::Error> = None;
                            for attempt in 1..=manifest_max_attempts {
                                if attempt > 1 {
                                    let multiplier = 2u64.saturating_pow(attempt - 2);
                                    let delay_ms = manifest_base_delay
                                        .as_millis()
                                        .saturating_mul(multiplier as u128);
                                    let mut delay = Duration::from_millis(delay_ms as u64);
                                    if let Some(cap) = manifest_max_delay
                                        && delay > cap
                                    {
                                        delay = cap;
                                    }
                                    log.warn(&format!(
                                        "manifest push attempt {}/{} failed, retrying in {:?}…",
                                        attempt - 1,
                                        manifest_max_attempts,
                                        delay,
                                    ));
                                    std::thread::sleep(delay);
                                }
                                log.status(&format!("running: {}", push_cmd.join(" ")));
                                let output = Command::new(&push_cmd[0])
                                    .args(&push_cmd[1..])
                                    .output()
                                    .with_context(|| {
                                        format!(
                                            "docker: manifest push for crate {} manifest {} (attempt {}/{})",
                                            krate.name, midx, attempt, manifest_max_attempts
                                        )
                                    })?;
                                // Capture stdout for digest extraction before checking status
                                let push_stdout = String::from_utf8_lossy(&output.stdout).to_string();
                                match log.check_output(output, "docker manifest push") {
                                    Ok(_) => {
                                        if attempt > 1 {
                                            log.status(&format!(
                                                "docker manifest push succeeded on attempt {}/{}",
                                                attempt, manifest_max_attempts
                                            ));
                                        }
                                        // Extract digest from push output (sha256:64hexchars)
                                        if let Some(start) = push_stdout.find("sha256:") {
                                            let candidate = &push_stdout[start..];
                                            // sha256: (7 chars) + 64 hex chars = 71
                                            if candidate.len() >= 71
                                                && candidate[7..71].chars().all(|c| c.is_ascii_hexdigit())
                                            {
                                                manifest_digest = Some(candidate[..71].to_string());
                                            }
                                        }
                                        last_err = None;
                                        break;
                                    }
                                    Err(e) => {
                                        let err_msg = format!("{:#}", e);
                                        if attempt < manifest_max_attempts
                                            && is_retriable_error(&err_msg)
                                        {
                                            last_err = Some(e);
                                        } else {
                                            return Err(e);
                                        }
                                    }
                                }
                            }
                            if let Some(e) = last_err {
                                return Err(e);
                            }
                        }
                    }

                    // Register DockerManifest artifact
                    let mut meta = HashMap::new();
                    meta.insert("manifest".to_string(), manifest_name.clone());
                    meta.insert("images".to_string(), rendered_images.join(","));
                    if let Some(ref id) = manifest_cfg.id {
                        meta.insert("id".to_string(), id.clone());
                    }
                    if let Some(ref digest) = manifest_digest {
                        meta.insert("digest".to_string(), digest.clone());
                    }

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::DockerManifest,
                        name: manifest_name.clone(),
                        path: PathBuf::from(&manifest_name),
                        target: None,
                        crate_name: krate.name.clone(),
                        metadata: meta,
                        size: None,
                    });
                }
            }
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_platform_to_arch() {
        assert_eq!(platform_to_arch("linux/amd64"), "amd64");
        assert_eq!(platform_to_arch("linux/arm64"), "arm64");
    }

    #[test]
    fn test_build_docker_command() {
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64", "linux/arm64"],
            &["ghcr.io/owner/app:v1.0.0", "ghcr.io/owner/app:latest"],
            &[],
            true,
            &[],
            &[],
            None,
        )
        .unwrap();
        assert!(cmd.contains(&"buildx".to_string()));
        assert!(cmd.contains(&"build".to_string()));
        assert!(cmd.contains(&"--platform=linux/amd64,linux/arm64".to_string()));
        assert!(cmd.contains(&"--push".to_string()));
        assert!(cmd.contains(&"--tag".to_string()));
    }

    #[test]
    fn test_build_docker_command_dry_run() {
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            false,
            &[],
            &[],
            None,
        )
        .unwrap();
        // When push=false, neither --push nor --load
        assert!(!cmd.contains(&"--push".to_string()));
    }

    #[test]
    fn test_stage_skips_without_docker_config() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = DockerStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_platform_to_arch_no_slash() {
        // Fallback: no slash in string returns the whole string
        assert_eq!(platform_to_arch("amd64"), "amd64");
    }

    #[test]
    fn test_build_docker_command_structure() {
        let cmd = build_docker_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["my-image:latest"],
            &[],
            true,
            &[],
            &[],
            Some("buildx"),
        )
        .unwrap();
        assert_eq!(cmd[0], "docker");
        assert_eq!(cmd[1], "buildx");
        assert_eq!(cmd[2], "build");
        // staging dir is the last argument
        assert_eq!(cmd.last().unwrap(), "/tmp/ctx");
    }

    #[test]
    fn test_build_docker_command_multiple_tags() {
        let cmd = build_docker_command(
            "/tmp/ctx",
            &["linux/amd64", "linux/arm64"],
            &["repo/img:v1.0.0", "repo/img:latest"],
            &[],
            true,
            &[],
            &[],
            None,
        )
        .unwrap();
        // Both tags should appear after --tag flags
        let tag_positions: Vec<usize> = cmd
            .iter()
            .enumerate()
            .filter_map(|(i, t)| if t == "--tag" { Some(i) } else { None })
            .collect();
        assert_eq!(tag_positions.len(), 2);
        assert_eq!(cmd[tag_positions[0] + 1], "repo/img:v1.0.0");
        assert_eq!(cmd[tag_positions[1] + 1], "repo/img:latest");
    }

    #[test]
    fn test_docker_stage_dry_run_registers_artifacts() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Create fake binaries so the stage has something to pick up
        let amd64_bin = tmp.path().join("myapp-amd64");
        let arm64_bin = tmp.path().join("myapp-arm64");
        fs::write(&amd64_bin, b"fake amd64 binary").unwrap();
        fs::write(&arm64_bin, b"fake arm64 binary").unwrap();

        // Create a fake Dockerfile (not needed in dry-run, but still)
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\nCOPY . /\n").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec![
                "ghcr.io/owner/myapp:{{ .Tag }}".to_string(),
                "ghcr.io/owner/myapp:latest".to_string(),
            ],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string(), "linux/arm64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: None,
            extra_files: None,
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
            disable: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        // Register binary artifacts
        let mut meta_amd64 = HashMap::new();
        meta_amd64.insert("binary".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: amd64_bin.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta_amd64,
            size: None,
        });

        let mut meta_arm64 = HashMap::new();
        meta_arm64.insert("binary".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: arm64_bin.clone(),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta_arm64,
            size: None,
        });

        let stage = DockerStage;
        stage.run(&mut ctx).unwrap();

        // Should have registered 2 DockerImage artifacts (one per rendered tag)
        let docker_images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        assert_eq!(docker_images.len(), 2);

        let tags: Vec<&str> = docker_images
            .iter()
            .map(|a| a.metadata.get("tag").unwrap().as_str())
            .collect();
        assert!(tags.contains(&"ghcr.io/owner/myapp:v1.0.0"));
        assert!(tags.contains(&"ghcr.io/owner/myapp:latest"));
    }

    // ------------------------------------------------------------------
    // New tests for skip_push, extra_files, push_flags
    // ------------------------------------------------------------------

    #[test]
    fn test_docker_config_parses_new_fields() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
skip_push: true
extra_files:
  - "config.yaml"
  - "scripts/init.sh"
push_flags:
  - "--cache-to=type=registry,ref=ghcr.io/owner/app:cache"
  - "--provenance=true"
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.skip_push, Some(SkipPushConfig::Bool(true)));
        let extra = cfg.extra_files.unwrap();
        assert_eq!(extra.len(), 2);
        assert_eq!(extra[0], "config.yaml");
        assert_eq!(extra[1], "scripts/init.sh");
        let pf = cfg.push_flags.unwrap();
        assert_eq!(pf.len(), 2);
        assert_eq!(
            pf[0],
            "--cache-to=type=registry,ref=ghcr.io/owner/app:cache"
        );
        assert_eq!(pf[1], "--provenance=true");
    }

    #[test]
    fn test_build_docker_command_skip_push() {
        // When push=false (i.e. skip_push is true or dry_run), --push should not appear
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            false,
            &[],
            &[],
            None,
        )
        .unwrap();
        assert!(!cmd.contains(&"--push".to_string()));

        // When push=true, --push should appear
        let cmd_push = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            true,
            &[],
            &[],
            None,
        )
        .unwrap();
        assert!(cmd_push.contains(&"--push".to_string()));
    }

    #[test]
    fn test_build_docker_command_push_flags() {
        let push_flags = vec![
            "--cache-to=type=registry,ref=ghcr.io/owner/app:cache".to_string(),
            "--provenance=true".to_string(),
        ];
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            true,
            &push_flags,
            &[],
            None,
        )
        .unwrap();
        assert!(cmd.contains(&"--push".to_string()));
        assert!(cmd.contains(&"--cache-to=type=registry,ref=ghcr.io/owner/app:cache".to_string()));
        assert!(cmd.contains(&"--provenance=true".to_string()));

        // push_flags should NOT appear when push=false
        let cmd_no_push = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            false,
            &push_flags,
            &[],
            None,
        )
        .unwrap();
        assert!(!cmd_no_push.contains(&"--push".to_string()));
        assert!(!cmd_no_push.contains(&"--provenance=true".to_string()));
    }

    #[test]
    fn test_extra_files_copied_to_staging_dry_run() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Create fake Dockerfile
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\nCOPY . /\n").unwrap();

        // Create fake extra files
        let extra1 = tmp.path().join("config.yaml");
        let extra2 = tmp.path().join("init.sh");
        fs::write(&extra1, b"key: value").unwrap();
        fs::write(&extra2, b"#!/bin/bash\necho hello").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/myapp:latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: Some(SkipPushConfig::Bool(true)),
            extra_files: Some(vec![
                extra1.to_string_lossy().into_owned(),
                extra2.to_string_lossy().into_owned(),
            ]),
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
            disable: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        // dry-run should succeed without actually copying files
        stage.run(&mut ctx).unwrap();

        // In dry-run mode, files are not actually copied, but the stage should
        // complete successfully and register artifacts
        let docker_images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        assert_eq!(docker_images.len(), 1);
    }

    #[test]
    fn test_extra_files_copied_to_staging_live() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig, DockerRetryConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Create fake Dockerfile
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\nCOPY . /\n").unwrap();

        // Create fake extra files
        let extra1 = tmp.path().join("config.yaml");
        let extra2 = tmp.path().join("init.sh");
        fs::write(&extra1, b"key: value").unwrap();
        fs::write(&extra2, b"#!/bin/bash\necho hello").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/myapp:latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: Some(SkipPushConfig::Bool(true)), // skip push so we don't actually run docker
            extra_files: Some(vec![
                extra1.to_string_lossy().into_owned(),
                extra2.to_string_lossy().into_owned(),
            ]),
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: Some(DockerRetryConfig {
                attempts: Some(1),
                delay: None,
                max_delay: None,
            }),
            use_backend: None,
            disable: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let dist = tmp.path().join("dist");
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        // The stage will fail at the docker buildx command (docker not available),
        // but we can verify the staging directory was set up correctly.
        let _result = stage_setup_only(&mut ctx);

        // Verify the staging directory was created with extra files
        let staging_dir = dist.join("docker").join("myapp").join("0");
        // The Dockerfile should be copied
        assert!(staging_dir.join("Dockerfile").exists());
        // Extra files should be copied
        assert!(staging_dir.join("config.yaml").exists());
        assert!(staging_dir.join("init.sh").exists());
        // Verify content
        assert_eq!(
            fs::read_to_string(staging_dir.join("config.yaml")).unwrap(),
            "key: value"
        );
        assert_eq!(
            fs::read_to_string(staging_dir.join("init.sh")).unwrap(),
            "#!/bin/bash\necho hello"
        );
    }

    /// Helper: runs the docker stage but catches the expected docker-not-found error.
    /// This lets us verify the staging directory setup without requiring docker.
    fn stage_setup_only(ctx: &mut Context) -> Result<()> {
        let stage = DockerStage;
        stage.run(ctx)
    }

    #[test]
    fn test_docker_config_new_fields_default_to_none() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.skip_push, None);
        assert_eq!(cfg.extra_files, None);
        assert_eq!(cfg.push_flags, None);
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_skip_push_prevents_push_flag_in_command() {
        // When skip_push=true and dry_run=false, should_push should be false
        // so the docker command should NOT contain --push
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            false, // push=false (because skip_push=true or dry_run)
            &["--provenance=true".to_string()],
            &[],
            None,
        )
        .unwrap();
        assert!(!cmd.contains(&"--push".to_string()));
        // push_flags should also NOT be included when push=false
        assert!(!cmd.contains(&"--provenance=true".to_string()));
    }

    #[test]
    fn test_push_flags_appended_to_command() {
        let push_flags = vec!["--provenance=true".to_string(), "--sbom=true".to_string()];
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["img:v1.0.0"],
            &[],
            true,
            &push_flags,
            &[],
            None,
        )
        .unwrap();
        assert!(cmd.contains(&"--push".to_string()));
        assert!(cmd.contains(&"--provenance=true".to_string()));
        assert!(cmd.contains(&"--sbom=true".to_string()));
        // push_flags should come after --push
        let push_idx = cmd.iter().position(|x| x == "--push").unwrap();
        let prov_idx = cmd.iter().position(|x| x == "--provenance=true").unwrap();
        assert!(prov_idx > push_idx, "push_flags should come after --push");
    }

    #[test]
    fn test_multi_platform_generates_correct_platform_flag() {
        let cmd = build_docker_command(
            "/tmp/ctx",
            &["linux/amd64", "linux/arm64", "linux/arm/v7"],
            &["img:latest"],
            &[],
            false,
            &[],
            &[],
            None,
        )
        .unwrap();
        assert!(cmd.contains(&"--platform=linux/amd64,linux/arm64,linux/arm/v7".to_string()));
    }

    #[test]
    fn test_platform_to_arch_various_formats() {
        assert_eq!(platform_to_arch("linux/amd64"), "amd64");
        assert_eq!(platform_to_arch("linux/arm64"), "arm64");
        assert_eq!(platform_to_arch("linux/arm/v7"), "armv7");
        assert_eq!(platform_to_arch("linux/arm/v6"), "armv6");
        assert_eq!(platform_to_arch("linux/386"), "386");
        assert_eq!(platform_to_arch("windows/amd64"), "amd64");
    }

    #[test]
    fn test_image_template_rendering_with_context() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\n").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec![
                "ghcr.io/owner/myapp:{{ .Version }}".to_string(),
                "ghcr.io/owner/myapp:{{ .Tag }}".to_string(),
                "ghcr.io/owner/myapp:latest".to_string(),
            ],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: None,
            extra_files: None,
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
            disable: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "2.5.0");
        ctx.template_vars_mut().set("Tag", "v2.5.0");

        let stage = DockerStage;
        stage.run(&mut ctx).unwrap();

        // Verify all 3 rendered tags appear in the registered artifacts
        let images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        assert_eq!(images.len(), 3);

        let tags: Vec<&str> = images
            .iter()
            .map(|a| a.metadata.get("tag").unwrap().as_str())
            .collect();
        assert!(tags.contains(&"ghcr.io/owner/myapp:2.5.0"));
        assert!(tags.contains(&"ghcr.io/owner/myapp:v2.5.0"));
        assert!(tags.contains(&"ghcr.io/owner/myapp:latest"));
    }

    #[test]
    fn test_binary_staging_per_architecture_subdirectory() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use anodize_core::config::{Config, CrateConfig, DockerConfig, DockerRetryConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Create fake binaries
        let amd64_bin = tmp.path().join("myapp-amd64");
        let arm64_bin = tmp.path().join("myapp-arm64");
        fs::write(&amd64_bin, b"fake amd64").unwrap();
        fs::write(&arm64_bin, b"fake arm64").unwrap();

        // Create Dockerfile
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\nCOPY . /\n").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/myapp:latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string(), "linux/arm64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: Some(SkipPushConfig::Bool(true)),
            extra_files: None,
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: Some(DockerRetryConfig {
                attempts: Some(1),
                delay: None,
                max_delay: None,
            }),
            use_backend: None,
            disable: None,
        };

        let dist = tmp.path().join("dist");
        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        // Register binary artifacts with correct target triples
        let mut meta_amd64 = HashMap::new();
        meta_amd64.insert("binary".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: amd64_bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta_amd64,
            size: None,
        });

        let mut meta_arm64 = HashMap::new();
        meta_arm64.insert("binary".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: arm64_bin,
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta_arm64,
            size: None,
        });

        // Run the stage — it will fail at docker buildx, but staging will be done
        let _result = DockerStage.run(&mut ctx);

        // Verify binaries are staged per arch subdirectory
        let staging_base = dist.join("docker").join("myapp").join("0");
        let amd64_dir = staging_base.join("binaries").join("amd64");
        let arm64_dir = staging_base.join("binaries").join("arm64");

        assert!(amd64_dir.exists(), "amd64 binaries dir should exist");
        assert!(arm64_dir.exists(), "arm64 binaries dir should exist");
        assert!(
            amd64_dir.join("myapp").exists(),
            "amd64 binary should be staged"
        );
        assert!(
            arm64_dir.join("myapp").exists(),
            "arm64 binary should be staged"
        );
    }

    #[test]
    fn test_build_docker_command_extra_build_flags() {
        let extra = vec![
            "--build-arg=APP_VERSION=1.0.0".to_string(),
            "--label=org.opencontainers.image.version=1.0.0".to_string(),
        ];
        let cmd = build_docker_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:v1.0.0"],
            &extra,
            false,
            &[],
            &[],
            None,
        )
        .unwrap();
        assert!(cmd.contains(&"--build-arg=APP_VERSION=1.0.0".to_string()));
        assert!(cmd.contains(&"--label=org.opencontainers.image.version=1.0.0".to_string()));
    }

    #[test]
    fn test_build_docker_command_context_dir_is_last() {
        let cmd = build_docker_command(
            "/my/staging/dir",
            &["linux/amd64"],
            &["img:latest"],
            &[],
            false,
            &[],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(cmd.last().unwrap(), "/my/staging/dir");
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_missing_dockerfile_errors_with_path() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/app:latest".to_string()],
            dockerfile: "/nonexistent/Dockerfile-that-does-not-exist".to_string(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: None,
            extra_files: None,
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
            disable: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "missing Dockerfile should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Dockerfile") || err.contains("docker"),
            "error should mention Dockerfile, got: {err}"
        );
    }

    #[test]
    fn test_docker_build_failure_dry_run_skips_execution() {
        // In dry-run mode, even with invalid config, docker should not fail
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/app:latest".to_string()],
            dockerfile: "/nonexistent/Dockerfile".to_string(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: None,
            extra_files: None,
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
            disable: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "dry-run should skip docker execution, got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_extra_files_directory_entry_errors() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Create a real Dockerfile so we get past that check
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\n").unwrap();

        // Create a directory to use as an extra_files entry
        let extra_dir = tmp.path().join("some_directory");
        fs::create_dir_all(&extra_dir).unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/app:latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: None,
            extra_files: Some(vec![extra_dir.to_string_lossy().into_owned()]),
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
            disable: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_err(),
            "directory as extra_files entry should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("directory") || err.contains("some_directory"),
            "error should mention that directories are not supported, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Tests for id, ids, labels config fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_docker_config_parses_id_ids_labels() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
id: my-docker
ids:
  - linux-build
  - windows-build
labels:
  org.opencontainers.image.title: "MyApp"
  org.opencontainers.image.version: "{{ .Version }}"
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.id.as_deref(), Some("my-docker"));
        let ids = cfg.ids.as_ref().unwrap();
        assert_eq!(ids, &["linux-build", "windows-build"]);
        let labels = cfg.labels.as_ref().unwrap();
        assert_eq!(
            labels.get("org.opencontainers.image.title").unwrap(),
            "MyApp"
        );
        assert_eq!(
            labels.get("org.opencontainers.image.version").unwrap(),
            "{{ .Version }}"
        );
    }

    #[test]
    fn test_labels_appear_in_docker_build_command() {
        let labels = vec![
            (
                "org.opencontainers.image.source".to_string(),
                "https://github.com/owner/app".to_string(),
            ),
            (
                "org.opencontainers.image.version".to_string(),
                "1.0.0".to_string(),
            ),
        ];
        let cmd = build_docker_command(
            "/tmp/staging",
            &["linux/amd64"],
            &["ghcr.io/owner/app:v1.0.0"],
            &[],
            false,
            &[],
            &labels,
            None,
        )
        .unwrap();
        assert!(
            cmd.contains(&"--label".to_string()),
            "command should contain --label flag"
        );
        assert!(
            cmd.contains(
                &"org.opencontainers.image.source=https://github.com/owner/app".to_string()
            ),
            "label key=value should appear in command"
        );
        assert!(
            cmd.contains(&"org.opencontainers.image.version=1.0.0".to_string()),
            "label key=value should appear in command"
        );
    }

    #[test]
    fn test_docker_config_new_fields_default_to_none_extended() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.id, None);
        assert_eq!(cfg.ids, None);
        assert!(cfg.labels.is_none());
        assert!(cfg.retry.is_none());
    }

    // -----------------------------------------------------------------------
    // Tests for retry configuration
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_duration_string_seconds() {
        let d = parse_duration_string("5s").unwrap();
        assert_eq!(d, Duration::from_secs(5));
    }

    #[test]
    fn test_parse_duration_string_milliseconds() {
        let d = parse_duration_string("500ms").unwrap();
        assert_eq!(d, Duration::from_millis(500));
    }

    #[test]
    fn test_parse_duration_string_minutes() {
        let d = parse_duration_string("2m").unwrap();
        assert_eq!(d, Duration::from_secs(120));
    }

    #[test]
    fn test_parse_duration_string_trims_whitespace() {
        let d = parse_duration_string("  3s  ").unwrap();
        assert_eq!(d, Duration::from_secs(3));
    }

    #[test]
    fn test_parse_duration_string_empty() {
        assert!(parse_duration_string("").is_err());
        assert!(parse_duration_string("   ").is_err());
    }

    #[test]
    fn test_parse_duration_string_bare_number_as_seconds() {
        let d = parse_duration_string("10").unwrap();
        assert_eq!(d, Duration::from_secs(10));
        let d = parse_duration_string("100").unwrap();
        assert_eq!(d, Duration::from_secs(100));
    }

    #[test]
    fn test_parse_duration_string_invalid_suffix() {
        assert!(parse_duration_string("5h").is_err());
    }

    #[test]
    fn test_parse_duration_string_invalid_number() {
        assert!(parse_duration_string("abcs").is_err());
        assert!(parse_duration_string("1.5s").is_err());
    }

    #[test]
    fn test_resolve_retry_params_none() {
        let (attempts, delay, max_delay) = resolve_retry_params(&None).unwrap();
        assert_eq!(attempts, 10);
        assert_eq!(delay, Duration::from_secs(10));
        // Default max_delay is 5 minutes to prevent unbounded backoff
        assert_eq!(max_delay, Some(Duration::from_secs(300)));
    }

    #[test]
    fn test_resolve_retry_params_defaults() {
        use anodize_core::config::DockerRetryConfig;
        let cfg = Some(DockerRetryConfig {
            attempts: None,
            delay: None,
            max_delay: None,
        });
        let (attempts, delay, max_delay) = resolve_retry_params(&cfg).unwrap();
        assert_eq!(attempts, 10);
        assert_eq!(delay, Duration::from_secs(10));
        // Default max_delay is 5 minutes to prevent unbounded backoff
        assert_eq!(max_delay, Some(Duration::from_secs(300)));
    }

    #[test]
    fn test_resolve_retry_params_full() {
        use anodize_core::config::DockerRetryConfig;
        let cfg = Some(DockerRetryConfig {
            attempts: Some(3),
            delay: Some("500ms".to_string()),
            max_delay: Some("10s".to_string()),
        });
        let (attempts, delay, max_delay) = resolve_retry_params(&cfg).unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(delay, Duration::from_millis(500));
        assert_eq!(max_delay, Some(Duration::from_secs(10)));
    }

    #[test]
    fn test_resolve_retry_params_invalid_delay() {
        use anodize_core::config::DockerRetryConfig;
        let cfg = Some(DockerRetryConfig {
            attempts: Some(3),
            delay: Some("invalid".to_string()),
            max_delay: None,
        });
        assert!(resolve_retry_params(&cfg).is_err());
    }

    #[test]
    fn test_docker_config_parses_retry() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
retry:
  attempts: 5
  delay: "2s"
  max_delay: "30s"
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let retry = cfg.retry.unwrap();
        assert_eq!(retry.attempts, Some(5));
        assert_eq!(retry.delay.as_deref(), Some("2s"));
        assert_eq!(retry.max_delay.as_deref(), Some("30s"));
    }

    #[test]
    fn test_retry_dry_run_logs_config() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig, DockerRetryConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\n").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/myapp:latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: None,
            extra_files: None,
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: Some(DockerRetryConfig {
                attempts: Some(3),
                delay: Some("1s".to_string()),
                max_delay: Some("10s".to_string()),
            }),
            use_backend: None,
            disable: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        // dry-run with retry config should succeed without actually running docker
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "dry-run with retry config should succeed, got: {:?}",
            result.err()
        );

        // Verify artifacts are still registered
        let images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        assert_eq!(images.len(), 1);
    }

    #[test]
    fn test_no_retry_config_single_attempt_dry_run() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\n").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/myapp:latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: None,
            extra_files: None,
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None, // No retry config = default 10 attempts
            use_backend: None,
            disable: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "dry-run without retry config should succeed, got: {:?}",
            result.err()
        );

        let images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        assert_eq!(images.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Task 8: skip_push auto, use_backend, docker_manifests, digest
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_skip_push_auto() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
skip_push: auto
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.skip_push, Some(SkipPushConfig::Auto));
    }

    #[test]
    fn test_config_skip_push_true_serde() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
skip_push: true
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.skip_push, Some(SkipPushConfig::Bool(true)));
    }

    #[test]
    fn test_config_skip_push_false_serde() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
skip_push: false
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.skip_push, Some(SkipPushConfig::Bool(false)));
    }

    #[test]
    fn test_config_use_backend_podman() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
use: podman
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_backend.as_deref(), Some("podman"));
    }

    #[test]
    fn test_config_use_backend_buildx() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
use: buildx
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_backend.as_deref(), Some("buildx"));
    }

    #[test]
    fn test_config_use_backend_docker() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
use: docker
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_backend.as_deref(), Some("docker"));
    }

    #[test]
    fn test_config_use_backend_default_none() {
        let yaml = r#"
image_templates:
  - "ghcr.io/owner/app:latest"
dockerfile: Dockerfile
"#;
        let cfg: anodize_core::config::DockerConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_backend, None);
    }

    #[test]
    fn test_config_docker_manifests_full() {
        use anodize_core::config::Config;
        let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    docker_manifests:
      - name_template: "ghcr.io/owner/app:{{ .Version }}"
        image_templates:
          - "ghcr.io/owner/app:{{ .Version }}-amd64"
          - "ghcr.io/owner/app:{{ .Version }}-arm64"
        create_flags:
          - "--amend"
        push_flags:
          - "--purge"
        skip_push: auto
        id: my-manifest
        use: docker
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let manifests = config.crates[0].docker_manifests.as_ref().unwrap();
        assert_eq!(manifests.len(), 1);
        let m = &manifests[0];
        assert_eq!(m.name_template, "ghcr.io/owner/app:{{ .Version }}");
        assert_eq!(m.image_templates.len(), 2);
        assert_eq!(m.create_flags.as_ref().unwrap(), &["--amend"]);
        assert_eq!(m.push_flags.as_ref().unwrap(), &["--purge"]);
        assert_eq!(m.skip_push, Some(SkipPushConfig::Auto));
        assert_eq!(m.id.as_deref(), Some("my-manifest"));
        assert_eq!(m.use_backend.as_deref(), Some("docker"));
    }

    #[test]
    fn test_config_docker_manifests_omitted() {
        use anodize_core::config::Config;
        let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.crates[0].docker_manifests.is_none());
    }

    #[test]
    fn test_resolve_skip_push_auto_prerelease() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Prerelease", "rc.1");

        let skip = resolve_skip_push(&Some(SkipPushConfig::Auto), &ctx);
        assert!(skip, "auto should skip push when Prerelease is non-empty");
    }

    #[test]
    fn test_resolve_skip_push_auto_no_prerelease() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Prerelease", "");

        let skip = resolve_skip_push(&Some(SkipPushConfig::Auto), &ctx);
        assert!(!skip, "auto should NOT skip push when Prerelease is empty");
    }

    #[test]
    fn test_resolve_skip_push_auto_prerelease_unset() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());

        let skip = resolve_skip_push(&Some(SkipPushConfig::Auto), &ctx);
        assert!(
            !skip,
            "auto should NOT skip push when Prerelease is not set"
        );
    }

    #[test]
    fn test_resolve_skip_push_bool_true() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());

        let skip = resolve_skip_push(&Some(SkipPushConfig::Bool(true)), &ctx);
        assert!(skip);
    }

    #[test]
    fn test_resolve_skip_push_bool_false() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());

        let skip = resolve_skip_push(&Some(SkipPushConfig::Bool(false)), &ctx);
        assert!(!skip);
    }

    #[test]
    fn test_resolve_skip_push_none() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let ctx = Context::new(config, ContextOptions::default());

        let skip = resolve_skip_push(&None, &ctx);
        assert!(!skip, "None should not skip push");
    }

    #[test]
    fn test_resolve_backend_buildx_explicit() {
        let (bin, subs) = resolve_backend(Some("buildx"), false).unwrap();
        assert_eq!(bin, "docker");
        assert_eq!(subs, vec!["buildx", "build"]);
    }

    #[test]
    fn test_resolve_backend_docker_explicit() {
        let (bin, subs) = resolve_backend(Some("docker"), false).unwrap();
        assert_eq!(bin, "docker");
        assert_eq!(subs, vec!["build"]);
    }

    #[test]
    fn test_resolve_backend_podman_explicit() {
        let (bin, subs) = resolve_backend(Some("podman"), false).unwrap();
        assert_eq!(bin, "podman");
        assert_eq!(subs, vec!["build"]);
    }

    #[test]
    fn test_resolve_backend_default_single_platform() {
        let (bin, subs) = resolve_backend(None, false).unwrap();
        assert_eq!(bin, "docker");
        assert_eq!(subs, vec!["build"]);
    }

    #[test]
    fn test_resolve_backend_default_multi_platform() {
        let (bin, subs) = resolve_backend(None, true).unwrap();
        assert_eq!(bin, "docker");
        assert_eq!(subs, vec!["buildx", "build"]);
    }

    #[test]
    fn test_resolve_backend_unknown_errors() {
        let result = resolve_backend(Some("containerd"), false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown docker backend 'containerd'"),
            "error should mention the unknown backend, got: {err}"
        );
    }

    #[test]
    fn test_build_docker_command_podman_backend() {
        let cmd = build_docker_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest"],
            &[],
            false,
            &[],
            &[],
            Some("podman"),
        )
        .unwrap();
        assert_eq!(cmd[0], "podman");
        assert_eq!(cmd[1], "build");
        assert_eq!(cmd.last().unwrap(), "/tmp/ctx");
    }

    #[test]
    fn test_build_docker_command_docker_backend() {
        let cmd = build_docker_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest"],
            &[],
            false,
            &[],
            &[],
            Some("docker"),
        )
        .unwrap();
        assert_eq!(cmd[0], "docker");
        assert_eq!(cmd[1], "build");
        // Should NOT have "buildx" subcommand
        assert!(!cmd.contains(&"buildx".to_string()));
    }

    #[test]
    fn test_build_docker_command_buildx_backend() {
        let cmd = build_docker_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest"],
            &[],
            false,
            &[],
            &[],
            Some("buildx"),
        )
        .unwrap();
        assert_eq!(cmd[0], "docker");
        assert_eq!(cmd[1], "buildx");
        assert_eq!(cmd[2], "build");
    }

    #[test]
    fn test_docker_manifest_dry_run() {
        use anodize_core::config::{Config, CrateConfig, DockerManifestConfig};
        use anodize_core::context::{Context, ContextOptions};

        let config = Config {
            project_name: "test".to_string(),
            crates: vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                docker_manifests: Some(vec![DockerManifestConfig {
                    name_template: "ghcr.io/owner/app:{{ .Version }}".to_string(),
                    image_templates: vec![
                        "ghcr.io/owner/app:{{ .Version }}-amd64".to_string(),
                        "ghcr.io/owner/app:{{ .Version }}-arm64".to_string(),
                    ],
                    create_flags: Some(vec!["--amend".to_string()]),
                    push_flags: None,
                    skip_push: None,
                    id: Some("multi-arch".to_string()),
                    use_backend: None,
                    retry: None,
                    disable: None,
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "dry-run manifest should succeed, got: {:?}",
            result.err()
        );

        // Verify DockerManifest artifact was registered
        let manifests = ctx.artifacts.by_kind(ArtifactKind::DockerManifest);
        assert_eq!(manifests.len(), 1);
        assert_eq!(
            manifests[0].metadata.get("manifest").unwrap(),
            "ghcr.io/owner/app:1.0.0"
        );
        assert_eq!(
            manifests[0].metadata.get("images").unwrap(),
            "ghcr.io/owner/app:1.0.0-amd64,ghcr.io/owner/app:1.0.0-arm64"
        );
        assert_eq!(manifests[0].metadata.get("id").unwrap(), "multi-arch");
    }

    #[test]
    fn test_docker_manifest_skip_push_auto_prerelease() {
        use anodize_core::config::{Config, CrateConfig, DockerManifestConfig};
        use anodize_core::context::{Context, ContextOptions};

        let config = Config {
            project_name: "test".to_string(),
            crates: vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                docker_manifests: Some(vec![DockerManifestConfig {
                    name_template: "ghcr.io/owner/app:{{ .Version }}".to_string(),
                    image_templates: vec!["ghcr.io/owner/app:{{ .Version }}-amd64".to_string()],
                    create_flags: None,
                    push_flags: None,
                    skip_push: Some(SkipPushConfig::Auto),
                    id: None,
                    use_backend: None,
                    retry: None,
                    disable: None,
                }]),
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0-rc.1");
        ctx.template_vars_mut().set("Tag", "v1.0.0-rc.1");
        ctx.template_vars_mut().set("Prerelease", "rc.1");

        let stage = DockerStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "manifest with auto skip_push + prerelease should succeed, got: {:?}",
            result.err()
        );

        // Artifact should still be registered even if push is skipped
        let manifests = ctx.artifacts.by_kind(ArtifactKind::DockerManifest);
        assert_eq!(manifests.len(), 1);
    }

    #[test]
    fn test_docker_manifest_with_use_backend_podman() {
        use anodize_core::config::DockerManifestConfig;
        let yaml = r#"
name_template: "ghcr.io/owner/app:latest"
image_templates:
  - "ghcr.io/owner/app:latest-amd64"
use: podman
"#;
        let cfg: DockerManifestConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.use_backend.as_deref(), Some("podman"));
    }

    #[test]
    fn test_docker_stage_uses_backend_in_artifact_metadata() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\n").unwrap();

        let docker_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/myapp:latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            binaries: None,
            build_flag_templates: None,
            skip_push: None,
            extra_files: None,
            templated_extra_files: None,
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: Some("podman".to_string()),
            disable: None,
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![docker_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        stage.run(&mut ctx).unwrap();

        let images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].metadata.get("use").unwrap(), "podman");
    }

    // ====================================================================
    // Docker V2 tests
    // ====================================================================

    #[test]
    fn test_generate_v2_image_tags() {
        let images = vec![
            "ghcr.io/owner/app".to_string(),
            "docker.io/owner/app".to_string(),
        ];
        let tags = vec!["latest".to_string(), "v1.0.0".to_string()];
        let result = generate_v2_image_tags(&images, &tags);
        assert_eq!(result.len(), 4);
        // Results are sorted and deduped
        assert_eq!(result[0], "docker.io/owner/app:latest");
        assert_eq!(result[1], "docker.io/owner/app:v1.0.0");
        assert_eq!(result[2], "ghcr.io/owner/app:latest");
        assert_eq!(result[3], "ghcr.io/owner/app:v1.0.0");
    }

    #[test]
    fn test_generate_v2_image_tags_empty() {
        assert!(generate_v2_image_tags(&[], &["latest".to_string()]).is_empty());
        assert!(generate_v2_image_tags(&["img".to_string()], &[]).is_empty());
    }

    #[test]
    fn test_generate_v2_image_tags_single() {
        let result = generate_v2_image_tags(
            &["ghcr.io/owner/app".to_string()],
            &["latest".to_string()],
        );
        assert_eq!(result, vec!["ghcr.io/owner/app:latest"]);
    }

    #[test]
    fn test_build_docker_v2_command_basic() {
        let image_tags = vec![
            "ghcr.io/owner/app:latest".to_string(),
            "ghcr.io/owner/app:v1.0.0".to_string(),
        ];
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &image_tags,
            &[],
            &[],
            &[],
            &[],
            false,
            false,
        )
        .unwrap();

        // V2 always uses buildx
        assert_eq!(cmd[0], "docker");
        assert_eq!(cmd[1], "buildx");
        assert_eq!(cmd[2], "build");

        // Platform
        assert!(cmd.contains(&"--platform=linux/amd64".to_string()));

        // Tags
        let tag_positions: Vec<usize> = cmd
            .iter()
            .enumerate()
            .filter_map(|(i, t)| if t == "--tag" { Some(i) } else { None })
            .collect();
        assert_eq!(tag_positions.len(), 2);
        assert_eq!(cmd[tag_positions[0] + 1], "ghcr.io/owner/app:latest");
        assert_eq!(cmd[tag_positions[1] + 1], "ghcr.io/owner/app:v1.0.0");

        // Context dir is last
        assert_eq!(cmd.last().unwrap(), "/tmp/ctx");
    }

    #[test]
    fn test_build_docker_v2_command_build_args() {
        let build_args = vec![
            ("APP_VERSION".to_string(), "1.0.0".to_string()),
            ("BUILD_DATE".to_string(), "2024-01-01".to_string()),
        ];
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest".to_string()],
            &build_args,
            &[],
            &[],
            &[],
            false,
            false,
        )
        .unwrap();

        // Check --build-arg flags
        let ba_positions: Vec<usize> = cmd
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                if t == "--build-arg" {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(ba_positions.len(), 2);
        assert_eq!(cmd[ba_positions[0] + 1], "APP_VERSION=1.0.0");
        assert_eq!(cmd[ba_positions[1] + 1], "BUILD_DATE=2024-01-01");
    }

    #[test]
    fn test_build_docker_v2_command_annotations() {
        let annotations = vec![
            (
                "org.opencontainers.image.source".to_string(),
                "https://github.com/owner/app".to_string(),
            ),
            (
                "org.opencontainers.image.version".to_string(),
                "1.0.0".to_string(),
            ),
        ];
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest".to_string()],
            &[],
            &annotations,
            &[],
            &[],
            false,
            false,
        )
        .unwrap();

        let ann_positions: Vec<usize> = cmd
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                if t == "--annotation" {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(ann_positions.len(), 2);
        assert_eq!(
            cmd[ann_positions[0] + 1],
            "org.opencontainers.image.source=https://github.com/owner/app"
        );
        assert_eq!(
            cmd[ann_positions[1] + 1],
            "org.opencontainers.image.version=1.0.0"
        );
    }

    #[test]
    fn test_build_docker_v2_command_labels() {
        let labels = vec![("maintainer".to_string(), "dev@example.com".to_string())];
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest".to_string()],
            &[],
            &[],
            &labels,
            &[],
            false,
            false,
        )
        .unwrap();

        assert!(cmd.contains(&"--label".to_string()));
        assert!(cmd.contains(&"maintainer=dev@example.com".to_string()));
    }

    #[test]
    fn test_build_docker_v2_command_sbom_true() {
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest".to_string()],
            &[],
            &[],
            &[],
            &[],
            true, // sbom enabled
            false,
        )
        .unwrap();

        assert!(cmd.contains(&"--attest=type=sbom".to_string()));
        // When sbom is true, auto --sbom=false should NOT be added
        assert!(!cmd.contains(&"--sbom=false".to_string()));
    }

    #[test]
    fn test_build_docker_v2_command_sbom_false() {
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest".to_string()],
            &[],
            &[],
            &[],
            &[],
            false, // sbom not enabled
            false,
        )
        .unwrap();

        assert!(!cmd.contains(&"--sbom=true".to_string()));
    }

    #[test]
    fn test_build_docker_v2_command_flags() {
        let flags = vec![
            "--cache-from=type=gha".to_string(),
            "--cache-to=type=gha".to_string(),
        ];
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest".to_string()],
            &[],
            &[],
            &[],
            &flags,
            false,
            false,
        )
        .unwrap();

        assert!(cmd.contains(&"--cache-from=type=gha".to_string()));
        assert!(cmd.contains(&"--cache-to=type=gha".to_string()));
    }

    #[test]
    fn test_build_docker_v2_command_push() {
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
            true, // push
        )
        .unwrap();

        assert!(cmd.contains(&"--push".to_string()));
        assert!(!cmd.contains(&"--load".to_string()));
    }

    #[test]
    fn test_build_docker_v2_command_no_push_single_platform_loads() {
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
            false, // no push
        )
        .unwrap();

        assert!(!cmd.contains(&"--push".to_string()));
        assert!(cmd.contains(&"--load".to_string()));
    }

    #[test]
    fn test_build_docker_v2_command_no_push_multi_platform_no_load() {
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64", "linux/arm64"],
            &["img:latest".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
            false, // no push
        )
        .unwrap();

        assert!(!cmd.contains(&"--push".to_string()));
        // --load is incompatible with multi-platform
        assert!(!cmd.contains(&"--load".to_string()));
    }

    #[test]
    fn test_build_docker_v2_command_combined() {
        let build_args = vec![("VERSION".to_string(), "1.0.0".to_string())];
        let annotations = vec![(
            "org.opencontainers.image.version".to_string(),
            "1.0.0".to_string(),
        )];
        let labels = vec![("maintainer".to_string(), "dev@example.com".to_string())];
        let flags = vec!["--no-cache".to_string()];

        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64", "linux/arm64"],
            &[
                "ghcr.io/owner/app:latest".to_string(),
                "ghcr.io/owner/app:v1.0.0".to_string(),
            ],
            &build_args,
            &annotations,
            &labels,
            &flags,
            true, // sbom
            true, // push
        )
        .unwrap();

        // Verify all parts are present
        assert!(cmd.contains(&"--platform=linux/amd64,linux/arm64".to_string()));
        assert!(cmd.contains(&"--build-arg".to_string()));
        assert!(cmd.contains(&"VERSION=1.0.0".to_string()));
        assert!(cmd.contains(&"--annotation".to_string()));
        // Multi-platform annotations get "index:" prefix
        assert!(cmd.contains(&"index:org.opencontainers.image.version=1.0.0".to_string()));
        assert!(cmd.contains(&"--label".to_string()));
        assert!(cmd.contains(&"maintainer=dev@example.com".to_string()));
        assert!(cmd.contains(&"--no-cache".to_string()));
        assert!(cmd.contains(&"--attest=type=sbom".to_string()));
        assert!(cmd.contains(&"--push".to_string()));
        assert_eq!(cmd.last().unwrap(), "/tmp/ctx");
    }

    #[test]
    fn test_docker_v2_config_parse_yaml() {
        let yaml = r#"
id: myapp-docker
ids:
  - myapp-build
dockerfile: Dockerfile.prod
images:
  - ghcr.io/owner/app
  - docker.io/owner/app
tags:
  - latest
  - "{{ .Version }}"
labels:
  maintainer: "dev@example.com"
annotations:
  org.opencontainers.image.source: "https://github.com/owner/app"
extra_files:
  - config.yaml
platforms:
  - linux/amd64
  - linux/arm64
build_args:
  APP_VERSION: "{{ .Version }}"
  BUILD_DATE: "2024-01-01"
flags:
  - "--no-cache"
disable: false
sbom: true
retry:
  attempts: 5
  delay: "2s"
"#;
        let cfg: anodize_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();

        assert_eq!(cfg.id, Some("myapp-docker".to_string()));
        assert_eq!(cfg.ids, Some(vec!["myapp-build".to_string()]));
        assert_eq!(cfg.dockerfile, "Dockerfile.prod");
        assert_eq!(cfg.images.len(), 2);
        assert_eq!(cfg.images[0], "ghcr.io/owner/app");
        assert_eq!(cfg.images[1], "docker.io/owner/app");
        assert_eq!(cfg.tags.len(), 2);
        assert_eq!(cfg.tags[0], "latest");
        assert_eq!(cfg.tags[1], "{{ .Version }}");

        let labels = cfg.labels.unwrap();
        assert_eq!(labels.get("maintainer").unwrap(), "dev@example.com");

        let annotations = cfg.annotations.unwrap();
        assert_eq!(
            annotations
                .get("org.opencontainers.image.source")
                .unwrap(),
            "https://github.com/owner/app"
        );

        assert_eq!(cfg.extra_files.unwrap(), vec!["config.yaml"]);

        let platforms = cfg.platforms.unwrap();
        assert_eq!(platforms.len(), 2);

        let build_args = cfg.build_args.unwrap();
        assert_eq!(build_args.get("APP_VERSION").unwrap(), "{{ .Version }}");
        assert_eq!(build_args.get("BUILD_DATE").unwrap(), "2024-01-01");

        assert_eq!(cfg.flags.unwrap(), vec!["--no-cache"]);

        assert_eq!(cfg.disable, Some(StringOrBool::Bool(false)));
        assert_eq!(cfg.sbom, Some(StringOrBool::Bool(true)));

        let retry = cfg.retry.unwrap();
        assert_eq!(retry.attempts, Some(5));
        assert_eq!(retry.delay, Some("2s".to_string()));
    }

    #[test]
    fn test_docker_v2_config_parse_minimal() {
        let yaml = r#"
dockerfile: Dockerfile
images:
  - ghcr.io/owner/app
tags:
  - latest
"#;
        let cfg: anodize_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();

        assert_eq!(cfg.id, None);
        assert_eq!(cfg.ids, None);
        assert_eq!(cfg.dockerfile, "Dockerfile");
        assert_eq!(cfg.images, vec!["ghcr.io/owner/app"]);
        assert_eq!(cfg.tags, vec!["latest"]);
        assert_eq!(cfg.labels, None);
        assert_eq!(cfg.annotations, None);
        assert_eq!(cfg.extra_files, None);
        assert_eq!(cfg.platforms, None);
        assert_eq!(cfg.build_args, None);
        assert_eq!(cfg.flags, None);
        assert_eq!(cfg.disable, None);
        assert_eq!(cfg.sbom, None);
        assert!(cfg.retry.is_none());
    }

    #[test]
    fn test_docker_v2_config_disable_as_bool() {
        let yaml = r#"
dockerfile: Dockerfile
images: ["img"]
tags: ["latest"]
disable: true
"#;
        let cfg: anodize_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.disable, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_docker_v2_config_disable_as_template() {
        let yaml = r#"
dockerfile: Dockerfile
images: ["img"]
tags: ["latest"]
disable: "{{ if .IsSnapshot }}true{{ end }}"
"#;
        let cfg: anodize_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();
        match cfg.disable {
            Some(StringOrBool::String(s)) => {
                assert!(s.contains("IsSnapshot"));
            }
            other => panic!("expected StringOrBool::String, got {:?}", other),
        }
    }

    #[test]
    fn test_docker_v2_config_sbom_as_bool() {
        let yaml = r#"
dockerfile: Dockerfile
images: ["img"]
tags: ["latest"]
sbom: true
"#;
        let cfg: anodize_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.sbom, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_docker_v2_config_sbom_as_string() {
        let yaml = r#"
dockerfile: Dockerfile
images: ["img"]
tags: ["latest"]
sbom: "true"
"#;
        let cfg: anodize_core::config::DockerV2Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.sbom, Some(StringOrBool::String("true".to_string())));
    }

    #[test]
    fn test_docker_v2_dry_run_registers_artifacts() {
        use anodize_core::config::{Config, CrateConfig, DockerV2Config};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\n").unwrap();

        let v2_cfg = DockerV2Config {
            id: Some("myapp-v2".to_string()),
            images: vec!["ghcr.io/owner/myapp".to_string()],
            tags: vec!["{{ .Tag }}".to_string(), "latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker_v2: Some(vec![v2_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        stage.run(&mut ctx).unwrap();

        let images = ctx.artifacts.by_kind(ArtifactKind::DockerImageV2);
        // images x tags = 1 x 2 = 2
        assert_eq!(images.len(), 2);

        let tags: Vec<&str> = images
            .iter()
            .map(|a| a.metadata.get("tag").unwrap().as_str())
            .collect();
        assert!(tags.contains(&"ghcr.io/owner/myapp:v1.0.0"));
        assert!(tags.contains(&"ghcr.io/owner/myapp:latest"));

        // Verify V2 metadata
        for img in &images {
            assert_eq!(img.metadata.get("api").unwrap(), "v2");
            assert_eq!(img.metadata.get("id").unwrap(), "myapp-v2");
        }
    }

    #[test]
    fn test_docker_v2_dry_run_multiple_images_and_tags() {
        use anodize_core::config::{Config, CrateConfig, DockerV2Config};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\n").unwrap();

        let v2_cfg = DockerV2Config {
            images: vec![
                "ghcr.io/owner/app".to_string(),
                "docker.io/owner/app".to_string(),
            ],
            tags: vec![
                "latest".to_string(),
                "{{ .Version }}".to_string(),
                "{{ .Tag }}".to_string(),
            ],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker_v2: Some(vec![v2_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "2.0.0");
        ctx.template_vars_mut().set("Tag", "v2.0.0");

        let stage = DockerStage;
        stage.run(&mut ctx).unwrap();

        // 2 images x 3 tags = 6 artifacts
        let images = ctx.artifacts.by_kind(ArtifactKind::DockerImageV2);
        assert_eq!(images.len(), 6);

        let tags: Vec<&str> = images
            .iter()
            .map(|a| a.metadata.get("tag").unwrap().as_str())
            .collect();
        assert!(tags.contains(&"ghcr.io/owner/app:latest"));
        assert!(tags.contains(&"ghcr.io/owner/app:2.0.0"));
        assert!(tags.contains(&"ghcr.io/owner/app:v2.0.0"));
        assert!(tags.contains(&"docker.io/owner/app:latest"));
        assert!(tags.contains(&"docker.io/owner/app:2.0.0"));
        assert!(tags.contains(&"docker.io/owner/app:v2.0.0"));
    }

    #[test]
    fn test_docker_v2_disable_skips_build() {
        use anodize_core::config::{Config, CrateConfig, DockerV2Config};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\n").unwrap();

        let v2_cfg = DockerV2Config {
            images: vec!["ghcr.io/owner/app".to_string()],
            tags: vec!["latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            disable: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker_v2: Some(vec![v2_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        stage.run(&mut ctx).unwrap();

        // Disabled config should produce no artifacts
        let images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        assert_eq!(images.len(), 0);
    }

    #[test]
    fn test_docker_v2_extra_files_staging_live() {
        use anodize_core::config::{Config, CrateConfig, DockerRetryConfig, DockerV2Config};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Create Dockerfile
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\nCOPY . /\n").unwrap();

        // Create extra files
        let extra1 = tmp.path().join("config.yaml");
        fs::write(&extra1, b"key: value").unwrap();

        let v2_cfg = DockerV2Config {
            images: vec!["ghcr.io/owner/app".to_string()],
            tags: vec!["latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            extra_files: Some(vec![extra1.to_string_lossy().into_owned()]),
            retry: Some(DockerRetryConfig {
                attempts: Some(1),
                delay: None,
                max_delay: None,
            }),
            ..Default::default()
        };

        let dist = tmp.path().join("dist");
        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker_v2: Some(vec![v2_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = dist.clone();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        // Run the stage (will fail at docker command, but staging is complete)
        let _result = DockerStage.run(&mut ctx);

        // Verify staging directory structure
        let staging_dir = dist.join("docker_v2").join("myapp").join("0");
        assert!(staging_dir.join("Dockerfile").exists());
        // Extra file (absolute path) should be in staging root
        assert!(staging_dir.join("config.yaml").exists());
        assert_eq!(
            fs::read_to_string(staging_dir.join("config.yaml")).unwrap(),
            "key: value"
        );
    }

    #[test]
    fn test_docker_v2_crate_config_field() {
        let yaml = r#"
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    docker_v2:
      - dockerfile: Dockerfile
        images:
          - ghcr.io/owner/app
        tags:
          - latest
        build_args:
          VERSION: "1.0.0"
        annotations:
          org.opencontainers.image.source: "https://github.com/owner/app"
        sbom: true
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.crates.len(), 1);
        let v2_configs = config.crates[0].docker_v2.as_ref().unwrap();
        assert_eq!(v2_configs.len(), 1);
        assert_eq!(v2_configs[0].dockerfile, "Dockerfile");
        assert_eq!(v2_configs[0].images, vec!["ghcr.io/owner/app"]);
        assert_eq!(v2_configs[0].tags, vec!["latest"]);

        let build_args = v2_configs[0].build_args.as_ref().unwrap();
        assert_eq!(build_args.get("VERSION").unwrap(), "1.0.0");

        let annotations = v2_configs[0].annotations.as_ref().unwrap();
        assert_eq!(
            annotations
                .get("org.opencontainers.image.source")
                .unwrap(),
            "https://github.com/owner/app"
        );

        assert_eq!(v2_configs[0].sbom, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_is_docker_v2_disabled_none() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(!is_docker_v2_disabled(&None, &ctx));
    }

    #[test]
    fn test_is_docker_v2_disabled_bool_true() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(is_docker_v2_disabled(
            &Some(StringOrBool::Bool(true)),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_disabled_bool_false() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(!is_docker_v2_disabled(
            &Some(StringOrBool::Bool(false)),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_sbom_enabled_none() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(!is_docker_v2_sbom_enabled(&None, &ctx));
    }

    #[test]
    fn test_is_docker_v2_sbom_enabled_bool_true() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(is_docker_v2_sbom_enabled(
            &Some(StringOrBool::Bool(true)),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_sbom_enabled_bool_false() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(!is_docker_v2_sbom_enabled(
            &Some(StringOrBool::Bool(false)),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_disabled_string_true() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(is_docker_v2_disabled(
            &Some(StringOrBool::String("true".to_string())),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_disabled_string_false() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(!is_docker_v2_disabled(
            &Some(StringOrBool::String("false".to_string())),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_disabled_template_snapshot_true() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut().set("IsSnapshot", "true");
        assert!(is_docker_v2_disabled(
            &Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_disabled_template_snapshot_false() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut().set("IsSnapshot", "false");
        assert!(!is_docker_v2_disabled(
            &Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_sbom_enabled_string_true() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(is_docker_v2_sbom_enabled(
            &Some(StringOrBool::String("true".to_string())),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_sbom_enabled_string_false() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        assert!(!is_docker_v2_sbom_enabled(
            &Some(StringOrBool::String("false".to_string())),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_sbom_enabled_template_snapshot_true() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut().set("IsSnapshot", "true");
        assert!(is_docker_v2_sbom_enabled(
            &Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
            &ctx
        ));
    }

    #[test]
    fn test_is_docker_v2_sbom_enabled_template_snapshot_false() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut().set("IsSnapshot", "false");
        assert!(!is_docker_v2_sbom_enabled(
            &Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
            &ctx
        ));
    }

    #[test]
    fn test_docker_v2_build_args_render_in_command() {
        // Verify that build_args end up in the V2 command correctly
        use anodize_core::config::{Config, CrateConfig, DockerV2Config};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\n").unwrap();

        let mut build_args = HashMap::new();
        build_args.insert("VERSION".to_string(), "{{ .Version }}".to_string());
        build_args.insert("STATIC".to_string(), "hello".to_string());

        let mut annotations = HashMap::new();
        annotations.insert(
            "org.opencontainers.image.version".to_string(),
            "{{ .Version }}".to_string(),
        );

        let v2_cfg = DockerV2Config {
            images: vec!["img".to_string()],
            tags: vec!["latest".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            build_args: Some(build_args),
            annotations: Some(annotations),
            sbom: Some(StringOrBool::Bool(true)),
            flags: Some(vec!["--no-cache".to_string()]),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker_v2: Some(vec![v2_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "3.0.0");
        ctx.template_vars_mut().set("Tag", "v3.0.0");

        let stage = DockerStage;
        stage.run(&mut ctx).unwrap();

        // The stage ran in dry-run mode, so it registered artifacts
        let images = ctx.artifacts.by_kind(ArtifactKind::DockerImageV2);
        assert_eq!(images.len(), 1);
        assert_eq!(
            images[0].metadata.get("tag").unwrap(),
            "img:latest"
        );
    }

    #[test]
    fn test_docker_v2_coexists_with_legacy() {
        use anodize_core::config::{Config, CrateConfig, DockerConfig, DockerV2Config};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        fs::write(&dockerfile, b"FROM scratch\n").unwrap();

        let legacy_cfg = DockerConfig {
            image_templates: vec!["ghcr.io/owner/app:legacy".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            ..Default::default()
        };

        let v2_cfg = DockerV2Config {
            images: vec!["ghcr.io/owner/app".to_string()],
            tags: vec!["v2".to_string()],
            dockerfile: dockerfile.to_string_lossy().into_owned(),
            platforms: Some(vec!["linux/amd64".to_string()]),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            docker: Some(vec![legacy_cfg]),
            docker_v2: Some(vec![v2_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");

        let stage = DockerStage;
        stage.run(&mut ctx).unwrap();

        let legacy_images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        let v2_images = ctx.artifacts.by_kind(ArtifactKind::DockerImageV2);
        // 1 from legacy + 1 from v2
        assert_eq!(legacy_images.len(), 1);
        assert_eq!(v2_images.len(), 1);

        let legacy_tag = legacy_images[0].metadata.get("tag").unwrap().as_str();
        assert_eq!(legacy_tag, "ghcr.io/owner/app:legacy");
        let v2_tag = v2_images[0].metadata.get("tag").unwrap().as_str();
        assert_eq!(v2_tag, "ghcr.io/owner/app:v2");
    }

    #[test]
    fn test_templated_extra_files_written_to_staging_dir() {
        use anodize_core::config::TemplatedExtraFile;
        use anodize_core::template::TemplateVars;

        let tmp = TempDir::new().unwrap();
        let staging_dir = tmp.path().join("staging");
        fs::create_dir_all(&staging_dir).unwrap();

        // Create a source template file
        let tpl_src = tmp.path().join("config.yaml.tpl");
        fs::write(
            &tpl_src,
            "app: {{ .ProjectName }}\nversion: {{ .Version }}",
        )
        .unwrap();

        let mut vars = TemplateVars::new();
        vars.set("ProjectName", "myapp");
        vars.set("Version", "1.0.0");

        let specs = vec![TemplatedExtraFile {
            src: tpl_src.to_string_lossy().to_string(),
            dst: Some("config.yaml".to_string()),
            mode: None,
        }];

        let results =
            anodize_core::templated_files::process_templated_extra_files_with_vars(
                &specs, &vars, &staging_dir, "docker",
            )
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "config.yaml");

        // Verify the file was written to the staging directory
        let output_path = staging_dir.join("config.yaml");
        assert!(output_path.exists(), "templated file should exist in staging dir");
        let content = fs::read_to_string(&output_path).unwrap();
        assert_eq!(content, "app: myapp\nversion: 1.0.0");
    }

    // -----------------------------------------------------------------------
    // Session J: New Docker behavioral gap tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_tag_suffix_amd64() {
        assert_eq!(tag_suffix("linux/amd64"), "amd64");
    }

    #[test]
    fn test_tag_suffix_arm64() {
        assert_eq!(tag_suffix("linux/arm64"), "arm64");
    }

    #[test]
    fn test_tag_suffix_arm_v7() {
        assert_eq!(tag_suffix("linux/arm/v7"), "armv7");
    }

    #[test]
    fn test_sbom_uses_attest_format() {
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest".to_string()],
            &[],
            &[],
            &[],
            &[],
            true,
            false,
        )
        .unwrap();
        assert!(
            cmd.contains(&"--attest=type=sbom".to_string()),
            "SBOM should use --attest=type=sbom, not --sbom=true"
        );
        assert!(
            !cmd.contains(&"--sbom=true".to_string()),
            "should not contain old --sbom=true flag"
        );
    }

    #[test]
    fn test_annotations_no_prefix_single_platform() {
        let annotations = vec![("foo".to_string(), "bar".to_string())];
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64"],
            &["img:latest".to_string()],
            &[],
            &annotations,
            &[],
            &[],
            false,
            false,
        )
        .unwrap();
        assert!(
            cmd.contains(&"foo=bar".to_string()),
            "single-platform annotations should NOT get index: prefix"
        );
    }

    #[test]
    fn test_annotations_get_index_prefix_multi_platform() {
        let annotations = vec![("foo".to_string(), "bar".to_string())];
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64", "linux/arm64"],
            &["img:latest".to_string()],
            &[],
            &annotations,
            &[],
            &[],
            false,
            true,
        )
        .unwrap();
        assert!(
            cmd.contains(&"index:foo=bar".to_string()),
            "multi-platform annotations should get index: prefix"
        );
    }

    #[test]
    fn test_annotations_no_double_index_prefix() {
        let annotations = vec![("index:foo".to_string(), "bar".to_string())];
        let cmd = build_docker_v2_command(
            "/tmp/ctx",
            &["linux/amd64", "linux/arm64"],
            &["img:latest".to_string()],
            &[],
            &annotations,
            &[],
            &[],
            false,
            true,
        )
        .unwrap();
        assert!(
            cmd.contains(&"index:foo=bar".to_string()),
            "already-prefixed annotations should not get double prefix"
        );
        assert!(
            !cmd.contains(&"index:index:foo=bar".to_string()),
            "must not double-prefix"
        );
    }

    #[test]
    fn test_docker_sign_config_output_bool() {
        use anodize_core::config::DockerSignConfig;
        let yaml = r#"
cmd: cosign
output: true
"#;
        let cfg: DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(cfg.output.unwrap().as_bool());
    }

    #[test]
    fn test_docker_sign_config_output_string() {
        use anodize_core::config::DockerSignConfig;
        let yaml = r#"
cmd: cosign
output: "false"
"#;
        let cfg: DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(!cfg.output.unwrap().as_bool());
    }

    #[test]
    fn test_docker_sign_config_output_missing() {
        use anodize_core::config::DockerSignConfig;
        let yaml = r#"
cmd: cosign
"#;
        let cfg: DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(cfg.output.is_none());
    }
}
