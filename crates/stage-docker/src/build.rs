use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result};

use anodizer_core::log::StageLogger;

use super::detect::{is_retriable_error, is_retriable_error_v2};

// ---------------------------------------------------------------------------
// list_staging_dir_recursive — diagnostic file listing
// ---------------------------------------------------------------------------

/// Recursively list files in the staging directory for COPY/ADD failure
/// diagnostics.  Logs each entry as a warning so users can see exactly which
/// files are staged.
pub(crate) fn list_staging_dir_recursive(
    dir: &std::path::Path,
    root: &std::path::Path,
    log: &StageLogger,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut items: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    items.sort_by_key(|e| e.file_name());
    for entry in items {
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        if path.is_dir() {
            log.warn(&format!("  {}/ (directory)", rel.display()));
            list_staging_dir_recursive(&path, root, log);
        } else {
            log.warn(&format!("  {}", rel.display()));
        }
    }
}

// ---------------------------------------------------------------------------
// find_sha256_digest — extract digest from docker push stdout
// ---------------------------------------------------------------------------

/// Extract a `sha256:<64-hex>` digest from text, typically the stdout of
/// `docker push`.  Uses plain string parsing to avoid a regex dependency.
pub(crate) fn find_sha256_digest(text: &str) -> Option<&str> {
    for word in text.split_whitespace() {
        if let Some(rest) = word.strip_prefix("sha256:")
            && rest.len() >= 64
            && rest[..64].chars().all(|c| c.is_ascii_hexdigit())
        {
            // Return exactly "sha256:" + 64 hex chars (ignore trailing chars)
            return Some(&word[..71]);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// DockerBuildJob — prepared data for a single docker build
// ---------------------------------------------------------------------------

/// All the information needed to execute a single docker build command.
///
/// The preparation phase (staging files, rendering templates, building the
/// command) is done sequentially because it needs `&mut Context`.  The
/// execution phase (running docker) can then run in parallel.
pub(crate) struct DockerBuildJob {
    /// Pre-built docker command arguments (binary + flags + context dir).
    pub(crate) cmd_args: Vec<String>,
    /// Human-readable backend label for log messages ("buildx", "docker", "podman").
    pub(crate) backend_label: String,
    /// Crate name (for error context).
    pub(crate) crate_name: String,
    /// Docker config index (for error context).
    pub(crate) idx: usize,
    /// Retry parameters.
    pub(crate) max_attempts: u32,
    pub(crate) base_delay: Duration,
    pub(crate) max_delay: Option<Duration>,
    /// Whether to push (and therefore capture digests after build).
    pub(crate) should_push: bool,
    /// Rendered image tags — used for digest capture and artifact registration.
    pub(crate) rendered_tags: Vec<String>,
    /// Docker platforms string (comma-separated, for artifact metadata).
    pub(crate) platforms_str: String,
    /// Staging directory path.
    pub(crate) staging_dir: PathBuf,
    /// Optional docker config id.
    pub(crate) id: Option<String>,
    /// Optional use_backend string.
    pub(crate) use_backend: Option<String>,
    /// Dist directory (for writing digest files).
    pub(crate) dist: PathBuf,
    /// Whether this is a V2 docker build (affects artifact type registration).
    pub(crate) is_v2: bool,
    /// Whether digest artifact creation is skipped.
    pub(crate) skip_digest: bool,
    /// Digest file name template (rendered). None = use default tag-based naming.
    pub(crate) digest_name_template: Option<String>,
    /// Context environment variables to inject into docker commands.
    /// These come from .env files and config `env:` sections.
    pub(crate) env_vars: HashMap<String, String>,
    /// Rendered push flags — passed to `docker push` for legacy (non-buildx)
    /// builds. For buildx builds these are baked into cmd_args via --push.
    pub(crate) push_flags: Vec<String>,
}

/// Result of executing a single docker build job.
pub(crate) struct DockerBuildResult {
    /// Digests captured after a successful push, keyed by tag.
    pub(crate) tag_digests: HashMap<String, String>,
    /// Paths to digest files written to the dist directory.
    pub(crate) digest_files: Vec<PathBuf>,
}

/// Execute a single docker build job with retry logic.
///
/// This is a free function (not a method) so it can be called from
/// `std::thread::scope` spawned threads without borrowing `self`.
pub(crate) fn execute_docker_build(
    job: &DockerBuildJob,
    log: &StageLogger,
) -> Result<DockerBuildResult> {
    log.status(&format!("running: {}", job.cmd_args.join(" ")));

    use anodizer_core::retry::{RetryPolicy, retry_sync};
    use std::ops::ControlFlow;
    let policy = RetryPolicy {
        max_attempts: job.max_attempts,
        base_delay: job.base_delay,
        max_delay: job.max_delay.unwrap_or(Duration::MAX),
    };
    retry_sync(&policy, |attempt| {
        if attempt > 1 {
            log.warn(&format!(
                "attempt {}/{} failed, retrying…",
                attempt - 1,
                job.max_attempts,
            ));
        }

        let mut cmd = Command::new(&job.cmd_args[0]);
        cmd.args(&job.cmd_args[1..])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (key, value) in &job.env_vars {
            cmd.env(key, value);
        }
        let mut output = match cmd.output() {
            Ok(o) => o,
            Err(e) => {
                return Err(ControlFlow::Break(anyhow::Error::from(e).context(format!(
                    "docker: execute {} for crate {} index {} (attempt {}/{})",
                    job.backend_label, job.crate_name, job.idx, attempt, job.max_attempts
                ))));
            }
        };

        // Redact secrets from stdout/stderr before any output or logging.
        let env_pairs: Vec<(String, String)> = job
            .env_vars
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .chain(std::env::vars())
            .collect();

        if !output.stdout.is_empty() {
            let redacted =
                anodizer_core::redact::string(&String::from_utf8_lossy(&output.stdout), &env_pairs);
            output.stdout = redacted.into_bytes();
        }
        if !output.stderr.is_empty() {
            let redacted =
                anodizer_core::redact::string(&String::from_utf8_lossy(&output.stderr), &env_pairs);
            output.stderr = redacted.into_bytes();
        }

        {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&output.stdout);
            let _ = std::io::stderr().write_all(&output.stderr);
        }

        let stderr_text = String::from_utf8_lossy(&output.stderr).to_string();

        match log.check_output(output, &format!("docker {}", job.backend_label)) {
            Ok(_) => {
                if attempt > 1 {
                    log.status(&format!(
                        "docker {} succeeded on attempt {}/{}",
                        job.backend_label, attempt, job.max_attempts
                    ));
                }
                Ok(())
            }
            Err(e) => {
                let err_msg = format!("{:#}", e);
                let is_retriable = if job.is_v2 {
                    is_retriable_error_v2(&err_msg)
                } else {
                    is_retriable_error(&err_msg)
                };
                if !is_retriable {
                    if stderr_text.contains("COPY") || stderr_text.contains("ADD") {
                        log.warn(
                            "the Dockerfile COPY/ADD failed — check that the \
                             files referenced in your Dockerfile exist in the \
                             staging directory; the available files may not match \
                             what the Dockerfile expects",
                        );
                        log.warn("files in the staging directory:");
                        list_staging_dir_recursive(&job.staging_dir, &job.staging_dir, log);
                    }
                    if stderr_text.contains("could not read certificates")
                        || stderr_text.contains("server gave HTTP response to HTTPS client")
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
                    Err(ControlFlow::Break(e.context(format!(
                        "docker: non-retriable failure for crate {} index {}",
                        job.crate_name, job.idx
                    ))))
                } else {
                    Err(ControlFlow::Continue(e))
                }
            }
        }
    })
    .with_context(|| {
        format!(
            "docker: all {} attempts failed for crate {} index {}",
            job.max_attempts, job.crate_name, job.idx
        )
    })?;

    // Legacy (non-buildx) push: `docker push` / `podman push` per tag.
    // Plain `docker build` does NOT support --push; only buildx does.
    // GoReleaser's legacy docker pipe builds first, then pushes each tag
    // separately with `docker push <image>`.
    // Digests captured from `docker push` stdout (more reliable than inspect).
    let mut push_stdout_digests: HashMap<String, String> = HashMap::new();

    if job.should_push && !job.is_v2 && job.backend_label != "buildx" {
        let push_bin = if job.backend_label == "podman" {
            "podman"
        } else {
            "docker"
        };
        for tag in &job.rendered_tags {
            log.status(&format!("pushing {}", tag));

            let push_policy = anodizer_core::retry::RetryPolicy {
                max_attempts: job.max_attempts,
                base_delay: job.base_delay,
                max_delay: job.max_delay.unwrap_or(Duration::MAX),
            };
            anodizer_core::retry::retry_sync(&push_policy, |attempt| {
                use std::ops::ControlFlow;
                if attempt > 1 {
                    log.warn(&format!(
                        "push attempt {}/{} for {} failed, retrying…",
                        attempt - 1,
                        job.max_attempts,
                        tag,
                    ));
                }

                let mut push_cmd = Command::new(push_bin);
                push_cmd.arg("push").arg(tag);
                for flag in &job.push_flags {
                    push_cmd.arg(flag);
                }
                for (key, value) in &job.env_vars {
                    push_cmd.env(key, value);
                }
                push_cmd
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());

                let push_output = match push_cmd.output() {
                    Ok(o) => o,
                    Err(e) => {
                        return Err(ControlFlow::Break(anyhow::Error::from(e).context(format!(
                            "docker: push {} for crate {} index {} (attempt {}/{})",
                            tag, job.crate_name, job.idx, attempt, job.max_attempts
                        ))));
                    }
                };

                // Redact secrets from push output
                let env_pairs: Vec<(String, String)> = job
                    .env_vars
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .chain(std::env::vars())
                    .collect();
                let mut stdout_bytes = push_output.stdout;
                let mut stderr_bytes = push_output.stderr;
                if !stdout_bytes.is_empty() {
                    let redacted = anodizer_core::redact::string(
                        &String::from_utf8_lossy(&stdout_bytes),
                        &env_pairs,
                    );
                    stdout_bytes = redacted.into_bytes();
                }
                if !stderr_bytes.is_empty() {
                    let redacted = anodizer_core::redact::string(
                        &String::from_utf8_lossy(&stderr_bytes),
                        &env_pairs,
                    );
                    stderr_bytes = redacted.into_bytes();
                }

                {
                    use std::io::Write;
                    let _ = std::io::stdout().write_all(&stdout_bytes);
                    let _ = std::io::stderr().write_all(&stderr_bytes);
                }

                if push_output.status.success() {
                    if attempt > 1 {
                        log.status(&format!(
                            "docker push {} succeeded on attempt {}/{}",
                            tag, attempt, job.max_attempts
                        ));
                    }
                    let push_stdout = String::from_utf8_lossy(&stdout_bytes);
                    if let Some(digest) = find_sha256_digest(&push_stdout) {
                        push_stdout_digests.insert(tag.clone(), digest.to_string());
                    }
                    Ok(())
                } else {
                    let err_msg = String::from_utf8_lossy(&stderr_bytes).to_string();
                    let err = anyhow::anyhow!("docker push {} failed: {}", tag, err_msg.trim());
                    if is_retriable_error(&err_msg) {
                        Err(ControlFlow::Continue(err))
                    } else {
                        Err(ControlFlow::Break(err.context(format!(
                            "docker: push failed for crate {} index {} tag {}",
                            job.crate_name, job.idx, tag
                        ))))
                    }
                }
            })
            .with_context(|| {
                format!(
                    "docker: all {} push attempts failed for crate {} index {} tag {}",
                    job.max_attempts, job.crate_name, job.idx, tag
                )
            })?;
        }
    }

    // Capture digests after successful build (and push, for legacy)
    let mut tag_digests = HashMap::new();
    let mut digest_files = Vec::new();

    if job.is_v2 {
        // V2: read digest from --iidfile (works even without push).
        // For multi-platform --push builds, older buildx versions may not
        // populate the iidfile; the `if let Ok(...)` handles this gracefully.
        // When present, the iidfile contains a single sha256 digest shared
        // across all tags (it's the manifest list digest for multi-platform).
        let iidfile = job.staging_dir.join("id.txt");
        if let Ok(digest_content) = fs::read_to_string(&iidfile) {
            let digest = digest_content.trim().to_string();
            if !digest.is_empty() {
                for tag in &job.rendered_tags {
                    tag_digests.insert(tag.clone(), digest.clone());
                }
                // Write per-tag digest files
                if !job.skip_digest {
                    for tag in &job.rendered_tags {
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
                            digest_files.push(digest_file);
                        }
                    }
                }
            }
        }
    } else if job.should_push {
        // Legacy: capture digests — prefer push stdout, fall back to docker inspect.
        for tag in &job.rendered_tags {
            // First check if we captured the digest from `docker push` stdout.
            // This is more reliable than `docker inspect` because it works even
            // if the image was cleaned up after push (matches GoReleaser).
            let digest = if let Some(d) = push_stdout_digests.get(tag) {
                Some(d.clone())
            } else {
                // Fallback: `docker inspect` to read RepoDigests
                let inspect_bin = if job.backend_label == "podman" {
                    "podman"
                } else {
                    "docker"
                };
                let digest_output = {
                    let mut inspect_cmd = Command::new(inspect_bin);
                    inspect_cmd.args(["inspect", "--format", "{{index .RepoDigests 0}}", tag]);
                    for (key, value) in &job.env_vars {
                        inspect_cmd.env(key, value);
                    }
                    inspect_cmd.output()
                };

                if let Ok(output) = digest_output
                    && output.status.success()
                {
                    let d = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if d.is_empty() { None } else { Some(d) }
                } else {
                    None
                }
            };

            if let Some(digest) = digest {
                tag_digests.insert(tag.clone(), digest.clone());

                // Write per-tag digest file unless docker_digest.skip is truthy.
                // Always use tag-based naming for per-tag files to avoid collisions
                // when multiple tags exist. The name_template controls the artifact
                // name (metadata), not the file path.
                if !job.skip_digest {
                    let safe_name = tag.replace(['/', ':'], "_");
                    let filename = format!("{}.digest", safe_name);
                    let digest_file = job.dist.join(&filename);
                    if let Err(e) = fs::write(&digest_file, &digest) {
                        log.warn(&format!(
                            "failed to write digest file {}: {}",
                            digest_file.display(),
                            e
                        ));
                    } else {
                        log.status(&format!("saved digest to {}", digest_file.display()));
                        digest_files.push(digest_file);
                    }
                }
            }
        }
    }

    Ok(DockerBuildResult {
        tag_digests,
        digest_files,
    })
}
