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
///
/// The `"podman"` backend is **Linux-only**, matching GoReleaser Pro's
/// `podman` pipe restriction. Configs that set `use: podman` on macOS or
/// Windows are rejected here with a clear error so users do not get a
/// confusing `podman: command not found` later in the pipeline.
pub fn resolve_backend(
    use_backend: Option<&str>,
    multi_platform: bool,
) -> Result<(&str, Vec<&str>)> {
    // `multi_platform` is consumed by callers for the podman tag-emission
    // branch (multi-platform podman writes a local manifest list via
    // `--manifest <name>`, single-platform podman uses `--tag <name>`); the
    // subcommand vector itself is identical for both arities. Reading it here
    // keeps the parameter load-bearing rather than silently ignored, and a
    // future backend whose *subcommand* depends on arity has the value ready.
    let _ = multi_platform;
    match use_backend {
        Some("docker") => Ok(("docker", vec!["build"])),
        Some("podman") => {
            enforce_podman_linux_only()?;
            Ok(("podman", vec!["build"]))
        }
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

/// Return an error when the current host OS is not Linux. Used to gate the
/// podman backend (both build and manifest paths) to Linux hosts only.
///
/// GoReleaser Pro's docs are explicit: "The Podman backend is exclusively a
/// GoReleaser Pro feature restricted to Linux environments." A macOS or
/// Windows user who sets `use: podman` would otherwise get a confusing
/// `podman: command not found` at build time; failing at config validation
/// produces an actionable error pointing back at the relevant field.
pub fn enforce_podman_linux_only() -> Result<()> {
    let os = std::env::consts::OS;
    if os != "linux" {
        anyhow::bail!(
            "podman backend is supported on Linux only (host OS: {}); \
             remove `use: podman` or run on a Linux host",
            os,
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Buildx-only flag validation (for podman backend)
// ---------------------------------------------------------------------------

/// Flags that ONLY make sense for `docker buildx build` and that plain
/// `podman build` does not recognise. Configs that opt into the podman
/// backend via `use: podman` are validated against this list so a typo
/// or copy-paste from a buildx config fails fast at config-load time
/// rather than at podman's argv parser with a generic "unknown flag".
///
/// The list is conservative: only flags whose presence under podman is a
/// load-bearing UX bug (`--rewrite-timestamp` for byte-stable layers,
/// `--sbom` / `--provenance` / `--attest` for attestation, `--output` for
/// the OCI exporter, `--cache-from` / `--cache-to` for BuildKit cache).
/// Bare `--build-arg`, `--label`, `--platform`, `--tag` are buildx-aligned
/// but accepted by plain podman too and intentionally NOT in this list.
pub const BUILDX_ONLY_FLAGS: &[&str] = &[
    "--rewrite-timestamp",
    "--sbom",
    "--provenance",
    "--attest",
    "--output",
    "--cache-from",
    "--cache-to",
];

/// Validate that a rendered flag list does not contain any buildx-only
/// flags when the configured backend is `podman`. Returns the offending
/// flag prefix in the error message so users can pinpoint the violation.
///
/// Accepts both bare-flag (`--sbom`) and key=value (`--sbom=true`) forms.
pub fn validate_podman_flag_compat(flags: &[String]) -> Result<()> {
    for flag in flags {
        // Normalise: strip any `=value` suffix so we match the flag prefix.
        let head = flag
            .split_once('=')
            .map(|(k, _)| k)
            .unwrap_or(flag.as_str());
        if BUILDX_ONLY_FLAGS.contains(&head) {
            anyhow::bail!(
                "docker_v2 with `use: podman` is incompatible with buildx-only flag '{}'; \
                 remove the flag or switch to `use: buildx`",
                flag,
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// resolve_manifester
// ---------------------------------------------------------------------------

/// Resolve the binary used for `docker manifest …` (or its podman cousin).
///
/// F7: GoReleaser's `validateManifester`
/// (`internal/pipe/docker/manifest.go:169-174`) errors at default-time when
/// the configured `manifest.use:` is not in the registered manifester set.
/// Previously anodizer silently fell back to `"docker"` for any unknown
/// value (including typos like `use: dockr`), masking bugs. We now enumerate
/// the supported set explicitly and surface a clear error for anything else.
///
/// Anodizer accepts `"podman"` in addition to GR's `"docker"`-only set
/// because podman ships a `podman manifest create/push` CLI that mirrors
/// `docker manifest`; that surface is already wired through the rest of
/// this module. `"buildx"` is NOT a valid manifester (buildx pushes manifest
/// lists as a side-effect of `--push`, it does not have a `buildx manifest`
/// subcommand) and rejecting it explicitly catches a common copy-paste bug
/// where users wire `use: buildx` from their build config into a manifest
/// stanza.
pub fn resolve_manifester(use_backend: Option<&str>) -> Result<&'static str> {
    match use_backend.unwrap_or("docker") {
        "docker" => Ok("docker"),
        "podman" => {
            // Mirror the build-path gate: podman is Linux-only.
            enforce_podman_linux_only()?;
            Ok("podman")
        }
        other => {
            anyhow::bail!(
                "docker manifest: invalid use '{}', valid options are: [docker, podman]",
                other,
            );
        }
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

/// Append `--manifest <name>` pairs for every entry.
///
/// Multi-platform `podman build` requires `--manifest <name>` (NOT `--tag`):
/// per the podman-build docs, when more than one `--platform` is given podman
/// only assembles a local manifest list when the target is named via
/// `--manifest`. A multi-platform `--tag` build does not produce a manifest
/// list, so a later `podman manifest push` / `podman push` of that name would
/// publish nothing valid (or a single-arch image).
fn push_manifest_targets<S: AsRef<str>>(cmd: &mut Vec<String>, names: &[S]) {
    for name in names {
        cmd.push("--manifest".to_string());
        cmd.push(name.as_ref().to_string());
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
    /// When true, adds `--attest=type=sbom`. Buildx-only — rejected at
    /// config-validation time when `backend == Some("podman")`.
    pub sbom: bool,
    /// When `true`, adds `--push` to the command.
    pub push: bool,
    /// When `true`, adds `--load` for single-platform non-push builds
    /// (requires a running Docker daemon).
    pub load: bool,
    /// Backend selector forwarded from `DockerV2Config.use`. `None` and
    /// `Some("buildx")` use `docker buildx build`; `Some("podman")` swaps
    /// to `podman build` (Linux-only) and forbids buildx-only flags. Any
    /// other value is rejected by [`resolve_backend`].
    pub backend: Option<&'a str>,
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
        backend,
    } = *spec;

    let multi_platform = platforms.len() > 1;
    // Backend selector: default to buildx (V2's historical behaviour).
    // `Some("podman")` swaps the binary to `podman build` and is gated
    // by `resolve_backend` (Linux-only) plus the buildx-only flag guards
    // applied immediately below.
    let resolved_backend = backend.unwrap_or("buildx");
    let is_podman = resolved_backend == "podman";
    if is_podman {
        // Guard rails: even if the caller forgot to validate, the build
        // command must not emit buildx-only flags under podman.
        validate_podman_flag_compat(flags)?;
        if sbom {
            anyhow::bail!(
                "docker_v2 with `use: podman` cannot enable `sbom: true` \
                 (buildx-only); set `sbom: false` or switch to `use: buildx`",
            );
        }
    }
    let (binary, subcommands) = resolve_backend(Some(resolved_backend), multi_platform)?;

    let mut cmd: Vec<String> = Vec::new();
    push_backend_prefix(&mut cmd, binary, &subcommands);
    // Always use plain progress for CI-friendly, verbose output that helps
    // diagnose buildx errors (e.g., COPY failures, attestation issues).
    cmd.push("--progress=plain".to_string());
    push_platforms(&mut cmd, platforms);
    // Multi-platform podman names the build target with `--manifest <name>` so
    // podman assembles a local manifest list; every other path (single-platform
    // podman, and buildx for any arity) names it with `--tag <name>`. See
    // `push_manifest_targets` for the podman-build constraint.
    if is_podman && multi_platform {
        push_manifest_targets(&mut cmd, image_tags);
    } else {
        push_tags(&mut cmd, image_tags);
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

    push_labels(&mut cmd, labels);

    // Arbitrary extra flags
    for flag in flags {
        cmd.push(flag.clone());
    }

    // Use --attest=type=sbom for proper OCI attestation (matching GoReleaser v2)
    // rather than the older --sbom=true flag. Buildx-only — already gated by
    // the `is_podman` check above (sbom forbidden under podman).
    if sbom && !is_podman {
        cmd.push("--attest=type=sbom".to_string());
    }

    // --push / --load logic. Both are buildx-only flags: `podman build` does
    // not accept either. buildx bakes `--push` directly into the build so the
    // image is published as a side-effect of the build. podman build can only
    // write the image (or, multi-platform, a local manifest list) into local
    // storage, so for the podman backend the build command never carries
    // `--push`; publication is performed afterwards by an explicit push per
    // rendered tag (`podman push` single-platform, `podman manifest push --all`
    // multi-platform — see [`build_podman_push_commands`] and the push loop in
    // `execute_docker_build`). That separate-push step is the reason the podman
    // backend must not silently rely on a baked-in `--push`.
    if !is_podman {
        if push {
            cmd.push("--push".to_string());
        } else if load && !multi_platform {
            cmd.push("--load".to_string());
        }
        // When neither push nor load: buildx builds to cache only (no daemon needed)
    }

    // NOTE: GoReleaser V2 does NOT auto-add --provenance=false or --sbom=false.
    // Only the legacy docker pipe does that. V2 relies on explicit user flags
    // or the --attest=type=sbom flag set above.

    // Write image digest to file for capture (GoReleaser V2 behavior).
    // This works even without --push (no daemon needed for digest capture).
    // buildx and single-platform podman support `--iidfile` (same flag name).
    // Multi-platform `podman build` rejects `--iidfile` (it errors when
    // `--platform` is given more than once), so it is suppressed there; the
    // downstream digest capture reads the iidfile only when present and
    // degrades gracefully when it is absent.
    if !(is_podman && multi_platform) {
        cmd.push(format!("--iidfile={}/id.txt", staging_dir));
    }

    // Build context directory (positional, last argument)
    cmd.push(staging_dir.to_string());

    Ok(cmd)
}

// ---------------------------------------------------------------------------
// build_podman_push_commands
// ---------------------------------------------------------------------------

/// Construct the publish argv for every rendered tag of a podman build.
///
/// `podman build` cannot bake `--push` into the build the way `docker buildx
/// build --push` does — it only writes the image (single-platform) or a local
/// manifest list (multi-platform, named via `--manifest`) into podman's local
/// storage. Publication therefore requires an explicit push after the build
/// succeeds. This helper produces that argv so the spawn site in
/// `execute_docker_build` stays a thin executor and the command shape is
/// unit-testable without invoking podman.
///
/// The publish verb depends on the build's platform arity:
/// - **single-platform** → `podman push <tag>` (pushes the lone image).
/// - **multi-platform** → `podman manifest push --all <tag>`. The build named
///   the local manifest list with `--manifest <tag>`; `manifest push` is the
///   command that publishes a manifest list, and `--all` is required so the
///   list's per-arch image contents are pushed alongside the list (without it
///   podman pushes only the list descriptor, leaving the per-arch blobs
///   missing from the registry). Pushing the contents is also what the
///   separate `docker_manifests` feature relies on being in the registry
///   before its own `manifest create`/`push` resolves per-arch tags.
pub fn build_podman_push_commands(tags: &[String], multi_platform: bool) -> Vec<Vec<String>> {
    tags.iter()
        .map(|tag| {
            if multi_platform {
                vec![
                    "podman".to_string(),
                    "manifest".to_string(),
                    "push".to_string(),
                    "--all".to_string(),
                    tag.clone(),
                ]
            } else {
                vec!["podman".to_string(), "push".to_string(), tag.clone()]
            }
        })
        .collect()
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
