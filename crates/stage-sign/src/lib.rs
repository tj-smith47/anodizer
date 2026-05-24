use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

mod helpers;
mod process;

use helpers::{prepare_stdin_from, resolve_sign_args};
use process::{ArtifactFilter, process_sign_configs};

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
            let id = cfg.resolved_id();
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
                let id = cfg.resolved_id();
                if !seen.insert(id.to_string()) {
                    anyhow::bail!("found 2 signs with the ID '{}'", id);
                }
            }
            let mut seen_bin = std::collections::HashSet::new();
            for cfg in &ctx.config.binary_signs {
                let id = cfg.resolved_id();
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
                    let id = cfg.resolved_id();
                    if !seen_docker.insert(id.to_string()) {
                        anyhow::bail!("found 2 docker_signs with the ID '{}'", id);
                    }
                }
            }

            for docker_sign_cfg in &docker_signs {
                let sign_id = docker_sign_cfg.resolved_id();

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

                let cmd = docker_sign_cfg.resolved_cmd().to_string();

                let args = docker_sign_cfg.resolved_args();

                let docker_filter = docker_sign_cfg.resolved_artifacts();

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

                    // Propagate template render errors instead of silently
                    // falling back to the unrendered template string —
                    // passing a literal `{{ Artifact }}` to `cosign sign`
                    // would sign the wrong reference (or fail opaquely).
                    // Sibling `binary-sign` / `sign` path (process.rs) uses
                    // the same `?`-propagation shape.
                    let fully_resolved: Vec<String> = resolved
                        .iter()
                        .map(|arg| {
                            ctx.render_template(arg).with_context(|| {
                                format!("docker-sign [{}]: render arg '{}'", sign_id, arg)
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;

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

                    // Parse and render docker-sign env in one pass; propagate errors
                    // instead of silently falling back to unrendered template strings.
                    // The rendered pairs are reused for redaction below.
                    let docker_rendered_env: Vec<(String, String)> =
                        anodizer_core::config::render_env_entries(
                            docker_sign_cfg.env.as_deref().unwrap_or(&[]),
                            |v| ctx.render_template(v),
                        )
                        .with_context(|| "docker-sign: render env entries")?;

                    for (k, v) in &docker_rendered_env {
                        command.env(k, v);
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
                    // Use the already-rendered env pairs (rendered values are what
                    // actually appear in command output, so redact those).
                    let docker_env_pairs: Vec<(String, String)> = docker_rendered_env
                        .into_iter()
                        .chain(std::env::vars())
                        .collect();

                    let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr_raw = String::from_utf8_lossy(&output.stderr).to_string();

                    let stdout_str = anodizer_core::redact::string(&stdout_raw, &docker_env_pairs);
                    let stderr_str = anodizer_core::redact::string(&stderr_raw, &docker_env_pairs);

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
mod tests;
