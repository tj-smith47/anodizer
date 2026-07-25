use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::HookEntry;
use anodizer_core::context::Context;
use anodizer_core::hooks::{HookRunContext, run_hooks};
use anodizer_core::run::run_capture_timeout;
use anodizer_core::stage::Stage;

/// Wall-clock bound on `docker manifest push` — the registry upload of the
/// assembled multi-arch manifest list. A wedged registry connection would
/// otherwise hang the release forever; on expiry the push subtree is killed and
/// the attempt retries within budget. Sized like a large remote upload.
const DOCKER_MANIFEST_PUSH_TIMEOUT: Duration = Duration::from_secs(600);

/// Wall-clock bound on `docker manifest create`. It assembles the manifest list
/// and resolves each member's digest against the registry, so it is a remote
/// metadata operation rather than a bulk upload — bounded shorter than the push.
const DOCKER_MANIFEST_CREATE_TIMEOUT: Duration = Duration::from_secs(300);

/// Per-docker_v2-config post-hook state captured during config preparation
/// and consumed after all parallel builds for that config have completed.
/// Fires exactly once per config (not per snapshot platform job) —
/// the build-image lifecycle.
pub(crate) struct PerConfigPostHook {
    idx: usize,
    id: Option<String>,
    hooks: Vec<HookEntry>,
    images_json: serde_json::Value,
    dockerfile_path: String,
    staging_dir: PathBuf,
    base_image_name: String,
    base_image_digest: String,
}

use super::DockerStage;
use super::baseimage::get_base_image;
use super::build::{DockerBuildJob, DockerBuildResult, execute_docker_build};
use super::command::{
    apply_docker_v2_defaults, build_docker_v2_command, generate_v2_image_tags,
    is_docker_v2_sbom_enabled, is_docker_v2_skipped, resolve_backend, resolve_digest_config,
    resolve_manifester, resolve_skip_push,
};
use super::detect;
use super::detect::{
    check_buildx_driver, check_buildx_version, is_docker_daemon_available, run_buildx_version_check,
};
use super::platform::tag_suffix;
use super::retry::resolve_retry_params;
use super::spelling::{find_image_digest, levenshtein_distance};
use super::staging::{
    copy_dockerfile, stage_artifacts_v2, stage_extra_files, warn_project_markers_in_extra_files,
};

mod build;
mod labels;
mod manifest;

pub(crate) use build::*;
pub use labels::*;
pub(crate) use manifest::*;

impl Stage for super::DockerStage {
    fn name(&self) -> &str {
        "docker"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("docker");

        // Operator-selection gate. DockerStage builds and PUSHES images to a
        // registry (an external, irreversible publish) but runs as a pipeline
        // stage OUTSIDE the trait-based dispatch chokepoint, so the uniform
        // `--skip` / `--publishers` filter does not reach it. Consult
        // `publisher_deselected("docker")` here so an operator who ran
        // `--publishers cargo` (or `--skip=docker`) does NOT push images.
        // Docker is not a `publish_report` participant — like its other skip
        // paths it records no report row — but the skip is never silent.
        if ctx.publisher_deselected("docker") {
            log.status(&ctx.deselected_reason("docker"));
            return Ok(());
        }

        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();
        let parallelism = ctx.options.parallelism.max(1);

        let crates = collect_docker_crates(ctx, &selected);

        if crates.is_empty() {
            return Ok(());
        }

        validate_docker_v2_id_uniqueness(&crates)?;

        // Attribute this stage's build/push and manifest backoff to the docker
        // scope. Without an active scope the retry summary files a flaky
        // registry's wait under "(unattributed)", so the operator sees the
        // total but cannot tell which remote burned it. The parallel build
        // workers spawned below all read the same constant scope value for the
        // stage's duration, so no task-local is needed.
        let _retry_scope = anodizer_core::retry::RetryScope::enter(self.name());

        if !dry_run && crates.iter().any(|c| c.dockers_v2.is_some()) {
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
        // parallel-per-config error semantic: a failed config
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
            let docker_v2_configs = match krate.dockers_v2.as_ref() {
                Some(cfgs) => cfgs.clone(),
                None => Vec::new(),
            };

            // Apply defaults to V2 configs. The per-crate
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
        // builds — the wait returns
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
    anodizer_core::git::resolve_github_slug(None, None)
        .ok()
        .map(|slug| slug.owner().to_string())
}

/// Fire per-config post-hooks once per docker_v2 config, after all
/// snapshot-platform jobs for that config have completed.
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
                "dockers_v2[{}]: post-hooks configured but no image digest captured \
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
            "post-dockers_v2[{}]",
            cph.id.as_deref().unwrap_or(&cph.idx.to_string())
        );
        run_hooks(
            &cph.hooks,
            &post_label,
            HookRunContext::new(false, log, Some(&hook_vars)),
        )?;
    }
    Ok(())
}

/// Insert the `Platforms` artifact-metadata entry on a `DockerImageV2`
/// artifact's metadata map. `Platforms` is the key (capital P,
/// JSON-array string) exposed as `extra.Platforms` so custom publishers can
/// route on the resolved platform list. The serialization is infallible
/// for `Vec<String>` slices — `.expect` documents the invariant so a silent
/// fallback to `""` cannot mask a future refactor that broadens the input
/// type (the downstream `JSON_LIST_KEYS` parser would otherwise read the
/// empty string and skip the key without warning).
pub(crate) fn insert_platforms_meta(
    meta: &mut HashMap<String, String>,
    plats: &[String],
) -> Result<()> {
    let encoded =
        serde_json::to_string(plats).context("docker: serialize Platforms metadata to JSON")?;
    meta.insert("Platforms".to_string(), encoded);
    Ok(())
}

/// Run `build_jobs` in parallel under a channel-based semaphore bounded by
/// `parallelism`.
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
            insert_platforms_meta(&mut meta, &job.platforms_list)?;
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
        // is representative since the post-hook lifecycle has only one
        // digest variable per config.
        if !config_first_digest.contains_key(&job.idx)
            && let Some(d) = build_result.tag_digests.values().next()
        {
            config_first_digest.insert(job.idx, d.clone());
        }
    }

    Ok(())
}

/// Write the combined `DockerDigest` format file. Each line is
/// `<hex_digest>  <image_name>`, sorted, with `sha256:` stripped from the
/// digest. The filename is resolved from the first non-empty
/// Collect crates from the universe (top-level `crates` plus every
/// `workspaces[].crates` entry) that declare docker output (`dockers_v2`
/// or `docker_manifests`) and pass the `--crate` selection.
pub(crate) fn collect_docker_crates(
    ctx: &Context,
    selected: &[String],
) -> Vec<anodizer_core::config::CrateConfig> {
    ctx.config
        .crate_universe()
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| c.dockers_v2.is_some() || c.docker_manifests.is_some())
        .cloned()
        .collect()
}

/// `docker_digest.name_template` across configured crates, falling back to
/// `digests.txt`.
pub(crate) fn write_combined_digest_file(
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
    // Cloned out of the universe: the render loop below needs `ctx` mutably.
    let crates_iter: Vec<anodizer_core::config::CrateConfig> =
        ctx.config.crate_universe().into_iter().cloned().collect();
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
            "wrote combined digest file {}",
            digest_file.display()
        ));
    }
    Ok(())
}
