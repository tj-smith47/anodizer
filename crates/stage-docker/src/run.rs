use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::HookEntry;
use anodizer_core::context::Context;
use anodizer_core::hooks::run_hooks;
use anodizer_core::stage::Stage;

/// Per-docker_v2-config post-hook state captured at Step 1 and consumed once
/// at the end of Step 3. Fires exactly once per config (not per snapshot
/// platform job) — matching GR's `buildImage` lifecycle.
struct PerConfigPostHook {
    idx: usize,
    id: Option<String>,
    hooks: Vec<HookEntry>,
    images_json: serde_json::Value,
    dockerfile_path: String,
    staging_dir: PathBuf,
    base_image_name: String,
    base_image_digest: String,
}

use super::baseimage::get_base_image;
use super::build::{DockerBuildJob, DockerBuildResult, execute_docker_build};
use super::command::{
    apply_docker_v2_defaults, build_docker_v2_command, generate_v2_image_tags,
    is_docker_v2_sbom_enabled, is_docker_v2_skipped, resolve_backend, resolve_digest_config,
    resolve_manifester, resolve_skip_push,
};
use super::detect::{
    check_buildx_driver, check_buildx_version, is_docker_daemon_available, run_buildx_version_check,
};
use super::platform::tag_suffix;
use super::retry::resolve_retry_params;
use super::spelling::{find_image_digest, levenshtein_distance};
use super::staging::{
    copy_dockerfile, stage_artifacts_v2, stage_extra_files, warn_project_markers_in_extra_files,
};

impl Stage for super::DockerStage {
    fn name(&self) -> &str {
        "docker"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("docker");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();
        let parallelism = ctx.options.parallelism.max(1);

        // Collect crates that have docker, docker_v2, or docker_manifests config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.docker_v2.is_some() || c.docker_manifests.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // Validate Docker V2 config ID uniqueness. Matches GoReleaser
        // `internal/ids/ids.go:26-36` — duplicate IDs are a hard error in
        // `v2/docker.go:93`, because downstream filters rely on IDs to
        // disambiguate artifacts.
        {
            let mut v2_ids: HashSet<String> = HashSet::new();
            for krate in &crates {
                if let Some(ref v2_cfgs) = krate.docker_v2 {
                    for v2_cfg in v2_cfgs {
                        if let Some(ref id) = v2_cfg.id
                            && !v2_ids.insert(id.clone())
                        {
                            anyhow::bail!(
                                "found 2 docker_v2 with the ID '{}', please fix your config",
                                id
                            );
                        }
                    }
                }
            }
        }

        // Validate the buildx plugin once if any V2 configs exist (V2 always
        // uses buildx). `check_buildx_version` confirms the plugin is
        // reachable (mirrors GoReleaser commit e09e23a / #6526), and
        // `check_buildx_driver` validates the active driver supports
        // multi-platform builds. Both are warn-only: downstream `buildx
        // build` surfaces a hard error if it cannot actually run.
        if !dry_run && crates.iter().any(|c| c.docker_v2.is_some()) {
            match &self.probe {
                Some(custom) => run_buildx_version_check(&log, || custom()),
                None => check_buildx_version(&log),
            }
            check_buildx_driver(&log);
        }

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        // Track image references pushed by docker_v2 multi-platform builds.
        // These are already multi-arch manifest lists — docker_manifests must
        // not try to re-create them from non-existent per-platform tags.
        let mut v2_multiplatform_tags: HashSet<String> = HashSet::new();

        // ==================================================================
        // Step 1: Prepare all docker build jobs sequentially
        //
        // This phase needs &mut Context for template rendering and artifact
        // lookups.  Each job is fully self-contained after preparation.
        // ==================================================================
        let mut build_jobs: Vec<DockerBuildJob> = Vec::new();
        let mut config_post_hooks: Vec<PerConfigPostHook> = Vec::new();
        let mut config_first_digest: std::collections::BTreeMap<usize, String> =
            std::collections::BTreeMap::new();
        // Pre-hook failures for individual docker_v2 configs are isolated:
        // mirrors GR's `semerrgroup` parallel-per-config error semantic at
        // `internal/pipe/docker/v2/docker.go:118-140`, where a failed config
        // does not cancel sibling configs already in flight. anodize collects
        // the errors and surfaces them after Step 3 — `continue` past a
        // failed pre-hook skips that config's build + post-hook queueing.
        let mut pre_hook_errors: Vec<anyhow::Error> = Vec::new();

        for krate in &crates {
            // ------------------------------------------------------------------
            // Docker V2 configs
            // ------------------------------------------------------------------
            let docker_v2_configs = match krate.docker_v2.as_ref() {
                Some(cfgs) => cfgs.clone(),
                None => Vec::new(),
            };

            // Apply GoReleaser-compatible defaults to V2 configs.
            let docker_v2_configs: Vec<_> = docker_v2_configs
                .into_iter()
                .map(|cfg| apply_docker_v2_defaults(cfg, &ctx.config.project_name))
                .collect();

            for (idx, v2_cfg) in docker_v2_configs.iter().enumerate() {
                // Check disable — skip when template evaluates to true
                if is_docker_v2_skipped(&v2_cfg.skip, ctx)? {
                    log.status(&format!(
                        "docker_v2[{}]: skipping config for crate {} (skip=true)",
                        idx, krate.name
                    ));
                    continue;
                }

                // Template-render platforms and filter empty results (GoReleaser's tpl.ApplySlice)
                let platforms: Vec<String> = v2_cfg
                    .platforms
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|p| ctx.render_template(&p).ok().filter(|r| !r.is_empty()))
                    .collect();

                // V2 always uses buildx
                resolve_backend(Some("buildx"), platforms.len() > 1)?;

                // Template-render the Dockerfile path FIRST so an empty
                // template short-circuits before we touch the filesystem
                // (avoids orphan staging dirs / stale staged artifacts when
                // `dockerfile: "{{ if .IsSnapshot }}Dockerfile{{ end }}"`
                // renders to "" during release).
                // GR parity (commit d788340): check the *rendered* template
                // for emptiness — not the raw template.
                let rendered_dockerfile =
                    ctx.render_template(&v2_cfg.dockerfile).with_context(|| {
                        format!(
                            "docker_v2: render dockerfile path '{}' for crate {}",
                            v2_cfg.dockerfile, krate.name
                        )
                    })?;
                if rendered_dockerfile.trim().is_empty() {
                    log.status(&format!(
                        "docker_v2[{}]: skipping crate {} — dockerfile template rendered empty",
                        idx, krate.name
                    ));
                    continue;
                }

                // Build staging directory — use "docker_v2" subdirectory to avoid
                // collisions with legacy docker configs.
                let staging_dir: PathBuf = dist
                    .join("docker_v2")
                    .join(&krate.name)
                    .join(idx.to_string());

                if !dry_run {
                    fs::create_dir_all(&staging_dir).with_context(|| {
                        format!("docker_v2: create staging dir {}", staging_dir.display())
                    })?;
                }

                // Stage artifacts using V2 layout (os/arch/name, multiple artifact types)
                stage_artifacts_v2(
                    &platforms,
                    &staging_dir,
                    dry_run,
                    v2_cfg.ids.as_ref(),
                    &krate.name,
                    ctx,
                    &log,
                )?;

                copy_dockerfile(
                    &rendered_dockerfile,
                    &staging_dir,
                    dry_run,
                    &log,
                    "docker_v2",
                )?;

                if let Some(ref extra_files) = v2_cfg.extra_files {
                    warn_project_markers_in_extra_files(extra_files, &log, "docker_v2");
                    stage_extra_files(extra_files, &staging_dir, dry_run, &log, "docker_v2")?;
                }

                // Resolve the Dockerfile's final-stage base image so the two
                // template vars `BaseImage` and `BaseImageDigest` are visible
                // to every downstream render (image tags, labels,
                // annotations, build args, flags, hooks). Failures are
                // soft — a missing annotation is better than a hard build
                // failure when, say, `docker buildx imagetools inspect` is
                // unreachable. Vars are cleared at the end of this v2_cfg
                // iteration so they don't leak into the next config.
                let base_image_info =
                    match get_base_image(std::path::Path::new(&rendered_dockerfile), dry_run, &log)
                    {
                        Ok(opt) => opt,
                        Err(e) => {
                            log.warn(&format!(
                                "docker_v2[{}]: could not parse base image from {}: {:#}",
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

                // Render tags through template engine
                let mut rendered_tags: Vec<String> = Vec::new();
                for tag_tmpl in &v2_cfg.tags {
                    let rendered = ctx.render_template(tag_tmpl).with_context(|| {
                        format!(
                            "docker_v2: render tag template '{}' for crate {}",
                            tag_tmpl, krate.name
                        )
                    })?;
                    if rendered.is_empty() {
                        continue;
                    }
                    rendered_tags.push(rendered);
                }

                // Render images through template engine
                let mut rendered_images: Vec<String> = Vec::new();
                for img_tmpl in &v2_cfg.images {
                    let rendered = ctx.render_template(img_tmpl).with_context(|| {
                        format!(
                            "docker_v2: render image template '{}' for crate {}",
                            img_tmpl, krate.name
                        )
                    })?;
                    if rendered.is_empty() {
                        continue;
                    }
                    rendered_images.push(rendered);
                }

                // For snapshot builds, GoReleaser splits multi-platform configs
                // into per-platform builds with --load (no push) and tag suffix.
                // This builds each platform separately so images are available locally.
                let snapshot_platforms: Vec<Vec<String>> =
                    if ctx.is_snapshot() && platforms.len() > 1 {
                        platforms.iter().map(|p| vec![p.clone()]).collect()
                    } else {
                        vec![platforms.clone()]
                    };

                // Pre-build hooks fire ONCE per docker_v2 config, matching
                // GR's `buildImage` lifecycle. `Images` is the full
                // cross-product of `rendered_images × rendered_tags` (no
                // per-platform arch suffix — that's a snapshot-only
                // tag-disambiguation step that runs after the hook). Exposed
                // as a real Tera list so `{% for img in Images %}` works,
                // mirroring GR's `tmpl.Fields{ keyImages: da.images }` where
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
                        "pre-docker_v2[{}]",
                        v2_cfg.id.as_deref().unwrap_or(&idx.to_string())
                    );
                    if let Err(e) =
                        run_hooks(&pre_hooks, &pre_label, dry_run, &log, Some(&hook_vars))
                    {
                        log.warn(&format!(
                            "{}: pre-hook failed; skipping this config's build (other configs continue): {:#}",
                            pre_label, e
                        ));
                        pre_hook_errors.push(e);
                        ctx.template_vars_mut().unset("BaseImage");
                        ctx.template_vars_mut().unset("BaseImageDigest");
                        continue;
                    }
                }

                for snapshot_plats in &snapshot_platforms {
                    let mut per_plat_tags = rendered_tags.clone();

                    // During snapshot, add platform arch suffix to each tag.
                    if ctx.is_snapshot() && snapshot_plats.len() == 1 {
                        let suffix = tag_suffix(&snapshot_plats[0]);
                        for tag in &mut per_plat_tags {
                            tag.push('-');
                            tag.push_str(&suffix);
                        }
                    }

                    // Generate image:tag combinations
                    let image_tags = generate_v2_image_tags(&rendered_images, &per_plat_tags);

                    if image_tags.is_empty() {
                        log.warn(&format!(
                        "docker_v2[{}]: no image tags produced for crate {} (images or tags resolved to empty); skipping",
                        idx, krate.name
                    ));
                        continue;
                    }

                    // Render build_args (template-aware keys and values, matching GoReleaser's tplMapFlags)
                    let mut rendered_build_args: Vec<(String, String)> = Vec::new();
                    if let Some(ref args_map) = v2_cfg.build_args {
                        for (key_tmpl, value_tmpl) in args_map {
                            let rendered_key =
                                ctx.render_template(key_tmpl).with_context(|| {
                                    format!("docker_v2: render build_arg key '{}'", key_tmpl)
                                })?;
                            let rendered_value =
                                ctx.render_template(value_tmpl).with_context(|| {
                                    format!("docker_v2: render build_arg value for '{}'", key_tmpl)
                                })?;
                            // Skip entries where key or value is empty after templating
                            if !rendered_key.is_empty() && !rendered_value.is_empty() {
                                rendered_build_args.push((rendered_key, rendered_value));
                            }
                        }
                        rendered_build_args.sort_by(|a, b| a.0.cmp(&b.0));
                    }

                    // Render annotations (template-aware keys and values)
                    let mut rendered_annotations: Vec<(String, String)> = Vec::new();
                    if let Some(ref ann_map) = v2_cfg.annotations {
                        for (key_tmpl, value_tmpl) in ann_map {
                            let rendered_key =
                                ctx.render_template(key_tmpl).with_context(|| {
                                    format!("docker_v2: render annotation key '{}'", key_tmpl)
                                })?;
                            let rendered_value =
                                ctx.render_template(value_tmpl).with_context(|| {
                                    format!("docker_v2: render annotation value for '{}'", key_tmpl)
                                })?;
                            if !rendered_key.is_empty() && !rendered_value.is_empty() {
                                rendered_annotations.push((rendered_key, rendered_value));
                            }
                        }
                        rendered_annotations.sort_by(|a, b| a.0.cmp(&b.0));
                    }

                    // Render labels (template-aware keys and values)
                    let mut rendered_labels: Vec<(String, String)> = Vec::new();
                    if let Some(ref label_map) = v2_cfg.labels {
                        for (key_tmpl, value_tmpl) in label_map {
                            let rendered_key =
                                ctx.render_template(key_tmpl).with_context(|| {
                                    format!("docker_v2: render label key '{}'", key_tmpl)
                                })?;
                            let rendered_value =
                                ctx.render_template(value_tmpl).with_context(|| {
                                    format!("docker_v2: render label value for '{}'", key_tmpl)
                                })?;
                            if !rendered_key.is_empty() && !rendered_value.is_empty() {
                                rendered_labels.push((rendered_key, rendered_value));
                            }
                        }
                        rendered_labels.sort_by(|a, b| a.0.cmp(&b.0));
                    }

                    // Render flags (template-aware, filter empty results)
                    let mut rendered_flags: Vec<String> = Vec::new();
                    if let Some(ref flag_list) = v2_cfg.flags {
                        for flag_tmpl in flag_list {
                            let rendered = ctx.render_template(flag_tmpl).with_context(|| {
                                format!("docker_v2: render flag '{}'", flag_tmpl)
                            })?;
                            if !rendered.is_empty() {
                                rendered_flags.push(rendered);
                            }
                        }
                    }

                    // Evaluate sbom — GoReleaser only adds SBOM in the Publish path (not snapshot).
                    let sbom_enabled = if ctx.is_snapshot() {
                        false
                    } else {
                        is_docker_v2_sbom_enabled(&v2_cfg.sbom, ctx)?
                    };

                    let platform_refs: Vec<&str> =
                        snapshot_plats.iter().map(|s| s.as_str()).collect();

                    // Snapshot builds never push (GoReleaser uses --load per-platform).
                    // The canonical `skip:` field suppresses publish via
                    // `is_active`-style gating earlier in the pipeline.
                    let should_push = if ctx.is_snapshot() { false } else { !dry_run };

                    // Determine whether --load is safe (requires a running daemon).
                    // In snapshot mode, warn if daemon is unavailable and skip --load.
                    let should_load = if ctx.is_snapshot() {
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
                        staging_dir: &staging_str,
                        platforms: &platform_refs,
                        image_tags: &image_tags,
                        build_args: &rendered_build_args,
                        annotations: &rendered_annotations,
                        labels: &rendered_labels,
                        flags: &rendered_flags,
                        sbom: sbom_enabled,
                        push: should_push,
                        load: should_load,
                    })?;

                    // Resolve retry configuration: per-pipe `docker_v2.retry`
                    // takes precedence (with deprecation warning) over the
                    // top-level `Project.Retry`; defaults apply when neither
                    // is set.
                    let (max_attempts, base_delay, max_delay) =
                        resolve_retry_params(&v2_cfg.retry, &ctx.config.retry).with_context(
                            || {
                                format!(
                                    "docker_v2: invalid retry config for crate {} index {}",
                                    krate.name, idx
                                )
                            },
                        )?;

                    // Track multi-platform V2 tags so docker_manifests can skip
                    // redundant manifest creation for images that are already
                    // multi-arch manifest lists.
                    if snapshot_plats.len() > 1 && should_push {
                        for tag in &image_tags {
                            v2_multiplatform_tags.insert(tag.clone());
                        }
                    }

                    if dry_run {
                        log.status(&format!("(dry-run) would run: {}", cmd_args.join(" ")));
                        if max_attempts > 1 {
                            log.status(&format!(
                                "(dry-run) retry: up to {} attempts, base delay {:?}{}",
                                max_attempts,
                                base_delay,
                                match max_delay {
                                    Some(d) => format!(", max delay {:?}", d),
                                    None => String::new(),
                                }
                            ));
                        }
                        // Register artifacts in dry-run
                        for tag in &image_tags {
                            let mut meta = HashMap::new();
                            meta.insert("tag".to_string(), tag.clone());
                            meta.insert("platforms".to_string(), snapshot_plats.join(","));
                            meta.insert("api".to_string(), "v2".to_string());
                            meta.insert("use".to_string(), "buildx".to_string());
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
                        // Resolve docker_digest config from the crate
                        let (skip_digest, digest_name_template) =
                            resolve_digest_config(krate.docker_digest.as_ref(), ctx)?;

                        build_jobs.push(DockerBuildJob {
                            cmd_args,
                            backend_label: "buildx".to_string(),
                            crate_name: krate.name.clone(),
                            idx,
                            max_attempts,
                            base_delay,
                            max_delay,
                            rendered_tags: image_tags,
                            platforms_str: snapshot_plats.join(","),
                            staging_dir: staging_dir.clone(),
                            id: v2_cfg.id.clone(),
                            use_backend: Some("buildx".to_string()),
                            dist: dist.clone(),
                            skip_digest,
                            digest_name_template,
                            env_vars: ctx
                                .template_vars()
                                .all_config_env()
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect(),
                        });
                    }
                } // end for snapshot_plats

                // Dry-run post-hooks fire ONCE per docker_v2 config with an
                // empty `Digest` so template typos still surface. Real-run
                // post-hooks fire from Step 3 below — also once per config,
                // keyed by `idx` against the first matching job's digest.
                if dry_run && !post_hooks.is_empty() {
                    let mut hook_vars = ctx.template_vars().clone();
                    hook_vars.set_structured("Images", cfg_images_json.clone());
                    hook_vars.set("Dockerfile", &rendered_dockerfile);
                    hook_vars.set("ContextDir", &staging_str);
                    hook_vars.set("Digest", "");
                    let post_label = format!(
                        "post-docker_v2[{}]",
                        v2_cfg.id.as_deref().unwrap_or(&idx.to_string())
                    );
                    run_hooks(&post_hooks, &post_label, dry_run, &log, Some(&hook_vars))?;
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

                // Remove per-config BaseImage / BaseImageDigest so the next
                // docker_v2 config — or any downstream stage — does not
                // observe stale values. `unset` (not `set("")`) so strict-
                // mode templates can distinguish "undefined" from
                // "defined-empty"; mirrors GR's overlay-drop semantic from
                // `tpl.WithExtraFields` in `v2/docker.go:319`.
                ctx.template_vars_mut().unset("BaseImage");
                ctx.template_vars_mut().unset("BaseImageDigest");
            }
        }

        // ==================================================================
        // Step 2: Execute docker build jobs in parallel
        //
        // Uses std::thread::scope with a simple semaphore pattern (channel-
        // based) bounded by ctx.parallelism, matching GoReleaser's
        // semerrgroup.New(ctx.Parallelism) behavior.
        // ==================================================================
        if !build_jobs.is_empty() {
            use std::sync::mpsc;

            /// Drop guard that returns a semaphore token to the channel when
            /// dropped, ensuring the token is returned even if the thread
            /// panics. Without this, a panic would permanently consume a slot
            /// and eventually deadlock the remaining threads.
            struct SemaphoreGuard<'a> {
                sender: &'a mpsc::SyncSender<()>,
            }
            impl Drop for SemaphoreGuard<'_> {
                fn drop(&mut self) {
                    // `send` cannot fail because thread::scope guarantees all
                    // guards drop before sem_rx; spawning a detached thread
                    // here would silently lose a token.
                    let _ = self.sender.send(());
                }
            }

            // Channel-based semaphore: pre-fill with `parallelism` tokens.
            // Each thread takes a token before starting and returns it on
            // completion.  This bounds active docker builds to `parallelism`.
            let (sem_tx, sem_rx) = mpsc::sync_channel::<()>(parallelism);
            for _ in 0..parallelism {
                let _ = sem_tx.send(());
            }

            // Collect results in order (indexed by job position).
            let job_count = build_jobs.len();
            let log_ref = &log;
            let results: Vec<Result<DockerBuildResult>> = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(job_count);

                for job in &build_jobs {
                    // Acquire a semaphore token (blocks if all slots are busy).
                    let _ = sem_rx.recv();
                    let sem_tx_ref = &sem_tx;

                    let handle = scope.spawn(move || {
                        // Guard returns the token on drop (including panic).
                        let _guard = SemaphoreGuard { sender: sem_tx_ref };
                        execute_docker_build(job, log_ref)
                    });
                    handles.push(handle);
                }

                handles
                    .into_iter()
                    .map(|h| {
                        anodizer_core::parallel::join_panic_to_err(h.join(), "docker build")
                            .and_then(|r| r)
                    })
                    .collect()
            });

            // ==================================================================
            // Step 3: Collect results and register artifacts
            // ==================================================================
            for (job, result) in build_jobs.iter().zip(results) {
                let build_result = result?;
                for tag in &job.rendered_tags {
                    let mut meta = HashMap::new();
                    meta.insert("tag".to_string(), tag.clone());
                    meta.insert("platforms".to_string(), job.platforms_str.clone());
                    if let Some(ref id) = job.id {
                        meta.insert("id".to_string(), id.clone());
                    }
                    if let Some(ref backend) = job.use_backend {
                        meta.insert("use".to_string(), backend.clone());
                    }
                    if let Some(d) = build_result.tag_digests.get(tag) {
                        meta.insert("digest".to_string(), d.clone());
                    }
                    // All anodizer docker builds are V2 → register as DockerImageV2.
                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::DockerImageV2,
                        name: tag.clone(),
                        path: PathBuf::from(tag),
                        target: None,
                        crate_name: job.crate_name.clone(),
                        metadata: meta,
                        size: None,
                    });
                }

                // Register digest files as artifacts.
                for digest_path in &build_result.digest_files {
                    let artifact_name = if let Some(ref tmpl) = job.digest_name_template {
                        // name_template controls the artifact name, not the file path
                        tmpl.clone()
                    } else {
                        digest_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string()
                    };
                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::DockerDigest,
                        name: artifact_name,
                        path: digest_path.clone(),
                        target: None,
                        crate_name: job.crate_name.clone(),
                        metadata: HashMap::new(),
                        size: None,
                    });
                }

                // Capture the first digest produced for this docker_v2
                // config so the per-config post-hook (fired below, after
                // all jobs complete) can render `{{ .Digest }}`. In snapshot
                // multi-platform mode anodize emits one job per platform —
                // any platform's digest is representative since GR's
                // post-hook lifecycle has only one digest variable per
                // config.
                if !config_first_digest.contains_key(&job.idx)
                    && let Some(d) = build_result.tag_digests.values().next()
                {
                    config_first_digest.insert(job.idx, d.clone());
                }
            }

            // Per-config post-hooks fire ONCE per docker_v2 config, after
            // every snapshot-platform job for that config has completed.
            // Matches GR's `buildImage` lifecycle (pre → build → post).
            for cph in &config_post_hooks {
                let digest_val = config_first_digest.get(&cph.idx).cloned().ok_or_else(|| {
                    anyhow::anyhow!(
                        "docker_v2[{}]: post-hooks configured but no image digest captured \
                         (iidfile id.txt missing or empty after a successful build); \
                         this usually means buildx + multi-platform --push produced no iidfile — \
                         upgrade buildx or remove the post-hook",
                        cph.id.as_deref().unwrap_or(&cph.idx.to_string())
                    )
                })?;
                let mut hook_vars = ctx.template_vars().clone();
                hook_vars.set_structured("Images", cph.images_json.clone());
                hook_vars.set("Dockerfile", &cph.dockerfile_path);
                hook_vars.set("ContextDir", &cph.staging_dir.to_string_lossy());
                hook_vars.set("Digest", &digest_val);
                hook_vars.set("BaseImage", &cph.base_image_name);
                hook_vars.set("BaseImageDigest", &cph.base_image_digest);
                let post_label = format!(
                    "post-docker_v2[{}]",
                    cph.id.as_deref().unwrap_or(&cph.idx.to_string())
                );
                run_hooks(&cph.hooks, &post_label, false, &log, Some(&hook_vars))?;
            }
        }

        // Surface accumulated pre-hook errors AFTER successful per-config
        // builds — matches GR's `g.Wait()` (`v2/docker.go:141`) which returns
        // the first error only after every parallel config has finished. The
        // first error is most informative; remaining errors were already
        // logged inline via `log.warn` in the Step 1 collector above.
        if let Some(first) = pre_hook_errors.into_iter().next() {
            return Err(first);
        }

        // ==================================================================
        // Docker manifests (must run after all builds complete, since they
        // reference the built image digests)
        // ==================================================================
        let manifest_env_vars = ctx.template_vars().all_config_env().clone();
        for krate in &crates {
            // ------------------------------------------------------------------
            // Docker manifests
            // ------------------------------------------------------------------
            if let Some(ref manifest_configs) = krate.docker_manifests {
                for (midx, manifest_cfg) in manifest_configs.iter().enumerate() {
                    // Validate: image_templates must not be empty — a manifest
                    // with zero images is always a configuration error.
                    if manifest_cfg.image_templates.is_empty() {
                        let fallback = format!("index {}", midx);
                        let manifest_label = manifest_cfg.id.as_deref().unwrap_or(&fallback);
                        anyhow::bail!(
                            "docker manifest '{}': image_templates must not be empty",
                            manifest_label
                        );
                    }

                    // Render the manifest name template
                    let manifest_name = ctx
                        .render_template(&manifest_cfg.name_template)
                        .with_context(|| {
                            format!(
                                "docker: render manifest name_template '{}' for crate {}",
                                manifest_cfg.name_template, krate.name
                            )
                        })?;

                    // Skip manifests whose target tag was already pushed as a
                    // multi-arch manifest list by docker_v2.  docker_v2 with
                    // --platform=linux/amd64,linux/arm64 --push creates a native
                    // multi-arch manifest; docker_manifests would try to re-create
                    // it from per-platform tags (e.g. :0.3.3-amd64) that don't
                    // exist, causing "manifest unknown" errors.
                    if v2_multiplatform_tags.contains(&manifest_name) {
                        log.status(&format!(
                            "docker: skipping manifest '{}' — already pushed as multi-arch by docker_v2",
                            manifest_name
                        ));
                        continue;
                    }

                    // Render image templates, skipping entries that resolve
                    // to empty strings (e.g. conditional templates that
                    // evaluate to nothing for certain configurations).
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
                                "docker: manifest image_template '{}' rendered to empty string, skipping",
                                tmpl
                            ));
                            continue;
                        }
                        rendered_images.push(img);
                    }

                    // Determine the binary for manifest commands. F7 — see
                    // `resolve_manifester` for the validation rationale.
                    let manifest_bin = resolve_manifester(manifest_cfg.use_backend.as_deref())?;

                    // Render create_flags through template engine
                    let rendered_create_flags: Vec<String> = manifest_cfg
                        .create_flags
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .map(|f| ctx.render_template(f).unwrap_or_else(|_| f.clone()))
                        .collect();

                    // Render push_flags through template engine
                    let rendered_push_flags: Vec<String> = manifest_cfg
                        .push_flags
                        .as_deref()
                        .unwrap_or(&[])
                        .iter()
                        .map(|f| ctx.render_template(f).unwrap_or_else(|_| f.clone()))
                        .collect();

                    // Build `docker manifest create` command.
                    // Pin image references to their digest (sha256:...) when
                    // available, so the manifest references immutable content
                    // rather than mutable tags.  Digests are captured during the
                    // image push phase and stored in the `new_artifacts` list.
                    let mut create_cmd: Vec<String> = vec![
                        manifest_bin.to_string(),
                        "manifest".to_string(),
                        "create".to_string(),
                        manifest_name.clone(),
                    ];
                    for img in &rendered_images {
                        if let Some(digest) = find_image_digest(&new_artifacts, img) {
                            let pinned = format!("{}@{}", img, digest);
                            log.verbose(&format!("manifest: pinning {} to digest {}", img, digest));
                            create_cmd.push(pinned);
                        } else {
                            // "Did you mean?" — find closest matching image by edit distance
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

                            // Distance > 0 to avoid suggesting the same name back (which
                            // happens when `img` itself is in the candidate set but its
                            // digest hadn't been recorded yet at lookup time — a stale-cache
                            // race, not a typo).
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
                                log.warn(&format!(
                                    "no digest found for {}, using tag reference",
                                    img
                                ));
                            }
                            create_cmd.push(img.clone());
                        }
                    }
                    for flag in &rendered_create_flags {
                        create_cmd.push(flag.clone());
                    }

                    // Determine whether to push
                    let manifest_skip_push = resolve_skip_push(&manifest_cfg.skip_push, ctx);
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
                        // Remove any existing manifest before recreating.
                        // Matches GoReleaser `internal/pipe/docker/api_docker.go:26`:
                        //   `_ = runCommand(ctx, ".", "docker", "manifest", "rm", manifest)`
                        // — all errors ignored. A missing manifest is the common case
                        // (first run / new tag), and any other failure (auth, network,
                        // daemon offline) will surface when `manifest create` runs
                        // right after, with a more actionable error.
                        let mut rm_cmd = Command::new(manifest_bin);
                        rm_cmd.args(["manifest", "rm", &manifest_name]);
                        for (key, value) in &manifest_env_vars {
                            rm_cmd.env(key, value);
                        }
                        rm_cmd.output().ok();

                        // Manifest create/push with retry logic — registry
                        // operations can fail transiently. Uses the
                        // manifest's retry config (same as docker build):
                        // per-pipe wins (with deprecation warning) over the
                        // top-level `Project.Retry`; defaults apply otherwise.
                        let (manifest_max_attempts, manifest_base_delay, manifest_max_delay) =
                            resolve_retry_params(&manifest_cfg.retry, &ctx.config.retry)
                                .with_context(|| {
                                    format!(
                                        "docker: invalid retry config for manifest {} crate {}",
                                        midx, krate.name
                                    )
                                })?;

                        {
                            use anodizer_core::retry::{RetryPolicy, retry_sync};
                            use std::ops::ControlFlow;
                            let policy = RetryPolicy {
                                max_attempts: manifest_max_attempts,
                                base_delay: manifest_base_delay,
                                max_delay: manifest_max_delay.unwrap_or(Duration::MAX),
                            };
                            retry_sync(&policy, |attempt| {
                                if attempt > 1 {
                                    log.warn(&format!(
                                        "manifest create attempt {}/{} failed, retrying…",
                                        attempt - 1,
                                        manifest_max_attempts,
                                    ));
                                }
                                log.status(&format!("running: {}", create_cmd.join(" ")));
                                let mut create_command = Command::new(&create_cmd[0]);
                                create_command.args(&create_cmd[1..]);
                                for (key, value) in &manifest_env_vars {
                                    create_command.env(key, value);
                                }
                                let output = match create_command.output() {
                                    Ok(o) => o,
                                    Err(e) => {
                                        return Err(ControlFlow::Break(
                                            anyhow::Error::from(e).context(format!(
                                                "docker: manifest create for crate {} manifest {} (attempt {}/{})",
                                                krate.name, midx, attempt, manifest_max_attempts
                                            )),
                                        ));
                                    }
                                };
                                match log.check_output(output, "docker manifest create") {
                                    Ok(_) => {
                                        if attempt > 1 {
                                            log.status(&format!(
                                                "docker manifest create succeeded on attempt {}/{}",
                                                attempt, manifest_max_attempts
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
                            })?;
                        }

                        // Push the manifest (with retry) and capture digest
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

                            use anodizer_core::retry::{RetryPolicy, retry_sync};
                            use std::ops::ControlFlow;
                            let policy = RetryPolicy {
                                max_attempts: manifest_max_attempts,
                                base_delay: manifest_base_delay,
                                max_delay: manifest_max_delay.unwrap_or(Duration::MAX),
                            };
                            retry_sync(&policy, |attempt| {
                                if attempt > 1 {
                                    log.warn(&format!(
                                        "manifest push attempt {}/{} failed, retrying…",
                                        attempt - 1,
                                        manifest_max_attempts,
                                    ));
                                }
                                log.status(&format!("running: {}", push_cmd.join(" ")));
                                let mut push_command = Command::new(&push_cmd[0]);
                                push_command.args(&push_cmd[1..]);
                                for (key, value) in &manifest_env_vars {
                                    push_command.env(key, value);
                                }
                                let output = match push_command.output() {
                                    Ok(o) => o,
                                    Err(e) => {
                                        return Err(ControlFlow::Break(
                                            anyhow::Error::from(e).context(format!(
                                                "docker: manifest push for crate {} manifest {} (attempt {}/{})",
                                                krate.name, midx, attempt, manifest_max_attempts
                                            )),
                                        ));
                                    }
                                };
                                // Capture stdout for digest extraction before checking status
                                let push_stdout =
                                    String::from_utf8_lossy(&output.stdout).to_string();
                                match log.check_output(output, "docker manifest push") {
                                    Ok(_) => {
                                        if attempt > 1 {
                                            log.status(&format!(
                                                "docker manifest push succeeded on attempt {}/{}",
                                                attempt, manifest_max_attempts
                                            ));
                                        }
                                        // Extract digest from push output (sha256:64hexchars)
                                        if let Some(start) = push_stdout.find("sha256:") {
                                            let candidate = &push_stdout[start..];
                                            if candidate.len() >= 71
                                                && candidate[7..71]
                                                    .chars()
                                                    .all(|c| c.is_ascii_hexdigit())
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
                            })?;
                        }
                    }

                    // Register DockerManifest artifact
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
                }
            }
        }

        // Write combined digests file (GoReleaser DockerDigest format).
        // Format: `<hex_digest>  <image_name>` per line, sorted,
        // where hex_digest is the sha256 hash WITHOUT the `sha256:` prefix.
        if !dry_run {
            let mut digest_lines: Vec<String> = Vec::new();
            for artifact in &new_artifacts {
                if let Some(digest) = artifact.metadata.get("digest") {
                    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
                    let name = artifact
                        .metadata
                        .get("tag")
                        .or(artifact.metadata.get("name"))
                        .or(artifact.metadata.get("manifest"))
                        .cloned()
                        .unwrap_or_default();
                    if !hex.is_empty() && !name.is_empty() {
                        digest_lines.push(format!("{}  {}", hex, name));
                    }
                }
            }
            if !digest_lines.is_empty() {
                digest_lines.sort();
                digest_lines.dedup();
                // Resolve the first non-empty `docker_digest.name_template`
                // across configured crates; fall back to `digests.txt`.
                let mut rendered_name: Option<String> = None;
                for krate in &ctx.config.crates {
                    let Some(dc) = krate.docker_digest.as_ref() else {
                        continue;
                    };
                    let Some(tmpl) = dc.name_template.as_ref() else {
                        continue;
                    };
                    let rendered = ctx.render_template(tmpl).with_context(|| {
                        format!(
                            "docker: render docker_digest.name_template '{}' for crate {}",
                            tmpl, krate.name
                        )
                    })?;
                    if !rendered.is_empty() {
                        rendered_name = Some(rendered);
                        break;
                    }
                }
                let digest_filename = rendered_name.unwrap_or_else(|| "digests.txt".to_string());
                let digest_file = dist.join(&digest_filename);
                if let Err(e) = fs::write(&digest_file, digest_lines.join("\n") + "\n") {
                    log.warn(&format!(
                        "failed to write combined digest file {}: {}",
                        digest_file.display(),
                        e
                    ));
                } else {
                    log.status(&format!(
                        "wrote combined digest file: {}",
                        digest_file.display()
                    ));
                }
            }
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}
