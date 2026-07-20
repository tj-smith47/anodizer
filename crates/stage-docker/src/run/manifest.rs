use super::*;

/// Process one `docker_manifests[N]` entry: render templates, build/push the
/// manifest (with retry), and register a `DockerManifest` artifact in
/// `new_artifacts`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_docker_manifest(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    krate: &anodizer_core::config::CrateConfig,
    midx: usize,
    manifest_cfg: &anodizer_core::config::DockerManifestConfig,
    v2_multiplatform_tags: &HashSet<String>,
    manifest_env_vars: &HashMap<String, String>,
    dry_run: bool,
    new_artifacts: &mut Vec<Artifact>,
) -> Result<()> {
    // image_templates must not be empty — a manifest with zero images is
    // always a configuration error.
    if manifest_cfg.image_templates.is_empty() {
        let fallback = format!("index {}", midx);
        let manifest_label = manifest_cfg.id.as_deref().unwrap_or(&fallback);
        anyhow::bail!(
            "docker manifest '{}': image_templates must not be empty",
            manifest_label
        );
    }

    let manifest_name = ctx
        .render_template(&manifest_cfg.name_template)
        .with_context(|| {
            format!(
                "docker: render manifest name_template '{}' for crate {}",
                manifest_cfg.name_template, krate.name
            )
        })?;

    // Skip manifests whose target tag was already pushed as a multi-arch
    // manifest list by docker_v2. docker_v2 with
    // --platform=linux/amd64,linux/arm64 --push creates a native multi-arch
    // manifest; docker_manifests would try to re-create it from per-platform
    // tags (e.g. :0.3.3-amd64) that don't exist, causing "manifest unknown"
    // errors.
    if v2_multiplatform_tags.contains(&manifest_name) {
        log.status(&format!(
            "skipped manifest '{}' — already pushed as multi-arch by dockers_v2",
            manifest_name
        ));
        return Ok(());
    }

    // Render image templates, skipping entries that resolve to empty
    // strings (e.g. conditional templates that evaluate to nothing for
    // certain configurations).
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
                "skipped manifest — image_template '{}' rendered to empty string",
                tmpl
            ));
            continue;
        }
        rendered_images.push(img);
    }

    // Determine the binary for manifest commands (see `resolve_manifester`
    // for the validation rationale).
    let manifest_bin = resolve_manifester(manifest_cfg.use_backend.as_deref())?;

    // Propagate flag-template render errors rather than feeding the raw,
    // unrendered `{{...}}` string to the manifest tool (which rejects it with an
    // opaque error far from the cause).
    let rendered_create_flags: Vec<String> = manifest_cfg
        .create_flags
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|f| {
            ctx.render_template(f)
                .with_context(|| format!("dockers_v2: render manifest create_flag '{f}'"))
        })
        .collect::<Result<_>>()?;
    let rendered_push_flags: Vec<String> = manifest_cfg
        .push_flags
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|f| {
            ctx.render_template(f)
                .with_context(|| format!("dockers_v2: render manifest push_flag '{f}'"))
        })
        .collect::<Result<_>>()?;

    let create_cmd = build_manifest_create_cmd(
        log,
        manifest_bin,
        &manifest_name,
        &rendered_images,
        &rendered_create_flags,
        new_artifacts,
    );

    let manifest_skip_push = resolve_skip_push(&manifest_cfg.skip_push, ctx)?;
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
        // Remove any existing manifest before recreating:
        //   `_ = runCommand(ctx, ".", "docker", "manifest", "rm", manifest)`
        // — all errors ignored. A missing manifest is the common case (first
        // run / new tag), and any other failure (auth, network, daemon
        // offline) will surface when `manifest create` runs right after, with
        // a more actionable error.
        let mut rm_cmd = Command::new(manifest_bin);
        rm_cmd.args(["manifest", "rm", &manifest_name]);
        for (key, value) in manifest_env_vars {
            rm_cmd.env(key, value);
        }
        rm_cmd.output().ok();

        // Manifest create/push with retry logic — registry operations can
        // fail transiently. Uses the manifest's retry config (same as docker
        // build): per-pipe wins (with deprecation warning) over the
        // top-level `Project.Retry`; defaults apply otherwise.
        let (manifest_max_attempts, manifest_base_delay, manifest_max_delay) =
            resolve_retry_params(&manifest_cfg.retry, &ctx.config.retry).with_context(|| {
                format!(
                    "docker: invalid retry config for manifest {} crate {}",
                    midx, krate.name
                )
            })?;

        run_manifest_create_with_retry(
            log,
            &create_cmd,
            manifest_env_vars,
            &krate.name,
            midx,
            manifest_max_attempts,
            manifest_base_delay,
            manifest_max_delay,
            ctx.retry_deadline(),
        )?;

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

            manifest_digest = run_manifest_push_with_retry(
                log,
                &push_cmd,
                manifest_env_vars,
                &krate.name,
                midx,
                manifest_max_attempts,
                manifest_base_delay,
                manifest_max_delay,
                ctx.retry_deadline(),
            )?;
            log.status(&format!("pushed manifest {}", manifest_name));
        } else {
            log.status(&format!("created manifest {}", manifest_name));
        }
    }

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

    Ok(())
}

/// Compose the `docker manifest create` command, pinning each image to its
/// digest when available. Emits a `did you mean?` warning for any
/// unknown-image input that has a near-match in the registered tag set.
pub(crate) fn build_manifest_create_cmd(
    log: &anodizer_core::log::StageLogger,
    manifest_bin: &str,
    manifest_name: &str,
    rendered_images: &[String],
    rendered_create_flags: &[String],
    new_artifacts: &[Artifact],
) -> Vec<String> {
    let mut create_cmd: Vec<String> = vec![
        manifest_bin.to_string(),
        "manifest".to_string(),
        "create".to_string(),
        manifest_name.to_string(),
    ];
    for img in rendered_images {
        if let Some(digest) = find_image_digest(new_artifacts, img) {
            let pinned = format!("{}@{}", img, digest);
            log.verbose(&format!("pinning manifest {} to digest {}", img, digest));
            create_cmd.push(pinned);
        } else {
            // "Did you mean?" — find closest matching image by edit distance.
            let all_image_names: Vec<&str> = new_artifacts
                .iter()
                .filter(|a| {
                    matches!(
                        a.kind,
                        ArtifactKind::DockerImage | ArtifactKind::DockerImageV2
                    )
                })
                .filter_map(|a| a.metadata.get("tag").map(|s| s.as_str()))
                .collect();

            // Distance > 0 to avoid suggesting the same name back (happens
            // when `img` itself is in the candidate set but its digest
            // hadn't been recorded yet at lookup time — a stale-cache race,
            // not a typo).
            if let Some((suggestion, dist)) = all_image_names
                .iter()
                .map(|name| (name, levenshtein_distance(img, name)))
                .min_by_key(|&(_, d)| d)
                .filter(|&(_, d)| d > 0 && d <= img.len() / 2)
            {
                log.warn(&format!(
                    "could not find {:?}, did you mean {:?}? (edit distance: {})",
                    img, suggestion, dist
                ));
            } else {
                log.warn(&format!("no digest found for {}, using tag reference", img));
            }
            create_cmd.push(img.clone());
        }
    }
    for flag in rendered_create_flags {
        create_cmd.push(flag.clone());
    }
    create_cmd
}

/// Run `docker manifest create` with retry on transient errors.
#[allow(clippy::too_many_arguments)]
fn run_manifest_create_with_retry(
    log: &anodizer_core::log::StageLogger,
    create_cmd: &[String],
    manifest_env_vars: &HashMap<String, String>,
    crate_name: &str,
    midx: usize,
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Option<Duration>,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    use anodizer_core::retry::{RetryLog, RetryPolicy, retry_sync_deadline};
    use std::ops::ControlFlow;
    let policy = RetryPolicy {
        max_attempts,
        base_delay,
        max_delay: max_delay.unwrap_or(anodizer_core::config::RetryConfig::DEFAULT_MAX_DELAY),
    };
    retry_sync_deadline(
        RetryLog::new("docker manifest create", log),
        &policy,
        deadline,
        |attempt| {
            log.verbose(&format!("running {}", create_cmd.join(" ")));
            let mut create_command = Command::new(&create_cmd[0]);
            create_command.args(&create_cmd[1..]);
            for (key, value) in manifest_env_vars {
                create_command.env(key, value);
            }
            let output = match run_capture_timeout(
                &mut create_command,
                log,
                "docker manifest create",
                DOCKER_MANIFEST_CREATE_TIMEOUT,
            ) {
                Ok(o) => o,
                Err(e) => {
                    let e = e.context(format!(
                        "docker: manifest create for crate {} manifest {} (attempt {}/{})",
                        crate_name, midx, attempt, max_attempts
                    ));
                    // A deadline kill (registry stalled) is wrapped Retriable → retry
                    // within budget; a spawn failure is not transient → break.
                    if anodizer_core::retry::is_retriable(e.as_ref()) {
                        return Err(ControlFlow::Continue(e));
                    }
                    return Err(ControlFlow::Break(e));
                }
            };
            match log.check_output(output, "docker manifest create") {
                Ok(_) => {
                    if attempt > 1 {
                        log.status(&format!(
                            "docker manifest create succeeded on attempt {}/{}",
                            attempt, max_attempts
                        ));
                    }
                    Ok(())
                }
                Err(e) => {
                    use super::detect::is_retriable_error;
                    let err_msg = format!("{:#}", e);
                    if is_retriable_error(&err_msg) {
                        Err(ControlFlow::Continue(e))
                    } else {
                        Err(ControlFlow::Break(e))
                    }
                }
            }
        },
    )
}

/// Run `docker manifest push` with retry, capturing the pushed manifest's
/// sha256 digest from stdout for downstream artifact metadata.
#[allow(clippy::too_many_arguments)]
fn run_manifest_push_with_retry(
    log: &anodizer_core::log::StageLogger,
    push_cmd: &[String],
    manifest_env_vars: &HashMap<String, String>,
    crate_name: &str,
    midx: usize,
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Option<Duration>,
    deadline: Option<std::time::Instant>,
) -> Result<Option<String>> {
    use anodizer_core::retry::{RetryLog, RetryPolicy, retry_sync_deadline};
    use std::ops::ControlFlow;
    let policy = RetryPolicy {
        max_attempts,
        base_delay,
        max_delay: max_delay.unwrap_or(anodizer_core::config::RetryConfig::DEFAULT_MAX_DELAY),
    };
    let mut manifest_digest: Option<String> = None;
    retry_sync_deadline(
        RetryLog::new("docker manifest push", log),
        &policy,
        deadline,
        |attempt| {
            log.verbose(&format!("running {}", push_cmd.join(" ")));
            let mut push_command = Command::new(&push_cmd[0]);
            push_command.args(&push_cmd[1..]);
            for (key, value) in manifest_env_vars {
                push_command.env(key, value);
            }
            let output = match run_capture_timeout(
                &mut push_command,
                log,
                "docker manifest push",
                DOCKER_MANIFEST_PUSH_TIMEOUT,
            ) {
                Ok(o) => o,
                Err(e) => {
                    let e = e.context(format!(
                        "docker: manifest push for crate {} manifest {} (attempt {}/{})",
                        crate_name, midx, attempt, max_attempts
                    ));
                    // A deadline kill (registry stalled) is wrapped Retriable → retry
                    // within budget; a spawn failure is not transient → break.
                    if anodizer_core::retry::is_retriable(e.as_ref()) {
                        return Err(ControlFlow::Continue(e));
                    }
                    return Err(ControlFlow::Break(e));
                }
            };
            // Capture stdout for digest extraction before checking status.
            let push_stdout = String::from_utf8_lossy(&output.stdout).to_string();
            match log.check_output(output, "docker manifest push") {
                Ok(_) => {
                    if attempt > 1 {
                        log.status(&format!(
                            "docker manifest push succeeded on attempt {}/{}",
                            attempt, max_attempts
                        ));
                    }
                    // Extract digest from push output (sha256:64hexchars).
                    if let Some(start) = push_stdout.find("sha256:") {
                        let candidate = &push_stdout[start..];
                        if candidate.len() >= 71
                            && candidate[7..71].chars().all(|c| c.is_ascii_hexdigit())
                        {
                            manifest_digest = Some(candidate[..71].to_string());
                        }
                    }
                    Ok(())
                }
                Err(e) => {
                    use super::detect::is_retriable_error;
                    let err_msg = format!("{:#}", e);
                    if is_retriable_error(&err_msg) {
                        Err(ControlFlow::Continue(e))
                    } else {
                        Err(ControlFlow::Break(e))
                    }
                }
            }
        },
    )?;
    Ok(manifest_digest)
}
