//! Shared sign processing — the core driver behind both `signs:` (normal
//! artifact signing) and `binary_signs:` (per-binary signing). Owns the
//! `SignJob` value type, the `process_sign_configs` driver, and the
//! parallel-execution wrapper.
//!
//! Split out from `lib.rs` so the per-job flow (filter → render → execute)
//! is independently reviewable without scrolling through SignStage glue.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::SignConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::target::map_target;

use crate::helpers::{
    default_sign_cmd, expand_shell_vars, prepare_stdin_from, resolve_sign_args,
    resolve_signature_path, should_sign_artifact,
};

/// Artifact filter mode for `process_sign_configs`.
#[derive(Clone, Copy)]
pub(crate) enum ArtifactFilter {
    /// Use the `artifacts` field from each SignConfig (or default to "none").
    FromConfig,
    /// Always restrict to `ArtifactKind::Binary`, regardless of config.
    BinaryOnly,
}

/// A fully-prepared sign job ready for parallel execution.
///
/// All template rendering and path resolution is done up-front so that the
/// actual signing command can be spawned without borrowing the `Context`.
struct SignJob {
    /// The signing command binary (e.g., "gpg", "cosign").
    cmd: String,
    /// Fully-resolved command arguments.
    args: Vec<String>,
    /// Optional stdin content to pipe to the signing command.
    stdin_data: Option<Vec<u8>>,
    /// Optional environment variables to set on the child process, ordered.
    env: Option<Vec<(String, String)>>,
    /// Human-readable label for log messages (e.g., "sign", "binary-sign").
    label: String,
    /// The sign config's `id` field for log messages.
    id_label: String,
    /// Display string for the artifact being signed (used in log messages).
    artifact_display: String,
    /// Display string for the signature output path (used in log messages).
    signature_display: String,
    /// Whether to capture and log the command's stdout/stderr.
    output_flag: bool,
    /// Artifact registrations to add after signing (signature + optional certificate).
    new_artifacts: Vec<anodizer_core::artifact::Artifact>,
}

/// Execute a single prepared sign job, returning `Ok(())` on success.
fn execute_sign_job(job: &SignJob, log: &StageLogger) -> Result<()> {
    log.status(&format!(
        "[{}] {} {} -> {}",
        job.id_label, job.label, job.artifact_display, job.signature_display
    ));

    let stdin_cfg = if job.stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::inherit()
    };

    let mut command = Command::new(&job.cmd);
    command
        .args(&job.args)
        .stdin(stdin_cfg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(ref env_vars) = job.env {
        for (k, v) in env_vars {
            command.env(k, v);
        }
    }

    let mut child = command.spawn().with_context(|| {
        format!(
            "{}: failed to spawn '{}' for {}",
            job.label, job.cmd, job.artifact_display
        )
    })?;

    if let Some(ref data) = job.stdin_data {
        if let Some(mut child_stdin) = child.stdin.take() {
            child_stdin.write_all(data).with_context(|| {
                format!(
                    "{}: failed to write stdin for {}",
                    job.label, job.artifact_display
                )
            })?;
            drop(child_stdin); // Explicitly close stdin so child sees EOF
        } else {
            log.warn(&format!(
                "{}: stdin data provided but child process stdin unavailable for {}",
                job.label, job.artifact_display
            ));
        }
    }

    let output = child.wait_with_output().with_context(|| {
        format!(
            "{}: failed to wait for '{}' for {}",
            job.label, job.cmd, job.artifact_display
        )
    })?;

    // Redact secrets from stdout/stderr before any output or logging.
    // Collect env vars: custom env from the job + process environment.
    let env_pairs: Vec<(String, String)> = job
        .env
        .iter()
        .flat_map(|m| m.iter().cloned())
        .chain(std::env::vars())
        .collect();

    let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_raw = String::from_utf8_lossy(&output.stderr).to_string();

    let stdout_str = anodizer_core::redact::redact_string(&stdout_raw, &env_pairs);
    let stderr_str = anodizer_core::redact::redact_string(&stderr_raw, &env_pairs);

    if job.output_flag {
        if !stdout_str.is_empty() {
            log.status(&format!("[{} stdout] {}", job.label, stdout_str.trim()));
        }
        if !stderr_str.is_empty() {
            log.status(&format!("[{} stderr] {}", job.label, stderr_str.trim()));
        }
    }

    let mut redacted_output = output;
    redacted_output.stdout = stdout_str.into_bytes();
    redacted_output.stderr = stderr_str.into_bytes();

    log.check_output(redacted_output, &job.cmd)?;
    Ok(())
}

/// Process a list of `SignConfig` entries against a set of artifacts, executing
/// the signing command for each matching artifact.  This is the shared
/// implementation behind both the `signs` and `binary_signs` top-level config
/// sections.
///
/// Signing commands are executed in parallel using `std::thread::scope` with
/// chunked parallelism (similar to the build stage), since each signing
/// invocation is an independent external process.
pub(crate) fn process_sign_configs(
    sign_configs: &[SignConfig],
    ctx: &mut Context,
    log: &StageLogger,
    filter_mode: ArtifactFilter,
    label: &str,
) -> Result<()> {
    let parallelism = std::cmp::max(
        1,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4),
    );

    for (sign_idx, sign_cfg) in sign_configs.iter().enumerate() {
        let sub_label = sign_cfg
            .id
            .clone()
            .unwrap_or_else(|| format!("{}[{}]", label, sign_idx));

        // Evaluate the `if` conditional template — skip when rendered
        // result is "false" or empty/whitespace-only.
        if let Some(ref condition) = sign_cfg.if_condition {
            match ctx.render_template(condition) {
                Ok(result) => {
                    let trimmed = result.trim();
                    if trimmed.is_empty() || trimmed == "false" {
                        let reason = format!("if condition evaluated to '{}'", trimmed);
                        log.verbose(&format!(
                            "skipping {} config '{}': {}",
                            label, sub_label, reason
                        ));
                        ctx.remember_skip(label, &sub_label, &reason);
                        continue;
                    }
                }
                Err(e) => {
                    anyhow::bail!(
                        "{} '{}': if condition render failed ({}): {}",
                        label,
                        sub_label,
                        condition,
                        e
                    );
                }
            }
        }

        let config_filter = sign_cfg.resolved_artifacts(match filter_mode {
            ArtifactFilter::FromConfig => SignConfig::DEFAULT_ARTIFACTS,
            ArtifactFilter::BinaryOnly => SignConfig::DEFAULT_ARTIFACTS_BINARY,
        });

        if sign_cfg.ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
            if config_filter == "checksum" {
                log.warn("when artifacts is `checksum`, `ids` has no effect. ignoring");
            } else if config_filter == "source" {
                log.warn("when artifacts is `source`, `ids` has no effect. ignoring");
            }
        }

        if config_filter == "none" {
            log.verbose(&format!(
                "skipping {} config '{}': artifacts: none",
                label, sub_label
            ));
            ctx.remember_skip(label, &sub_label, "artifacts: none");
            continue;
        }

        let cmd = sign_cfg
            .cmd
            .as_deref()
            .map(|s| s.to_string())
            .unwrap_or_else(default_sign_cmd);

        if sign_cfg.args.as_ref().is_some_and(|a| a.is_empty()) {
            log.warn(&format!(
                "{} config has empty args — did you mean to omit args for defaults?",
                label
            ));
        }

        let args = sign_cfg.resolved_args();

        type ArtifactEntry = (
            std::path::PathBuf,
            String,
            std::collections::HashMap<String, String>,
            Option<String>,
        );
        let artifact_paths: Vec<ArtifactEntry> = {
            let mut matched = Vec::new();
            for a in ctx.artifacts.all().iter() {
                match filter_mode {
                    ArtifactFilter::FromConfig => {
                        if !should_sign_artifact(a.kind, config_filter)? {
                            continue;
                        }
                    }
                    ArtifactFilter::BinaryOnly => {
                        if a.kind != ArtifactKind::Binary {
                            continue;
                        }
                    }
                }
                if let Some(ref ids) = sign_cfg.ids {
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
                    if !(matches_id || matches_name) {
                        continue;
                    }
                }
                matched.push((
                    a.path.clone(),
                    a.crate_name.clone(),
                    a.metadata.clone(),
                    a.target.clone(),
                ));
            }
            matched
        };

        let mut sign_jobs: Vec<SignJob> = Vec::new();

        let default_sig_template: &str = match filter_mode {
            ArtifactFilter::BinaryOnly => SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
            ArtifactFilter::FromConfig => SignConfig::DEFAULT_SIGNATURE_TEMPLATE,
        };

        for (artifact_path, artifact_crate_name, artifact_metadata, artifact_target) in
            &artifact_paths
        {
            let artifact_str = artifact_path.to_string_lossy();
            let artifact_name = artifact_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let artifact_id = artifact_metadata
                .get("id")
                .map(|s| s.as_str())
                .unwrap_or("");

            if matches!(filter_mode, ArtifactFilter::BinaryOnly) {
                if let Some(target) = artifact_target {
                    let (os, arch) = map_target(target);
                    ctx.template_vars_mut().set("Os", &os);
                    if let Some(version) = arch.strip_prefix("armv") {
                        ctx.template_vars_mut().set("Arch", "arm");
                        ctx.template_vars_mut().set("Arm", version);
                    } else {
                        ctx.template_vars_mut().set("Arch", &arch);
                        ctx.template_vars_mut().set("Arm", "");
                    }
                    let amd64 = if arch == "amd64" {
                        artifact_metadata
                            .get("amd64_level")
                            .map(|s| s.as_str())
                            .unwrap_or("v1")
                    } else {
                        ""
                    };
                    ctx.template_vars_mut().set("Amd64", amd64);
                    let mips = artifact_metadata
                        .get("mips_variant")
                        .map(|s| s.as_str())
                        .unwrap_or("");
                    ctx.template_vars_mut().set("Mips", mips);
                } else {
                    ctx.template_vars_mut().set("Os", "");
                    ctx.template_vars_mut().set("Arch", "");
                    ctx.template_vars_mut().set("Arm", "");
                    ctx.template_vars_mut().set("Amd64", "");
                    ctx.template_vars_mut().set("Mips", "");
                }
            }

            let signature_str =
                resolve_signature_path(sign_cfg, &artifact_str, ctx, log, default_sig_template)?;

            let certificate_str = sign_cfg
                .certificate
                .as_ref()
                .map(|tmpl| {
                    let preprocessed = tmpl
                        .replace("{{ .Artifact }}", &artifact_str)
                        .replace("{{ Artifact }}", &artifact_str);
                    ctx.render_template(&preprocessed).with_context(|| {
                        format!(
                            "sign: render certificate template '{}' for artifact {}",
                            tmpl, artifact_str
                        )
                    })
                })
                .transpose()?;

            let certificate_for_vars = certificate_str.clone();
            let shell_vars: HashMap<&str, &str> = HashMap::from([
                ("artifact", artifact_str.as_ref()),
                ("signature", signature_str.as_str()),
                ("certificate", certificate_for_vars.as_deref().unwrap_or("")),
                (
                    "digest",
                    artifact_metadata
                        .get("digest")
                        .map(|s| s.as_str())
                        .unwrap_or(""),
                ),
                ("artifactName", artifact_name),
                ("artifactID", artifact_id),
            ]);

            let signature_str = expand_shell_vars(&signature_str, &shell_vars);
            let certificate_str = certificate_str.map(|c| expand_shell_vars(&c, &shell_vars));

            let resolved = resolve_sign_args(
                &args,
                artifact_str.as_ref(),
                &signature_str,
                certificate_str.as_deref(),
            );

            let fully_resolved: Vec<String> = resolved
                .iter()
                .map(|arg| -> Result<String> {
                    let rendered = ctx
                        .render_template(arg)
                        .with_context(|| format!("sign: render {} arg '{}'", label, arg))?;
                    Ok(expand_shell_vars(&rendered, &shell_vars))
                })
                .collect::<Result<Vec<_>>>()?;

            let dist = &ctx.config.dist;
            let sig_path = {
                let resolved = std::path::PathBuf::from(&signature_str);
                if !resolved.starts_with(dist) {
                    dist.join(&resolved)
                } else {
                    resolved
                }
            };
            let mut sig_metadata = std::collections::HashMap::new();
            sig_metadata.insert("type".to_string(), "Signature".to_string());
            let sig_name = sig_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| sig_path.display().to_string());
            let mut job_artifacts = vec![anodizer_core::artifact::Artifact {
                kind: ArtifactKind::Signature,
                name: sig_name,
                path: sig_path,
                target: None,
                crate_name: artifact_crate_name.clone(),
                metadata: sig_metadata,
                size: None,
            }];

            if let Some(ref cert_path_str) = certificate_str {
                let cert_resolved = std::path::PathBuf::from(cert_path_str);
                let cert_path = if !cert_resolved.starts_with(dist) {
                    dist.join(&cert_resolved)
                } else {
                    cert_resolved
                };
                let cert_name = cert_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| cert_path.display().to_string());
                let mut cert_metadata = std::collections::HashMap::new();
                cert_metadata.insert("type".to_string(), "Certificate".to_string());
                job_artifacts.push(anodizer_core::artifact::Artifact {
                    kind: ArtifactKind::Certificate,
                    name: cert_name,
                    path: cert_path,
                    target: None,
                    crate_name: artifact_crate_name.clone(),
                    metadata: cert_metadata,
                    size: None,
                });
            }

            if ctx.is_dry_run() {
                log.status(&format!(
                    "(dry-run) would run: {} {}",
                    cmd,
                    fully_resolved.join(" ")
                ));
                for artifact in job_artifacts {
                    ctx.artifacts.add(artifact);
                }
                continue;
            }

            let (_, stdin_data) = prepare_stdin_from(
                sign_cfg.stdin.as_deref(),
                sign_cfg.stdin_file.as_deref(),
                label,
            )?;

            let mut rendered_env: Vec<(String, String)> = sign_cfg
                .env
                .as_deref()
                .map(|env_list| {
                    anodizer_core::config::render_env_entries(env_list, |v| ctx.render_template(v))
                        .with_context(|| format!("sign[{label}]: render env entries"))
                })
                .transpose()?
                .unwrap_or_default();

            for (k, v) in shell_vars.iter() {
                if v.is_empty() {
                    continue;
                }
                if !rendered_env.iter().any(|(ek, _)| ek == *k) {
                    rendered_env.push(((*k).to_string(), (*v).to_string()));
                }
            }

            let rendered_env = if rendered_env.is_empty() {
                None
            } else {
                Some(rendered_env)
            };

            sign_jobs.push(SignJob {
                cmd: cmd.clone(),
                args: fully_resolved,
                stdin_data,
                env: rendered_env,
                label: label.to_string(),
                id_label: sign_cfg.resolved_id().to_string(),
                artifact_display: artifact_str.to_string(),
                signature_display: signature_str.clone(),
                output_flag: match sign_cfg.output.as_ref() {
                    Some(s) => s
                        .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                        .with_context(|| "sign: render output template")?,
                    None => false,
                },
                new_artifacts: job_artifacts,
            });
        }

        if !sign_jobs.is_empty() {
            log.status(&format!(
                "signing {} artifacts with parallelism={}",
                sign_jobs.len(),
                parallelism
            ));
        }

        let mut all_new_artifacts: Vec<anodizer_core::artifact::Artifact> = Vec::new();

        let static_label = label_to_static(label);
        let verbosity = log.verbosity();
        let stage_name: &'static str = match static_label {
            "binary-sign" => "binary-sign",
            _ => "sign",
        };
        anodizer_core::parallel::run_parallel_chunks(&sign_jobs, parallelism, stage_name, |job| {
            let thread_log = anodizer_core::log::StageLogger::new(static_label, verbosity);
            execute_sign_job(job, &thread_log)
        })?;

        for job in &sign_jobs {
            all_new_artifacts.extend(job.new_artifacts.iter().cloned());
        }

        for artifact in all_new_artifacts {
            ctx.artifacts.add(artifact);
        }
    }

    if matches!(filter_mode, ArtifactFilter::BinaryOnly) {
        ctx.template_vars_mut().set("Os", "");
        ctx.template_vars_mut().set("Arch", "");
        ctx.template_vars_mut().set("Arm", "");
        ctx.template_vars_mut().set("Amd64", "");
        ctx.template_vars_mut().set("Mips", "");
    }

    Ok(())
}

/// Convert a runtime label string to a `&'static str` for `StageLogger::new`.
fn label_to_static(label: &str) -> &'static str {
    match label {
        "sign" => "sign",
        "binary-sign" => "binary-sign",
        _ => "sign",
    }
}
