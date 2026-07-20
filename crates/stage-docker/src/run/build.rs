use super::*;

/// Prepare a single docker_v2 config: render templates, stage artifacts,
/// fire the pre-hook, queue one or more build jobs (one per
/// snapshot-platform slice), and enqueue the post-hook record. Isolates
/// pre-hook failure so sibling configs continue (the
/// `semerrgroup` semantics).
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_v2_config(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    krate: &anodizer_core::config::CrateConfig,
    idx: usize,
    v2_cfg: &anodizer_core::config::DockerV2Config,
    dist: &std::path::Path,
    dry_run: bool,
    build_jobs: &mut Vec<DockerBuildJob>,
    v2_multiplatform_tags: &mut HashSet<String>,
    new_artifacts: &mut Vec<Artifact>,
    pre_hook_errors: &mut Vec<anyhow::Error>,
    config_post_hooks: &mut Vec<PerConfigPostHook>,
) -> Result<()> {
    // Check disable — skip when template evaluates to true.
    if is_docker_v2_skipped(&v2_cfg.skip, ctx)? {
        log.status(&format!(
            "skipped dockers_v2[{}] for crate {} — skip=true",
            idx, krate.name
        ));
        return Ok(());
    }

    // Template-render platforms, propagating render errors (a typo'd
    // `linux/{{ .BadVar }}` must fail, not silently shrink the build matrix and
    // ship fewer arches) and dropping only genuinely-empty renders.
    let mut platforms: Vec<String> = Vec::new();
    for p in v2_cfg.platforms.clone().unwrap_or_default() {
        let rendered = ctx.render_template(&p).with_context(|| {
            format!(
                "dockers_v2: render platform '{}' for crate {}",
                p, krate.name
            )
        })?;
        if !rendered.is_empty() {
            platforms.push(rendered);
        }
    }

    // V2 always uses buildx.
    resolve_backend(Some("buildx"), platforms.len() > 1)?;

    // Template-render the Dockerfile path FIRST so an empty template
    // short-circuits before touching the filesystem (avoids orphan staging
    // dirs / stale staged artifacts when
    // `dockerfile: "{{ if .IsSnapshot }}Dockerfile{{ end }}"` renders to ""
    // during release). Check the *rendered*
    // template for emptiness — not the raw template.
    let rendered_dockerfile = ctx.render_template(&v2_cfg.dockerfile).with_context(|| {
        format!(
            "dockers_v2: render dockerfile path '{}' for crate {}",
            v2_cfg.dockerfile, krate.name
        )
    })?;
    if rendered_dockerfile.trim().is_empty() {
        log.status(&format!(
            "skipped dockers_v2[{}] for crate {} — dockerfile template rendered empty",
            idx, krate.name
        ));
        return Ok(());
    }

    // "docker_v2" subdirectory avoids collisions with legacy docker configs.
    let staging_dir: PathBuf = dist
        .join("docker_v2")
        .join(&krate.name)
        .join(idx.to_string());

    if !dry_run {
        fs::create_dir_all(&staging_dir)
            .with_context(|| format!("dockers_v2: create staging dir {}", staging_dir.display()))?;
    }

    // Stage artifacts using V2 layout (os/arch/name, multiple artifact types).
    stage_artifacts_v2(
        &platforms,
        &staging_dir,
        dry_run,
        v2_cfg.ids.as_ref(),
        &krate.name,
        ctx,
        log,
    )?;

    copy_dockerfile(
        &rendered_dockerfile,
        &staging_dir,
        dry_run,
        log,
        "dockers_v2",
    )?;

    if let Some(ref extra_files) = v2_cfg.extra_files {
        warn_project_markers_in_extra_files(extra_files, log, "dockers_v2");
        stage_extra_files(extra_files, &staging_dir, None, dry_run, log, "dockers_v2")?;
    }

    // Resolve the Dockerfile's final-stage base image so the two template
    // vars `BaseImage` and `BaseImageDigest` are visible to every downstream
    // render (image tags, labels, annotations, build args, flags, hooks).
    // Failures are soft — a missing annotation is better than a hard build
    // failure when, say, `docker buildx imagetools inspect` is unreachable.
    // Vars are cleared at the end of this function so they don't leak into
    // the next config.
    let base_image_info =
        match get_base_image(std::path::Path::new(&rendered_dockerfile), dry_run, log) {
            Ok(opt) => opt,
            Err(e) => {
                log.warn(&format!(
                    "could not parse base image for dockers_v2[{}] from {}: {:#}",
                    idx, rendered_dockerfile, e
                ));
                None
            }
        };
    let (base_image_name, base_image_digest) = base_image_info
        .map(|b| (b.name, b.digest))
        .unwrap_or_default();
    ctx.template_vars_mut().set("BaseImage", &base_image_name);
    ctx.template_vars_mut()
        .set("BaseImageDigest", &base_image_digest);

    let mut rendered_tags: Vec<String> = Vec::new();
    for tag_tmpl in &v2_cfg.tags {
        let rendered = ctx.render_template(tag_tmpl).with_context(|| {
            format!(
                "dockers_v2: render tag template '{}' for crate {}",
                tag_tmpl, krate.name
            )
        })?;
        if rendered.is_empty() {
            continue;
        }
        rendered_tags.push(rendered);
    }

    let mut rendered_images: Vec<String> = Vec::new();
    for img_tmpl in &v2_cfg.images {
        let rendered = ctx.render_template(img_tmpl).with_context(|| {
            format!(
                "dockers_v2: render image template '{}' for crate {}",
                img_tmpl, krate.name
            )
        })?;
        if rendered.is_empty() {
            continue;
        }
        rendered_images.push(rendered);
    }

    // For snapshot builds, multi-platform configs are split into
    // per-platform builds with --load (no push) and tag suffix, so images
    // are available locally.
    let snapshot_platforms: Vec<Vec<String>> = if ctx.is_snapshot() && platforms.len() > 1 {
        platforms.iter().map(|p| vec![p.clone()]).collect()
    } else {
        vec![platforms.clone()]
    };

    // Pre-build hooks fire ONCE per docker_v2 config.
    // `buildImage` lifecycle. `Images` is the full cross-product of
    // `rendered_images × rendered_tags` (no per-platform arch suffix —
    // that's a snapshot-only tag-disambiguation step that runs after the
    // hook). Exposed as a real Tera list so `{% for img in Images %}`
    // works: the `keyImages` field carries `da.images`, where
    // `images` is `[]string`.
    let staging_str = staging_dir.to_string_lossy().into_owned();
    let cfg_image_tags = generate_v2_image_tags(&rendered_images, &rendered_tags);
    let cfg_images_json = serde_json::Value::Array(
        cfg_image_tags
            .iter()
            .map(|t| serde_json::Value::String(t.clone()))
            .collect(),
    );
    let pre_hooks: Vec<_> = v2_cfg
        .hooks
        .as_ref()
        .and_then(|h| h.pre.as_ref())
        .cloned()
        .unwrap_or_default();
    let post_hooks: Vec<_> = v2_cfg
        .hooks
        .as_ref()
        .and_then(|h| h.post.as_ref())
        .cloned()
        .unwrap_or_default();

    if !pre_hooks.is_empty() {
        let mut hook_vars = ctx.template_vars().clone();
        hook_vars.set_structured("Images", cfg_images_json.clone());
        hook_vars.set("Dockerfile", &rendered_dockerfile);
        hook_vars.set("ContextDir", &staging_str);
        let pre_label = format!(
            "pre-dockers_v2[{}]",
            v2_cfg.id.as_deref().unwrap_or(&idx.to_string())
        );
        if let Err(e) = run_hooks(
            &pre_hooks,
            &pre_label,
            HookRunContext::new(dry_run, log, Some(&hook_vars)),
        ) {
            log.warn(&format!(
                "skipped this config's build — pre-hook {} failed (other configs continue): {:#}",
                pre_label, e
            ));
            pre_hook_errors.push(e);
            ctx.template_vars_mut().unset("BaseImage");
            ctx.template_vars_mut().unset("BaseImageDigest");
            return Ok(());
        }
    }

    for snapshot_plats in &snapshot_platforms {
        queue_v2_build_for_platforms(
            ctx,
            log,
            krate,
            idx,
            v2_cfg,
            snapshot_plats,
            &rendered_tags,
            &rendered_images,
            &staging_str,
            &staging_dir,
            dist,
            dry_run,
            build_jobs,
            v2_multiplatform_tags,
            new_artifacts,
        )?;
    }

    // Dry-run post-hooks fire ONCE per docker_v2 config with an empty
    // `Digest` so template typos still surface. Real-run post-hooks fire
    // from `execute_jobs_and_register`'s caller — also once per config,
    // keyed by `idx` against the first matching job's digest.
    if dry_run && !post_hooks.is_empty() {
        let mut hook_vars = ctx.template_vars().clone();
        hook_vars.set_structured("Images", cfg_images_json.clone());
        hook_vars.set("Dockerfile", &rendered_dockerfile);
        hook_vars.set("ContextDir", &staging_str);
        hook_vars.set("Digest", "");
        let post_label = format!(
            "post-dockers_v2[{}]",
            v2_cfg.id.as_deref().unwrap_or(&idx.to_string())
        );
        run_hooks(
            &post_hooks,
            &post_label,
            HookRunContext::new(dry_run, log, Some(&hook_vars)),
        )?;
    } else if !dry_run && !post_hooks.is_empty() {
        config_post_hooks.push(PerConfigPostHook {
            idx,
            id: v2_cfg.id.clone(),
            hooks: post_hooks,
            images_json: cfg_images_json,
            dockerfile_path: rendered_dockerfile.clone(),
            staging_dir: staging_dir.clone(),
            base_image_name: base_image_name.clone(),
            base_image_digest: base_image_digest.clone(),
        });
    }

    // Remove per-config BaseImage / BaseImageDigest so the next docker_v2
    // config — or any downstream stage — does not observe stale values.
    // `unset` (not `set("")`) so strict-mode templates can distinguish
    // "undefined" from "defined-empty"; an overlay-drop semantic.
    ctx.template_vars_mut().unset("BaseImage");
    ctx.template_vars_mut().unset("BaseImageDigest");

    Ok(())
}

/// Queue a single docker build job for one platform tuple (either the full
/// multi-platform vector or one element of the snapshot-split list). Mutates
/// `build_jobs`, `v2_multiplatform_tags`, and `new_artifacts` (dry-run only).
#[allow(clippy::too_many_arguments)]
fn queue_v2_build_for_platforms(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    krate: &anodizer_core::config::CrateConfig,
    idx: usize,
    v2_cfg: &anodizer_core::config::DockerV2Config,
    snapshot_plats: &[String],
    rendered_tags: &[String],
    rendered_images: &[String],
    staging_str: &str,
    staging_dir: &std::path::Path,
    dist: &std::path::Path,
    dry_run: bool,
    build_jobs: &mut Vec<DockerBuildJob>,
    v2_multiplatform_tags: &mut HashSet<String>,
    new_artifacts: &mut Vec<Artifact>,
) -> Result<()> {
    let mut per_plat_tags: Vec<String> = rendered_tags.to_vec();

    // During snapshot, add platform arch suffix to each tag.
    if ctx.is_snapshot() && snapshot_plats.len() == 1 {
        let suffix = tag_suffix(&snapshot_plats[0]);
        for tag in &mut per_plat_tags {
            tag.push('-');
            tag.push_str(&suffix);
        }
    }

    let image_tags = generate_v2_image_tags(rendered_images, &per_plat_tags);

    if image_tags.is_empty() {
        log.warn(&format!(
            "skipped dockers_v2[{}] for crate {} — no image tags produced (images or tags resolved to empty)",
            idx, krate.name
        ));
        return Ok(());
    }

    let rendered_build_args = render_v2_kv_map(ctx, v2_cfg.build_args.as_ref(), "build_arg")?;
    let rendered_annotations = render_v2_kv_map(ctx, v2_cfg.annotations.as_ref(), "annotation")?;
    let user_labels = render_v2_kv_map(ctx, v2_cfg.labels.as_ref(), "label")?;
    // Auto-inject the standard predefined OCI image labels (default on),
    // merged with the user's `labels:` where an explicit user key always wins.
    let rendered_labels = if oci_labels_enabled(&v2_cfg.oci_labels, ctx)? {
        merge_oci_labels(auto_oci_labels(ctx, krate), user_labels)
    } else {
        user_labels
    };
    let rendered_flags = render_v2_flag_list(ctx, v2_cfg.flags.as_ref())?;

    // BuildKit reproducibility note:
    //
    // `SOURCE_DATE_EPOCH` is exported into the subprocess env below when the
    // build stage has seeded `ctx.determinism` — that gives every cargo /
    // build script invocation a stable epoch, AND any user BuildKit stage
    // that reads `$SOURCE_DATE_EPOCH` in its Dockerfile (`ARG
    // SOURCE_DATE_EPOCH` + tar mtimes inside RUN steps) picks it up.
    //
    // For byte-stable image layers across rebuilds, the user must
    // additionally supply
    // `--output=type=image,rewrite-timestamp=true,push=true` (or
    // `type=registry,rewrite-timestamp=true`) via `flag_templates:` — the
    // attribute is BuildKit's output-side knob, not a top-level CLI flag, so
    // it cannot be cleanly injected without overriding the user's `--push` /
    // `--load` choice. The determinism harness's `--stages=docker` mode
    // bypasses this by driving its own `docker buildx build --output ...`
    // through `core::docker_build` with the attribute pre-baked.

    // Backend selector: `use: podman` opts into `podman build`, otherwise
    // V2 invokes `docker buildx build`. Validation here gives a friendlier
    // error (config path + field name) than the generic resolver bail-out
    // that would otherwise surface at command construction.
    let backend = v2_cfg.use_backend.as_deref();
    match backend {
        Some("buildx") | Some("podman") | None => {}
        Some(other) => {
            anyhow::bail!(
                "dockers_v2[{}]: invalid `use: {}` for crate {} — expected `buildx` or `podman`",
                idx,
                other,
                krate.name
            );
        }
    }
    let is_podman = backend == Some("podman");
    if is_podman {
        // Linux-only enforcement upstream of the resolver so the error
        // points at the config index, not at a Command::new failure later.
        crate::command::enforce_podman_linux_only().with_context(|| {
            format!(
                "dockers_v2[{}]: `use: podman` for crate {} is not supported on this OS",
                idx, krate.name
            )
        })?;
        crate::command::validate_podman_flag_compat(&rendered_flags).with_context(|| {
            format!(
                "dockers_v2[{}]: incompatible flag with `use: podman` for crate {}",
                idx, krate.name
            )
        })?;
    }

    // Evaluate sbom — SBOM is only added in the Publish path (not snapshot).
    // SBOM is a buildx-only attestation; under `use: podman` it must be off.
    let sbom_enabled = if ctx.is_snapshot() {
        false
    } else {
        is_docker_v2_sbom_enabled(&v2_cfg.sbom, ctx)?
    };
    if is_podman && sbom_enabled {
        anyhow::bail!(
            "dockers_v2[{}]: `use: podman` for crate {} cannot enable `sbom: true` \
             (buildx-only attestation); set `sbom: false` or switch to `use: buildx`",
            idx,
            krate.name
        );
    }

    let platform_refs: Vec<&str> = snapshot_plats.iter().map(|s| s.as_str()).collect();

    // Snapshot builds never push (--load is used per-platform). The
    // canonical `skip:` field suppresses publish via `is_active`-style gating
    // earlier in the pipeline.
    let should_push = if ctx.is_snapshot() { false } else { !dry_run };

    // Determine whether --load is safe (requires a running daemon). In
    // snapshot mode, warn if the daemon is unavailable and skip --load.
    // `--load` is buildx-only — podman builds load into local storage by
    // default and the command builder suppresses `load` for the podman
    // backend, so the daemon probe / warn only applies to buildx.
    let should_load = if is_podman {
        false
    } else if ctx.is_snapshot() {
        let daemon_ok = is_docker_daemon_available();
        if !daemon_ok {
            log.warn(
                "docker daemon not available; snapshot build will skip --load \
                 (image won't be loaded into local daemon)",
            );
        }
        daemon_ok
    } else {
        true
    };

    let cmd_args = build_docker_v2_command(&crate::command::DockerV2Spec {
        staging_dir: staging_str,
        platforms: &platform_refs,
        image_tags: &image_tags,
        build_args: &rendered_build_args,
        annotations: &rendered_annotations,
        labels: &rendered_labels,
        flags: &rendered_flags,
        sbom: sbom_enabled,
        push: should_push,
        load: should_load,
        backend,
    })?;

    // Per-pipe `docker_v2.retry` takes precedence (with deprecation warning)
    // over the top-level `Project.Retry`; defaults apply when neither is set.
    let (max_attempts, base_delay, max_delay) =
        resolve_retry_params(&v2_cfg.retry, &ctx.config.retry).with_context(|| {
            format!(
                "dockers_v2: invalid retry config for crate {} index {}",
                krate.name, idx
            )
        })?;

    // Track multi-platform V2 tags so docker_manifests can skip redundant
    // manifest creation for images that are already multi-arch manifest
    // lists.
    if snapshot_plats.len() > 1 && should_push {
        for tag in &image_tags {
            v2_multiplatform_tags.insert(tag.clone());
        }
    }

    if dry_run {
        log.status(&format!("(dry-run) would run: {}", cmd_args.join(" ")));
        if max_attempts > 1 {
            log.status(&format!(
                "(dry-run) would retry up to {} attempts, base delay {:?}{}",
                max_attempts,
                base_delay,
                match max_delay {
                    Some(d) => format!(", max delay {:?}", d),
                    None => String::new(),
                }
            ));
        }
        for tag in &image_tags {
            let mut meta = HashMap::new();
            meta.insert("tag".to_string(), tag.clone());
            insert_platforms_meta(&mut meta, snapshot_plats)?;
            meta.insert("api".to_string(), "v2".to_string());
            meta.insert(
                "use".to_string(),
                if is_podman { "podman" } else { "buildx" }.to_string(),
            );
            if let Some(ref id) = v2_cfg.id {
                meta.insert("id".to_string(), id.clone());
            }
            new_artifacts.push(Artifact {
                kind: ArtifactKind::DockerImageV2,
                name: tag.clone(),
                path: PathBuf::from(tag),
                target: None,
                crate_name: krate.name.clone(),
                metadata: meta,
                size: None,
            });
        }
    } else {
        let (skip_digest, digest_name_template) =
            resolve_digest_config(krate.docker_digest.as_ref(), ctx)?;

        // Pair with `--rewrite-timestamp` above: BuildKit needs
        // `SOURCE_DATE_EPOCH` in the build subprocess env to know what value
        // to rewrite layer mtimes to. Inherited from the harness's hermetic
        // env block; re-exported here so non-harness release runs with
        // determinism seeded also get reproducible images. User-supplied
        // `SOURCE_DATE_EPOCH` in `env:` blocks wins via the `or_insert` path.
        let mut env_vars: std::collections::BTreeMap<String, String> = ctx
            .template_vars()
            .all_config_env()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Some(det) = ctx.determinism.as_ref() {
            env_vars
                .entry("SOURCE_DATE_EPOCH".to_string())
                .or_insert_with(|| det.sde.to_string());
        }

        let backend_label = if is_podman { "podman" } else { "buildx" };
        build_jobs.push(DockerBuildJob {
            cmd_args,
            backend_label: backend_label.to_string(),
            crate_name: krate.name.clone(),
            idx,
            max_attempts,
            base_delay,
            max_delay,
            rendered_tags: image_tags,
            platforms_list: snapshot_plats.to_vec(),
            staging_dir: staging_dir.to_path_buf(),
            id: v2_cfg.id.clone(),
            use_backend: Some(backend_label.to_string()),
            deadline: ctx.retry_deadline(),
            is_podman,
            push: should_push,
            dist: dist.to_path_buf(),
            skip_digest,
            digest_name_template,
            env_vars,
        });
    }

    Ok(())
}

/// Validate Docker V2 config ID uniqueness. Duplicate IDs are a hard
/// error because downstream filters rely on IDs to disambiguate
/// artifacts.
pub(crate) fn validate_docker_v2_id_uniqueness(
    crates: &[anodizer_core::config::CrateConfig],
) -> Result<()> {
    let mut v2_ids: HashSet<String> = HashSet::new();
    for krate in crates {
        if let Some(ref v2_cfgs) = krate.dockers_v2 {
            for v2_cfg in v2_cfgs {
                if let Some(ref id) = v2_cfg.id
                    && !v2_ids.insert(id.clone())
                {
                    anyhow::bail!(
                        "found 2 dockers_v2 with the ID '{}', please fix your config",
                        id
                    );
                }
            }
        }
    }
    Ok(())
}

/// Validate the buildx plugin once if any V2 configs exist (V2 always uses
/// buildx). `check_buildx_version` confirms the plugin is reachable (mirrors
/// the driver check), and `check_buildx_driver` validates
/// the active driver supports multi-platform builds. Both are warn-only:
/// downstream `buildx build` surfaces a hard error if it cannot actually run.
pub(crate) fn run_buildx_probes(stage: &super::DockerStage, log: &anodizer_core::log::StageLogger) {
    match &stage.probe {
        Some(custom) => run_buildx_version_check(log, || custom()),
        None => check_buildx_version(log),
    }
    check_buildx_driver(log);
}
