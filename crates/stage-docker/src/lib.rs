use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{DockerRetryConfig, SkipPushConfig};
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anodize_core::target::map_target;

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
/// - max_delay defaults to None (no cap)
pub fn resolve_retry_params(
    retry: &Option<DockerRetryConfig>,
) -> Result<(u32, Duration, Option<Duration>)> {
    match retry {
        None => Ok((10, Duration::from_secs(10), None)),
        Some(cfg) => {
            let attempts = cfg.attempts.unwrap_or(10);
            let base_delay = match &cfg.delay {
                Some(d) => parse_duration_string(d)?,
                None => Duration::from_secs(10),
            };
            let max_delay = match &cfg.max_delay {
                Some(d) => Some(parse_duration_string(d)?),
                None => None,
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
pub fn resolve_backend(use_backend: Option<&str>, multi_platform: bool) -> Result<(&str, Vec<&str>)> {
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

    // --push in live mode (unless skip_push); omit both --push and --load in
    // dry-run (--load is incompatible with multi-platform builds)
    if push {
        cmd.push("--push".to_string());
        // Additional push flags
        for flag in push_flags {
            cmd.push(flag.clone());
        }
    }

    // Build context directory (positional, last argument)
    cmd.push(staging_dir.to_string());

    Ok(cmd)
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
        None => false,
    }
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

        // Collect crates that have docker or docker_manifests config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.docker.is_some() || c.docker_manifests.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        let mut new_artifacts: Vec<Artifact> = Vec::new();

        for krate in &crates {
            let docker_configs = match krate.docker.as_ref() {
                Some(cfgs) => cfgs.clone(),
                None => Vec::new(),
            };

            for (idx, docker_cfg) in docker_configs.iter().enumerate() {
                // Determine platforms (default: linux/amd64 + linux/arm64)
                let platforms: Vec<String> = docker_cfg
                    .platforms
                    .clone()
                    .unwrap_or_else(|| vec!["linux/amd64".to_string(), "linux/arm64".to_string()]);

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
                // Stage binaries per platform/arch
                // ------------------------------------------------------------------
                for platform in &platforms {
                    let arch = platform_to_arch(platform);

                    let binaries_dir = staging_dir.join("binaries").join(arch);
                    if !dry_run {
                        fs::create_dir_all(&binaries_dir).with_context(|| {
                            format!("docker: create binaries dir {}", binaries_dir.display())
                        })?;
                    }

                    // Determine which binary names this docker config cares about
                    let binary_filter = docker_cfg.binaries.as_ref();
                    let ids_filter = docker_cfg.ids.as_ref();

                    // Find Binary artifacts whose target maps to this arch
                    let matching_binaries: Vec<_> = ctx
                        .artifacts
                        .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                        .into_iter()
                        .filter(|b| {
                            // Check the arch of the artifact's target triple matches
                            let artifact_arch = b
                                .target
                                .as_deref()
                                .map(|t| map_target(t).1)
                                .unwrap_or_default();
                            if artifact_arch != arch {
                                return false;
                            }
                            // Apply optional IDs filter
                            if let Some(ids) = ids_filter {
                                let artifact_id =
                                    b.metadata.get("id").map(|s| s.as_str()).unwrap_or("");
                                if !ids.iter().any(|id| id == artifact_id) {
                                    return false;
                                }
                            }
                            // Apply optional binary name filter
                            match binary_filter {
                                None => true,
                                Some(names) => {
                                    let bin_name =
                                        b.metadata.get("binary").map(|s| s.as_str()).unwrap_or("");
                                    names.iter().any(|n| n == bin_name)
                                }
                            }
                        })
                        .collect();

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
                                "(dry-run) would copy {} → {}",
                                bin_artifact.path.display(),
                                dest.display()
                            ));
                        } else {
                            log.status(&format!(
                                "staging binary {} → {}",
                                bin_artifact.path.display(),
                                dest.display()
                            ));
                            fs::copy(&bin_artifact.path, &dest).with_context(|| {
                                format!(
                                    "docker: copy binary {} to {}",
                                    bin_artifact.path.display(),
                                    dest.display()
                                )
                            })?;
                        }
                    }
                }

                // ------------------------------------------------------------------
                // Copy Dockerfile
                // ------------------------------------------------------------------
                let dockerfile_src = PathBuf::from(&docker_cfg.dockerfile);
                let dockerfile_dest = staging_dir.join("Dockerfile");

                if dry_run {
                    log.status(&format!(
                        "(dry-run) would copy Dockerfile {} → {}",
                        dockerfile_src.display(),
                        dockerfile_dest.display()
                    ));
                } else {
                    log.status(&format!(
                        "copying Dockerfile {} → {}",
                        dockerfile_src.display(),
                        dockerfile_dest.display()
                    ));
                    fs::copy(&dockerfile_src, &dockerfile_dest).with_context(|| {
                        format!(
                            "docker: copy Dockerfile from {} to {}",
                            dockerfile_src.display(),
                            dockerfile_dest.display()
                        )
                    })?;
                }

                // ------------------------------------------------------------------
                // Copy extra_files into staging directory
                // ------------------------------------------------------------------
                if let Some(ref extra_files) = docker_cfg.extra_files {
                    for file_path in extra_files {
                        let src = PathBuf::from(file_path);
                        if src.is_dir() {
                            anyhow::bail!(
                                "docker: extra_files entry '{}' is a directory; only files are supported",
                                file_path
                            );
                        }
                        // Preserve relative directory structure instead of
                        // flattening to just the filename.  For absolute paths,
                        // fall back to just the filename (no relative structure
                        // to preserve).
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
                                "(dry-run) would copy extra file {} → {}",
                                src.display(),
                                dest.display()
                            ));
                        } else {
                            // Ensure parent directories exist
                            if let Some(parent) = dest.parent() {
                                fs::create_dir_all(parent).with_context(|| {
                                    format!(
                                        "docker: create parent dirs for extra file {}",
                                        dest.display()
                                    )
                                })?;
                            }
                            log.status(&format!(
                                "copying extra file {} → {}",
                                src.display(),
                                dest.display()
                            ));
                            fs::copy(&src, &dest).with_context(|| {
                                format!(
                                    "docker: copy extra file {} to {}",
                                    src.display(),
                                    dest.display()
                                )
                            })?;
                        }
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

                // ------------------------------------------------------------------
                // Build and run the docker buildx command
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
                let (max_attempts, base_delay, max_delay) =
                    resolve_retry_params(&docker_cfg.retry).with_context(|| {
                        format!(
                            "docker: invalid retry config for crate {} index {}",
                            krate.name, idx
                        )
                    })?;

                let (backend_binary, backend_subcmds) =
                    resolve_backend(docker_cfg.use_backend.as_deref(), platforms.len() > 1)?;
                let backend_label = if backend_subcmds.contains(&"buildx") {
                    "buildx"
                } else {
                    backend_binary
                };

                let mut tag_digests: HashMap<String, String> = HashMap::new();

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
                } else {
                    log.status(&format!("running: {}", cmd_args.join(" ")));

                    let mut last_err: Option<anyhow::Error> = None;
                    for attempt in 1..=max_attempts {
                        if attempt > 1 {
                            // Calculate exponential backoff delay:
                            // delay * 2^(attempt-2), capped at max_delay
                            let multiplier = 2u64.saturating_pow(attempt - 2);
                            let delay_ms = base_delay
                                .as_millis()
                                .saturating_mul(multiplier as u128);
                            let mut delay = Duration::from_millis(delay_ms as u64);
                            if let Some(cap) = max_delay
                                && delay > cap
                            {
                                delay = cap;
                            }
                            log.warn(&format!(
                                "attempt {}/{} failed, retrying in {:?}…",
                                attempt - 1,
                                max_attempts,
                                delay,
                            ));
                            std::thread::sleep(delay);
                        }

                        let output = Command::new(&cmd_args[0])
                            .args(&cmd_args[1..])
                            .output()
                            .with_context(|| {
                                format!(
                                    "docker: execute {} for crate {} index {} (attempt {}/{})",
                                    backend_label, krate.name, idx, attempt, max_attempts
                                )
                            })?;

                        match log.check_output(output, &format!("docker {}", backend_label)) {
                            Ok(_) => {
                                if attempt > 1 {
                                    log.status(&format!(
                                        "docker {} succeeded on attempt {}/{}",
                                        backend_label, attempt, max_attempts
                                    ));
                                }
                                last_err = None;
                                break;
                            }
                            Err(e) => {
                                last_err = Some(e);
                            }
                        }
                    }

                    if let Some(e) = last_err {
                        return Err(e).with_context(|| {
                            format!(
                                "docker: all {} attempts failed for crate {} index {}",
                                max_attempts, krate.name, idx
                            )
                        });
                    }

                    // --------------------------------------------------------------
                    // Capture docker digest files after successful push
                    // --------------------------------------------------------------
                    if should_push {
                        for tag in &rendered_tags {
                            let inspect_bin = if backend_label == "podman" {
                                "podman"
                            } else {
                                "docker"
                            };
                            let digest_output = Command::new(inspect_bin)
                                .args([
                                    "inspect",
                                    "--format",
                                    "{{index .RepoDigests 0}}",
                                    tag,
                                ])
                                .output();

                            if let Ok(output) = digest_output
                                && output.status.success()
                            {
                                let digest =
                                    String::from_utf8_lossy(&output.stdout).trim().to_string();
                                if !digest.is_empty() {
                                    tag_digests.insert(tag.clone(), digest.clone());
                                    // Sanitize image name for filename
                                    let safe_name =
                                        tag.replace(['/', ':'], "_");
                                    let digest_file =
                                        dist.join(format!("{}.digest", safe_name));
                                    if let Err(e) = fs::write(&digest_file, &digest) {
                                        log.warn(&format!(
                                            "failed to write digest file {}: {}",
                                            digest_file.display(),
                                            e
                                        ));
                                    } else {
                                        log.status(&format!(
                                            "saved digest to {}",
                                            digest_file.display()
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }

                // ------------------------------------------------------------------
                // Register DockerImage artifacts
                // ------------------------------------------------------------------
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
                    if let Some(d) = tag_digests.get(tag) {
                        meta.insert("digest".to_string(), d.clone());
                    }

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::DockerImage,
                        path: staging_dir.clone(),
                        target: None,
                        crate_name: krate.name.clone(),
                        metadata: meta,
                    });
                }
            }

            // ------------------------------------------------------------------
            // Docker manifests
            // ------------------------------------------------------------------
            if let Some(ref manifest_configs) = krate.docker_manifests {
                for (midx, manifest_cfg) in manifest_configs.iter().enumerate() {
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
                    let manifest_name =
                        ctx.render_template(&manifest_cfg.name_template).with_context(|| {
                            format!(
                                "docker: render manifest name_template '{}' for crate {}",
                                manifest_cfg.name_template, krate.name
                            )
                        })?;

                    // Render image templates
                    let mut rendered_images: Vec<String> = Vec::new();
                    for tmpl in &manifest_cfg.image_templates {
                        let img = ctx.render_template(tmpl).with_context(|| {
                            format!(
                                "docker: render manifest image_template '{}' for crate {}",
                                tmpl, krate.name
                            )
                        })?;
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

                    // Build `docker manifest create` command
                    let mut create_cmd: Vec<String> = vec![
                        manifest_bin.to_string(),
                        "manifest".to_string(),
                        "create".to_string(),
                        manifest_name.clone(),
                    ];
                    for img in &rendered_images {
                        create_cmd.push(img.clone());
                    }
                    for flag in &rendered_create_flags {
                        create_cmd.push(flag.clone());
                    }

                    // Determine whether to push
                    let manifest_skip_push = resolve_skip_push(&manifest_cfg.skip_push, ctx);

                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would run: {} manifest rm {}",
                            manifest_bin, manifest_name
                        ));
                        log.status(&format!(
                            "(dry-run) would run: {}",
                            create_cmd.join(" ")
                        ));
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
                            log.status(&format!(
                                "(dry-run) would run: {}",
                                push_cmd.join(" ")
                            ));
                        }
                    } else {
                        // Remove any existing manifest to prevent stale manifest
                        // failures on re-runs (GoReleaser does this too).
                        let _ = Command::new(manifest_bin)
                            .args(["manifest", "rm", &manifest_name])
                            .output();

                        log.status(&format!("running: {}", create_cmd.join(" ")));
                        let output = Command::new(&create_cmd[0])
                            .args(&create_cmd[1..])
                            .output()
                            .with_context(|| {
                                format!(
                                    "docker: manifest create for crate {} manifest {}",
                                    krate.name, midx
                                )
                            })?;
                        log.check_output(output, "docker manifest create")?;

                        // Push the manifest
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

                            log.status(&format!("running: {}", push_cmd.join(" ")));
                            let output = Command::new(&push_cmd[0])
                                .args(&push_cmd[1..])
                                .output()
                                .with_context(|| {
                                    format!(
                                        "docker: manifest push for crate {} manifest {}",
                                        krate.name, midx
                                    )
                                })?;
                            log.check_output(output, "docker manifest push")?;
                        }
                    }

                    // Register DockerManifest artifact
                    let mut meta = HashMap::new();
                    meta.insert("manifest".to_string(), manifest_name.clone());
                    meta.insert(
                        "images".to_string(),
                        rendered_images.join(","),
                    );
                    if let Some(ref id) = manifest_cfg.id {
                        meta.insert("id".to_string(), id.clone());
                    }

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::DockerManifest,
                        path: dist.clone(),
                        target: None,
                        crate_name: krate.name.clone(),
                        metadata: meta,
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
        ).unwrap();
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
        ).unwrap();
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
        ).unwrap();
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
        ).unwrap();
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
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
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
            path: amd64_bin.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta_amd64,
        });

        let mut meta_arm64 = HashMap::new();
        meta_arm64.insert("binary".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: arm64_bin.clone(),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta_arm64,
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
        ).unwrap();
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
        ).unwrap();
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
        ).unwrap();
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
        ).unwrap();
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
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
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
        ).unwrap();
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
        ).unwrap();
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
        ).unwrap();
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
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
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
            path: amd64_bin,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta_amd64,
        });

        let mut meta_arm64 = HashMap::new();
        meta_arm64.insert("binary".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path: arm64_bin,
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta_arm64,
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
        ).unwrap();
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
        ).unwrap();
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
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
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
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
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
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: None,
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
        ).unwrap();
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
    fn test_parse_duration_string_invalid_suffix() {
        assert!(parse_duration_string("5h").is_err());
        assert!(parse_duration_string("100").is_err());
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
        assert!(max_delay.is_none());
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
        assert!(max_delay.is_none());
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
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None, // No retry config = default 10 attempts
            use_backend: None,
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
        ).unwrap();
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
        ).unwrap();
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
        ).unwrap();
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
        assert_eq!(
            manifests[0].metadata.get("id").unwrap(),
            "multi-arch"
        );
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
                    image_templates: vec![
                        "ghcr.io/owner/app:{{ .Version }}-amd64".to_string(),
                    ],
                    create_flags: None,
                    push_flags: None,
                    skip_push: Some(SkipPushConfig::Auto),
                    id: None,
                    use_backend: None,
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
            push_flags: None,
            id: None,
            ids: None,
            labels: None,
            retry: None,
            use_backend: Some("podman".to_string()),
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
        assert_eq!(
            images[0].metadata.get("use").unwrap(),
            "podman"
        );
    }
}
