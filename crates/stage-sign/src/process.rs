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

use anodizer_core::EnvSource;
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
    // Per-artifact detail — at default verbosity the `signing N artifacts`
    // summary (emitted once before this loop) is the status-level signal; the
    // per-artifact `sign X -> Y` line would flood the log on wide fan-outs.
    log.verbose(&format!(
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

    let stdout_str = anodizer_core::redact::string(&stdout_raw, &env_pairs);
    let stderr_str = anodizer_core::redact::string(&stderr_raw, &env_pairs);

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
        // result is falsy. Render failure hard-errors.
        let proceed = anodizer_core::config::evaluate_if_condition(
            sign_cfg.if_condition.as_deref(),
            &format!("{label} '{sub_label}'"),
            |t| ctx.render_template(t),
        )?;
        if !proceed {
            let reason = "`if` condition evaluated falsy".to_string();
            log.verbose(&format!(
                "skipping {} config '{}': {}",
                label, sub_label, reason
            ));
            ctx.remember_skip(label, &sub_label, &reason);
            continue;
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
            // Invariant: every value below is supplied by anodizer itself,
            // not by raw user input. Sources:
            //   - artifact / artifactName: stage-derived path / basename of
            //     an Artifact produced upstream (build/archive/etc.).
            //   - signature / certificate: rendered from sign-stage
            //     templates against the controlled template var set, then
            //     joined with a `dist/` prefix below if not already
            //     absolute.
            //   - digest / artifactID: read from artifact metadata, also
            //     populated by stages (no direct config write surface).
            // Values feed `Command::args` (no shell), so shell metacharacters
            // (`;`, backticks, `$()`) cannot escape into a subshell. Keep
            // this invariant in mind when adding new entries — anything
            // user-controllable that reaches argv must still be free of
            // path-traversal / option-injection risk.
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

            // Empty rendered args (from conditional Tera blocks that
            // evaluated to "") are dropped — passing them to the signer
            // as empty positional args confuses gpg.
            let mut fully_resolved: Vec<String> = resolved
                .iter()
                .map(|arg| -> Result<Option<String>> {
                    let rendered = ctx
                        .render_template(arg)
                        .with_context(|| format!("sign: render {} arg '{}'", label, arg))?;
                    let expanded = expand_shell_vars(&rendered, &shell_vars);
                    if expanded.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(expanded))
                    }
                })
                .filter_map(|r| r.transpose())
                .collect::<Result<Vec<_>>>()?;

            inject_gpg_faked_system_time(&cmd, &mut fully_resolved, ctx.env_source());

            let dist = &ctx.config.dist;
            let sig_path = {
                let resolved = std::path::PathBuf::from(&signature_str);
                if !resolved.starts_with(dist) {
                    dist.join(&resolved)
                } else {
                    resolved
                }
            };
            let is_binary_sign = matches!(filter_mode, ArtifactFilter::BinaryOnly);
            let mut sig_metadata = std::collections::HashMap::new();
            sig_metadata.insert("type".to_string(), "Signature".to_string());
            if is_binary_sign {
                sig_metadata.insert("binary_sign".to_string(), "true".to_string());
            }
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
                if is_binary_sign {
                    cert_metadata.insert("binary_sign".to_string(), "true".to_string());
                }
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

/// Inject `--faked-system-time=<SOURCE_DATE_EPOCH>!` after the first
/// arg when `cmd` is gpg and SDE is set, so the OpenPGP signature
/// packet's creation timestamp is pinned. With an EdDSA key this gives
/// byte-identical detached signatures across runs (RFC 8032). No-op if
/// the user already supplied `--faked-system-time`.
fn inject_gpg_faked_system_time(cmd: &str, args: &mut Vec<String>, env: &dyn EnvSource) {
    if cmd != "gpg" {
        return;
    }
    let Some(sde) = env.var("SOURCE_DATE_EPOCH") else {
        return;
    };
    if args
        .iter()
        .any(|a| a == "--faked-system-time" || a.starts_with("--faked-system-time="))
    {
        return;
    }
    let injection = format!("--faked-system-time={}!", sde);
    let insert_at = if args.is_empty() { 0 } else { 1 };
    args.insert(insert_at, injection);
}

#[cfg(test)]
mod faked_time_tests {
    use super::inject_gpg_faked_system_time;
    use anodizer_core::MapEnvSource;

    fn env_with_sde(value: &str) -> MapEnvSource {
        MapEnvSource::new().with("SOURCE_DATE_EPOCH", value)
    }

    fn env_without_sde() -> MapEnvSource {
        MapEnvSource::new()
    }

    #[test]
    fn injects_after_first_arg_for_gpg_with_sde() {
        let env = env_with_sde("1715000000");
        let mut args = vec![
            "--batch".into(),
            "--local-user".into(),
            "ABCD".into(),
            "--detach-sig".into(),
            "file".into(),
        ];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        assert_eq!(args[0], "--batch");
        assert_eq!(args[1], "--faked-system-time=1715000000!");
        assert_eq!(args[2], "--local-user");
    }

    #[test]
    fn no_inject_when_sde_unset() {
        let env = env_without_sde();
        let mut args = vec!["--batch".into(), "--detach-sig".into()];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        assert_eq!(args, vec!["--batch".to_string(), "--detach-sig".into()]);
    }

    #[test]
    fn no_inject_when_cmd_is_not_gpg() {
        let env = env_with_sde("1715000000");
        let mut args = vec!["sign-blob".into(), "--key=env://KEY".into()];
        inject_gpg_faked_system_time("cosign", &mut args, &env);
        assert_eq!(
            args,
            vec!["sign-blob".to_string(), "--key=env://KEY".into()]
        );
    }

    #[test]
    fn no_inject_when_user_already_passed_faked_system_time() {
        let env = env_with_sde("1715000000");
        let mut args = vec![
            "--batch".into(),
            "--faked-system-time=999!".into(),
            "--detach-sig".into(),
        ];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        let count = args
            .iter()
            .filter(|a| a.starts_with("--faked-system-time"))
            .count();
        assert_eq!(count, 1);
        assert_eq!(args[1], "--faked-system-time=999!");
    }

    #[test]
    fn injects_at_position_zero_when_args_empty() {
        let env = env_with_sde("42");
        let mut args: Vec<String> = vec![];
        inject_gpg_faked_system_time("gpg", &mut args, &env);
        assert_eq!(args, vec!["--faked-system-time=42!".to_string()]);
    }
}
