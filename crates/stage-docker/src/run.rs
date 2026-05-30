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

/// Per-docker_v2-config post-hook state captured during config preparation
/// and consumed after all parallel builds for that config have completed.
/// Fires exactly once per config (not per snapshot platform job) —
/// matching GR's `buildImage` lifecycle.
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

        validate_docker_v2_id_uniqueness(&crates)?;

        if !dry_run && crates.iter().any(|c| c.docker_v2.is_some()) {
            run_buildx_probes(self, &log);
        }

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        // Track image references pushed by docker_v2 multi-platform builds.
        // These are already multi-arch manifest lists — docker_manifests must
        // not try to re-create them from non-existent per-platform tags.
        let mut v2_multiplatform_tags: HashSet<String> = HashSet::new();

        // ==================================================================
        // Prepare all docker build jobs sequentially.
        //
        // Needs &mut Context for template rendering and artifact lookups.
        // Each job is fully self-contained after preparation.
        // ==================================================================
        let mut build_jobs: Vec<DockerBuildJob> = Vec::new();
        let mut config_post_hooks: Vec<PerConfigPostHook> = Vec::new();
        let mut config_first_digest: std::collections::BTreeMap<usize, String> =
            std::collections::BTreeMap::new();
        // Pre-hook failures for individual docker_v2 configs are isolated:
        // mirrors GR's `semerrgroup` parallel-per-config error semantic at
        // `internal/pipe/docker/v2/docker.go:118-140`, where a failed config
        // does not cancel sibling configs already in flight. anodize collects
        // the errors and surfaces them after all parallel jobs finish — an
        // early-return past a failed pre-hook skips that config's build +
        // post-hook queueing.
        let mut pre_hook_errors: Vec<anyhow::Error> = Vec::new();

        // Resolve the registry owner ONCE for the whole run — the per-crate
        // `images` default is `ghcr.io/{owner}/{crate}`, so the owner (the
        // GitHub org/user) is shared while the image name varies per crate.
        // Prefer the already-resolved `release.github.owner` (auto-filled from
        // the remote at config load) to avoid an extra `git remote` shell-out;
        // fall back to a single git-remote probe; `None` leaves the default off.
        let registry_owner = resolve_registry_owner(ctx, &crates);

        for krate in &crates {
            let docker_v2_configs = match krate.docker_v2.as_ref() {
                Some(cfgs) => cfgs.clone(),
                None => Vec::new(),
            };

            // Apply GoReleaser-compatible defaults to V2 configs. The per-crate
            // `images` default uses THIS crate's name, not the project primary.
            let docker_v2_configs: Vec<_> = docker_v2_configs
                .into_iter()
                .map(|cfg| {
                    apply_docker_v2_defaults(
                        cfg,
                        &ctx.config.project_name,
                        registry_owner.as_deref(),
                        &krate.name,
                    )
                })
                .collect();

            for (idx, v2_cfg) in docker_v2_configs.iter().enumerate() {
                prepare_v2_config(
                    ctx,
                    &log,
                    krate,
                    idx,
                    v2_cfg,
                    &dist,
                    dry_run,
                    &mut build_jobs,
                    &mut v2_multiplatform_tags,
                    &mut new_artifacts,
                    &mut pre_hook_errors,
                    &mut config_post_hooks,
                )?;
            }
        }

        if !build_jobs.is_empty() {
            execute_jobs_and_register(
                &log,
                &build_jobs,
                parallelism,
                &mut new_artifacts,
                &mut config_first_digest,
            )?;

            run_docker_post_hooks(ctx, &log, &config_post_hooks, &config_first_digest)?;
        }

        // Surface accumulated pre-hook errors AFTER successful per-config
        // builds — matches GR's `g.Wait()` (`v2/docker.go:141`) which returns
        // the first error only after every parallel config has finished. The
        // first error is most informative; remaining errors were already
        // logged inline via `log.warn` in the per-config collector above.
        if let Some(first) = pre_hook_errors.into_iter().next() {
            return Err(first);
        }

        // Docker manifests must run after all builds complete, since they
        // reference the built image digests.
        let manifest_env_vars = ctx.template_vars().all_config_env().clone();
        for krate in &crates {
            if let Some(ref manifest_configs) = krate.docker_manifests {
                for (midx, manifest_cfg) in manifest_configs.iter().enumerate() {
                    process_docker_manifest(
                        ctx,
                        &log,
                        krate,
                        midx,
                        manifest_cfg,
                        &v2_multiplatform_tags,
                        &manifest_env_vars,
                        dry_run,
                        &mut new_artifacts,
                    )?;
                }
            }
        }

        if !dry_run {
            write_combined_digest_file(ctx, &log, &dist, &new_artifacts)?;
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Run helpers
// ---------------------------------------------------------------------------

/// Resolve the registry owner used for the per-crate `ghcr.io/{owner}/{crate}`
/// `images` default. Resolution order (no new blocking network call):
///
/// 1. the first non-empty `release.github.owner` among the docker-bearing
///    crates — already auto-filled from the `origin` remote at config load, so
///    this is a pure config read;
/// 2. a single `git remote get-url origin` probe (GitHub-only) as a fallback
///    when no crate carries a resolved `release.github`.
///
/// Returns `None` when neither source yields an owner — the caller then leaves
/// `images` empty and the docker pipe emits no tags for that config (unchanged
/// behaviour). Resolved once per run, never per crate.
pub(crate) fn resolve_registry_owner(
    ctx: &Context,
    crates: &[anodizer_core::config::CrateConfig],
) -> Option<String> {
    let from_config = crates
        .iter()
        .filter_map(|c| c.release.as_ref())
        .filter_map(|r| r.github.as_ref())
        .map(|g| g.owner.clone())
        .find(|o| !o.is_empty());
    if from_config.is_some() {
        return from_config;
    }
    // Also consult the top-level `release.github` block (single-crate configs
    // declare the SCM repo there rather than per crate).
    if let Some(owner) = ctx
        .config
        .release
        .as_ref()
        .and_then(|r| r.github.as_ref())
        .map(|g| g.owner.clone())
        .filter(|o| !o.is_empty())
    {
        return Some(owner);
    }
    anodizer_core::git::detect_github_repo()
        .ok()
        .map(|(owner, _name)| owner)
}

/// Fire per-config post-hooks once per docker_v2 config, after all
/// snapshot-platform jobs for that config have completed. Matches GR's
/// `buildImage` lifecycle (pre -> build -> post).
fn run_docker_post_hooks(
    ctx: &Context,
    log: &anodizer_core::log::StageLogger,
    config_post_hooks: &[PerConfigPostHook],
    config_first_digest: &std::collections::BTreeMap<usize, String>,
) -> Result<()> {
    for cph in config_post_hooks {
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
        run_hooks(&cph.hooks, &post_label, false, log, Some(&hook_vars))?;
    }
    Ok(())
}

/// Insert the `Platforms` artifact-metadata entry on a `DockerImageV2`
/// artifact's metadata map. `Platforms` is the GR-aligned key (capital P,
/// JSON-array string) exposed as `extra.Platforms` so custom publishers can
/// route on the resolved platform list. Mirrors `ExtraPlatforms = "Platforms"`
/// in `internal/pipe/docker/v2/docker.go`. The serialization is infallible
/// for `Vec<String>` slices — `.expect` documents the invariant so a silent
/// fallback to `""` cannot mask a future refactor that broadens the input
/// type (the downstream `JSON_LIST_KEYS` parser would otherwise read the
/// empty string and skip the key without warning).
fn insert_platforms_meta(meta: &mut HashMap<String, String>, plats: &[String]) {
    meta.insert(
        "Platforms".to_string(),
        serde_json::to_string(plats).expect("serde_json::to_string on Vec<String> cannot fail"),
    );
}

/// Run `build_jobs` in parallel under a channel-based semaphore bounded by
/// `parallelism`, matching GoReleaser's `semerrgroup.New(ctx.Parallelism)`.
/// After all jobs return, registers `DockerImageV2` + `DockerDigest`
/// artifacts in `new_artifacts` and captures the first digest per docker_v2
/// config index into `config_first_digest` for the post-hook lifecycle.
fn execute_jobs_and_register(
    log: &anodizer_core::log::StageLogger,
    build_jobs: &[DockerBuildJob],
    parallelism: usize,
    new_artifacts: &mut Vec<Artifact>,
    config_first_digest: &mut std::collections::BTreeMap<usize, String>,
) -> Result<()> {
    use std::sync::mpsc;

    /// Drop guard that returns a semaphore token to the channel when
    /// dropped, ensuring the token is returned even if the thread panics.
    /// Without this, a panic would permanently consume a slot and
    /// eventually deadlock the remaining threads.
    struct SemaphoreGuard<'a> {
        sender: &'a mpsc::SyncSender<()>,
    }
    impl Drop for SemaphoreGuard<'_> {
        fn drop(&mut self) {
            // `send` cannot fail because thread::scope guarantees all guards
            // drop before sem_rx; spawning a detached thread here would
            // silently lose a token.
            let _ = self.sender.send(());
        }
    }

    // Channel-based semaphore: pre-fill with `parallelism` tokens. Each
    // thread takes a token before starting and returns it on completion.
    // This bounds active docker builds to `parallelism`.
    let (sem_tx, sem_rx) = mpsc::sync_channel::<()>(parallelism);
    for _ in 0..parallelism {
        let _ = sem_tx.send(());
    }

    let job_count = build_jobs.len();
    let results: Vec<Result<DockerBuildResult>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(job_count);

        for job in build_jobs {
            // Acquire a semaphore token (blocks if all slots are busy).
            let _ = sem_rx.recv();
            let sem_tx_ref = &sem_tx;

            let handle = scope.spawn(move || {
                // Guard returns the token on drop (including panic).
                let _guard = SemaphoreGuard { sender: sem_tx_ref };
                execute_docker_build(job, log)
            });
            handles.push(handle);
        }

        handles
            .into_iter()
            .map(|h| {
                anodizer_core::parallel::join_panic_to_err(h.join(), "docker build").and_then(|r| r)
            })
            .collect()
    });

    for (job, result) in build_jobs.iter().zip(results) {
        let build_result = result?;
        for tag in &job.rendered_tags {
            let mut meta = HashMap::new();
            meta.insert("tag".to_string(), tag.clone());
            insert_platforms_meta(&mut meta, &job.platforms_list);
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

        // Capture the first digest produced for this docker_v2 config so the
        // per-config post-hook (fired by the caller, after all jobs
        // complete) can render `{{ .Digest }}`. In snapshot multi-platform
        // mode anodize emits one job per platform — any platform's digest
        // is representative since GR's post-hook lifecycle has only one
        // digest variable per config.
        if !config_first_digest.contains_key(&job.idx)
            && let Some(d) = build_result.tag_digests.values().next()
        {
            config_first_digest.insert(job.idx, d.clone());
        }
    }

    Ok(())
}

/// Prepare a single docker_v2 config: render templates, stage artifacts,
/// fire the pre-hook, queue one or more build jobs (one per
/// snapshot-platform slice), and enqueue the post-hook record. Isolates
/// pre-hook failure so sibling configs continue (matches GR's
/// `semerrgroup` semantics).
#[allow(clippy::too_many_arguments)]
fn prepare_v2_config(
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
            "docker_v2[{}]: skipping config for crate {} (skip=true)",
            idx, krate.name
        ));
        return Ok(());
    }

    // Template-render platforms and filter empty results (GoReleaser's tpl.ApplySlice).
    let platforms: Vec<String> = v2_cfg
        .platforms
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| ctx.render_template(&p).ok().filter(|r| !r.is_empty()))
        .collect();

    // V2 always uses buildx.
    resolve_backend(Some("buildx"), platforms.len() > 1)?;

    // Template-render the Dockerfile path FIRST so an empty template
    // short-circuits before touching the filesystem (avoids orphan staging
    // dirs / stale staged artifacts when
    // `dockerfile: "{{ if .IsSnapshot }}Dockerfile{{ end }}"` renders to ""
    // during release). GR parity (commit d788340): check the *rendered*
    // template for emptiness — not the raw template.
    let rendered_dockerfile = ctx.render_template(&v2_cfg.dockerfile).with_context(|| {
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
        return Ok(());
    }

    // "docker_v2" subdirectory avoids collisions with legacy docker configs.
    let staging_dir: PathBuf = dist
        .join("docker_v2")
        .join(&krate.name)
        .join(idx.to_string());

    if !dry_run {
        fs::create_dir_all(&staging_dir)
            .with_context(|| format!("docker_v2: create staging dir {}", staging_dir.display()))?;
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
        "docker_v2",
    )?;

    if let Some(ref extra_files) = v2_cfg.extra_files {
        warn_project_markers_in_extra_files(extra_files, log, "docker_v2");
        stage_extra_files(extra_files, &staging_dir, dry_run, log, "docker_v2")?;
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

    // For snapshot builds, GoReleaser splits multi-platform configs into
    // per-platform builds with --load (no push) and tag suffix, so images
    // are available locally.
    let snapshot_platforms: Vec<Vec<String>> = if ctx.is_snapshot() && platforms.len() > 1 {
        platforms.iter().map(|p| vec![p.clone()]).collect()
    } else {
        vec![platforms.clone()]
    };

    // Pre-build hooks fire ONCE per docker_v2 config, matching GR's
    // `buildImage` lifecycle. `Images` is the full cross-product of
    // `rendered_images × rendered_tags` (no per-platform arch suffix —
    // that's a snapshot-only tag-disambiguation step that runs after the
    // hook). Exposed as a real Tera list so `{% for img in Images %}`
    // works, mirroring GR's `tmpl.Fields{ keyImages: da.images }` where
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
        if let Err(e) = run_hooks(&pre_hooks, &pre_label, dry_run, log, Some(&hook_vars)) {
            log.warn(&format!(
                "{}: pre-hook failed; skipping this config's build (other configs continue): {:#}",
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
            "post-docker_v2[{}]",
            v2_cfg.id.as_deref().unwrap_or(&idx.to_string())
        );
        run_hooks(&post_hooks, &post_label, dry_run, log, Some(&hook_vars))?;
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
    // "undefined" from "defined-empty"; mirrors GR's overlay-drop semantic
    // from `tpl.WithExtraFields` in `v2/docker.go:319`.
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
            "docker_v2[{}]: no image tags produced for crate {} (images or tags resolved to empty); skipping",
            idx, krate.name
        ));
        return Ok(());
    }

    let rendered_build_args = render_v2_kv_map(ctx, v2_cfg.build_args.as_ref(), "build_arg")?;
    let rendered_annotations = render_v2_kv_map(ctx, v2_cfg.annotations.as_ref(), "annotation")?;
    let rendered_labels = render_v2_kv_map(ctx, v2_cfg.labels.as_ref(), "label")?;
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
                "docker_v2[{}]: invalid `use: {}` for crate {} — expected `buildx` or `podman`",
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
                "docker_v2[{}]: `use: podman` for crate {} is not supported on this OS",
                idx, krate.name
            )
        })?;
        crate::command::validate_podman_flag_compat(&rendered_flags).with_context(|| {
            format!(
                "docker_v2[{}]: incompatible flag with `use: podman` for crate {}",
                idx, krate.name
            )
        })?;
    }

    // Evaluate sbom — GoReleaser only adds SBOM in the Publish path (not snapshot).
    // SBOM is a buildx-only attestation; under `use: podman` it must be off.
    let sbom_enabled = if ctx.is_snapshot() {
        false
    } else {
        is_docker_v2_sbom_enabled(&v2_cfg.sbom, ctx)?
    };
    if is_podman && sbom_enabled {
        anyhow::bail!(
            "docker_v2[{}]: `use: podman` for crate {} cannot enable `sbom: true` \
             (buildx-only attestation); set `sbom: false` or switch to `use: buildx`",
            idx,
            krate.name
        );
    }

    let platform_refs: Vec<&str> = snapshot_plats.iter().map(|s| s.as_str()).collect();

    // Snapshot builds never push (GoReleaser uses --load per-platform). The
    // canonical `skip:` field suppresses publish via `is_active`-style gating
    // earlier in the pipeline.
    let should_push = if ctx.is_snapshot() { false } else { !dry_run };

    // Determine whether --load is safe (requires a running daemon). In
    // snapshot mode, warn if daemon is unavailable and skip --load.
    // `--load` is buildx-only — podman builds load into local storage by
    // default, so the flag is suppressed below in the spec.
    let should_load = if ctx.is_snapshot() {
        let daemon_ok = is_podman || is_docker_daemon_available();
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
                "docker_v2: invalid retry config for crate {} index {}",
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
                "(dry-run) retry: up to {} attempts, base delay {:?}{}",
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
            insert_platforms_meta(&mut meta, snapshot_plats);
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

/// Validate Docker V2 config ID uniqueness. Matches GoReleaser
/// `internal/ids/ids.go:26-36` — duplicate IDs are a hard error in
/// `v2/docker.go:93`, because downstream filters rely on IDs to disambiguate
/// artifacts.
fn validate_docker_v2_id_uniqueness(crates: &[anodizer_core::config::CrateConfig]) -> Result<()> {
    let mut v2_ids: HashSet<String> = HashSet::new();
    for krate in crates {
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
    Ok(())
}

/// Validate the buildx plugin once if any V2 configs exist (V2 always uses
/// buildx). `check_buildx_version` confirms the plugin is reachable (mirrors
/// GoReleaser commit e09e23a / #6526), and `check_buildx_driver` validates
/// the active driver supports multi-platform builds. Both are warn-only:
/// downstream `buildx build` surfaces a hard error if it cannot actually run.
fn run_buildx_probes(stage: &super::DockerStage, log: &anodizer_core::log::StageLogger) {
    match &stage.probe {
        Some(custom) => run_buildx_version_check(log, || custom()),
        None => check_buildx_version(log),
    }
    check_buildx_driver(log);
}

/// Render a `key: value` template map (build_args / annotations / labels)
/// through the engine. Empty rendered keys or values are dropped. Output is
/// sorted by rendered key for deterministic emission. Matches GoReleaser's
/// `tplMapFlags`.
fn render_v2_kv_map(
    ctx: &mut Context,
    map: Option<&HashMap<String, String>>,
    label: &str,
) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(entries) = map {
        for (key_tmpl, value_tmpl) in entries {
            let rendered_key = ctx
                .render_template(key_tmpl)
                .with_context(|| format!("docker_v2: render {} key '{}'", label, key_tmpl))?;
            let rendered_value = ctx
                .render_template(value_tmpl)
                .with_context(|| format!("docker_v2: render {} value for '{}'", label, key_tmpl))?;
            if !rendered_key.is_empty() && !rendered_value.is_empty() {
                out.push((rendered_key, rendered_value));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
    }
    Ok(out)
}

/// Render a list of template strings, dropping any that render empty.
fn render_v2_flag_list(ctx: &mut Context, flags: Option<&Vec<String>>) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    if let Some(flag_list) = flags {
        for flag_tmpl in flag_list {
            let rendered = ctx
                .render_template(flag_tmpl)
                .with_context(|| format!("docker_v2: render flag '{}'", flag_tmpl))?;
            if !rendered.is_empty() {
                out.push(rendered);
            }
        }
    }
    Ok(out)
}

/// Process one `docker_manifests[N]` entry: render templates, build/push the
/// manifest (with retry), and register a `DockerManifest` artifact in
/// `new_artifacts`. Mirrors GoReleaser's docker-manifest pipe.
#[allow(clippy::too_many_arguments)]
fn process_docker_manifest(
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
            "docker: skipping manifest '{}' — already pushed as multi-arch by docker_v2",
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
                "docker: manifest image_template '{}' rendered to empty string, skipping",
                tmpl
            ));
            continue;
        }
        rendered_images.push(img);
    }

    // Determine the binary for manifest commands (see `resolve_manifester`
    // for the validation rationale).
    let manifest_bin = resolve_manifester(manifest_cfg.use_backend.as_deref())?;

    let rendered_create_flags: Vec<String> = manifest_cfg
        .create_flags
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|f| ctx.render_template(f).unwrap_or_else(|_| f.clone()))
        .collect();
    let rendered_push_flags: Vec<String> = manifest_cfg
        .push_flags
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|f| ctx.render_template(f).unwrap_or_else(|_| f.clone()))
        .collect();

    let create_cmd = build_manifest_create_cmd(
        log,
        manifest_bin,
        &manifest_name,
        &rendered_images,
        &rendered_create_flags,
        new_artifacts,
    );

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
        // Remove any existing manifest before recreating. Matches GoReleaser
        // `internal/pipe/docker/api_docker.go:26`:
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
            )?;
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
fn build_manifest_create_cmd(
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
            log.verbose(&format!("manifest: pinning {} to digest {}", img, digest));
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
) -> Result<()> {
    use anodizer_core::retry::{RetryPolicy, retry_sync};
    use std::ops::ControlFlow;
    let policy = RetryPolicy {
        max_attempts,
        base_delay,
        max_delay: max_delay.unwrap_or(Duration::MAX),
    };
    retry_sync(&policy, |attempt| {
        if attempt > 1 {
            log.warn(&format!(
                "manifest create attempt {}/{} failed, retrying…",
                attempt - 1,
                max_attempts,
            ));
        }
        log.status(&format!("running: {}", create_cmd.join(" ")));
        let mut create_command = Command::new(&create_cmd[0]);
        create_command.args(&create_cmd[1..]);
        for (key, value) in manifest_env_vars {
            create_command.env(key, value);
        }
        let output = match create_command.output() {
            Ok(o) => o,
            Err(e) => {
                return Err(ControlFlow::Break(anyhow::Error::from(e).context(format!(
                    "docker: manifest create for crate {} manifest {} (attempt {}/{})",
                    crate_name, midx, attempt, max_attempts
                ))));
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
    })
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
) -> Result<Option<String>> {
    use anodizer_core::retry::{RetryPolicy, retry_sync};
    use std::ops::ControlFlow;
    let policy = RetryPolicy {
        max_attempts,
        base_delay,
        max_delay: max_delay.unwrap_or(Duration::MAX),
    };
    let mut manifest_digest: Option<String> = None;
    retry_sync(&policy, |attempt| {
        if attempt > 1 {
            log.warn(&format!(
                "manifest push attempt {}/{} failed, retrying…",
                attempt - 1,
                max_attempts,
            ));
        }
        log.status(&format!("running: {}", push_cmd.join(" ")));
        let mut push_command = Command::new(&push_cmd[0]);
        push_command.args(&push_cmd[1..]);
        for (key, value) in manifest_env_vars {
            push_command.env(key, value);
        }
        let output = match push_command.output() {
            Ok(o) => o,
            Err(e) => {
                return Err(ControlFlow::Break(anyhow::Error::from(e).context(format!(
                    "docker: manifest push for crate {} manifest {} (attempt {}/{})",
                    crate_name, midx, attempt, max_attempts
                ))));
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
    })?;
    Ok(manifest_digest)
}

/// Write the combined GoReleaser `DockerDigest` format file. Each line is
/// `<hex_digest>  <image_name>`, sorted, with `sha256:` stripped from the
/// digest. The filename is resolved from the first non-empty
/// `docker_digest.name_template` across configured crates, falling back to
/// `digests.txt`.
fn write_combined_digest_file(
    ctx: &mut Context,
    log: &anodizer_core::log::StageLogger,
    dist: &std::path::Path,
    new_artifacts: &[Artifact],
) -> Result<()> {
    let mut digest_lines: Vec<String> = Vec::new();
    for artifact in new_artifacts {
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
    if digest_lines.is_empty() {
        return Ok(());
    }

    digest_lines.sort();
    digest_lines.dedup();
    let mut rendered_name: Option<String> = None;
    let crates_iter: Vec<_> = ctx.config.crates.clone();
    for krate in &crates_iter {
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
    Ok(())
}
