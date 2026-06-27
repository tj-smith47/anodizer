use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result};

use anodizer_core::log::StageLogger;
use anodizer_core::run::run_capture_timeout;

use super::detect::is_retriable_error_v2;

/// Wall-clock bound on a single `podman push <tag>` to a remote registry. A
/// stalled registry upload (wedged TLS handshake, half-open connection, a
/// registry that accepts the connection then never drains the layer) would
/// otherwise hang the release forever, exactly like the snapcraft-upload stall.
/// On expiry the whole push subtree is killed and the attempt retries within
/// the push budget. Matches the snapcraft upload bound (large remote upload).
const PODMAN_PUSH_TIMEOUT: Duration = Duration::from_secs(600);

/// Wall-clock bound on a single docker/podman build job. A buildx `build --push`
/// fuses image assembly with a registry upload in one process, so a stalled push
/// (or a remote base-image fetch that never drains) would otherwise hang the
/// release forever. Sized to the operator-facing whole-build ceiling: a single
/// build outliving an hour is already past the expected envelope, so killing it
/// there catches a true stall without false-killing a slow but legitimate
/// multi-arch build. On expiry the whole build subtree is killed and the attempt
/// retries within the build budget.
const DOCKER_BUILD_TIMEOUT: Duration = Duration::from_secs(3600);

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
// format_v2_created_images_log — structured log helper
// ---------------------------------------------------------------------------

/// Format the v2 created-images log line with `images` and `digest` as
/// **separate** fields. Older
/// versions embedded `image@digest` in a single field, which was hard to
/// parse from log aggregators. Now each field is independently
/// addressable.
pub(crate) fn format_v2_created_images_log(images: &[String], digest: &str) -> String {
    format!(
        "created images — images={} digest={}",
        images.join(","),
        digest
    )
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
    /// Rendered image tags — used for digest capture and artifact registration.
    pub(crate) rendered_tags: Vec<String>,
    /// Docker platforms for this job. Used by the artifact-metadata
    /// `Platforms` key (JSON-array encoded), populated from the resolved
    /// platform set.
    pub(crate) platforms_list: Vec<String>,
    /// Staging directory path.
    pub(crate) staging_dir: PathBuf,
    /// Optional docker config id.
    pub(crate) id: Option<String>,
    /// Optional use_backend string.
    pub(crate) use_backend: Option<String>,
    /// True when the build uses the podman backend. `podman build` cannot
    /// bake `--push` into the build, so publication is performed by an
    /// explicit `podman push <tag>` after a successful build (gated on
    /// [`Self::push`]).
    pub(crate) is_podman: bool,
    /// Whether the built tags should be pushed to their registries. For
    /// buildx this is already baked into the build via `--push`; for podman
    /// it drives the explicit post-build `podman push` loop. False for
    /// snapshot and dry-run builds (which never publish).
    pub(crate) push: bool,
    /// Dist directory (for writing digest files).
    pub(crate) dist: PathBuf,
    /// Whether digest artifact creation is skipped.
    pub(crate) skip_digest: bool,
    /// Digest file name template (rendered). None = use default tag-based naming.
    pub(crate) digest_name_template: Option<String>,
    /// Context environment variables to inject into docker commands.
    /// These come from .env files and config `env:` sections. Ordered map
    /// so iteration order is stable across runs (load-bearing for the
    /// determinism harness: any env-iteration leak into command argv or
    /// log lines would otherwise drift).
    pub(crate) env_vars: BTreeMap<String, String>,
}

/// Result of executing a single docker build job.
pub(crate) struct DockerBuildResult {
    /// Digests captured after a successful push, keyed by tag. Ordered
    /// map so per-tag digest files are written in stable order (see
    /// `env_vars` note above).
    pub(crate) tag_digests: BTreeMap<String, String>,
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
    log.verbose(&format!("running {}", job.cmd_args.join(" ")));

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
        let mut output = match run_capture_timeout(
            &mut cmd,
            log,
            &format!("docker {}", job.backend_label),
            DOCKER_BUILD_TIMEOUT,
        ) {
            Ok(o) => o,
            Err(e) => {
                let e = e.context(format!(
                    "docker: execute {} for crate {} index {} (attempt {}/{})",
                    job.backend_label, job.crate_name, job.idx, attempt, job.max_attempts
                ));
                // A deadline kill (build/push stalled past the whole-build
                // ceiling) is wrapped Retriable → retry within the build budget;
                // a spawn failure (binary missing) is fatal → break without
                // burning retries.
                if anodizer_core::retry::is_retriable(e.as_ref()) {
                    return Err(ControlFlow::Continue(e));
                }
                return Err(ControlFlow::Break(e));
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

        // The captured per-layer buildx progress (≈hundreds of lines, plus the
        // ephemeral `/tmp/.tmpXXXX` staging-context path) is a firehose that
        // drowns the default register; the concise "created images" summary
        // below is the default-verbosity signal. `run_capture_timeout` already
        // teed the raw stream live (redacted) to stderr under `-v`; on the
        // failure path `check_output` re-emits the redacted output regardless of
        // verbosity, so a build error still surfaces its context.
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
                let is_retriable = is_retriable_error_v2(&err_msg);
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

    // buildx bakes `--push` into the build invocation, so for the buildx
    // backend publication already happened above and there is no separate
    // push step. `podman build` cannot bake `--push`, so for the podman
    // backend the image now lives only in local storage — publish it here by
    // pushing each rendered tag explicitly. This runs only on a real publish
    // (`job.push`); snapshot and dry-run builds leave `push` false and never
    // publish. Per-arch tags are pushed before any `docker_manifests`
    // `manifest push` runs because the entire build phase (including this
    // loop) completes before the manifest stage executes.
    if job.is_podman && job.push {
        push_podman_tags(job, log)?;
    }

    // Capture digests from the --iidfile (written by both buildx and podman).
    let mut tag_digests = BTreeMap::new();
    let mut digest_files = Vec::new();

    // V2: read digest from --iidfile (works even without push).
    // For multi-platform --push builds, older buildx versions may not
    // populate the iidfile; the `if let Ok(...)` handles this gracefully.
    // When present, the iidfile contains a single sha256 digest shared
    // across all tags (it's the manifest list digest for multi-platform).
    let iidfile = job.staging_dir.join("id.txt");
    if let Ok(digest_content) = fs::read_to_string(&iidfile) {
        let digest = digest_content.trim().to_string();
        if !digest.is_empty() {
            // Emit the created-images log with
            // `images` and `digest` as *separate* structured fields rather
            // than embedding `image@digest` in a single field. Easier to
            // query in log aggregators (the `images` field carries
            // ...).WithField("digest", ...)` shape.
            tracing::info!(
                images = %job.rendered_tags.join(","),
                digest = %digest,
                "created images",
            );
            log.status(&format_v2_created_images_log(&job.rendered_tags, &digest));
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

    Ok(DockerBuildResult {
        tag_digests,
        digest_files,
    })
}

/// Publish a podman-backend build by pushing every rendered tag.
///
/// `podman build` cannot bake `--push` into the build the way `docker buildx
/// build --push` does, so publication happens here. Single-platform builds
/// push the lone image (`podman push <tag>`); multi-platform builds named a
/// local manifest list with `--manifest <tag>` and publish it with `podman
/// manifest push --all <tag>` (see
/// [`crate::command::build_podman_push_commands`]). Each push uses the same
/// retry policy as the build so a transient registry error is retried rather
/// than failing the release. A push failure is a hard error: a release that
/// builds an image but never publishes it has shipped nothing, so the error is
/// propagated with context, never swallowed.
fn push_podman_tags(job: &DockerBuildJob, log: &StageLogger) -> Result<()> {
    use anodizer_core::retry::{RetryPolicy, retry_sync};
    use std::ops::ControlFlow;

    let multi_platform = job.platforms_list.len() > 1;
    let push_cmds = crate::command::build_podman_push_commands(&job.rendered_tags, multi_platform);
    let policy = RetryPolicy {
        max_attempts: job.max_attempts,
        base_delay: job.base_delay,
        max_delay: job.max_delay.unwrap_or(Duration::MAX),
    };

    for push_args in &push_cmds {
        log.verbose(&format!("running {}", push_args.join(" ")));
        retry_sync(&policy, |attempt| {
            if attempt > 1 {
                log.warn(&format!(
                    "podman push attempt {}/{} failed, retrying…",
                    attempt - 1,
                    job.max_attempts,
                ));
            }

            let mut cmd = Command::new(&push_args[0]);
            cmd.args(&push_args[1..]);
            for (key, value) in &job.env_vars {
                cmd.env(key, value);
            }
            let mut output =
                match run_capture_timeout(&mut cmd, log, "podman push", PODMAN_PUSH_TIMEOUT) {
                    Ok(o) => o,
                    Err(e) => {
                        let e = e.context(format!(
                            "podman push: execute for crate {} index {} (attempt {}/{})",
                            job.crate_name, job.idx, attempt, job.max_attempts
                        ));
                        // A deadline kill (push stalled on the registry) is wrapped
                        // Retriable → retry within budget. A spawn failure (binary
                        // missing) is not transient → break without burning retries.
                        if anodizer_core::retry::is_retriable(e.as_ref()) {
                            return Err(ControlFlow::Continue(e));
                        }
                        return Err(ControlFlow::Break(e));
                    }
                };

            // Redact secrets from stdout/stderr before any output or logging,
            // mirroring the build path's redaction (registry auth tokens can
            // appear in podman push diagnostics).
            let env_pairs: Vec<(String, String)> = job
                .env_vars
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .chain(std::env::vars())
                .collect();
            if !output.stdout.is_empty() {
                let redacted = anodizer_core::redact::string(
                    &String::from_utf8_lossy(&output.stdout),
                    &env_pairs,
                );
                output.stdout = redacted.into_bytes();
            }
            if !output.stderr.is_empty() {
                let redacted = anodizer_core::redact::string(
                    &String::from_utf8_lossy(&output.stderr),
                    &env_pairs,
                );
                output.stderr = redacted.into_bytes();
            }
            {
                use std::io::Write;
                std::io::stdout().write_all(&output.stdout).ok();
                std::io::stderr().write_all(&output.stderr).ok();
            }

            match log.check_output(output, "podman push") {
                Ok(_) => Ok(()),
                Err(e) => {
                    let err_msg = format!("{:#}", e);
                    if is_retriable_error_v2(&err_msg) {
                        Err(ControlFlow::Continue(e))
                    } else {
                        Err(ControlFlow::Break(e.context(format!(
                            "podman push: non-retriable failure for crate {} index {}",
                            job.crate_name, job.idx
                        ))))
                    }
                }
            }
        })
        .with_context(|| {
            format!(
                "podman push: all {} attempts failed for crate {} index {} ({})",
                job.max_attempts,
                job.crate_name,
                job.idx,
                push_args.last().map(String::as_str).unwrap_or(""),
            )
        })?;
        log.status(&format!(
            "pushed image {}",
            push_args.last().map(String::as_str).unwrap_or("")
        ));
    }

    Ok(())
}
