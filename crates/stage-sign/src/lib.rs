use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result};

use anodize_core::artifact::ArtifactKind;
use anodize_core::config::SignConfig;
use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns `true` if an artifact of `kind` should be signed given the `filter`
/// string from `SignConfig::artifacts` / `DockerSignConfig::artifacts`.
///
/// Filter values:
/// - `"none"`          → nothing is signed
/// - `"all"`           → every artifact kind is signed
/// - `"source"`        → only `ArtifactKind::SourceArchive`
/// - `"archive"`       → only `ArtifactKind::Archive`
/// - `"binary"`        → only `ArtifactKind::Binary`
/// - `"package"`       → only `ArtifactKind::LinuxPackage`
/// - `"installer"`     → only `ArtifactKind::Installer`
/// - `"diskimage"`     → only `ArtifactKind::DiskImage`
/// - `"sbom"`          → only `ArtifactKind::Sbom`
/// - `"snap"`          → only `ArtifactKind::Snap`
/// - `"macos_package"` → only `ArtifactKind::MacOsPackage`
/// - `"checksum"`      → only `ArtifactKind::Checksum`
///
/// Any other value returns an error.
pub(crate) fn should_sign_artifact(kind: ArtifactKind, filter: &str) -> Result<bool> {
    match filter {
        "none" => Ok(false),
        "all" => Ok(true),
        "source" => Ok(kind == ArtifactKind::SourceArchive),
        "archive" => Ok(kind == ArtifactKind::Archive),
        "binary" => Ok(kind == ArtifactKind::Binary),
        "package" => Ok(kind == ArtifactKind::LinuxPackage),
        "installer" => Ok(kind == ArtifactKind::Installer),
        "diskimage" => Ok(kind == ArtifactKind::DiskImage),
        "sbom" => Ok(kind == ArtifactKind::Sbom),
        "snap" => Ok(kind == ArtifactKind::Snap),
        "macos_package" => Ok(kind == ArtifactKind::MacOsPackage),
        "checksum" => Ok(kind == ArtifactKind::Checksum),
        other => anyhow::bail!("invalid sign artifacts filter: {other}"),
    }
}

/// Resolve the signature output path from a `SignConfig::signature` template
/// or fall back to the default `{artifact}.sig`.
fn resolve_signature_path(
    sign_cfg: &SignConfig,
    artifact_path: &str,
    ctx: &Context,
    log: &StageLogger,
) -> String {
    if let Some(ref sig_template) = sign_cfg.signature {
        // Set Artifact as a template variable so Tera can resolve it natively.
        // Also do a Go-compat string replacement for {{ .Artifact }} patterns
        // that may appear alongside Tera expressions.
        let preprocessed = sig_template
            .replace("{{ .Artifact }}", artifact_path)
            .replace("{{ Artifact }}", artifact_path);
        ctx.render_template(&preprocessed).unwrap_or_else(|e| {
            log.warn(&format!(
                "failed to render signature template '{}': {}, falling back to {}.sig",
                sig_template, e, artifact_path
            ));
            format!("{}.sig", artifact_path)
        })
    } else {
        format!("{}.sig", artifact_path)
    }
}

/// Pipe `stdin_content` or the contents of `stdin_file` to a child process's
/// stdin. Returns the appropriate `Stdio` and an optional content buffer.
///
/// Shared by both `SignConfig` and `DockerSignConfig` — both expose the same
/// `stdin` / `stdin_file` fields.
fn prepare_stdin_from(
    stdin: Option<&str>,
    stdin_file: Option<&str>,
    label: &str,
) -> Result<(Stdio, Option<Vec<u8>>)> {
    if let Some(content) = stdin {
        Ok((Stdio::piped(), Some(content.as_bytes().to_vec())))
    } else if let Some(path) = stdin_file {
        let data = std::fs::read(path)
            .with_context(|| format!("{}: failed to read stdin_file '{}'", label, path))?;
        Ok((Stdio::piped(), Some(data)))
    } else {
        Ok((Stdio::inherit(), None))
    }
}

/// Determine the default signing command by checking `git config gpg.program`
/// first, falling back to "gpg" if unset or unavailable.
fn default_sign_cmd() -> String {
    if let Ok(output) = std::process::Command::new("git")
        .args(["config", "gpg.program"])
        .output()
    {
        let cmd = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !cmd.is_empty() {
            return cmd;
        }
    }
    "gpg".to_string()
}

/// Expand shell-style variable references (`$var` and `${var}`) in a string.
///
/// GoReleaser supports both Go template syntax and shell variable syntax in
/// signing args, signature paths, and certificate paths.
///
/// Uses single-pass replacement to prevent double-substitution: if `$artifact`
/// expands to a value containing `$signature`, the `$signature` in the expanded
/// value is NOT re-expanded.
fn expand_shell_vars(s: &str, vars: &HashMap<&str, &str>) -> String {
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '$' {
            // Try `${var}` syntax first.
            if i + 1 < len && chars[i + 1] == '{'
                && let Some(close) = chars[i + 2..].iter().position(|&c| c == '}')
            {
                let var_name: String = chars[i + 2..i + 2 + close].iter().collect();
                if let Some(&val) = vars.get(var_name.as_str()) {
                    result.push_str(val);
                    i += 2 + close + 1; // skip past '}'
                    continue;
                }
            }
            // Try `$var` syntax: match the longest variable name at this position.
            // Sort candidates longest-first to prefer `$artifactName` over `$artifact`.
            let remaining: String = chars[i + 1..].iter().collect();
            let mut matched = false;
            let mut candidates: Vec<&&str> = vars.keys().collect();
            candidates.sort_by_key(|b| std::cmp::Reverse(b.len()));
            for key in candidates {
                if remaining.starts_with(*key) {
                    result.push_str(vars[key]);
                    i += 1 + key.len();
                    matched = true;
                    break;
                }
            }
            if !matched {
                result.push(chars[i]);
                i += 1;
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

/// Replace `{{ .Artifact }}`, `{{ .Signature }}`, and `{{ .Certificate }}`
/// placeholders in each arg.
pub(crate) fn resolve_sign_args(
    args: &[String],
    artifact_path: &str,
    signature_path: &str,
    certificate_path: Option<&str>,
) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let mut resolved = arg
                .replace("{{ .Artifact }}", artifact_path)
                .replace("{{ .Signature }}", signature_path);
            // Replace certificate placeholder: with actual path if set, empty string otherwise.
            // This prevents `{{ .Certificate }}` from being fed to Tera and causing spurious warnings.
            let cert = certificate_path.unwrap_or("");
            resolved = resolved.replace("{{ .Certificate }}", cert);
            resolved
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared sign processing — used by both `signs` and `binary_signs` loops
// ---------------------------------------------------------------------------

/// Artifact filter mode for `process_sign_configs`.
#[derive(Clone, Copy)]
enum ArtifactFilter {
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
    /// Optional environment variables to set on the child process.
    env: Option<HashMap<String, String>>,
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
    new_artifacts: Vec<anodize_core::artifact::Artifact>,
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
        command.envs(env_vars);
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

    // Capture output BEFORE the error bail so stdout/stderr from a
    // failed signing command is still logged when `output: true`.
    let stdout_str = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();

    if job.output_flag {
        if !stdout_str.is_empty() {
            log.status(&format!("[{} stdout] {}", job.label, stdout_str.trim()));
        }
        if !stderr_str.is_empty() {
            log.status(&format!("[{} stderr] {}", job.label, stderr_str.trim()));
        }
    }

    // Now check exit status (bails on non-zero).
    log.check_output(output, &job.cmd)?;
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
fn process_sign_configs(
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

    for sign_cfg in sign_configs {
        // Evaluate the `if` conditional template — skip when rendered
        // result is "false" or empty/whitespace-only.
        //
        // NOTE: `if_condition` is `Option<String>` with inverted semantics
        // compared to `disable: Option<StringOrBool>`:
        //   - `if_condition`: skip when rendered to "false" or empty (opt-IN)
        //   - `disable`:      skip when rendered to "true" (opt-OUT)
        // This intentional difference mirrors GoReleaser's `if` vs `disable`.
        if let Some(ref condition) = sign_cfg.if_condition {
            match ctx.render_template(condition) {
                Ok(result) => {
                    let trimmed = result.trim();
                    if trimmed.is_empty() || trimmed == "false" {
                        log.verbose(&format!(
                            "skipping {} config: if condition evaluated to '{}'",
                            label, trimmed
                        ));
                        continue;
                    }
                }
                Err(e) => {
                    log.warn(&format!(
                        "if condition render failed ({}), skipping: {}",
                        condition, e
                    ));
                    continue;
                }
            }
        }

        let config_filter = sign_cfg.artifacts.as_deref().unwrap_or(match filter_mode {
            ArtifactFilter::FromConfig => "none",
            ArtifactFilter::BinaryOnly => "binary",
        });

        // For the normal `signs` path, respect the artifacts filter.
        // For `binary_signs`, skip if the config explicitly says "none".
        if config_filter == "none" {
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

        let args = sign_cfg.args.clone().unwrap_or_else(|| {
            vec![
                "--output".to_string(),
                "{{ .Signature }}".to_string(),
                "--detach-sign".to_string(),
                "{{ .Artifact }}".to_string(),
            ]
        });

        // Collect matching artifacts (avoid holding an immutable borrow
        // while we later add new ones, so clone paths up-front).
        let artifact_paths: Vec<(
            std::path::PathBuf,
            String,
            std::collections::HashMap<String, String>,
        )> = {
            let mut matched = Vec::new();
            for a in ctx.artifacts.all().iter() {
                // Apply artifact kind filter.
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
                // If `ids` filter is set, only sign artifacts whose metadata
                // contains a matching "id" or "name" entry.
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
                matched.push((a.path.clone(), a.crate_name.clone(), a.metadata.clone()));
            }
            matched
        };

        // ----------------------------------------------------------------
        // Phase 1: Prepare all sign jobs (template rendering, path
        // resolution).  This requires `&ctx` but produces owned data that
        // can be sent to worker threads.
        // ----------------------------------------------------------------
        let mut sign_jobs: Vec<SignJob> = Vec::new();

        for (artifact_path, artifact_crate_name, artifact_metadata) in &artifact_paths {
            let artifact_str = artifact_path.to_string_lossy();
            let artifact_name = artifact_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let artifact_id = artifact_metadata
                .get("id")
                .map(|s| s.as_str())
                .unwrap_or("");
            let signature_str = resolve_signature_path(sign_cfg, &artifact_str, ctx, log);

            // Resolve the certificate path from template if configured.
            let certificate_str = sign_cfg.certificate.as_ref().map(|tmpl| {
                let preprocessed = tmpl
                    .replace("{{ .Artifact }}", &artifact_str)
                    .replace("{{ Artifact }}", &artifact_str);
                ctx.render_template(&preprocessed).unwrap_or_else(|e| {
                    log.warn(&format!(
                        "failed to render certificate template '{}': {}, using raw value",
                        tmpl, e
                    ));
                    preprocessed
                })
            });

            // Build shell variable map for $artifact / ${artifact} expansion.
            // GoReleaser supports both Go template syntax and shell variable syntax.
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

            // Apply shell variable expansion to signature and certificate paths.
            let signature_str = expand_shell_vars(&signature_str, &shell_vars);
            let certificate_str = certificate_str.map(|c| expand_shell_vars(&c, &shell_vars));

            let resolved = resolve_sign_args(
                &args,
                artifact_str.as_ref(),
                &signature_str,
                certificate_str.as_deref(),
            );

            // Also resolve any remaining template variables (e.g., {{ .Env.GPG_FINGERPRINT }})
            let fully_resolved: Vec<String> = resolved
                .iter()
                .map(|arg| {
                    let rendered = ctx.render_template(arg).unwrap_or_else(|e| {
                        log.warn(&format!(
                            "failed to render {} arg '{}': {}, using raw value",
                            label, arg, e
                        ));
                        arg.clone()
                    });
                    // Apply shell-style variable expansion after template rendering.
                    expand_shell_vars(&rendered, &shell_vars)
                })
                .collect();

            // Build artifact registrations (signature + optional certificate).
            // These are registered regardless of dry-run mode so downstream
            // stages (release) can reference them.
            //
            // relativeToDist: if the resolved signature path is not already
            // inside the dist directory, prepend the dist path so the file
            // lands alongside other release artifacts.
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
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let mut job_artifacts = vec![anodize_core::artifact::Artifact {
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
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                let mut cert_metadata = std::collections::HashMap::new();
                cert_metadata.insert("type".to_string(), "Certificate".to_string());
                job_artifacts.push(anodize_core::artifact::Artifact {
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
                // Still register artifacts in dry-run mode for downstream stages.
                for artifact in job_artifacts {
                    ctx.artifacts.add(artifact);
                }
                continue;
            }

            // Prepare stdin data up-front so it can be sent to worker threads.
            let (_, stdin_data) = prepare_stdin_from(
                sign_cfg.stdin.as_deref(),
                sign_cfg.stdin_file.as_deref(),
                label,
            )?;

            sign_jobs.push(SignJob {
                cmd: cmd.clone(),
                args: fully_resolved,
                stdin_data,
                env: sign_cfg.env.clone(),
                label: label.to_string(),
                id_label: sign_cfg.id.as_deref().unwrap_or("default").to_string(),
                artifact_display: artifact_str.to_string(),
                signature_display: signature_str.clone(),
                output_flag: sign_cfg.output.unwrap_or(false),
                new_artifacts: job_artifacts,
            });
        }

        // ----------------------------------------------------------------
        // Phase 2: Execute sign jobs in parallel chunks.
        // Each chunk runs up to `parallelism` signing commands concurrently
        // via std::thread::scope, matching the build stage's pattern.
        // ----------------------------------------------------------------
        if !sign_jobs.is_empty() {
            log.status(&format!(
                "signing {} artifacts with parallelism={}",
                sign_jobs.len(),
                parallelism
            ));
        }

        // Collect all new artifacts from jobs to register after execution.
        let mut all_new_artifacts: Vec<anodize_core::artifact::Artifact> = Vec::new();

        for chunk in sign_jobs.chunks(parallelism) {
            let results: Vec<Result<()>> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|job| {
                        let thread_log = anodize_core::log::StageLogger::new(
                            label_to_static(label),
                            log.verbosity(),
                        );
                        s.spawn(move || execute_sign_job(job, &thread_log))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("sign thread panicked"))
                    .collect()
            });

            // Check for errors — fail fast on the first error in this chunk.
            for result in &results {
                if let Err(e) = result {
                    // Re-create the error since we can't move out of a reference.
                    anyhow::bail!("{}", e);
                }
            }

            // Register artifacts from successful jobs in this chunk.
            for job in chunk {
                all_new_artifacts.extend(job.new_artifacts.iter().cloned());
            }
        }

        for artifact in all_new_artifacts {
            ctx.artifacts.add(artifact);
        }
    }

    Ok(())
}

/// Convert a runtime label string to a `&'static str` for `StageLogger::new`.
///
/// Only the known label values are used; unknown labels fall back to "sign".
fn label_to_static(label: &str) -> &'static str {
    match label {
        "sign" => "sign",
        "binary-sign" => "binary-sign",
        _ => "sign",
    }
}

// ---------------------------------------------------------------------------
// SignStage
// ---------------------------------------------------------------------------

/// Sign stage: signs artifacts using GPG, cosign, or other signing tools.
///
/// **Note on `Artifacts` template variable**: This stage does NOT call
/// `ctx.refresh_artifacts_var()` because it only renders signing-related
/// templates (e.g. `if_condition`, `signature`, `certificate`, args) — not
/// user-facing release body or announce templates where `{{ Artifacts }}`
/// would be iterated. The `Artifacts` variable is refreshed by the release
/// and announce stages just before they render their body templates.
pub struct SignStage;

impl Stage for SignStage {
    fn name(&self) -> &str {
        "sign"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("sign");

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

        // ----------------------------------------------------------------
        // Docker image signing via `docker_signs` config
        // ----------------------------------------------------------------
        if let Some(docker_signs) = ctx.config.docker_signs.clone() {
            for docker_sign_cfg in &docker_signs {
                // Evaluate the `if` conditional template for docker signs.
                // (See comment in process_sign_configs for why this uses
                // inverted logic compared to `disable`.)
                if let Some(ref condition) = docker_sign_cfg.if_condition {
                    match ctx.render_template(condition) {
                        Ok(result) => {
                            let trimmed = result.trim();
                            if trimmed.is_empty() || trimmed == "false" {
                                log.verbose(&format!(
                                    "skipping docker-sign config: if condition evaluated to '{}'",
                                    trimmed
                                ));
                                continue;
                            }
                        }
                        Err(e) => {
                            log.warn(&format!(
                                "docker-sign if condition render failed ({}), skipping: {}",
                                condition, e
                            ));
                            continue;
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
                    continue;
                }

                // Collect docker artifacts based on the filter mode:
                // "images" or "" (default) → DockerImage only
                // "manifests" → DockerManifest only
                // "all" → both DockerImage and DockerManifest
                // anything else → warn and default to DockerImage
                let docker_artifacts: Vec<_> = match docker_filter {
                    "images" | "" => ctx.artifacts.by_kind(ArtifactKind::DockerImage),
                    "manifests" => ctx.artifacts.by_kind(ArtifactKind::DockerManifest),
                    "all" => {
                        let mut arts = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
                        arts.extend(ctx.artifacts.by_kind(ArtifactKind::DockerManifest));
                        arts
                    }
                    other => {
                        log.warn(&format!(
                            "unknown docker_signs artifacts filter '{}', defaulting to images",
                            other
                        ));
                        ctx.artifacts.by_kind(ArtifactKind::DockerImage)
                    }
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
                    // `digest` — the docker image digest (e.g., sha256:abc123...)
                    // `artifactID` — the artifact's id field from metadata
                    // Always set (even to empty) to avoid stale values from a
                    // previous iteration leaking to this image.
                    ctx.template_vars_mut().set(
                        "digest",
                        metadata.get("digest").map(|s| s.as_str()).unwrap_or(""),
                    );
                    ctx.template_vars_mut().set(
                        "artifactID",
                        metadata.get("id").map(|s| s.as_str()).unwrap_or(""),
                    );

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

                    log.status(&format!("docker-sign {}", image_str));

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

                    // Merge custom env vars if configured on docker sign.
                    if let Some(ref env_vars) = docker_sign_cfg.env {
                        command.envs(env_vars);
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

                    // Capture output BEFORE the error bail so stdout/stderr from
                    // a failed docker signing command is still logged.
                    let stdout_str = String::from_utf8_lossy(&output.stdout).to_string();
                    let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();

                    if docker_sign_cfg.output.unwrap_or(true) {
                        if !stdout_str.is_empty() {
                            log.status(&format!("[docker-sign stdout] {}", stdout_str.trim()));
                        }
                        if !stderr_str.is_empty() {
                            log.status(&format!("[docker-sign stderr] {}", stderr_str.trim()));
                        }
                    }

                    // Now check exit status (bails on non-zero).
                    log.check_output(output, &cmd)?;
                }
            }

            // Clear docker-specific template vars so they don't leak to
            // downstream stages that may inspect the template context.
            ctx.template_vars_mut().set("digest", "");
            ctx.template_vars_mut().set("artifactID", "");
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::test_helpers::TestContextBuilder;

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
        assert!(should_sign_artifact(ArtifactKind::Checksum, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Archive, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Binary, "all").unwrap());
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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        // "all" matches everything
        assert!(should_sign_artifact(ArtifactKind::Archive, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Binary, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Checksum, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::LinuxPackage, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Installer, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::DiskImage, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Sbom, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::Snap, "all").unwrap());
        assert!(should_sign_artifact(ArtifactKind::MacOsPackage, "all").unwrap());

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_missing_signing_binary_errors_with_command_name() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
  COSIGN_EXPERIMENTAL: "1"
  MY_KEY: "my_value"
"#;
        let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let env = cfg.env.unwrap();
        assert_eq!(env.get("COSIGN_EXPERIMENTAL").unwrap(), "1");
        assert_eq!(env.get("MY_KEY").unwrap(), "my_value");
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
        let cfg: anodize_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let ids = cfg.ids.unwrap();
        assert_eq!(ids, vec!["my-docker-image", "another-image"]);
    }

    #[test]
    fn test_docker_sign_stdin_config_parsing() {
        let yaml = r#"
cmd: "cosign"
stdin: "my-password"
"#;
        let cfg: anodize_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.stdin.as_deref(), Some("my-password"));
    }

    #[test]
    fn test_docker_sign_stdin_file_config_parsing() {
        let yaml = r#"
cmd: "cosign"
stdin_file: "/path/to/password"
"#;
        let cfg: anodize_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.stdin_file.as_deref(), Some("/path/to/password"));
    }

    #[test]
    fn test_sign_env_vars_passed_to_command() {
        // Verify that custom env vars reach the signing command.
        // Use `sh -c` to write the env var value to a file so we can verify it.
        use anodize_core::artifact::{Artifact, ArtifactKind};

        let tmp = tempfile::TempDir::new().unwrap();
        let marker_path = tmp.path().join("env_check.txt");
        let marker_str = marker_path.to_string_lossy().to_string();

        let mut env_map = std::collections::HashMap::new();
        env_map.insert(
            "ANODIZE_TEST_SIGN_ENV".to_string(),
            "hello_from_sign".to_string(),
        );

        let signs = vec![SignConfig {
            id: Some("test-env".to_string()),
            cmd: Some("sh".to_string()),
            args: Some(vec![
                "-c".to_string(),
                format!("echo $ANODIZE_TEST_SIGN_ENV > {}", marker_str),
            ]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: Some(env_map),
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
        let env_output = std::fs::read_to_string(&marker_path)
            .expect("marker file should exist — env var was written by signing command");
        assert_eq!(
            env_output.trim(),
            "hello_from_sign",
            "ANODIZE_TEST_SIGN_ENV should have been passed to the signing command"
        );
    }

    #[test]
    fn test_docker_sign_ids_filter() {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use anodize_core::config::DockerSignConfig;

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        let cfg: anodize_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.id.as_deref(), Some("my-docker-signer"));
    }

    #[test]
    fn test_docker_sign_env_config_parsing() {
        let yaml = r#"
cmd: "cosign"
env:
  COSIGN_EXPERIMENTAL: "1"
  REGISTRY_TOKEN: "secret"
"#;
        let cfg: anodize_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let env = cfg.env.unwrap();
        assert_eq!(env.get("COSIGN_EXPERIMENTAL").unwrap(), "1");
        assert_eq!(env.get("REGISTRY_TOKEN").unwrap(), "secret");
    }

    #[test]
    fn test_docker_sign_env_vars_passed_to_command() {
        // Verify that custom env vars reach the docker signing command.
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use anodize_core::config::DockerSignConfig;

        let tmp = tempfile::TempDir::new().unwrap();
        let marker_path = tmp.path().join("docker_env_check.txt");
        let marker_str = marker_path.to_string_lossy().to_string();

        let mut env_map = std::collections::HashMap::new();
        env_map.insert(
            "ANODIZE_TEST_DOCKER_ENV".to_string(),
            "docker_hello".to_string(),
        );

        let docker_signs = vec![DockerSignConfig {
            id: Some("test-env".to_string()),
            cmd: Some("sh".to_string()),
            args: Some(vec![
                "-c".to_string(),
                format!("echo $ANODIZE_TEST_DOCKER_ENV > {}", marker_str),
            ]),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: Some(env_map),
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

        let stage = SignStage;
        stage.run(&mut ctx).unwrap();

        let env_output = std::fs::read_to_string(&marker_path).unwrap();
        assert_eq!(
            env_output.trim(),
            "docker_hello",
            "ANODIZE_TEST_DOCKER_ENV should have been passed to the docker signing command"
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
        assert_eq!(cfg.output, Some(true));
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
        assert_eq!(cfg.output, Some(true));
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
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.binary_signs.len(), 1);
        assert_eq!(config.binary_signs[0].cmd.as_deref(), Some("gpg"));
    }

    #[test]
    fn test_binary_signs_defaults_to_empty() {
        let yaml = "project_name: test\ncrates: []";
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert!(config.binary_signs.is_empty());
    }

    #[test]
    fn test_if_condition_false_skips_sign() {
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

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
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use anodize_core::config::DockerSignConfig;

        let tmp = tempfile::TempDir::new().unwrap();
        let marker_path = tmp.path().join("docker_vars.txt");
        let marker_str = marker_path.to_string_lossy().to_string();

        // Use sh -c to capture template-resolved variables
        let docker_signs = vec![DockerSignConfig {
            id: Some("test-vars".to_string()),
            cmd: Some("sh".to_string()),
            args: Some(vec![
                "-c".to_string(),
                format!(
                    "echo \"digest={{{{ digest }}}} artifactID={{{{ artifactID }}}}\" > {}",
                    marker_str
                ),
            ]),
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

        let stage = SignStage;
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
        use anodize_core::artifact::{Artifact, ArtifactKind};
        use anodize_core::config::DockerSignConfig;

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
        use anodize_core::artifact::{Artifact, ArtifactKind};

        // Use echo to produce stdout; with output: true it should be captured
        let signs = vec![SignConfig {
            id: Some("test-output".to_string()),
            cmd: Some("echo".to_string()),
            args: Some(vec!["hello-from-sign".to_string()]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: Some(true),
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
}
