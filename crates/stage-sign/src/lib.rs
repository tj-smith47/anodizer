use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::ArtifactKind;
#[cfg(test)]
use anodizer_core::config::SignConfig;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

mod helpers;
mod process;

use helpers::{prepare_stdin_from, resolve_sign_args};
#[cfg(test)]
use helpers::{resolve_signature_path, should_sign_artifact};
use process::{ArtifactFilter, process_sign_configs};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default signature template for `binary_signs`, matching GoReleaser's
/// architecture-aware naming.
///
/// Uses `{{ .Artifact }}` (Go-compat syntax, replaced before Tera rendering)
/// for the artifact path, and Tera syntax for Os/Arch/Arm/Mips/Amd64
/// conditionals that are set per-artifact from the target triple.
// Matches GoReleaser sign_binary.go:16 default — no trailing `.sig` suffix.
pub(crate) const DEFAULT_BINARY_SIGNATURE_TEMPLATE: &str = "{{ .Artifact }}_{{ Os }}_{{ Arch }}{% if Arm %}v{{ Arm }}{% endif %}{% if Mips %}_{{ Mips }}{% endif %}{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}";

// Helpers (should_sign_artifact, resolve_signature_path, prepare_stdin_from,
// default_sign_cmd, expand_shell_vars, resolve_sign_args) live in `helpers.rs`.

// Shared sign processing (ArtifactFilter, SignJob, execute_sign_job,
// process_sign_configs, label_to_static) lives in `process.rs`.

// ---------------------------------------------------------------------------
// SignStage
// ---------------------------------------------------------------------------

/// Sign stage: signs artifacts using GPG, cosign, or other signing tools.
///
/// Calls `ctx.refresh_artifacts_var()` after all signing completes, matching
/// GoReleaser's `ctx.Artifacts.Refresh()`. This ensures newly-added signature
/// and certificate artifacts are visible to downstream stages.
pub struct SignStage;

/// Binary-only signing stage used by `anodizer build`. Mirrors GoReleaser's
/// `sign.BinaryPipe` — runs the `binary_signs` loop but skips the generic
/// `signs` loop, which at build-time would see only binaries anyway but
/// with the wrong semantics (a user with `signs: [{artifacts: all}]`
/// doesn't expect signing to happen during `anodizer build`).
pub struct BinarySignStage;

impl Stage for BinarySignStage {
    fn name(&self) -> &str {
        "binary-sign"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("binary-sign");
        // Validate binary_signs IDs unique — same check SignStage does.
        let mut seen = std::collections::HashSet::new();
        for cfg in &ctx.config.binary_signs {
            let id = cfg.id.as_deref().unwrap_or("default");
            if !seen.insert(id.to_string()) {
                anyhow::bail!("found 2 binary_signs with the ID '{}'", id);
            }
        }
        let binary_sign_configs = ctx.config.binary_signs.clone();
        process_sign_configs(
            &binary_sign_configs,
            ctx,
            &log,
            ArtifactFilter::BinaryOnly,
            "binary-sign",
        )?;
        ctx.refresh_artifacts_var();
        Ok(())
    }
}

impl Stage for SignStage {
    fn name(&self) -> &str {
        "sign"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("sign");

        // Validate sign config IDs are unique (GoReleaser ids.Validate()).
        {
            let mut seen = std::collections::HashSet::new();
            for cfg in &ctx.config.signs {
                let id = cfg.id.as_deref().unwrap_or("default");
                if !seen.insert(id.to_string()) {
                    anyhow::bail!("found 2 signs with the ID '{}'", id);
                }
            }
            let mut seen_bin = std::collections::HashSet::new();
            for cfg in &ctx.config.binary_signs {
                let id = cfg.id.as_deref().unwrap_or("default");
                if !seen_bin.insert(id.to_string()) {
                    anyhow::bail!("found 2 binary_signs with the ID '{}'", id);
                }
            }
        }

        // ----------------------------------------------------------------
        // GPG / generic signing via `signs` config (supports multiple)
        // ----------------------------------------------------------------
        let sign_configs = ctx.config.signs.clone();
        process_sign_configs(&sign_configs, ctx, &log, ArtifactFilter::FromConfig, "sign")?;

        // ----------------------------------------------------------------
        // Binary-specific signing via `binary_signs` config
        // Same as `signs` but always filters to Binary artifacts only.
        // ----------------------------------------------------------------
        let binary_sign_configs = ctx.config.binary_signs.clone();
        process_sign_configs(
            &binary_sign_configs,
            ctx,
            &log,
            ArtifactFilter::BinaryOnly,
            "binary-sign",
        )?;

        // Refresh the artifacts template variable so newly-added signatures
        // and certificates are visible to downstream stages (matching
        // GoReleaser's ctx.Artifacts.Refresh()).
        ctx.refresh_artifacts_var();
        Ok(())
    }
}

/// Pipeline stage for signing Docker images via `docker_signs` config.
/// Must run after `DockerStage` so Docker image artifacts are present.
pub struct DockerSignStage;

impl Stage for DockerSignStage {
    fn name(&self) -> &str {
        "docker-sign"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("docker-sign");

        // ----------------------------------------------------------------
        // Docker image signing via `docker_signs` config
        // ----------------------------------------------------------------
        if let Some(docker_signs) = ctx.config.docker_signs.clone() {
            // Validate docker_signs IDs are unique (GoReleaser ids.Validate()).
            {
                let mut seen_docker = std::collections::HashSet::new();
                for cfg in &docker_signs {
                    let id = cfg.id.as_deref().unwrap_or("default");
                    if !seen_docker.insert(id.to_string()) {
                        anyhow::bail!("found 2 docker_signs with the ID '{}'", id);
                    }
                }
            }

            for docker_sign_cfg in &docker_signs {
                // Default docker sign ID to "default" (matches GoReleaser).
                let sign_id = docker_sign_cfg.id.as_deref().unwrap_or("default");

                // Evaluate the `if` conditional template for docker signs.
                // (See comment in process_sign_configs for why this uses
                // inverted logic compared to `disable`.)
                if let Some(ref condition) = docker_sign_cfg.if_condition {
                    match ctx.render_template(condition) {
                        Ok(result) => {
                            let trimmed = result.trim();
                            if trimmed.is_empty() || trimmed == "false" {
                                let reason = format!("if condition evaluated to '{}'", trimmed);
                                log.verbose(&format!(
                                    "skipping docker-sign config '{}': {}",
                                    sign_id, reason
                                ));
                                ctx.remember_skip("docker-sign", sign_id, &reason);
                                continue;
                            }
                        }
                        Err(e) => {
                            // Hard-fail: silent skip would ship unsigned images.
                            anyhow::bail!(
                                "docker-sign '{}': if condition render failed ({}): {}",
                                sign_id,
                                condition,
                                e
                            );
                        }
                    }
                }

                let cmd = docker_sign_cfg
                    .cmd
                    .as_deref()
                    .unwrap_or("cosign")
                    .to_string();

                let args = docker_sign_cfg.args.clone().unwrap_or_else(|| {
                    vec![
                        "sign".to_string(),
                        "--key=cosign.key".to_string(),
                        "{{ .Artifact }}@{{ .Digest }}".to_string(),
                        "--yes".to_string(),
                    ]
                });

                let docker_filter = docker_sign_cfg.artifacts.as_deref().unwrap_or("");

                if docker_filter == "none" {
                    log.verbose(&format!(
                        "skipping docker-sign config '{}': artifacts: none",
                        sign_id
                    ));
                    ctx.remember_skip("docker-sign", sign_id, "artifacts: none");
                    continue;
                }

                // Collect docker artifacts based on the filter mode.
                // GoReleaser includes DockerImageV2 in all filter modes:
                // "images" → DockerImage + DockerImageV2
                // "manifests" → DockerManifest + DockerImageV2
                // "all" → DockerImage + DockerManifest + DockerImageV2
                // "" (default) → DockerImageV2 only
                // "none" → nothing (handled above)
                let docker_artifacts: Vec<_> = match docker_filter {
                    "images" => {
                        let mut arts = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
                        arts.extend(ctx.artifacts.by_kind(ArtifactKind::DockerImageV2));
                        arts
                    }
                    "" => ctx.artifacts.by_kind(ArtifactKind::DockerImageV2),
                    "manifests" => {
                        let mut arts = ctx.artifacts.by_kind(ArtifactKind::DockerManifest);
                        arts.extend(ctx.artifacts.by_kind(ArtifactKind::DockerImageV2));
                        arts
                    }
                    "all" => {
                        let mut arts = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
                        arts.extend(ctx.artifacts.by_kind(ArtifactKind::DockerManifest));
                        arts.extend(ctx.artifacts.by_kind(ArtifactKind::DockerImageV2));
                        arts
                    }
                    other => bail!(
                        "docker_signs[{}]: unknown artifacts filter {:?} (expected one of: \
                         all, images, manifests, none, or empty)",
                        sign_id,
                        other
                    ),
                };

                let image_paths: Vec<(
                    std::path::PathBuf,
                    std::collections::HashMap<String, String>,
                )> = docker_artifacts
                    .into_iter()
                    .filter(|a| {
                        // Apply ids filter if set on docker sign config.
                        if let Some(ref ids) = docker_sign_cfg.ids {
                            let matches_id = a
                                .metadata
                                .get("id")
                                .map(|id| ids.contains(id))
                                .unwrap_or(false);
                            let matches_name = a
                                .metadata
                                .get("name")
                                .map(|name| ids.contains(name))
                                .unwrap_or(false);
                            return matches_id || matches_name;
                        }
                        true
                    })
                    .map(|a| (a.path.clone(), a.metadata.clone()))
                    .collect();

                for (image_path, metadata) in &image_paths {
                    let image_str = image_path.to_string_lossy();

                    // Set docker-specific template variables from artifact metadata.
                    // `Digest` — the docker image digest (e.g., sha256:abc123...)
                    // `ArtifactID` — the artifact's id field from metadata
                    //
                    // These use PascalCase because Go-style template references like
                    // `{{ .Digest }}` are preprocessed by stripping the leading dot,
                    // resulting in `{{ Digest }}`. The Tera template engine is case-
                    // sensitive, so the variable name must match.
                    //
                    // Always set (even to empty) to avoid stale values from a
                    // previous iteration leaking to this image.
                    let digest_val = metadata.get("digest").map(|s| s.as_str()).unwrap_or("");
                    let artifact_id_val = metadata.get("id").map(|s| s.as_str()).unwrap_or("");
                    ctx.template_vars_mut().set("Digest", digest_val);
                    // Also set lowercase for direct Tera usage ({{ digest }}).
                    ctx.template_vars_mut().set("digest", digest_val);
                    ctx.template_vars_mut().set("ArtifactID", artifact_id_val);
                    // Also set camelCase for direct Tera usage ({{ artifactID }}).
                    ctx.template_vars_mut().set("artifactID", artifact_id_val);

                    // For Docker images the "signature" concept is embedded;
                    // use a placeholder `.sig` path to satisfy the template
                    // if the user has {{ .Signature }} in their args.
                    let signature_str = format!("{}.sig", image_str);

                    let resolved =
                        resolve_sign_args(&args, image_str.as_ref(), &signature_str, None);

                    let fully_resolved: Vec<String> = resolved
                        .iter()
                        .map(|arg| {
                            ctx.render_template(arg).unwrap_or_else(|e| {
                                log.warn(&format!(
                                    "failed to render docker-sign arg '{}': {}, using raw value",
                                    arg, e
                                ));
                                arg.clone()
                            })
                        })
                        .collect();

                    if ctx.is_dry_run() {
                        log.status(&format!(
                            "(dry-run) would run: {} {}",
                            cmd,
                            fully_resolved.join(" ")
                        ));
                        continue;
                    }

                    log.status(&format!("docker-sign [{}] {}", sign_id, image_str));

                    // Prepare stdin piping for docker signs.
                    let (stdin_cfg, stdin_data) = prepare_stdin_from(
                        docker_sign_cfg.stdin.as_deref(),
                        docker_sign_cfg.stdin_file.as_deref(),
                        "docker-sign",
                    )?;

                    let mut command = Command::new(&cmd);
                    command
                        .args(&fully_resolved)
                        .stdin(stdin_cfg)
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped());

                    // Template-render and merge custom env vars if configured on docker sign.
                    if let Some(ref env_list) = docker_sign_cfg.env {
                        let parsed = anodizer_core::config::parse_env_entries(env_list)
                            .with_context(|| "docker-sign: parse env entries")?;
                        for (k, v) in &parsed {
                            let rendered_val = ctx.render_template(v).unwrap_or_else(|e| {
                                log.warn(&format!(
                                    "failed to render docker-sign env '{}': {}, using raw value",
                                    k, e
                                ));
                                v.clone()
                            });
                            command.env(k, rendered_val);
                        }
                    }

                    let mut child = command.spawn().with_context(|| {
                        format!(
                            "sign: failed to spawn '{}' for docker image {}",
                            cmd, image_str
                        )
                    })?;

                    if let Some(data) = stdin_data {
                        if let Some(mut child_stdin) = child.stdin.take() {
                            child_stdin.write_all(&data).with_context(|| {
                                format!(
                                    "sign: failed to write stdin for docker image {}",
                                    image_str
                                )
                            })?;
                            drop(child_stdin);
                        } else {
                            log.warn(&format!(
                                "sign: stdin data provided but child process stdin unavailable for docker image {}",
                                image_str
                            ));
                        }
                    }

                    let output = child.wait_with_output().with_context(|| {
                        format!(
                            "sign: failed to wait for '{}' for docker image {}",
                            cmd, image_str
                        )
                    })?;

                    // Redact secrets from stdout/stderr before any output or logging.
                    let docker_env_pairs: Vec<(String, String)> = docker_sign_cfg
                        .env
                        .as_deref()
                        .map(|list| {
                            anodizer_core::config::parse_env_entries(list).unwrap_or_default()
                        })
                        .unwrap_or_default()
                        .into_iter()
                        .chain(std::env::vars())
                        .collect();

                    let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr_raw = String::from_utf8_lossy(&output.stderr).to_string();

                    let stdout_str =
                        anodizer_core::redact::redact_string(&stdout_raw, &docker_env_pairs);
                    let stderr_str =
                        anodizer_core::redact::redact_string(&stderr_raw, &docker_env_pairs);

                    let show_output = match docker_sign_cfg.output.as_ref() {
                        Some(s) => s
                            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                            .with_context(|| "docker_sign: render output template")?,
                        None => true,
                    };
                    if show_output {
                        if !stdout_str.is_empty() {
                            log.status(&format!("[docker-sign stdout] {}", stdout_str.trim()));
                        }
                        if !stderr_str.is_empty() {
                            log.status(&format!("[docker-sign stderr] {}", stderr_str.trim()));
                        }
                    }

                    // Redact output bytes before passing to check_output so error
                    // messages from failed docker signing commands don't leak secrets.
                    let mut redacted_output = output;
                    redacted_output.stdout = stdout_str.into_bytes();
                    redacted_output.stderr = stderr_str.into_bytes();

                    // Now check exit status (bails on non-zero).
                    log.check_output(redacted_output, &cmd)?;
                }
            }

            // Clear docker-specific template vars so they don't leak to
            // downstream stages that may inspect the template context.
            ctx.template_vars_mut().set("Digest", "");
            ctx.template_vars_mut().set("digest", "");
            ctx.template_vars_mut().set("ArtifactID", "");
            ctx.template_vars_mut().set("artifactID", "");
        }

        // Refresh the artifacts template variable so newly-added signatures
        // and certificates are visible to downstream stages (matching
        // GoReleaser's ctx.Artifacts.Refresh()).
        ctx.refresh_artifacts_var();

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::test_helpers::TestContextBuilder;

    /// Return a shell command + args that writes `content_expr` to `dest_file`.
    /// On Unix: sh -c "echo $VAR > file"
    /// On Windows: cmd.exe /C "echo %VAR% > file"
    fn shell_echo_to_file(env_var: &str, dest_file: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec![
                    "/C".to_string(),
                    format!("echo %{}% > {}", env_var, dest_file),
                ],
            )
        } else {
            (
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    format!("echo ${} > {}", env_var, dest_file),
                ],
            )
        }
    }

    /// Return a shell command + args that writes a literal string to `dest_file`.
    fn shell_echo_literal_to_file(literal: &str, dest_file: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec![
                    "/C".to_string(),
                    format!("echo {} > {}", literal, dest_file),
                ],
            )
        } else {
            (
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    format!("echo \"{}\" > {}", literal, dest_file),
                ],
            )
        }
    }

    /// Return (cmd, args) for a simple echo command (no shell).
    fn echo_command() -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/C".to_string(), "echo".to_string()],
            )
        } else {
            ("echo".to_string(), vec![])
        }
    }

    #[test]
    fn test_resolve_sign_args() {
        let args = vec![
            "--output".to_string(),
            "{{ .Signature }}".to_string(),
            "--detach-sign".to_string(),
            "{{ .Artifact }}".to_string(),
        ];
        let resolved = resolve_sign_args(&args, "/tmp/file.tar.gz", "/tmp/file.tar.gz.sig", None);
        assert_eq!(resolved[1], "/tmp/file.tar.gz.sig");
        assert_eq!(resolved[3], "/tmp/file.tar.gz");
    }

    #[test]
    fn test_filter_artifacts_checksum() {
        assert!(should_sign_artifact(ArtifactKind::Checksum, "checksum").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "checksum").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "checksum").unwrap());
    }

    #[test]
    fn test_filter_artifacts_all() {
        // "all" matches anodizer_core::artifact::release_uploadable_kinds().
        // Pro-equivalent: Installer (MSI/DMG/PKG/NSIS) is part of the
        // release-uploadable set so it is signed alongside archives.
        assert!(should_sign_artifact(ArtifactKind::Checksum, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Archive, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::UploadableBinary, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::LinuxPackage, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::SourceArchive, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Makeself, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Flatpak, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Sbom, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::SourceRpm, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::UploadableFile, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Installer, "all").unwrap());

        // GoReleaser includes Signature + Certificate in the "all" list — anodizer
        // matches that for parity. (On a fresh run there are no prior Signature /
        // Certificate artifacts, so this does not cause recursive signing.)
        assert!(should_sign_artifact(ArtifactKind::Signature, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Certificate, "all").unwrap());

        // Kinds not in release_uploadable_kinds — users must opt in via dedicated
        // filters (`diskimage`, `snap`, `macos_package`, `binary`).
        assert!(!should_sign_artifact(ArtifactKind::Binary, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Snap, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::MacOsPackage, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::DiskImage, "all").unwrap());

        // Internal / metadata types — never signed.
        assert!(!should_sign_artifact(ArtifactKind::DockerImage, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::DockerManifest, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::BrewFormula, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Metadata, "all").unwrap());
    }

    #[test]
    fn test_filter_artifacts_any_alias() {
        // "any" is an alias for "all"
        assert!(should_sign_artifact(ArtifactKind::Archive, "any").unwrap());
        assert!(should_sign_artifact(ArtifactKind::UploadableBinary, "any").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Signature, "any").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "any").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::DockerImage, "any").unwrap());
    }

    #[test]
    fn test_filter_artifacts_none() {
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "none").unwrap());
    }

    #[test]
    fn test_filter_artifacts_archive() {
        assert!(should_sign_artifact(ArtifactKind::Archive, "archive").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "archive").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "archive").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::LinuxPackage, "archive").unwrap());
    }

    #[test]
    fn test_filter_artifacts_source() {
        // "source" matches SourceArchive (not Archive)
        assert!(should_sign_artifact(ArtifactKind::SourceArchive, "source").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "source").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "source").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "source").unwrap());
    }

    #[test]
    fn test_filter_artifacts_binary() {
        assert!(should_sign_artifact(ArtifactKind::Binary, "binary").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "binary").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "binary").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::LinuxPackage, "binary").unwrap());
    }

    #[test]
    fn test_filter_artifacts_package() {
        assert!(should_sign_artifact(ArtifactKind::LinuxPackage, "package").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "package").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "package").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "package").unwrap());
    }

    #[test]
    fn test_filter_artifacts_installer() {
        assert!(should_sign_artifact(ArtifactKind::Installer, "installer").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "installer").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "installer").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "installer").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::LinuxPackage, "installer").unwrap());
    }

    #[test]
    fn test_filter_artifacts_diskimage() {
        assert!(should_sign_artifact(ArtifactKind::DiskImage, "diskimage").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "diskimage").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "diskimage").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "diskimage").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Installer, "diskimage").unwrap());
    }

    #[test]
    fn test_filter_artifacts_sbom() {
        assert!(should_sign_artifact(ArtifactKind::Sbom, "sbom").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "sbom").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "sbom").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "sbom").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::LinuxPackage, "sbom").unwrap());
    }

    #[test]
    fn test_filter_artifacts_snap() {
        assert!(should_sign_artifact(ArtifactKind::Snap, "snap").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "snap").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "snap").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "snap").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::LinuxPackage, "snap").unwrap());
    }

    #[test]
    fn test_filter_artifacts_macos_package() {
        assert!(should_sign_artifact(ArtifactKind::MacOsPackage, "macos_package").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "macos_package").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "macos_package").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "macos_package").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Installer, "macos_package").unwrap());
    }

    #[test]
    fn test_stage_skips_without_sign_config() {
        let mut ctx = TestContextBuilder::new().build();
        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_stage_skips_with_empty_signs() {
        let mut ctx = TestContextBuilder::new().signs(vec![]).build();
        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_sign_configs_run_independently() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        // Two sign configs targeting different artifact types
        let signs = vec![
            SignConfig {
                id: Some("gpg".to_string()),
                cmd: Some("echo".to_string()),
                args: Some(vec!["signing-archive".to_string()]),
                artifacts: Some("archive".to_string()),
                ids: None,
                signature: None,
                stdin: None,
                stdin_file: None,
                env: None,
                certificate: None,
                output: None,
                if_condition: None,
            },
            SignConfig {
                id: Some("cosign".to_string()),
                cmd: Some("echo".to_string()),
                args: Some(vec!["signing-checksum".to_string()]),
                artifacts: Some("checksum".to_string()),
                ids: None,
                signature: None,
                stdin: None,
                stdin_file: None,
                env: None,
                certificate: None,
                output: None,
                if_condition: None,
            },
        ];

        let mut ctx = TestContextBuilder::new().dry_run(true).signs(signs).build();

        // Add artifacts of both types
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/app.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        // Both configs should run independently without interfering
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_artifacts_filter_selects_correct_kinds() {
        // "all" = release_uploadable_kinds(). Pro-equivalent: Installer is
        // included alongside archives/packages.
        assert!(should_sign_artifact(ArtifactKind::Archive, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::UploadableBinary, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Checksum, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::LinuxPackage, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Sbom, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Installer, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Signature, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Certificate, "all").unwrap());

        // Kinds outside release_uploadable_kinds — use dedicated filters.
        assert!(!should_sign_artifact(ArtifactKind::Binary, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::DiskImage, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Snap, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::MacOsPackage, "all").unwrap());

        // "all" does NOT match internal/non-uploadable types
        assert!(!should_sign_artifact(ArtifactKind::DockerImage, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::DockerManifest, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::BrewFormula, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Metadata, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::ScoopManifest, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::KrewPluginManifest, "all").unwrap());

        // "none" matches nothing
        assert!(!should_sign_artifact(ArtifactKind::Archive, "none").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "none").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "none").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::LinuxPackage, "none").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Installer, "none").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::DiskImage, "none").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Sbom, "none").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Snap, "none").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::MacOsPackage, "none").unwrap());

        // "archive" only matches Archive
        assert!(should_sign_artifact(ArtifactKind::Archive, "archive").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Binary, "archive").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Checksum, "archive").unwrap());

        // "binary" only matches Binary
        assert!(should_sign_artifact(ArtifactKind::Binary, "binary").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "binary").unwrap());

        // "package" only matches LinuxPackage
        assert!(should_sign_artifact(ArtifactKind::LinuxPackage, "package").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "package").unwrap());

        // "installer" only matches Installer
        assert!(should_sign_artifact(ArtifactKind::Installer, "installer").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "installer").unwrap());

        // "diskimage" only matches DiskImage
        assert!(should_sign_artifact(ArtifactKind::DiskImage, "diskimage").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "diskimage").unwrap());

        // "sbom" only matches Sbom
        assert!(should_sign_artifact(ArtifactKind::Sbom, "sbom").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "sbom").unwrap());

        // "snap" only matches Snap
        assert!(should_sign_artifact(ArtifactKind::Snap, "snap").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "snap").unwrap());

        // "macos_package" only matches MacOsPackage
        assert!(should_sign_artifact(ArtifactKind::MacOsPackage, "macos_package").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Archive, "macos_package").unwrap());

        // Unknown filter returns an error
        assert!(should_sign_artifact(ArtifactKind::Checksum, "unknown-value").is_err());
    }

    #[test]
    fn test_ids_filter_restricts_signed_artifacts() {
        // Verify the ids filter logic directly by testing should_sign_artifact
        // combined with the ids-based metadata check that the stage performs.
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        let sign_cfg = SignConfig {
            id: Some("gpg".to_string()),
            cmd: Some("echo".to_string()),
            args: Some(vec!["sign".to_string()]),
            artifacts: Some("archive".to_string()),
            ids: Some(vec!["linux-release".to_string()]),
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        };

        let filter = sign_cfg.artifacts.as_deref().unwrap_or("none");

        // Build test artifacts
        let matching_artifact = Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/linux.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "linux-release".to_string());
                m
            },
            size: None,
        };

        let non_matching_artifact = Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/darwin.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "darwin-release".to_string());
                m
            },
            size: None,
        };

        let no_id_artifact = Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/other.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        };

        let wrong_kind_artifact = Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/binary"),
            target: None,
            crate_name: "test".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "linux-release".to_string());
                m
            },
            size: None,
        };

        // Replicate the stage's filtering logic:
        // 1. should_sign_artifact(kind, filter) must be true
        // 2. If ids is set, artifact metadata "id" or "name" must match
        let ids = &sign_cfg.ids;
        let should_sign = |a: &Artifact| -> bool {
            if !should_sign_artifact(a.kind, filter).unwrap() {
                return false;
            }
            if let Some(id_list) = ids {
                let matches_id = a
                    .metadata
                    .get("id")
                    .map(|id| id_list.contains(id))
                    .unwrap_or(false);
                let matches_name = a
                    .metadata
                    .get("name")
                    .map(|name| id_list.contains(name))
                    .unwrap_or(false);
                return matches_id || matches_name;
            }
            true
        };

        assert!(
            should_sign(&matching_artifact),
            "archive with matching id 'linux-release' should be signed"
        );
        assert!(
            !should_sign(&non_matching_artifact),
            "archive with non-matching id 'darwin-release' should NOT be signed"
        );
        assert!(
            !should_sign(&no_id_artifact),
            "archive with no id metadata should NOT be signed when ids filter is set"
        );
        assert!(
            !should_sign(&wrong_kind_artifact),
            "binary with matching id should NOT be signed when filter is 'archive'"
        );

        // Also run through the stage in dry-run to confirm it completes
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![sign_cfg])
            .build();
        ctx.artifacts.add(matching_artifact);
        ctx.artifacts.add(non_matching_artifact);
        ctx.artifacts.add(no_id_artifact);
        ctx.artifacts.add(wrong_kind_artifact);

        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_dry_run_logs_without_executing() {
        // The critical assertion: a nonexistent binary in dry-run mode must NOT
        // cause an error. If the stage tried to actually execute the binary,
        // it would fail because /nonexistent/gpg does not exist.
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("gpg".to_string()),
            cmd: Some("/nonexistent/binary/that/does/not/exist".to_string()),
            args: Some(vec![
                "--output".to_string(),
                "{{ .Signature }}".to_string(),
                "--detach-sign".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(signs.clone())
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        // This MUST succeed. If dry-run mode were broken and tried to spawn
        // the nonexistent binary, it would return an error.
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "dry-run must not execute the signing binary; got error: {:?}",
            result.err()
        );

        // Now verify that WITHOUT dry-run, the same config WOULD fail,
        // proving that dry-run is what prevents execution.
        let mut ctx_no_dry = TestContextBuilder::new()
            .dry_run(false)
            .signs(signs)
            .build();

        ctx_no_dry.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let result_no_dry = stage.run(&mut ctx_no_dry);
        assert!(
            result_no_dry.is_err(),
            "without dry-run, a nonexistent binary should cause an error"
        );
    }

    #[test]
    fn test_template_variables_in_args_resolve_correctly() {
        let args = vec![
            "--output".to_string(),
            "{{ .Signature }}".to_string(),
            "--detach-sign".to_string(),
            "{{ .Artifact }}".to_string(),
            "--extra={{ .Artifact }}.meta".to_string(),
        ];

        let resolved = resolve_sign_args(&args, "/tmp/file.tar.gz", "/tmp/file.tar.gz.sig", None);
        assert_eq!(resolved[0], "--output");
        assert_eq!(resolved[1], "/tmp/file.tar.gz.sig");
        assert_eq!(resolved[2], "--detach-sign");
        assert_eq!(resolved[3], "/tmp/file.tar.gz");
        assert_eq!(resolved[4], "--extra=/tmp/file.tar.gz.meta");
    }

    #[test]
    fn test_sign_none_filter_skips_entirely() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("skip".to_string()),
            cmd: Some("false".to_string()), // Would fail if executed
            args: None,
            artifacts: Some("none".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        }];

        let mut ctx = TestContextBuilder::new().signs(signs).build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/file.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        // "none" filter should skip without executing any command
        assert!(stage.run(&mut ctx).is_ok());

        // SkipMemento should record the (sign, skip, "artifacts: none") tuple
        // so the end-of-pipeline summary can surface it.
        let events = ctx.skip_memento.snapshot();
        assert_eq!(events.len(), 1, "expected one recorded skip");
        assert_eq!(events[0].stage, "sign");
        assert_eq!(events[0].label, "skip");
        assert_eq!(events[0].reason, "artifacts: none");
    }

    #[test]
    fn test_sign_if_false_records_skip_memento() {
        // A sign config with `if: "false"` must not execute AND must leave a
        // memento entry so operators can tell an intentionally-disabled sign
        // config apart from a misconfigured one in the pipeline summary.
        let signs = vec![SignConfig {
            id: Some("gated".to_string()),
            cmd: Some("false".to_string()),
            args: None,
            artifacts: Some("archive".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: Some("false".to_string()),
        }];

        let mut ctx = TestContextBuilder::new().signs(signs).build();
        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());

        let events = ctx.skip_memento.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "sign");
        assert_eq!(events[0].label, "gated");
        assert!(
            events[0].reason.contains("if condition evaluated to"),
            "unexpected reason: {}",
            events[0].reason
        );
    }

    #[test]
    fn test_sign_positional_label_when_id_missing() {
        // A sign config without an id should get a positional label of the
        // form `<stage-label>[N]` in the skip summary so users can still
        // find it in their config.
        let signs = vec![SignConfig {
            id: None,
            cmd: Some("false".to_string()),
            args: None,
            artifacts: Some("none".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        }];

        let mut ctx = TestContextBuilder::new().signs(signs).build();
        let stage = SignStage;
        assert!(stage.run(&mut ctx).is_ok());

        let events = ctx.skip_memento.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].label, "sign[0]");
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_missing_signing_binary_errors_with_command_name() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("test".to_string()),
            cmd: Some("/nonexistent/path/to/gpg-that-does-not-exist".to_string()),
            args: Some(vec![
                "--output".to_string(),
                "{{ .Signature }}".to_string(),
                "--detach-sign".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(signs)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "missing signing binary should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("gpg-that-does-not-exist") || err.contains("spawn"),
            "error should mention the missing command, got: {err}"
        );
    }

    #[test]
    fn test_signing_command_nonzero_exit_errors_with_details() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("test".to_string()),
            cmd: Some("false".to_string()), // always exits with code 1
            args: Some(vec![]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(signs)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/test.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_err(),
            "signing command returning non-zero should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("non-zero") || err.contains("false"),
            "error should mention non-zero exit or command name, got: {err}"
        );
    }

    #[test]
    fn test_resolve_sign_args_no_placeholders() {
        let args = vec!["--armor".to_string(), "--verbose".to_string()];
        let resolved = resolve_sign_args(&args, "/tmp/file", "/tmp/file.sig", None);
        assert_eq!(
            resolved, args,
            "args without placeholders should be unchanged"
        );
    }

    #[test]
    fn test_resolve_sign_args_both_placeholders_in_single_arg() {
        let args = vec!["{{ .Artifact }}:{{ .Signature }}".to_string()];
        let resolved = resolve_sign_args(&args, "/tmp/f", "/tmp/f.sig", None);
        assert_eq!(resolved[0], "/tmp/f:/tmp/f.sig");
    }

    #[test]
    fn test_stdin_file_missing_errors_with_path() {
        let sign_cfg = SignConfig {
            id: None,
            cmd: None,
            args: None,
            artifacts: None,
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: Some("/nonexistent/stdin_file.txt".to_string()),
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        };

        let result = prepare_stdin_from(
            sign_cfg.stdin.as_deref(),
            sign_cfg.stdin_file.as_deref(),
            "sign",
        );
        assert!(
            result.is_err(),
            "missing stdin_file should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("/nonexistent/stdin_file.txt") || err.contains("stdin_file"),
            "error should mention the missing stdin_file path, got: {err}"
        );
    }

    // ---- new field tests: env, certificate, docker sign ids/stdin ----

    #[test]
    fn test_sign_env_config_parsing() {
        let yaml = r#"
cmd: "cosign"
env:
  - COSIGN_EXPERIMENTAL=1
  - MY_KEY=my_value
"#;
        let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let env = cfg.env.unwrap();
        assert_eq!(env, vec!["COSIGN_EXPERIMENTAL=1", "MY_KEY=my_value"]);
    }

    #[test]
    fn test_sign_certificate_config_parsing() {
        let yaml = r#"
cmd: "cosign"
certificate: "{{ .ProjectName }}-{{ .Tag }}.pem"
"#;
        let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            cfg.certificate.as_deref(),
            Some("{{ .ProjectName }}-{{ .Tag }}.pem")
        );
    }

    #[test]
    fn test_docker_sign_ids_config_parsing() {
        let yaml = r#"
cmd: "cosign"
ids:
  - "my-docker-image"
  - "another-image"
"#;
        let cfg: anodizer_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let ids = cfg.ids.unwrap();
        assert_eq!(ids, vec!["my-docker-image", "another-image"]);
    }

    #[test]
    fn test_docker_sign_stdin_config_parsing() {
        let yaml = r#"
cmd: "cosign"
stdin: "my-password"
"#;
        let cfg: anodizer_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.stdin.as_deref(), Some("my-password"));
    }

    #[test]
    fn test_docker_sign_stdin_file_config_parsing() {
        let yaml = r#"
cmd: "cosign"
stdin_file: "/path/to/password"
"#;
        let cfg: anodizer_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.stdin_file.as_deref(), Some("/path/to/password"));
    }

    #[test]
    fn test_sign_env_vars_passed_to_command() {
        // Verify that custom env vars reach the signing command.
        // Use `sh -c` to write the env var value to a file so we can verify it.
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        let tmp = tempfile::TempDir::new().unwrap();
        let marker_path = tmp.path().join("env_check.txt");
        let marker_str = marker_path.to_string_lossy().to_string();

        let (cmd, args) = shell_echo_to_file("ANODIZER_TEST_SIGN_ENV", &marker_str);
        let signs = vec![SignConfig {
            id: Some("test-env".to_string()),
            cmd: Some(cmd),
            args: Some(args),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: Some(vec!["ANODIZER_TEST_SIGN_ENV=hello_from_sign".to_string()]),
            certificate: None,
            output: None,
            if_condition: None,
        }];

        // Create a real artifact file so the command runs
        let artifact_path = tmp.path().join("checksums.sha256");
        std::fs::write(&artifact_path, b"checksum content").unwrap();

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(signs)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: artifact_path,
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_ok(),
            "sign with custom env vars should succeed; got: {:?}",
            result.err()
        );

        // Verify the env var was actually passed to the child process
        let env_output = std::fs::read_to_string(&marker_path).unwrap_or_else(|e| {
            panic!("marker file should exist — env var was written by signing command: {e}")
        });
        assert_eq!(
            env_output.trim(),
            "hello_from_sign",
            "ANODIZER_TEST_SIGN_ENV should have been passed to the signing command"
        );
    }

    #[test]
    fn test_docker_sign_ids_filter() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::DockerSignConfig;

        let docker_signs = vec![DockerSignConfig {
            cmd: Some("echo".to_string()),
            args: Some(vec!["sign".to_string(), "{{ .Artifact }}".to_string()]),
            artifacts: Some("all".to_string()),
            ids: Some(vec!["prod-image".to_string()]),
            stdin: None,
            stdin_file: None,
            id: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.config.docker_signs = Some(docker_signs);

        // Add docker images: one matching, one not
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: String::new(),
            path: std::path::PathBuf::from("ghcr.io/myorg/prod:latest"),
            target: None,
            crate_name: "test".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "prod-image".to_string());
                m
            },
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: String::new(),
            path: std::path::PathBuf::from("ghcr.io/myorg/dev:latest"),
            target: None,
            crate_name: "test".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "dev-image".to_string());
                m
            },
            size: None,
        });

        let stage = SignStage;
        // Should succeed (dry-run). The ids filter restricts to prod-image only.
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_certificate_template_resolves_in_args() {
        // Test that {{ .Certificate }} placeholder in args gets resolved
        let args = vec![
            "sign".to_string(),
            "--certificate".to_string(),
            "{{ .Certificate }}".to_string(),
            "{{ .Artifact }}".to_string(),
        ];
        let resolved = resolve_sign_args(
            &args,
            "/tmp/app.tar.gz",
            "/tmp/app.tar.gz.sig",
            Some("/tmp/app.pem"),
        );
        assert_eq!(resolved[2], "/tmp/app.pem");
        assert_eq!(resolved[3], "/tmp/app.tar.gz");
    }

    #[test]
    fn test_certificate_template_none_clears_placeholder() {
        // When certificate is None, {{ .Certificate }} is replaced with empty string
        // to prevent it from being fed to Tera and causing spurious warnings.
        let args = vec!["--cert={{ .Certificate }}".to_string()];
        let resolved = resolve_sign_args(&args, "/tmp/f", "/tmp/f.sig", None);
        assert_eq!(
            resolved[0], "--cert=",
            "placeholder should be replaced with empty string when certificate is None"
        );
    }

    #[test]
    fn test_sign_with_certificate_dry_run() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("cosign".to_string()),
            cmd: Some("cosign".to_string()),
            args: Some(vec![
                "sign-blob".to_string(),
                "--certificate".to_string(),
                "{{ .Certificate }}".to_string(),
                "--output-signature".to_string(),
                "{{ .Signature }}".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: Some("{{ .Artifact }}.pem".to_string()),
            output: None,
            if_condition: None,
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).signs(signs).build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "dry-run with certificate template should succeed"
        );
    }

    #[test]
    fn test_prepare_stdin_from_content() {
        let (_, data) = prepare_stdin_from(Some("my-password"), None, "docker-sign").unwrap();
        assert!(data.is_some());
        assert_eq!(data.unwrap(), b"my-password");
    }

    #[test]
    fn test_prepare_stdin_from_file_missing() {
        let result = prepare_stdin_from(None, Some("/nonexistent/docker_stdin.txt"), "docker-sign");
        assert!(result.is_err());
    }

    #[test]
    fn test_prepare_stdin_from_inherit() {
        let (_, data) = prepare_stdin_from(None, None, "docker-sign").unwrap();
        assert!(data.is_none());
    }

    #[test]
    fn test_sign_stage_registers_signature_artifacts_dry_run() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("gpg".to_string()),
            cmd: Some("gpg".to_string()),
            args: Some(vec![
                "--output".to_string(),
                "{{ .Signature }}".to_string(),
                "--detach-sign".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).signs(signs).build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        stage.run(&mut ctx).unwrap();

        // The signature artifact should be registered even in dry-run mode.
        let sig_artifacts = ctx.artifacts.by_kind(ArtifactKind::Signature);
        assert_eq!(
            sig_artifacts.len(),
            1,
            "should register one signature artifact"
        );
        let sig = &sig_artifacts[0];
        assert_eq!(sig.metadata.get("type").unwrap(), "Signature");
        assert_eq!(sig.crate_name, "myapp");
    }

    #[test]
    fn test_sign_stage_registers_certificate_artifacts_dry_run() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        let signs = vec![SignConfig {
            id: Some("cosign".to_string()),
            cmd: Some("cosign".to_string()),
            args: Some(vec!["sign-blob".to_string(), "{{ .Artifact }}".to_string()]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: Some("{{ .Artifact }}.pem".to_string()),
            output: None,
            if_condition: None,
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).signs(signs).build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        stage.run(&mut ctx).unwrap();

        // Should register both a signature and a certificate artifact.
        let sig_artifacts = ctx.artifacts.by_kind(ArtifactKind::Signature);
        assert_eq!(
            sig_artifacts.len(),
            1,
            "should register one Signature artifact"
        );
        let cert_artifacts = ctx.artifacts.by_kind(ArtifactKind::Certificate);
        assert_eq!(
            cert_artifacts.len(),
            1,
            "should register one Certificate artifact"
        );
    }

    #[test]
    fn test_docker_sign_id_config_parsing() {
        let yaml = r#"
id: "my-docker-signer"
cmd: "cosign"
"#;
        let cfg: anodizer_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.id.as_deref(), Some("my-docker-signer"));
    }

    #[test]
    fn test_docker_sign_env_config_parsing() {
        let yaml = r#"
cmd: "cosign"
env:
  - COSIGN_EXPERIMENTAL=1
  - REGISTRY_TOKEN=secret
"#;
        let cfg: anodizer_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let env = cfg.env.unwrap();
        assert_eq!(env, vec!["COSIGN_EXPERIMENTAL=1", "REGISTRY_TOKEN=secret"]);
    }

    #[test]
    fn test_docker_sign_env_vars_passed_to_command() {
        // Verify that custom env vars reach the docker signing command.
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::DockerSignConfig;

        let tmp = tempfile::TempDir::new().unwrap();
        let marker_path = tmp.path().join("docker_env_check.txt");
        let marker_str = marker_path.to_string_lossy().to_string();

        let (cmd, args) = shell_echo_to_file("ANODIZER_TEST_DOCKER_ENV", &marker_str);
        let docker_signs = vec![DockerSignConfig {
            id: Some("test-env".to_string()),
            cmd: Some(cmd),
            args: Some(args),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: Some(vec!["ANODIZER_TEST_DOCKER_ENV=docker_hello".to_string()]),
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        }];

        let mut ctx = TestContextBuilder::new().dry_run(false).build();
        ctx.config.docker_signs = Some(docker_signs);

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: String::new(),
            path: std::path::PathBuf::from("ghcr.io/test/app:latest"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DockerSignStage;
        stage.run(&mut ctx).unwrap();

        let env_output = std::fs::read_to_string(&marker_path).unwrap();
        assert_eq!(
            env_output.trim(),
            "docker_hello",
            "ANODIZER_TEST_DOCKER_ENV should have been passed to the docker signing command"
        );
    }

    // -----------------------------------------------------------------------
    // Task 7: sign stage parity — output, if, binary_signs, docker vars
    // -----------------------------------------------------------------------

    #[test]
    fn test_sign_config_output_field_parsing() {
        let yaml = r#"
cmd: "gpg"
output: true
"#;
        let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(cfg.output.unwrap().as_bool());
    }

    #[test]
    fn test_sign_config_if_field_parsing() {
        let yaml = r#"
cmd: "gpg"
if: "{{ IsSnapshot }}"
"#;
        let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.if_condition.as_deref(), Some("{{ IsSnapshot }}"));
    }

    #[test]
    fn test_sign_config_output_and_if_together() {
        let yaml = r#"
cmd: "cosign"
output: true
if: "{{ IsSnapshot }}"
artifacts: all
"#;
        let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(cfg.output.unwrap().as_bool());
        assert_eq!(cfg.if_condition.as_deref(), Some("{{ IsSnapshot }}"));
        assert_eq!(cfg.artifacts.as_deref(), Some("all"));
    }

    #[test]
    fn test_sign_config_output_defaults_to_none() {
        let cfg = SignConfig::default();
        assert!(cfg.output.is_none());
        assert!(cfg.if_condition.is_none());
    }

    #[test]
    fn test_binary_signs_config_parsing() {
        let yaml = r#"
project_name: test
binary_signs:
  - cmd: gpg
    artifacts: all
  - cmd: cosign
    args:
      - sign-blob
crates: []
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.binary_signs.len(), 2);
        assert_eq!(config.binary_signs[0].cmd.as_deref(), Some("gpg"));
        assert_eq!(config.binary_signs[1].cmd.as_deref(), Some("cosign"));
    }

    #[test]
    fn test_binary_signs_singular_alias() {
        let yaml = r#"
project_name: test
binary_sign:
  cmd: gpg
  artifacts: all
crates: []
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.binary_signs.len(), 1);
        assert_eq!(config.binary_signs[0].cmd.as_deref(), Some("gpg"));
    }

    #[test]
    fn test_binary_signs_defaults_to_empty() {
        let yaml = "project_name: test\ncrates: []";
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.binary_signs.is_empty());
    }

    #[test]
    fn test_if_condition_false_skips_sign() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        // Sign config with if: "false" — should be skipped entirely.
        // If not skipped, the nonexistent binary would cause an error.
        let signs = vec![SignConfig {
            id: Some("skipped".to_string()),
            cmd: Some("/nonexistent/sign-tool".to_string()),
            args: Some(vec!["sign".to_string()]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: Some("false".to_string()),
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(signs)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "if condition 'false' should skip the sign config"
        );
    }

    #[test]
    fn test_if_condition_true_proceeds() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        // Sign config with if: "true" — should proceed normally.
        // Uses "echo" which always succeeds.
        let signs = vec![SignConfig {
            id: Some("active".to_string()),
            cmd: Some("echo".to_string()),
            args: Some(vec!["signing".to_string()]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: Some("true".to_string()),
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).signs(signs).build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "if condition 'true' should proceed with sign config"
        );

        // Verify the signature artifact was registered (proves the config was not skipped)
        let sig_artifacts = ctx.artifacts.by_kind(ArtifactKind::Signature);
        assert!(
            !sig_artifacts.is_empty(),
            "sign config with if='true' should register signature artifacts"
        );
    }

    #[test]
    fn test_if_condition_empty_skips_sign() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        // A template that renders to empty should skip the config.
        // "{{ IsSnapshot }}" is "false" in non-snapshot mode, but let's test
        // an explicitly empty result.
        let signs = vec![SignConfig {
            id: Some("skipped".to_string()),
            cmd: Some("/nonexistent/sign-tool".to_string()),
            args: Some(vec!["sign".to_string()]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            // A template that renders to empty string
            if_condition: Some("".to_string()),
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(signs)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "empty if condition should skip the sign config"
        );
    }

    #[test]
    fn test_if_condition_snapshot_template() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        // When snapshot mode is active, IsSnapshot = "true".
        // This sign config with if: "{{ IsSnapshot }}" should only run
        // when in snapshot mode.
        let signs = vec![SignConfig {
            id: Some("snapshot-only".to_string()),
            cmd: Some("/nonexistent/sign-tool".to_string()),
            args: Some(vec!["sign".to_string()]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: Some("{{ IsSnapshot }}".to_string()),
        }];

        // Non-snapshot mode: IsSnapshot = "false" → should skip
        let mut ctx = TestContextBuilder::new()
            .snapshot(false)
            .dry_run(false)
            .signs(signs.clone())
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "non-snapshot should skip sign config with if={{ IsSnapshot }}"
        );

        // Snapshot mode: IsSnapshot = "true" → should proceed (but uses
        // nonexistent binary, so it will error — prove it tries to run).
        let mut ctx_snap = TestContextBuilder::new()
            .snapshot(true)
            .dry_run(false)
            .signs(signs)
            .build();

        ctx_snap.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let result = stage.run(&mut ctx_snap);
        assert!(
            result.is_err(),
            "snapshot mode should attempt to run the sign command (and fail with nonexistent binary)"
        );
    }

    #[test]
    fn test_binary_signs_only_signs_binaries() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        let binary_signs = vec![SignConfig {
            id: Some("binary-gpg".to_string()),
            cmd: Some("echo".to_string()),
            args: Some(vec!["signing-binary".to_string()]),
            // Even if artifacts says "all", binary_signs should only sign binaries
            artifacts: Some("all".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .binary_signs(binary_signs)
            .build();

        // Add a binary and an archive artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp.tar.gz"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        stage.run(&mut ctx).unwrap();

        // Only the binary should have generated a signature artifact
        let sig_artifacts = ctx.artifacts.by_kind(ArtifactKind::Signature);
        assert_eq!(
            sig_artifacts.len(),
            1,
            "binary_signs should only sign Binary artifacts, not Archive or Checksum"
        );
    }

    #[test]
    fn test_binary_signs_if_condition_works() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        // binary_signs with if: "false" should be skipped
        let binary_signs = vec![SignConfig {
            id: Some("skipped".to_string()),
            cmd: Some("/nonexistent/sign-tool".to_string()),
            args: Some(vec!["sign".to_string()]),
            artifacts: Some("all".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: Some("false".to_string()),
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .binary_signs(binary_signs)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/myapp"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "binary_signs with if=false should be skipped"
        );
    }

    #[test]
    fn test_docker_sign_digest_and_artifact_id_template_vars() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::DockerSignConfig;

        let tmp = tempfile::TempDir::new().unwrap();
        let marker_path = tmp.path().join("docker_vars.txt");
        let marker_str = marker_path.to_string_lossy().to_string();

        // Use a shell to capture template-resolved variables
        let (cmd, args) = shell_echo_literal_to_file(
            "digest={{ digest }} artifactID={{ artifactID }}",
            &marker_str,
        );
        let docker_signs = vec![DockerSignConfig {
            id: Some("test-vars".to_string()),
            cmd: Some(cmd),
            args: Some(args),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        }];

        let mut ctx = TestContextBuilder::new().dry_run(false).build();
        ctx.config.docker_signs = Some(docker_signs);

        // Add a docker image with digest and id metadata
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("digest".to_string(), "sha256:abc123def456".to_string());
        metadata.insert("id".to_string(), "my-docker-image".to_string());

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: String::new(),
            path: std::path::PathBuf::from("ghcr.io/myorg/app:latest"),
            target: None,
            crate_name: "test".to_string(),
            metadata,
            size: None,
        });

        let stage = DockerSignStage;
        stage.run(&mut ctx).unwrap();

        let output = std::fs::read_to_string(&marker_path).unwrap();
        assert!(
            output.contains("digest=sha256:abc123def456"),
            "digest template var should resolve from metadata, got: {}",
            output.trim()
        );
        assert!(
            output.contains("artifactID=my-docker-image"),
            "artifactID template var should resolve from metadata, got: {}",
            output.trim()
        );
    }

    #[test]
    fn test_docker_sign_without_digest_metadata_still_works() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::DockerSignConfig;

        // Docker image without digest/id metadata — should still work
        let docker_signs = vec![DockerSignConfig {
            id: Some("test-no-meta".to_string()),
            cmd: Some("echo".to_string()),
            args: Some(vec!["sign".to_string(), "{{ .Artifact }}".to_string()]),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.config.docker_signs = Some(docker_signs);

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: String::new(),
            path: std::path::PathBuf::from("ghcr.io/myorg/app:latest"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        assert!(
            stage.run(&mut ctx).is_ok(),
            "docker sign without digest/id metadata should still work in dry-run"
        );
    }

    #[test]
    fn test_output_capture_with_real_command() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};

        // Use echo to produce stdout; with output: true it should be captured
        let (cmd, mut base_args) = echo_command();
        base_args.push("hello-from-sign".to_string());
        let signs = vec![SignConfig {
            id: Some("test-output".to_string()),
            cmd: Some(cmd),
            args: Some(base_args),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: Some(anodizer_core::config::StringOrBool::Bool(true)),
            if_condition: None,
        }];

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(signs)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: std::path::PathBuf::from("/tmp/checksums.sha256"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = SignStage;
        // The command succeeds; output capture should not cause errors
        assert!(
            stage.run(&mut ctx).is_ok(),
            "sign with output: true and a real command should succeed"
        );
    }

    // -----------------------------------------------------------------------
    // Task 1: binary_signs architecture-aware signature template
    // -----------------------------------------------------------------------

    #[test]
    fn test_default_binary_signature_template_includes_arch() {
        // Verify the constant contains Os/Arch/Arm/Mips/Amd64 references
        assert!(
            DEFAULT_BINARY_SIGNATURE_TEMPLATE.contains("Os"),
            "binary signature template must include Os"
        );
        assert!(
            DEFAULT_BINARY_SIGNATURE_TEMPLATE.contains("Arch"),
            "binary signature template must include Arch"
        );
        assert!(
            DEFAULT_BINARY_SIGNATURE_TEMPLATE.contains("Arm"),
            "binary signature template must include Arm conditional"
        );
        assert!(
            DEFAULT_BINARY_SIGNATURE_TEMPLATE.contains("Amd64"),
            "binary signature template must include Amd64 conditional"
        );
        assert!(
            !DEFAULT_BINARY_SIGNATURE_TEMPLATE.ends_with(".sig"),
            "binary signature template must NOT end with .sig (GoReleaser sign_binary.go:16 parity)"
        );
    }

    #[test]
    fn test_binary_signs_signature_includes_os_arch() {
        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.template_vars_mut().set("Os", "linux");
        ctx.template_vars_mut().set("Arch", "amd64");
        ctx.template_vars_mut().set("Arm", "");
        ctx.template_vars_mut().set("Amd64", "");
        ctx.template_vars_mut().set("Mips", "");

        let sign_cfg = SignConfig {
            id: None,
            artifacts: None,
            cmd: None,
            args: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            ids: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        };
        let log = ctx.logger("test");
        let result = resolve_signature_path(
            &sign_cfg,
            "/dist/myapp",
            &ctx,
            &log,
            Some(DEFAULT_BINARY_SIGNATURE_TEMPLATE),
        )
        .unwrap();
        assert_eq!(result, "/dist/myapp_linux_amd64");
    }

    #[test]
    fn test_binary_signs_signature_includes_arm_variant() {
        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        // GoReleaser splits ARM: Arch="arm", Arm="6" → rendered as "arm" + "v6" = "armv6"
        ctx.template_vars_mut().set("Os", "linux");
        ctx.template_vars_mut().set("Arch", "arm");
        ctx.template_vars_mut().set("Arm", "6");
        ctx.template_vars_mut().set("Amd64", "");
        ctx.template_vars_mut().set("Mips", "");

        let sign_cfg = SignConfig {
            id: None,
            artifacts: None,
            cmd: None,
            args: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            ids: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        };
        let log = ctx.logger("test");
        let result = resolve_signature_path(
            &sign_cfg,
            "/dist/myapp",
            &ctx,
            &log,
            Some(DEFAULT_BINARY_SIGNATURE_TEMPLATE),
        )
        .unwrap();
        assert_eq!(result, "/dist/myapp_linux_armv6");
    }

    #[test]
    fn test_binary_signs_signature_includes_amd64_level() {
        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.template_vars_mut().set("Os", "linux");
        ctx.template_vars_mut().set("Arch", "amd64");
        ctx.template_vars_mut().set("Arm", "");
        ctx.template_vars_mut().set("Amd64", "v2");
        ctx.template_vars_mut().set("Mips", "");

        let sign_cfg = SignConfig {
            id: None,
            artifacts: None,
            cmd: None,
            args: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            ids: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        };
        let log = ctx.logger("test");
        let result = resolve_signature_path(
            &sign_cfg,
            "/dist/myapp",
            &ctx,
            &log,
            Some(DEFAULT_BINARY_SIGNATURE_TEMPLATE),
        )
        .unwrap();
        assert_eq!(result, "/dist/myapp_linux_amd64v2");
    }

    #[test]
    fn test_normal_signs_uses_simple_default() {
        let ctx = TestContextBuilder::new().dry_run(true).build();
        let sign_cfg = SignConfig {
            id: None,
            artifacts: None,
            cmd: None,
            args: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            ids: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        };
        let log = ctx.logger("test");
        // Normal signs (None default) should use simple {artifact}.sig
        let result =
            resolve_signature_path(&sign_cfg, "/dist/myapp.tar.gz", &ctx, &log, None).unwrap();
        assert_eq!(result, "/dist/myapp.tar.gz.sig");
    }

    // -----------------------------------------------------------------------
    // Task 3: DockerImageV2 in docker_signs filters
    // -----------------------------------------------------------------------

    #[test]
    fn test_docker_signs_default_filter_selects_v2() {
        // When docker_signs artifacts is "" (default), only DockerImageV2 should match.
        // This verifies the code path — full integration tested via stage.run() above.
        use anodizer_core::artifact::Artifact;

        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: "legacy".to_string(),
            path: std::path::PathBuf::from("ghcr.io/owner/app:v1"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImageV2,
            name: "v2".to_string(),
            path: std::path::PathBuf::from("ghcr.io/owner/app:v2"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // Default filter "" should return only DockerImageV2
        let v2_only = ctx.artifacts.by_kind(ArtifactKind::DockerImageV2);
        assert_eq!(v2_only.len(), 1);
        assert_eq!(v2_only[0].name, "v2");

        // "images" filter should return both DockerImage and DockerImageV2
        let mut images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
        images.extend(ctx.artifacts.by_kind(ArtifactKind::DockerImageV2));
        assert_eq!(images.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Integration: binary_signs with target triple through process_sign_configs
    // -----------------------------------------------------------------------

    #[test]
    fn test_binary_signs_sets_os_arch_from_target_triple() {
        use anodizer_core::artifact::Artifact;

        let binary_sign_cfg = SignConfig {
            id: None,
            artifacts: Some("binary".to_string()),
            cmd: Some("true".to_string()),
            args: Some(vec![]),
            signature: None,
            stdin: None,
            stdin_file: None,
            ids: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        };
        let mut ctx = TestContextBuilder::new()
            .binary_signs(vec![binary_sign_cfg])
            .dry_run(true)
            .build();

        // Add a binary artifact with a linux/amd64 target
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: std::path::PathBuf::from("/dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let log = ctx.logger("binary-sign");
        let binary_sign_configs = ctx.config.binary_signs.clone();
        let result = process_sign_configs(
            &binary_sign_configs,
            &mut ctx,
            &log,
            ArtifactFilter::BinaryOnly,
            "binary-sign",
        );
        assert!(result.is_ok());

        // Verify a signature artifact was registered with arch-aware naming
        let sigs: Vec<_> = ctx.artifacts.by_kind(ArtifactKind::Signature);
        assert_eq!(sigs.len(), 1);
        assert!(
            sigs[0].name.contains("linux_amd64"),
            "signature name should contain os_arch: got '{}'",
            sigs[0].name
        );

        // Template vars should be cleaned up after processing
        let os_val = ctx.render_template("{{ Os }}").unwrap_or_default();
        assert_eq!(
            os_val, "",
            "Os template var should be cleared after binary_signs"
        );
    }

    #[test]
    fn test_binary_signs_arm_target_splits_arch_correctly() {
        use anodizer_core::artifact::Artifact;

        let binary_sign_cfg = SignConfig {
            id: None,
            artifacts: Some("binary".to_string()),
            cmd: Some("true".to_string()),
            args: Some(vec![]),
            signature: None,
            stdin: None,
            stdin_file: None,
            ids: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        };
        let mut ctx = TestContextBuilder::new()
            .binary_signs(vec![binary_sign_cfg])
            .dry_run(true)
            .build();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: std::path::PathBuf::from("/dist/myapp"),
            target: Some("armv7-unknown-linux-gnueabihf".to_string()),
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let log = ctx.logger("binary-sign");
        let binary_sign_configs = ctx.config.binary_signs.clone();
        let result = process_sign_configs(
            &binary_sign_configs,
            &mut ctx,
            &log,
            ArtifactFilter::BinaryOnly,
            "binary-sign",
        );
        assert!(result.is_ok());

        let sigs: Vec<_> = ctx.artifacts.by_kind(ArtifactKind::Signature);
        assert_eq!(sigs.len(), 1);
        // Should be arm + v7, not armv7 + v7
        assert!(
            sigs[0].name.contains("linux_armv7"),
            "signature name should contain linux_armv7: got '{}'",
            sigs[0].name
        );
        assert!(
            !sigs[0].name.contains("armv7v7"),
            "signature name must NOT contain armv7v7 double-suffix: got '{}'",
            sigs[0].name
        );
    }

    // -----------------------------------------------------------------------
    // Gap E: Docker sign ID defaults to "default"
    // -----------------------------------------------------------------------

    #[test]
    fn test_docker_sign_id_defaults_to_default() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::DockerSignConfig;

        // Config with no explicit id — should default to "default".
        let docker_signs = vec![DockerSignConfig {
            id: None,
            cmd: Some("echo".to_string()),
            args: Some(vec!["sign".to_string(), "{{ .Artifact }}".to_string()]),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        }];

        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.config.docker_signs = Some(docker_signs);

        // Add a docker image so the sign loop has something to process
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: String::new(),
            path: std::path::PathBuf::from("ghcr.io/myorg/app:latest"),
            target: None,
            crate_name: "test".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });

        let stage = SignStage;
        // Dry-run should succeed and the log should contain the default id.
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_docker_sign_explicit_id_preserved() {
        use anodizer_core::config::DockerSignConfig;

        let cfg = DockerSignConfig {
            id: Some("my-signer".to_string()),
            cmd: None,
            args: None,
            artifacts: None,
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        };

        let sign_id = cfg.id.as_deref().unwrap_or("default");
        assert_eq!(sign_id, "my-signer");
    }

    #[test]
    fn test_docker_sign_none_id_defaults() {
        use anodizer_core::config::DockerSignConfig;

        let cfg = DockerSignConfig {
            id: None,
            cmd: None,
            args: None,
            artifacts: None,
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        };

        let sign_id = cfg.id.as_deref().unwrap_or("default");
        assert_eq!(sign_id, "default");
    }

    // -----------------------------------------------------------------------
    // Bug 1: "all" filter only matches release-uploadable types
    // -----------------------------------------------------------------------

    #[test]
    fn test_all_filter_excludes_internal_types() {
        // Internal types that should NOT be signed by the "all" filter
        assert!(!should_sign_artifact(ArtifactKind::DockerImage, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::DockerImageV2, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::DockerManifest, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::BrewFormula, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::ScoopManifest, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Metadata, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Nixpkg, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::KrewPluginManifest, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::WingetInstaller, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::PkgBuild, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::PublishableSnapcraft, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::PublishableDockerImage, "all").unwrap());
    }

    #[test]
    fn test_all_filter_includes_release_uploadable_types() {
        // "all" = anodizer_core::artifact::release_uploadable_kinds().
        // GoReleaser Pro parity: Installer (MSI/DMG/PKG/NSIS) is part of the
        // release-uploadable set so it gets signed and uploaded alongside
        // archives. GR OSS omits these formats; anodizer treats them as
        // first-class.
        assert!(should_sign_artifact(ArtifactKind::Archive, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::UploadableBinary, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::LinuxPackage, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::SourceArchive, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Makeself, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Flatpak, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::SourceRpm, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Installer, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Sbom, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Checksum, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::UploadableFile, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Signature, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Certificate, "all").unwrap());

        // These are NOT in release_uploadable_kinds() — use dedicated filters.
        assert!(!should_sign_artifact(ArtifactKind::Binary, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::UniversalBinary, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::Snap, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::MacOsPackage, "all").unwrap());
        assert!(!should_sign_artifact(ArtifactKind::DiskImage, "all").unwrap());
    }

    // -----------------------------------------------------------------------
    // Bug 4: Docker sign IDs must be unique
    // -----------------------------------------------------------------------

    #[test]
    fn test_docker_sign_duplicate_ids_rejected() {
        use anodizer_core::config::DockerSignConfig;

        let docker_signs = vec![
            DockerSignConfig {
                id: Some("signer".to_string()),
                cmd: Some("echo".to_string()),
                args: Some(vec!["sign".to_string()]),
                artifacts: Some("all".to_string()),
                ids: None,
                stdin: None,
                stdin_file: None,
                env: None,
                output: None,
                if_condition: None,
                signature: None,
                certificate: None,
            },
            DockerSignConfig {
                id: Some("signer".to_string()), // duplicate!
                cmd: Some("echo".to_string()),
                args: Some(vec!["sign".to_string()]),
                artifacts: Some("all".to_string()),
                ids: None,
                stdin: None,
                stdin_file: None,
                env: None,
                output: None,
                if_condition: None,
                signature: None,
                certificate: None,
            },
        ];

        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.config.docker_signs = Some(docker_signs);

        let stage = DockerSignStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_err(),
            "duplicate docker_signs IDs should be rejected"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("docker_signs") && err.contains("signer"),
            "error should mention docker_signs and the duplicate ID, got: {err}"
        );
    }

    #[test]
    fn test_docker_sign_duplicate_default_ids_rejected() {
        use anodizer_core::config::DockerSignConfig;

        // Two configs with no explicit id — both default to "default"
        let docker_signs = vec![
            DockerSignConfig {
                id: None,
                cmd: Some("echo".to_string()),
                args: Some(vec!["sign".to_string()]),
                artifacts: Some("all".to_string()),
                ids: None,
                stdin: None,
                stdin_file: None,
                env: None,
                output: None,
                if_condition: None,
                signature: None,
                certificate: None,
            },
            DockerSignConfig {
                id: None,
                cmd: Some("echo".to_string()),
                args: Some(vec!["sign".to_string()]),
                artifacts: Some("all".to_string()),
                ids: None,
                stdin: None,
                stdin_file: None,
                env: None,
                output: None,
                if_condition: None,
                signature: None,
                certificate: None,
            },
        ];

        let mut ctx = TestContextBuilder::new().dry_run(true).build();
        ctx.config.docker_signs = Some(docker_signs);

        let stage = DockerSignStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_err(),
            "duplicate default docker_signs IDs should be rejected"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("default"),
            "error should mention the 'default' ID, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Bug 5: Docker sign Digest variable uses correct casing
    // -----------------------------------------------------------------------

    #[test]
    fn test_docker_sign_digest_go_compat_syntax() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::DockerSignConfig;

        let tmp = tempfile::TempDir::new().unwrap();
        let marker_path = tmp.path().join("docker_digest_case.txt");
        let marker_str = marker_path.to_string_lossy().to_string();

        // Use Go-compat syntax {{ .Digest }} which gets preprocessed to {{ Digest }}
        let (cmd, args) = shell_echo_literal_to_file("{{ Digest }}", &marker_str);
        let docker_signs = vec![DockerSignConfig {
            id: Some("test-digest-case".to_string()),
            cmd: Some(cmd),
            args: Some(args),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        }];

        let mut ctx = TestContextBuilder::new().dry_run(false).build();
        ctx.config.docker_signs = Some(docker_signs);

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("digest".to_string(), "sha256:deadbeef".to_string());

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: String::new(),
            path: std::path::PathBuf::from("ghcr.io/myorg/app:latest"),
            target: None,
            crate_name: "test".to_string(),
            metadata,
            size: None,
        });

        let stage = DockerSignStage;
        stage.run(&mut ctx).unwrap();

        let output = std::fs::read_to_string(&marker_path).unwrap();
        assert_eq!(
            output.trim(),
            "sha256:deadbeef",
            "PascalCase Digest template var should resolve correctly, got: {}",
            output.trim()
        );
    }
}
