//! Helpers extracted from `run.rs` to reduce that file's god-function size
//! while keeping behavior identical.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{CrateConfig, HookEntry};
use anodizer_core::context::Context;
use anodizer_core::crate_scope::{
    apply_var_overrides, crate_template_overrides, resolve_crate_tag, restore_var_overrides,
};
use anodizer_core::hooks::{HookRunContext, run_hooks};
use anodizer_core::log::StageLogger;
use anodizer_core::template::TemplateVars;

use crate::binstall;
use crate::command::BuildCommand;
use crate::universal::{build_universal_binary, project_universal_out_path};
use crate::version_sync;
use crate::workspace::{
    find_workspace_root, resolve_binary_path, resolve_copy_from,
    resolve_reproducible_epoch_with_env,
};
use anodizer_core::elf::is_dynamically_linked;

pub(crate) struct BuildJob {
    pub cmd: Option<BuildCommand>,
    pub copy_from: Option<(PathBuf, PathBuf)>,
    pub bin_path: PathBuf,
    pub artifact_kind: ArtifactKind,
    pub target: String,
    pub crate_name: String,
    pub binary_name: String,
    pub build_id: Option<String>,
    pub reproducible: bool,
    pub pre_hooks: Vec<HookEntry>,
    pub post_hooks: Vec<HookEntry>,
    pub no_unique_dist_dir: bool,
    pub crate_path: String,
    pub mod_timestamp: Option<String>,
    pub amd64_variant: Option<String>,
    /// Fully-rendered per-(crate, target) `builds[].env` map for THIS job.
    /// Layered into this job's build hooks beneath each hook's own `env:`,
    /// the build-hook env precedence. Empty when the build
    /// declares no `env:` for the active target.
    pub build_env: HashMap<String, String>,
}

pub(crate) struct BuildResult {
    pub bin_path: PathBuf,
    pub artifact_kind: ArtifactKind,
    pub target: String,
    pub crate_name: String,
    pub binary_name: String,
    pub build_id: Option<String>,
    pub no_unique_dist_dir: bool,
    pub amd64_variant: Option<String>,
}

pub(crate) fn artifact_meta(
    binary: &str,
    build_id: &Option<String>,
    amd64_variant: &Option<String>,
) -> HashMap<String, String> {
    let id = build_id.clone().unwrap_or_else(|| binary.to_string());
    let mut m = HashMap::from([
        ("binary".to_string(), binary.to_string()),
        ("id".to_string(), id),
    ]);
    if let Some(v) = amd64_variant {
        m.insert("amd64_variant".to_string(), v.clone());
    }
    m
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn add_artifact(
    ctx: &mut Context,
    dist_dir: &Path,
    dry_run: bool,
    job_bin_path: &Path,
    artifact_kind: ArtifactKind,
    target: &str,
    crate_name: &str,
    binary_name: &str,
    build_id: &Option<String>,
    no_unique_dist_dir: bool,
    amd64_variant: &Option<String>,
) -> Result<()> {
    ctx.template_vars_mut().set("Binary", binary_name);
    let mut meta = artifact_meta(binary_name, build_id, amd64_variant);
    let artifact_path = if no_unique_dist_dir {
        meta.insert("no_unique_dist_dir".to_string(), "true".to_string());
        let file_name = job_bin_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| binary_name.to_string());
        let flat_path = dist_dir.join(&file_name);
        if !dry_run {
            if let Some(parent) = flat_path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create dist dir: {}", parent.display()))?;
            }
            if job_bin_path.exists() {
                std::fs::copy(job_bin_path, &flat_path).with_context(|| {
                    format!(
                        "no_unique_dist_dir: failed to copy {} → {}",
                        job_bin_path.display(),
                        flat_path.display()
                    )
                })?;
            }
        }
        flat_path
    } else {
        job_bin_path.to_path_buf()
    };
    if artifact_path.exists()
        && is_dynamically_linked(&artifact_path)
            .with_context(|| format!("inspect ELF linkage of {}", artifact_path.display()))?
    {
        meta.insert("DynamicallyLinked".to_string(), "true".to_string());
    }
    let artifact_name = artifact_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| artifact_path.display().to_string());
    ctx.artifacts.add(Artifact {
        kind: artifact_kind,
        name: artifact_name,
        path: artifact_path,
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
    Ok(())
}

pub(crate) fn apply_source_mutations(
    ctx: &mut Context,
    crates: &[CrateConfig],
    default_targets: &[String],
    dry_run: bool,
    log: &StageLogger,
) -> Result<()> {
    apply_source_mutations_with_resolver(
        ctx,
        crates,
        default_targets,
        dry_run,
        log,
        &resolve_crate_tag,
    )
}

/// Inner body of [`apply_source_mutations`] with the per-crate tag source
/// injected. Production passes [`resolve_crate_tag`] (git-backed); tests pass a
/// closure returning fixed tags so the per-crate scoping can be exercised
/// without a git fixture.
fn apply_source_mutations_with_resolver(
    ctx: &mut Context,
    crates: &[CrateConfig],
    default_targets: &[String],
    dry_run: bool,
    log: &StageLogger,
    resolve_tag: &dyn Fn(&Context, &CrateConfig) -> Option<String>,
) -> Result<()> {
    // Snapshot AND nightly versions are synthesized (never tag-derived), so
    // stamping them into Cargo.toml / binstall metadata would write a version
    // no registry release ever carries — both modes skip source mutations, and
    // a tagless repo (the state a rollback/re-cut leaves behind) must not trip
    // the tag guard below in either mode.
    let skip_mutations = ctx.is_snapshot() || ctx.is_nightly();
    let mode = if ctx.is_snapshot() {
        "snapshot"
    } else {
        "nightly"
    };
    for crate_cfg in crates {
        let needs_mutation = crate_cfg
            .version_sync
            .as_ref()
            .is_some_and(|vs| vs.enabled.unwrap_or(false))
            || crate_cfg
                .binstall
                .as_ref()
                .is_some_and(|bs| bs.enabled.unwrap_or(false));
        if !needs_mutation {
            continue;
        }

        // Re-scope the version/name vars to THIS crate before rendering its
        // source mutations, then restore so one crate's vars never leak into
        // the next. Snapshot and nightly modes skip mutation entirely, so no
        // git lookup (and no re-scope) happens there.
        //
        // A mutation-enabled crate with no resolvable per-crate tag/version is
        // a fail-loud error, never a silent fall-back to the first crate's
        // vars: stamping crate B with crate A's version (or rendering B's
        // binstall URL with A's version) ships a wrong, hard-to-spot artifact.
        let saved: Vec<(&'static str, Option<String>)> = if skip_mutations {
            Vec::new()
        } else {
            let tag = resolve_tag(ctx, crate_cfg).with_context(|| {
                anodizer_core::crate_scope::no_matching_tag_error(
                    ctx,
                    crate_cfg,
                    "version-sync/binstall",
                )
            })?;
            let overrides = crate_template_overrides(&crate_cfg.name, &tag)?;
            apply_var_overrides(ctx, &overrides)
        };

        let result = (|| -> Result<()> {
            if let Some(ref vs) = crate_cfg.version_sync
                && vs.enabled.unwrap_or(false)
            {
                if skip_mutations {
                    log.verbose(&format!(
                        "skipped version sync for {} — {mode} mode does not mutate source files",
                        crate_cfg.path
                    ));
                } else {
                    let version = ctx
                        .template_vars()
                        .get("RawVersion")
                        .or_else(|| ctx.template_vars().get("Version"))
                        .cloned()
                        .unwrap_or_default();
                    if !version.is_empty() {
                        version_sync::sync_version(&crate_cfg.path, &version, dry_run, log)?;
                    }
                }
            }
            if let Some(ref bs) = crate_cfg.binstall
                && bs.enabled.unwrap_or(false)
            {
                if skip_mutations {
                    log.verbose(&format!(
                        "skipped binstall metadata for {} — {mode} mode does not mutate source files",
                        crate_cfg.path
                    ));
                } else {
                    binstall::generate_binstall_metadata(
                        crate_cfg,
                        bs,
                        default_targets,
                        ctx,
                        dry_run,
                    )?;
                }
            }
            Ok(())
        })();

        // Restore even on error so a failed crate doesn't poison the next.
        restore_var_overrides(ctx, saved);
        result?;
    }
    Ok(())
}

pub(crate) fn seed_determinism_state(
    ctx: &mut Context,
    commit_timestamp: &str,
    log: &StageLogger,
) -> Result<()> {
    if ctx.determinism.is_none() {
        if ctx.options.snapshot {
            let repo = ctx
                .options
                .project_root
                .clone()
                .unwrap_or_else(|| PathBuf::from("."));
            match anodizer_core::git::resolve_snapshot_sde(&repo) {
                Ok(epoch) => {
                    ctx.determinism =
                        Some(anodizer_core::DeterminismState::seed_from_commit(epoch)?);
                }
                Err(err) => {
                    log.status(&format!(
                        "snapshot SDE resolution failed; falling back to commit timestamp: {}",
                        err
                    ));
                    if let Some(epoch) =
                        resolve_reproducible_epoch_with_env(commit_timestamp, ctx.env_source())
                    {
                        ctx.determinism =
                            Some(anodizer_core::DeterminismState::seed_from_commit(epoch)?);
                    }
                }
            }
        } else if let Some(epoch) =
            resolve_reproducible_epoch_with_env(commit_timestamp, ctx.env_source())
        {
            ctx.determinism = Some(anodizer_core::DeterminismState::seed_from_commit(epoch)?);
        }
    }
    if let Some(state) = ctx.determinism.as_mut() {
        for (name, reason) in &ctx.options.runtime_nondeterministic_allowlist {
            state.append_runtime(name.clone(), reason.clone());
        }
    }
    Ok(())
}

/// Shared per-stage borrows threaded into each execution path.
///
/// Holds only borrows that do not collide with `&mut ctx`; `env_source`
/// is re-borrowed via `ctx.env_source()` inside `run_parallel`'s chunk
/// loop so the helper can keep mutable access to `ctx` between chunks.
pub(crate) struct BuildExec<'a> {
    pub log: &'a StageLogger,
    pub template_vars: &'a TemplateVars,
    pub dist_dir: &'a Path,
    pub dry_run: bool,
    pub commit_timestamp: &'a str,
}

/// Dry-run path: log what each job would do and register artifacts without
/// spawning any compiler invocations.
pub(crate) fn run_dry_run(
    ctx: &mut Context,
    exec: &BuildExec<'_>,
    build_jobs: &[BuildJob],
    copy_jobs: &[BuildJob],
) -> Result<()> {
    for job in build_jobs.iter().chain(copy_jobs.iter()) {
        if !job.pre_hooks.is_empty() {
            run_hooks(
                &job.pre_hooks,
                "pre-build",
                HookRunContext {
                    dry_run: true,
                    log: exec.log,
                    template_vars: Some(exec.template_vars),
                    build_env: Some(&job.build_env),
                    extra_env: None,
                },
            )?;
        }
        if let Some(ref cmd) = job.cmd {
            exec.log
                .status(&format!("(dry-run) {} {}", cmd.program, cmd.args.join(" ")));
        } else if let Some((ref src, ref dst)) = job.copy_from {
            exec.log.status(&format!(
                "(dry-run) copy {} → {}",
                src.display(),
                dst.display()
            ));
        }
        if !job.post_hooks.is_empty() {
            run_hooks(
                &job.post_hooks,
                "post-build",
                HookRunContext {
                    dry_run: true,
                    log: exec.log,
                    template_vars: Some(exec.template_vars),
                    build_env: Some(&job.build_env),
                    extra_env: None,
                },
            )?;
        }
        add_artifact(
            ctx,
            exec.dist_dir,
            exec.dry_run,
            &job.bin_path,
            job.artifact_kind,
            &job.target,
            &job.crate_name,
            &job.binary_name,
            &job.build_id,
            job.no_unique_dist_dir,
            &job.amd64_variant,
        )?;
    }
    Ok(())
}

/// Resolve the profile directory (`target/<triple>/release/`) a binary lands
/// in, mirroring where the executor's [`resolve_binary_path`] finds the
/// produced binary — but EXISTENCE-INDEPENDENTLY.
///
/// `resolve_binary_path` gates the workspace-root rewrite on the binary
/// already existing, so it returns the planned RELATIVE path before the build
/// and the workspace-root ABSOLUTE path after. The reaper must key seed (pre-
/// build) and completion (post-build) on ONE stable value, so this resolver
/// drops that existence gate: if a workspace root is found from `crate_path`,
/// the key is always `<ws_root>/<bin_path>`'s parent (where cargo writes in a
/// workspace); otherwise the planned `bin_path` parent. Same answer at both
/// call sites regardless of build timing — without it, the prune silently
/// never fires for any real workspace build.
fn resolved_profile_dir(bin_path: &Path, crate_path: &str) -> Option<PathBuf> {
    let resolved = if bin_path.is_absolute() {
        bin_path.to_path_buf()
    } else if let Some(ws_root) = find_workspace_root(crate_path) {
        ws_root.join(bin_path)
    } else {
        bin_path.to_path_buf()
    };
    resolved.parent().map(Path::to_path_buf)
}

/// Tracks how many build jobs still target each profile directory
/// (`target/<triple>/release/`), so a triple's cargo intermediates are only
/// freed once its LAST job has produced its binary — never mid-build while a
/// sibling crate's job still reads the shared `deps/`.
///
/// Active ONLY inside the hermetic determinism-harness rebuild (gated on the
/// `ANODIZER_IN_DETERMINISM_HARNESS` marker, the same signal stage-sign and
/// `Context` read for hermetic behavior). The harness builds into a throwaway
/// `.det-tmp/target/`; a plain local `--snapshot` build uses the user's
/// persistent `target/` and MUST keep its cargo cache, so this is a no-op
/// there — `pending` is empty and [`note_build_complete`] never prunes.
struct IntermediateReaper {
    /// Remaining job count keyed by the resolved profile dir each job's binary
    /// lands in (via [`resolved_profile_dir`] over the job's PLANNED
    /// `bin_path` + `crate_path`). The key is computed from the SAME planned
    /// inputs at both seed and completion, so the workspace-root fallback can
    /// never produce a seed-vs-lookup mismatch. Decremented as each job
    /// completes; the triple is pruned when its count reaches zero.
    pending: HashMap<PathBuf, usize>,
}

impl IntermediateReaper {
    /// Build a reaper over `build_jobs`. When `enabled` is false (not the
    /// harness rebuild) the map is empty, making every method an inert no-op.
    fn new(enabled: bool, build_jobs: &[BuildJob]) -> Self {
        let mut pending: HashMap<PathBuf, usize> = HashMap::new();
        if enabled {
            for job in build_jobs {
                if let Some(profile_dir) = resolved_profile_dir(&job.bin_path, &job.crate_path) {
                    *pending.entry(profile_dir).or_insert(0) += 1;
                }
            }
        }
        Self { pending }
    }

    /// Record that `job` finished building and, if it was the last pending job
    /// for that profile dir, free the triple's cargo intermediates.
    ///
    /// The key is re-derived through [`resolved_profile_dir`] from the job's
    /// PLANNED `bin_path` + `crate_path` — byte-for-byte the same inputs
    /// [`IntermediateReaper::new`] seeded with — so the lookup always hits and
    /// the freed dir is exactly the resolved profile dir cargo wrote into.
    fn note_build_complete(&mut self, job: &BuildJob, log: &StageLogger) {
        let Some(profile_dir) = resolved_profile_dir(&job.bin_path, &job.crate_path) else {
            return;
        };
        let Some(remaining) = self.pending.get_mut(&profile_dir) else {
            return;
        };
        *remaining = remaining.saturating_sub(1);
        if *remaining == 0 {
            self.pending.remove(&profile_dir);
            let freed = anodizer_core::util::free_cargo_build_intermediates(&profile_dir, log);
            if !freed.is_empty() {
                log.verbose(&format!(
                    "freed build intermediates ({}) under {}",
                    freed.join(", "),
                    profile_dir.display()
                ));
            }
        }
    }
}

/// True when this build is the hermetic determinism-harness rebuild, in which
/// disk-bound context cargo intermediates are pruned as each triple finishes.
/// Reads the `ANODIZER_IN_DETERMINISM_HARNESS` marker the harness injects into
/// every child (see `determinism_harness/env.rs`), the same precedent
/// stage-sign and `Context` follow. A plain local `--snapshot` lacks the
/// marker and keeps its persistent cargo cache.
fn harness_intermediate_prune_enabled(ctx: &Context) -> bool {
    ctx.env_var("ANODIZER_IN_DETERMINISM_HARNESS").is_some()
}

/// Sequential path: compile each job in-process, register the produced
/// artifact, then drain `copy_jobs` after all source builds complete.
pub(crate) fn run_sequential(
    ctx: &mut Context,
    exec: &BuildExec<'_>,
    build_jobs: &[BuildJob],
    copy_jobs: &[BuildJob],
) -> Result<()> {
    let mut reaper = IntermediateReaper::new(harness_intermediate_prune_enabled(ctx), build_jobs);
    for job in build_jobs {
        if let Some(parent) = job.bin_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create bin output dir: {} (for pre-hook)",
                    parent.display()
                )
            })?;
        }
        if !job.pre_hooks.is_empty() {
            run_hooks(
                &job.pre_hooks,
                "pre-build",
                HookRunContext {
                    dry_run: false,
                    log: exec.log,
                    template_vars: Some(exec.template_vars),
                    build_env: Some(&job.build_env),
                    extra_env: None,
                },
            )?;
        }

        let cmd = job
            .cmd
            .as_ref()
            .context("build job has no cmd (programmer bug: planner should populate)")?;
        exec.log
            .verbose(&format!("running {} {}", cmd.program, cmd.args.join(" ")));
        let mut command = Command::new(&cmd.program);
        command.args(&cmd.args).envs(&cmd.env).current_dir(&cmd.cwd);
        // Target-qualify the label so the liveness heartbeat attributes a slow
        // build to its target (`still running cargo (aarch64-…)`) instead of a
        // bare `cargo` that is ambiguous once builds run concurrently.
        let label = format!("{} ({})", cmd.program, job.target);
        anodizer_core::run::run_checked(&mut command, exec.log, &label)?;
        exec.log.status(&format!(
            "built {}/{} for {}",
            job.crate_name, job.binary_name, job.target
        ));

        let resolved_bin = resolve_binary_path(&job.bin_path, &job.crate_path);

        if !resolved_bin.exists() {
            anyhow::bail!(
                "build succeeded but binary not found at {} (also checked workspace root): \
                 check that the binary name matches your Cargo.toml [bin] section",
                job.bin_path.display()
            );
        }

        if job.reproducible && resolved_bin.exists() {
            if let Some(epoch) =
                resolve_reproducible_epoch_with_env(exec.commit_timestamp, ctx.env_source())
            {
                anodizer_core::util::set_file_mtime_epoch(&resolved_bin, epoch)?;
            } else {
                exec.log.warn(
                    "reproducible build requested but could not determine epoch \
                     from SOURCE_DATE_EPOCH or CommitTimestamp; mtime will not be set",
                );
            }
        }

        if let Some(ref ts) = job.mod_timestamp
            && resolved_bin.exists()
        {
            let rendered_ts = ctx
                .render_template(ts)
                .with_context(|| format!("build: render mod_timestamp template '{ts}'"))?;
            let mtime = anodizer_core::util::parse_mod_timestamp(&rendered_ts)?;
            anodizer_core::util::set_file_mtime(&resolved_bin, mtime)?;
            exec.log.verbose(&format!(
                "applied mod_timestamp={rendered_ts} to {}",
                resolved_bin.display()
            ));
        }

        if !job.post_hooks.is_empty() {
            run_hooks(
                &job.post_hooks,
                "post-build",
                HookRunContext {
                    dry_run: false,
                    log: exec.log,
                    template_vars: Some(exec.template_vars),
                    build_env: Some(&job.build_env),
                    extra_env: None,
                },
            )?;
        }

        add_artifact(
            ctx,
            exec.dist_dir,
            exec.dry_run,
            &resolved_bin,
            job.artifact_kind,
            &job.target,
            &job.crate_name,
            &job.binary_name,
            &job.build_id,
            job.no_unique_dist_dir,
            &job.amd64_variant,
        )?;

        // Clean-as-you-go: once this triple's last job has produced its
        // binary, its cargo scratch is dead weight. Freeing it here — before
        // the next triple builds — keeps the disk-bound macOS harness shard
        // (two darwin target trees in one `.det-tmp/target/`) under the runner
        // disk ceiling. No-op outside the harness rebuild.
        reaper.note_build_complete(job, exec.log);
    }

    run_copy_jobs(ctx, exec, copy_jobs)
}

/// Parallel path: fan jobs out across `parallelism`-sized chunks via
/// `thread::scope`. Each thread brackets its compile with the job's
/// pre/post hooks so per-job ordering matches the sequential path.
///
/// The `env_source` borrow is captured inside each chunk via
/// `ctx.env_source()` and held only for the `thread::scope` body, so
/// post-chunk artifact registration regains the `&mut ctx` borrow under
/// NLL.
pub(crate) fn run_parallel(
    ctx: &mut Context,
    exec: &BuildExec<'_>,
    build_jobs: &[BuildJob],
    copy_jobs: &[BuildJob],
    parallelism: usize,
) -> Result<()> {
    exec.log.status(&format!(
        "building {} jobs with parallelism={}",
        build_jobs.len(),
        parallelism
    ));

    let mut reaper = IntermediateReaper::new(harness_intermediate_prune_enabled(ctx), build_jobs);

    for chunk in build_jobs.chunks(parallelism) {
        let template_vars = exec.template_vars;
        let log = exec.log;
        let commit_timestamp = exec.commit_timestamp;
        let env_source: &dyn anodizer_core::EnvSource = ctx.env_source();
        let results: Vec<Result<BuildResult>> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|job| {
                    // `job.cmd` is populated for every build job (copy-from-only
                    // jobs take a separate code path). If it's absent here, that's a
                    // pipeline invariant violation — surface as an error, not a panic,
                    // so the worker thread unwinds through the Result channel instead
                    // of killing the process.
                    let cmd_opt = job.cmd.clone();
                    let crate_name_for_err = job.crate_name.clone();
                    let program = cmd_opt.as_ref().map(|c| c.program.clone());
                    let args = cmd_opt.as_ref().map(|c| c.args.clone());
                    let env = cmd_opt.as_ref().map(|c| c.env.clone());
                    let cwd = cmd_opt.as_ref().map(|c| c.cwd.clone());
                    let bin_path = job.bin_path.clone();
                    let artifact_kind = job.artifact_kind;
                    let target = job.target.clone();
                    let crate_name = job.crate_name.clone();
                    let binary_name = job.binary_name.clone();
                    let build_id = job.build_id.clone();
                    let reproducible = job.reproducible;
                    let no_unique_dist_dir = job.no_unique_dist_dir;
                    let job_crate_path = job.crate_path.clone();
                    let commit_ts = commit_timestamp.to_string();
                    let pre_hooks = job.pre_hooks.clone();
                    let post_hooks = job.post_hooks.clone();
                    let build_env = job.build_env.clone();
                    let job_mod_timestamp = job.mod_timestamp.clone();
                    let job_amd64_variant = job.amd64_variant.clone();
                    let thread_tvars = template_vars.clone();
                    let thread_log = log.clone();
                    let warn_log = log.clone();

                    s.spawn(move || -> Result<BuildResult> {
                        let program = program.ok_or_else(|| anyhow::anyhow!(
                            "build: planner invariant violation — job for crate {} reached execution without a cmd",
                            crate_name_for_err
                        ))?;
                        let args = args.unwrap_or_default();
                        let env = env.unwrap_or_default();
                        let cwd = cwd.unwrap_or_default();

                        if let Some(parent) = bin_path.parent() {
                            std::fs::create_dir_all(parent).with_context(|| {
                                format!(
                                    "failed to create bin output dir: {} (for pre-hook)",
                                    parent.display()
                                )
                            })?;
                        }
                        if !pre_hooks.is_empty() {
                            run_hooks(
                                &pre_hooks,
                                "pre-build",
                                HookRunContext {
                                    dry_run: false,
                                    log: &thread_log,
                                    template_vars: Some(&thread_tvars),
                                    build_env: Some(&build_env),
                                    extra_env: None,
                                },
                            )?;
                        }

                        thread_log.verbose(&format!("running {} {}", program, args.join(" ")));
                        let mut command = Command::new(&program);
                        command.args(&args).envs(&env).current_dir(&cwd);
                        // Target-qualify the label so concurrent build heartbeats
                        // are distinguishable (`still running cargo (aarch64-…)`)
                        // rather than an ambiguous bare `cargo` shared by all jobs.
                        let label = format!("{program} ({target})");
                        anodizer_core::run::run_checked(&mut command, &thread_log, &label)?;

                        let bin_path = resolve_binary_path(&bin_path, &job_crate_path);

                        if !bin_path.exists() {
                            anyhow::bail!(
                                "build succeeded but binary not found at {} (also checked workspace root): \
                                 check that the binary name matches your Cargo.toml [bin] section",
                                bin_path.display()
                            );
                        }

                        if reproducible && bin_path.exists() {
                            if let Some(epoch) =
                                resolve_reproducible_epoch_with_env(&commit_ts, env_source)
                            {
                                anodizer_core::util::set_file_mtime_epoch(&bin_path, epoch)?;
                            } else {
                                warn_log.warn(
                                    "reproducible build requested but could not determine epoch \
                                     from SOURCE_DATE_EPOCH or CommitTimestamp; mtime will not be set",
                                );
                            }
                        }

                        if let Some(ref ts) = job_mod_timestamp
                            && bin_path.exists()
                        {
                            // Thread context doesn't have ctx for template rendering,
                            // so render using Tera directly with thread-local vars.
                            let rendered_ts = anodizer_core::template::render(ts, &thread_tvars)
                                .with_context(|| format!("build: render mod_timestamp template '{ts}'"))?;
                            let mtime = anodizer_core::util::parse_mod_timestamp(&rendered_ts)?;
                            anodizer_core::util::set_file_mtime(&bin_path, mtime)?;
                            thread_log.verbose(&format!(
                                "applied mod_timestamp={rendered_ts} to {}",
                                bin_path.display()
                            ));
                        }

                        if !post_hooks.is_empty() {
                            run_hooks(
                                &post_hooks,
                                "post-build",
                                HookRunContext {
                                    dry_run: false,
                                    log: &thread_log,
                                    template_vars: Some(&thread_tvars),
                                    build_env: Some(&build_env),
                                    extra_env: None,
                                },
                            )?;
                        }

                        Ok(BuildResult {
                            bin_path,
                            artifact_kind,
                            target,
                            crate_name,
                            binary_name,
                            build_id,
                            no_unique_dist_dir,
                            amd64_variant: job_amd64_variant,
                        })
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|h| {
                    anodizer_core::parallel::join_panic_to_err(h.join(), "build").and_then(|r| r)
                })
                .collect()
        });

        // Zip each result back to its source job so the reaper re-derives its
        // key from the job's PLANNED inputs (identical to the seed), not the
        // post-build resolved path. `results` preserves `chunk` order (the
        // handles are spawned and joined in order).
        for (job, result) in chunk.iter().zip(results) {
            let r = result?;
            exec.log.status(&format!(
                "built {}/{} for {}",
                r.crate_name, r.binary_name, r.target
            ));
            add_artifact(
                ctx,
                exec.dist_dir,
                exec.dry_run,
                &r.bin_path,
                r.artifact_kind,
                &r.target,
                &r.crate_name,
                &r.binary_name,
                &r.build_id,
                r.no_unique_dist_dir,
                &r.amd64_variant,
            )?;

            // Free this triple's cargo scratch once its last job lands — see
            // the sequential path for the disk-ceiling rationale. No-op
            // outside the harness rebuild.
            reaper.note_build_complete(job, exec.log);
        }
    }

    run_copy_jobs(ctx, exec, copy_jobs)
}

/// Drain `copy_from` jobs after the source builds complete. Shared between
/// the sequential and parallel paths to keep their semantics identical.
fn run_copy_jobs(ctx: &mut Context, exec: &BuildExec<'_>, copy_jobs: &[BuildJob]) -> Result<()> {
    for job in copy_jobs {
        let (src, dst) = job
            .copy_from
            .as_ref()
            .context("copy_from job without copy_from pair (programmer bug)")?;
        resolve_copy_from(ctx, src, dst, &job.target, &job.crate_name)?;

        add_artifact(
            ctx,
            exec.dist_dir,
            exec.dry_run,
            &job.bin_path,
            job.artifact_kind,
            &job.target,
            &job.crate_name,
            &job.binary_name,
            &job.build_id,
            job.no_unique_dist_dir,
            &job.amd64_variant,
        )?;
    }
    Ok(())
}

pub(crate) fn process_universal_binaries(
    ctx: &mut Context,
    crates: &[CrateConfig],
    dry_run: bool,
) -> Result<()> {
    let mut seen_universal_outputs: HashSet<PathBuf> = HashSet::new();
    for crate_cfg in crates {
        if let Some(ref ub_configs) = crate_cfg.universal_binaries {
            for ub in ub_configs {
                let projected = project_universal_out_path(crate_cfg.name.as_str(), ub, ctx)?;
                if let Some(existing) = projected.as_ref()
                    && !seen_universal_outputs.insert(existing.clone())
                {
                    anyhow::bail!(
                        "build: two universal_binaries entries resolve to the same output path {:?}; set distinct `name_template` or `id` values to disambiguate",
                        existing
                    );
                }
                build_universal_binary(crate_cfg.name.as_str(), ub, ctx, dry_run)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod source_mutation_tests {
    use super::*;
    use anodizer_core::config::{BinstallConfig, Config, CrateConfig, VersionSyncConfig};
    use anodizer_core::context::{Context, ContextOptions};

    fn crate_cfg(name: &str, path: &str, tag_template: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: path.to_string(),
            tag_template: Some(tag_template.to_string()),
            version_sync: Some(VersionSyncConfig {
                enabled: Some(true),
                mode: None,
            }),
            binstall: Some(BinstallConfig {
                enabled: Some(true),
                pkg_url: Some(
                    "https://github.com/myorg/{{ .ProjectName }}/releases/download/v{{ .Version }}/{{ .ProjectName }}-{{ .Version }}-{ target }.tar.gz"
                        .to_string(),
                ),
                bin_dir: None,
                pkg_fmt: Some("tgz".to_string()),
                overrides: None,
            }),
            ..Default::default()
        }
    }

    fn seed_cargo_toml(dir: &std::path::Path, name: &str) {
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n"),
        )
        .unwrap();
    }

    #[test]
    fn crate_template_overrides_derives_per_crate_version_and_name() {
        let ov = crate_template_overrides("alpha", "alpha-v1.2.3").unwrap();
        let map: std::collections::HashMap<_, _> = ov.into_iter().collect();
        assert_eq!(map.get("RawVersion").map(String::as_str), Some("1.2.3"));
        assert_eq!(map.get("Version").map(String::as_str), Some("1.2.3"));
        assert_eq!(map.get("Tag").map(String::as_str), Some("alpha-v1.2.3"));
        assert_eq!(map.get("ProjectName").map(String::as_str), Some("alpha"));
        assert_eq!(map.get("Name").map(String::as_str), Some("alpha"));

        // Prerelease + build metadata mirror populate_git_vars.
        let ov = crate_template_overrides("beta", "beta-v2.0.0-rc.1+build.7").unwrap();
        let map: std::collections::HashMap<_, _> = ov.into_iter().collect();
        assert_eq!(map.get("RawVersion").map(String::as_str), Some("2.0.0"));
        assert_eq!(
            map.get("Version").map(String::as_str),
            Some("2.0.0-rc.1+build.7")
        );
    }

    #[test]
    fn crate_template_overrides_rejects_unparseable_tag() {
        let err = crate_template_overrides("gamma", "not-a-version").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("gamma") && msg.contains("not-a-version"),
            "error must name the crate and its bad tag, got: {msg}"
        );
    }

    /// Two crates with DIFFERENT versions and names in one invocation: each
    /// crate's Cargo.toml must receive ITS OWN version + binstall pkg-url, and
    /// crate B must NOT inherit crate A's version/name (the headline bleed bug).
    #[test]
    fn apply_source_mutations_scopes_version_and_binstall_per_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let alpha_dir = tmp.path().join("alpha");
        let beta_dir = tmp.path().join("beta");
        std::fs::create_dir_all(&alpha_dir).unwrap();
        std::fs::create_dir_all(&beta_dir).unwrap();
        seed_cargo_toml(&alpha_dir, "alpha");
        seed_cargo_toml(&beta_dir, "beta");

        let crates = vec![
            crate_cfg(
                "alpha",
                alpha_dir.to_str().unwrap(),
                "alpha-v{{ .Version }}",
            ),
            crate_cfg("beta", beta_dir.to_str().unwrap(), "beta-v{{ .Version }}"),
        ];

        // Context seeded the way populate_git_vars would for the FIRST crate
        // only: alpha's version/name. Without per-crate scoping, beta inherits
        // these.
        let config = Config {
            project_name: "alpha".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.2.3");
        ctx.template_vars_mut().set("RawVersion", "1.2.3");
        ctx.template_vars_mut().set("Tag", "alpha-v1.2.3");
        ctx.template_vars_mut().set("ProjectName", "alpha");

        // Inject each crate's own tag so the test needs no git fixture.
        let resolver = |_ctx: &Context, c: &CrateConfig| -> Option<String> {
            match c.name.as_str() {
                "alpha" => Some("alpha-v1.2.3".to_string()),
                "beta" => Some("beta-v4.5.6".to_string()),
                _ => None,
            }
        };

        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        apply_source_mutations_with_resolver(&mut ctx, &crates, &[], false, &log, &resolver)
            .unwrap();

        // version_sync: each Cargo.toml carries ITS OWN version.
        let alpha_toml = std::fs::read_to_string(alpha_dir.join("Cargo.toml")).unwrap();
        let beta_toml = std::fs::read_to_string(beta_dir.join("Cargo.toml")).unwrap();
        let alpha_doc = alpha_toml.parse::<toml_edit::DocumentMut>().unwrap();
        let beta_doc = beta_toml.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(alpha_doc["package"]["version"].as_str().unwrap(), "1.2.3");
        assert_eq!(
            beta_doc["package"]["version"].as_str().unwrap(),
            "4.5.6",
            "beta must get ITS OWN version, not alpha's 1.2.3"
        );

        // binstall pkg-url: each rendered with its own Version + ProjectName.
        let alpha_url = alpha_doc["package"]["metadata"]["binstall"]["pkg-url"]
            .as_str()
            .unwrap();
        let beta_url = beta_doc["package"]["metadata"]["binstall"]["pkg-url"]
            .as_str()
            .unwrap();
        assert!(
            alpha_url.contains("/myorg/alpha/")
                && alpha_url.contains("/v1.2.3/")
                && alpha_url.contains("alpha-1.2.3-"),
            "alpha pkg-url should carry alpha's name+version, got: {alpha_url}"
        );
        assert!(
            beta_url.contains("/myorg/beta/")
                && beta_url.contains("/v4.5.6/")
                && beta_url.contains("beta-4.5.6-"),
            "beta pkg-url must carry beta's OWN name+version, not alpha's, got: {beta_url}"
        );
        assert!(
            !beta_url.contains("alpha") && !beta_url.contains("1.2.3"),
            "beta pkg-url must not bleed alpha's name/version, got: {beta_url}"
        );

        // After the loop, the global vars are restored to the first crate's
        // values (no leak past the helper).
        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("1.2.3")
        );
        assert_eq!(
            ctx.template_vars().get("ProjectName").map(String::as_str),
            Some("alpha")
        );
    }

    /// Single-crate / lockstep parity: when the per-crate tag yields the same
    /// version the context already carries, the result is unchanged.
    #[test]
    fn apply_source_mutations_single_crate_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("solo");
        std::fs::create_dir_all(&dir).unwrap();
        seed_cargo_toml(&dir, "solo");

        let crates = vec![crate_cfg("solo", dir.to_str().unwrap(), "v{{ .Version }}")];
        let config = Config {
            project_name: "solo".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "3.3.3");
        ctx.template_vars_mut().set("RawVersion", "3.3.3");
        ctx.template_vars_mut().set("Tag", "v3.3.3");
        ctx.template_vars_mut().set("ProjectName", "solo");

        let resolver = |_ctx: &Context, _c: &CrateConfig| Some("v3.3.3".to_string());
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        apply_source_mutations_with_resolver(&mut ctx, &crates, &[], false, &log, &resolver)
            .unwrap();

        let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        let doc = toml.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(doc["package"]["version"].as_str().unwrap(), "3.3.3");
        let url = doc["package"]["metadata"]["binstall"]["pkg-url"]
            .as_str()
            .unwrap();
        assert!(url.contains("/v3.3.3/") && url.contains("solo-3.3.3-"));
    }

    /// Lockstep workspace: two crates share ONE version. Each must receive that
    /// shared version with no bleed and no spurious error.
    #[test]
    fn apply_source_mutations_lockstep_multi_crate_shared_version() {
        let tmp = tempfile::tempdir().unwrap();
        let core_dir = tmp.path().join("core");
        let cli_dir = tmp.path().join("cli");
        std::fs::create_dir_all(&core_dir).unwrap();
        std::fs::create_dir_all(&cli_dir).unwrap();
        seed_cargo_toml(&core_dir, "core");
        seed_cargo_toml(&cli_dir, "cli");

        let crates = vec![
            crate_cfg("core", core_dir.to_str().unwrap(), "v{{ .Version }}"),
            crate_cfg("cli", cli_dir.to_str().unwrap(), "v{{ .Version }}"),
        ];
        let config = Config {
            project_name: "myws".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "2.0.0");
        ctx.template_vars_mut().set("RawVersion", "2.0.0");
        ctx.template_vars_mut().set("Tag", "v2.0.0");
        ctx.template_vars_mut().set("ProjectName", "myws");

        // Lockstep: both crates resolve to the SAME shared tag.
        let resolver = |_ctx: &Context, _c: &CrateConfig| Some("v2.0.0".to_string());
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        apply_source_mutations_with_resolver(&mut ctx, &crates, &[], false, &log, &resolver)
            .unwrap();

        for (dir, name) in [(&core_dir, "core"), (&cli_dir, "cli")] {
            let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
            let doc = toml.parse::<toml_edit::DocumentMut>().unwrap();
            assert_eq!(
                doc["package"]["version"].as_str().unwrap(),
                "2.0.0",
                "{name} should carry the shared lockstep version"
            );
            let url = doc["package"]["metadata"]["binstall"]["pkg-url"]
                .as_str()
                .unwrap();
            assert!(
                url.contains("/v2.0.0/") && url.contains(&format!("{name}-2.0.0-")),
                "{name} pkg-url should carry its own name + shared version, got: {url}"
            );
        }
    }

    /// `--crate X` subset: only the selected crate is passed in `crates`, so
    /// only it is mutated through this path.
    #[test]
    fn apply_source_mutations_crate_subset_mutates_only_selected() {
        let tmp = tempfile::tempdir().unwrap();
        let alpha_dir = tmp.path().join("alpha");
        let beta_dir = tmp.path().join("beta");
        std::fs::create_dir_all(&alpha_dir).unwrap();
        std::fs::create_dir_all(&beta_dir).unwrap();
        seed_cargo_toml(&alpha_dir, "alpha");
        seed_cargo_toml(&beta_dir, "beta");

        // Only beta is selected (mirrors the BuildStage filtering crates by
        // `selected_crates` before calling apply_source_mutations).
        let crates = vec![crate_cfg(
            "beta",
            beta_dir.to_str().unwrap(),
            "beta-v{{ .Version }}",
        )];
        let config = Config {
            project_name: "alpha".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "4.5.6");
        ctx.template_vars_mut().set("RawVersion", "4.5.6");
        ctx.template_vars_mut().set("Tag", "beta-v4.5.6");
        ctx.template_vars_mut().set("ProjectName", "beta");

        let resolver = |_ctx: &Context, _c: &CrateConfig| Some("beta-v4.5.6".to_string());
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        apply_source_mutations_with_resolver(&mut ctx, &crates, &[], false, &log, &resolver)
            .unwrap();

        // beta mutated.
        let beta_toml = std::fs::read_to_string(beta_dir.join("Cargo.toml")).unwrap();
        let beta_doc = beta_toml.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(beta_doc["package"]["version"].as_str().unwrap(), "4.5.6");

        // alpha left at its seeded 0.0.0 with no binstall section: untouched.
        let alpha_toml = std::fs::read_to_string(alpha_dir.join("Cargo.toml")).unwrap();
        let alpha_doc = alpha_toml.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(alpha_doc["package"]["version"].as_str().unwrap(), "0.0.0");
        assert!(
            alpha_doc["package"].get("metadata").is_none(),
            "alpha must not be mutated when only beta is selected"
        );
    }

    /// Error-path restore: a crate whose binstall template fails to render must
    /// propagate the error AND restore the global vars (not leave them at the
    /// failed crate's scoped values).
    #[test]
    fn apply_source_mutations_restores_vars_on_render_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("boom");
        std::fs::create_dir_all(&dir).unwrap();
        seed_cargo_toml(&dir, "boom");

        let mut bad = crate_cfg("boom", dir.to_str().unwrap(), "boom-v{{ .Version }}");
        // An unterminated Tera tag is a hard render error.
        bad.binstall.as_mut().unwrap().pkg_url =
            Some("https://example.com/{{ .Version ".to_string());
        let crates = vec![bad];

        let config = Config {
            project_name: "first".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "9.9.9");
        ctx.template_vars_mut().set("RawVersion", "9.9.9");
        ctx.template_vars_mut().set("Tag", "first-v9.9.9");
        ctx.template_vars_mut().set("ProjectName", "first");

        let resolver = |_ctx: &Context, _c: &CrateConfig| Some("boom-v1.0.0".to_string());
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        let result =
            apply_source_mutations_with_resolver(&mut ctx, &crates, &[], false, &log, &resolver);
        assert!(result.is_err(), "bad binstall template must error");

        // Global vars restored to the pre-loop ("first" crate) values.
        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("9.9.9"),
            "Version must be restored after a failed crate, not left at boom's 1.0.0"
        );
        assert_eq!(
            ctx.template_vars().get("ProjectName").map(String::as_str),
            Some("first")
        );
        assert_eq!(
            ctx.template_vars().get("Tag").map(String::as_str),
            Some("first-v9.9.9")
        );
    }

    /// No resolvable tag for a mutation-enabled crate is a fail-loud error
    /// naming the crate — never a silent fall-back to the first crate's vars.
    #[test]
    fn apply_source_mutations_no_tag_fails_loud() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("orphan");
        std::fs::create_dir_all(&dir).unwrap();
        seed_cargo_toml(&dir, "orphan");

        let crates = vec![crate_cfg(
            "orphan",
            dir.to_str().unwrap(),
            "orphan-v{{ .Version }}",
        )];
        let config = Config {
            project_name: "first".to_string(),
            ..Default::default()
        };
        // Anchor tag discovery on the (tagless, non-repo) tempdir; the
        // default "." would leak the developer checkout's own tags into the
        // zero-tags-branch assertion below.
        let options = ContextOptions {
            project_root: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        let mut ctx = Context::new(config, options);
        ctx.template_vars_mut().set("Version", "1.2.3");
        ctx.template_vars_mut().set("ProjectName", "first");

        // Resolver finds NO tag for this crate.
        let resolver = |_ctx: &Context, _c: &CrateConfig| None;
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        let err =
            apply_source_mutations_with_resolver(&mut ctx, &crates, &[], false, &log, &resolver)
                .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("orphan") && msg.contains("no release tag matching its tag_template"),
            "error must name the crate and the no-tag cause, got: {msg}"
        );
        // The test cwd is not a git repo, so the diagnosis must take the
        // zero-tags branch: name the state and the remedies, not just the
        // template mismatch.
        assert!(
            msg.contains("no git tags at all")
                && msg.contains("git fetch --tags")
                && msg.contains("--snapshot")
                && msg.contains("orphan-v{{ .Version }}"),
            "tagless-repo error must state the zero-tags cause, remedies, and the tag_template, got: {msg}"
        );

        // The crate's Cargo.toml must NOT have been stamped with the first
        // crate's version.
        let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(
            toml.contains("version = \"0.0.0\""),
            "no-tag crate must remain unstamped, got:\n{toml}"
        );
    }

    /// An unparseable resolved tag for a mutation-enabled crate is a fail-loud
    /// error (the partial-override case: Tag derivable but Version not).
    #[test]
    fn apply_source_mutations_unparseable_tag_fails_loud() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("weird");
        std::fs::create_dir_all(&dir).unwrap();
        seed_cargo_toml(&dir, "weird");

        let crates = vec![crate_cfg(
            "weird",
            dir.to_str().unwrap(),
            "weird-{{ .Version }}",
        )];
        let config = Config {
            project_name: "first".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.2.3");
        ctx.template_vars_mut().set("ProjectName", "first");

        // Resolver returns a non-semver string.
        let resolver = |_ctx: &Context, _c: &CrateConfig| Some("weird-nightly".to_string());
        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        let err =
            apply_source_mutations_with_resolver(&mut ctx, &crates, &[], false, &log, &resolver)
                .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("weird") && msg.contains("weird-nightly"),
            "error must name the crate and its unparseable tag, got: {msg}"
        );
        let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(
            toml.contains("version = \"0.0.0\""),
            "unparseable-tag crate must remain unstamped, got:\n{toml}"
        );
    }

    // -- git-backed integration: the REAL resolve_crate_tag path --------------

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(args).current_dir(dir);
                cmd
            },
            "git",
        );
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The key missing proof: drive the PRODUCTION `apply_source_mutations`
    /// (which calls the real git-backed `resolve_crate_tag`) against a temp repo
    /// with two crates tagged at INDEPENDENT versions at HEAD. Each crate's
    /// Cargo.toml version AND binstall pkg-url must carry ITS OWN version/name.
    #[test]
    fn apply_source_mutations_git_backed_per_crate_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let alpha_dir = root.join("alpha");
        let beta_dir = root.join("beta");
        std::fs::create_dir_all(&alpha_dir).unwrap();
        std::fs::create_dir_all(&beta_dir).unwrap();
        seed_cargo_toml(&alpha_dir, "alpha");
        seed_cargo_toml(&beta_dir, "beta");

        run_git(root, &["init"]);
        run_git(root, &["config", "user.email", "test@test.com"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-m", "initial"]);
        // Two INDEPENDENT-version tags on the SAME HEAD commit.
        run_git(root, &["tag", "alpha-v1.2.3"]);
        run_git(root, &["tag", "beta-v4.5.6"]);

        let crates = vec![
            crate_cfg(
                "alpha",
                alpha_dir.to_str().unwrap(),
                "alpha-v{{ .Version }}",
            ),
            crate_cfg("beta", beta_dir.to_str().unwrap(), "beta-v{{ .Version }}"),
        ];
        let config = Config {
            project_name: "alpha".to_string(),
            ..Default::default()
        };
        // Drive git discovery against the temp repo without mutating cwd.
        let options = ContextOptions {
            project_root: Some(root.to_path_buf()),
            ..Default::default()
        };
        let mut ctx = Context::new(config, options);
        // Seed the first-crate vars exactly as resolve_git_context would.
        ctx.template_vars_mut().set("Version", "1.2.3");
        ctx.template_vars_mut().set("RawVersion", "1.2.3");
        ctx.template_vars_mut().set("Tag", "alpha-v1.2.3");
        ctx.template_vars_mut().set("ProjectName", "alpha");

        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        // Production entry point — exercises the real resolve_crate_tag.
        apply_source_mutations(&mut ctx, &crates, &[], false, &log).unwrap();

        let alpha_toml = std::fs::read_to_string(alpha_dir.join("Cargo.toml")).unwrap();
        let beta_toml = std::fs::read_to_string(beta_dir.join("Cargo.toml")).unwrap();
        let alpha_doc = alpha_toml.parse::<toml_edit::DocumentMut>().unwrap();
        let beta_doc = beta_toml.parse::<toml_edit::DocumentMut>().unwrap();

        assert_eq!(alpha_doc["package"]["version"].as_str().unwrap(), "1.2.3");
        assert_eq!(
            beta_doc["package"]["version"].as_str().unwrap(),
            "4.5.6",
            "git-backed: beta must get ITS OWN tag's version 4.5.6, not alpha's 1.2.3"
        );

        let beta_url = beta_doc["package"]["metadata"]["binstall"]["pkg-url"]
            .as_str()
            .unwrap();
        assert!(
            beta_url.contains("/myorg/beta/")
                && beta_url.contains("/v4.5.6/")
                && beta_url.contains("beta-4.5.6-"),
            "git-backed: beta pkg-url must carry beta's own name+version, got: {beta_url}"
        );
        assert!(
            !beta_url.contains("alpha") && !beta_url.contains("1.2.3"),
            "git-backed: beta pkg-url must not bleed alpha, got: {beta_url}"
        );
    }
}

#[cfg(test)]
mod run_helpers_tests {
    use super::*;
    use anodizer_core::MapEnvSource;
    use anodizer_core::artifact::ArtifactKind;
    use anodizer_core::config::{Config, CrateConfig, UniversalBinaryConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::Verbosity;
    use anodizer_core::template::TemplateVars;
    use std::collections::HashMap;

    fn mk_ctx() -> Context {
        let config = Config {
            project_name: "myproj".to_string(),
            ..Default::default()
        };
        Context::new(config, ContextOptions::default())
    }

    fn mk_log() -> StageLogger {
        StageLogger::new("test", Verbosity::Quiet)
    }

    // -------------------------------------------------------------------------
    // artifact_meta
    // -------------------------------------------------------------------------

    /// amd64_variant is inserted only when Some — proves the conditional branch.
    #[test]
    fn artifact_meta_inserts_amd64_variant_only_when_some() {
        let m = artifact_meta("mybinary", &None, &Some("v3".to_string()));
        assert_eq!(m.get("amd64_variant").map(String::as_str), Some("v3"));
        let m2 = artifact_meta("mybinary", &None, &None);
        assert!(
            !m2.contains_key("amd64_variant"),
            "amd64_variant must be absent when None"
        );
    }

    /// build_id overrides the binary name in the `id` key.
    #[test]
    fn artifact_meta_build_id_overrides_binary_in_id_key() {
        let m = artifact_meta("mybin", &Some("custom-id".to_string()), &None);
        assert_eq!(m.get("id").map(String::as_str), Some("custom-id"));
        assert_eq!(m.get("binary").map(String::as_str), Some("mybin"));
    }

    // -------------------------------------------------------------------------
    // add_artifact — no_unique_dist_dir branch (lines 95-117)
    // -------------------------------------------------------------------------

    /// no_unique_dist_dir=true, dry_run=true: artifact path placed flat under
    /// dist_dir but NO copy happens (dry-run). The artifact is registered with
    /// `no_unique_dist_dir` in metadata.
    #[test]
    fn add_artifact_no_unique_dist_dir_dry_run_registers_flat_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dist_dir = tmp.path().join("dist");
        std::fs::create_dir_all(&dist_dir).unwrap();

        let fake_bin = tmp.path().join("subdir").join("mybin");
        // Don't create the file — dry_run should not copy.

        let mut ctx = mk_ctx();
        add_artifact(
            &mut ctx,
            &dist_dir,
            true, // dry_run
            &fake_bin,
            ArtifactKind::Binary,
            "x86_64-unknown-linux-gnu",
            "myproj",
            "mybin",
            &None,
            true, // no_unique_dist_dir
            &None,
        )
        .unwrap();

        let arts = ctx.artifacts.by_kind(ArtifactKind::Binary);
        assert_eq!(arts.len(), 1);
        let a = &arts[0];
        // Flat path under dist_dir.
        assert_eq!(a.path, dist_dir.join("mybin"));
        assert_eq!(
            a.metadata.get("no_unique_dist_dir").map(String::as_str),
            Some("true"),
            "metadata must carry no_unique_dist_dir flag"
        );
        // Dry-run: the file should NOT have been created.
        assert!(!dist_dir.join("mybin").exists());
    }

    /// no_unique_dist_dir=true, dry_run=false, source exists: the binary is
    /// copied to the flat dist path.
    #[test]
    fn add_artifact_no_unique_dist_dir_copies_file_when_not_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let dist_dir = tmp.path().join("dist");
        std::fs::create_dir_all(&dist_dir).unwrap();

        let src_bin = tmp.path().join("mybin");
        std::fs::write(&src_bin, b"fake-binary-content").unwrap();

        let mut ctx = mk_ctx();
        add_artifact(
            &mut ctx,
            &dist_dir,
            false, // not dry_run
            &src_bin,
            ArtifactKind::Binary,
            "x86_64-unknown-linux-gnu",
            "myproj",
            "mybin",
            &None,
            true, // no_unique_dist_dir
            &None,
        )
        .unwrap();

        let flat = dist_dir.join("mybin");
        assert!(flat.exists(), "binary must be copied to flat dist path");
        assert_eq!(
            std::fs::read(&flat).unwrap(),
            b"fake-binary-content",
            "copied content must match source"
        );
    }

    /// no_unique_dist_dir=false path: artifact registered at the original path
    /// (no copy).
    #[test]
    fn add_artifact_normal_path_uses_original_bin_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dist_dir = tmp.path().join("dist");
        std::fs::create_dir_all(&dist_dir).unwrap();
        let bin_path = tmp.path().join("mybin");

        let mut ctx = mk_ctx();
        add_artifact(
            &mut ctx,
            &dist_dir,
            false,
            &bin_path,
            ArtifactKind::Binary,
            "x86_64-unknown-linux-gnu",
            "myproj",
            "mybin",
            &None,
            false, // no_unique_dist_dir = false
            &None,
        )
        .unwrap();

        let arts = ctx.artifacts.by_kind(ArtifactKind::Binary);
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].path, bin_path);
        assert!(
            !arts[0].metadata.contains_key("no_unique_dist_dir"),
            "normal path must not inject no_unique_dist_dir metadata"
        );
    }

    // -------------------------------------------------------------------------
    // apply_source_mutations — synthesized-version modes (snapshot / nightly)
    // -------------------------------------------------------------------------

    /// Snapshot mode: mutations are completely skipped for every crate, even if
    /// version_sync and binstall are both enabled. The Cargo.toml stays unchanged.
    #[test]
    fn apply_source_mutations_snapshot_mode_skips_all_mutations() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("solo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"solo\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        let mut crate_cfg = CrateConfig {
            name: "solo".to_string(),
            path: dir.to_str().unwrap().to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            version_sync: Some(anodizer_core::config::VersionSyncConfig {
                enabled: Some(true),
                mode: None,
            }),
            binstall: Some(anodizer_core::config::BinstallConfig {
                enabled: Some(true),
                pkg_url: Some("https://example.com/v{{ .Version }}/solo.tar.gz".to_string()),
                bin_dir: None,
                pkg_fmt: Some("tgz".to_string()),
                overrides: None,
            }),
            ..Default::default()
        };
        let _ = &mut crate_cfg;

        let config = Config {
            project_name: "solo".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                snapshot: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "5.0.0");
        ctx.template_vars_mut().set("RawVersion", "5.0.0");
        ctx.template_vars_mut().set("Tag", "v5.0.0");
        ctx.template_vars_mut().set("ProjectName", "solo");

        // Resolver should never be called in snapshot mode (saved vec is empty).
        let resolver = |_: &Context, _: &CrateConfig| -> Option<String> {
            panic!("resolver must not be called in snapshot mode")
        };
        let log = mk_log();
        apply_source_mutations_with_resolver(&mut ctx, &[crate_cfg], &[], false, &log, &resolver)
            .unwrap();

        // Cargo.toml must remain at 0.0.0 — no version-sync applied.
        let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(
            toml.contains("version = \"0.0.0\""),
            "snapshot mode must not mutate Cargo.toml, got:\n{toml}"
        );
        // No binstall metadata section created.
        let doc = toml.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(
            doc["package"].get("metadata").is_none(),
            "snapshot mode must not inject binstall metadata"
        );
    }

    /// Nightly mode skips source mutations exactly like snapshot — its version
    /// is synthesized, so stamping it into Cargo.toml/binstall metadata would
    /// write a version no registry release carries. A TAGLESS repo (the state a
    /// rollback/re-cut leaves behind) must not die at the tag guard: the
    /// resolver is never consulted.
    #[test]
    fn apply_source_mutations_nightly_mode_skips_all_mutations_tagless() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("solo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"solo\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        let crate_cfg = CrateConfig {
            name: "solo".to_string(),
            path: dir.to_str().unwrap().to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            version_sync: Some(anodizer_core::config::VersionSyncConfig {
                enabled: Some(true),
                mode: None,
            }),
            binstall: Some(anodizer_core::config::BinstallConfig {
                enabled: Some(true),
                pkg_url: Some("https://example.com/v{{ .Version }}/solo.tar.gz".to_string()),
                bin_dir: None,
                pkg_fmt: Some("tgz".to_string()),
                overrides: None,
            }),
            ..Default::default()
        };

        let config = Config {
            project_name: "solo".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            ContextOptions {
                nightly: true,
                ..Default::default()
            },
        );
        // The synthesized nightly version, as apply_nightly_template_vars
        // would have stamped it before the pipeline ran.
        ctx.template_vars_mut()
            .set("Version", "5.0.1-abc123d-nightly");
        ctx.template_vars_mut()
            .set("RawVersion", "5.0.1-abc123d-nightly");
        ctx.template_vars_mut().set("ProjectName", "solo");

        // A tagless repo resolves NO tag; nightly must never even ask.
        let resolver = |_: &Context, _: &CrateConfig| -> Option<String> {
            panic!("resolver must not be called in nightly mode")
        };
        let log = mk_log();
        apply_source_mutations_with_resolver(&mut ctx, &[crate_cfg], &[], false, &log, &resolver)
            .unwrap();

        let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(
            toml.contains("version = \"0.0.0\""),
            "nightly mode must not mutate Cargo.toml, got:\n{toml}"
        );
        let doc = toml.parse::<toml_edit::DocumentMut>().unwrap();
        assert!(
            doc["package"].get("metadata").is_none(),
            "nightly mode must not inject binstall metadata"
        );
    }

    // -------------------------------------------------------------------------
    // seed_determinism_state (lines 272-282, 288-290)
    // -------------------------------------------------------------------------

    /// Non-snapshot mode with a valid commit timestamp: determinism state is
    /// seeded from the epoch.
    #[test]
    fn seed_determinism_state_non_snapshot_seeds_from_commit_timestamp() {
        let mut ctx = mk_ctx();
        // inject a clean MapEnvSource with no SOURCE_DATE_EPOCH so the
        // commit_timestamp branch fires.
        ctx.set_env_source(MapEnvSource::new());
        let log = mk_log();
        seed_determinism_state(&mut ctx, "1700000000", &log).unwrap();
        assert!(
            ctx.determinism.is_some(),
            "determinism state must be seeded from commit timestamp"
        );
    }

    /// Non-snapshot mode with SOURCE_DATE_EPOCH overriding the commit timestamp.
    #[test]
    fn seed_determinism_state_respects_source_date_epoch_env() {
        let mut ctx = mk_ctx();
        ctx.set_env_source(MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1600000000"));
        let log = mk_log();
        seed_determinism_state(&mut ctx, "0", &log).unwrap();
        assert!(
            ctx.determinism.is_some(),
            "determinism state must be seeded from SOURCE_DATE_EPOCH"
        );
    }

    /// When determinism is already set before calling seed_determinism_state, it
    /// is not overwritten.
    #[test]
    fn seed_determinism_state_does_not_overwrite_existing() {
        let mut ctx = mk_ctx();
        ctx.set_env_source(MapEnvSource::new());
        let log = mk_log();
        // Seed once.
        seed_determinism_state(&mut ctx, "1700000000", &log).unwrap();
        let state_ptr = ctx.determinism.as_ref().unwrap() as *const _;
        // Seed again — must not replace.
        seed_determinism_state(&mut ctx, "1800000000", &log).unwrap();
        let state_ptr2 = ctx.determinism.as_ref().unwrap() as *const _;
        assert_eq!(
            state_ptr, state_ptr2,
            "already-set determinism state must not be replaced on second call"
        );
    }

    /// runtime_nondeterministic_allowlist entries are appended to the state.
    #[test]
    fn seed_determinism_state_appends_runtime_allowlist() {
        let mut ctx = Context::new(
            Config {
                project_name: "myproj".to_string(),
                ..Default::default()
            },
            ContextOptions {
                runtime_nondeterministic_allowlist: vec![(
                    "myartifact".to_string(),
                    "platform-specific".to_string(),
                )],
                ..Default::default()
            },
        );
        ctx.set_env_source(MapEnvSource::new());
        let log = mk_log();
        seed_determinism_state(&mut ctx, "1700000000", &log).unwrap();
        assert!(
            ctx.determinism.is_some(),
            "state must be seeded so allowlist append runs"
        );
        // The state was seeded and then append_runtime was called with our pair.
        // We can't inspect internals directly, but we can assert no panic/error
        // and that state is present, which is meaningful because the allowlist-
        // append path (line 292-294) is exercised.
    }

    /// Non-snapshot, commit_timestamp = "0" and no SOURCE_DATE_EPOCH: epoch is
    /// zero, so determinism is NOT seeded (epoch must be > 0).
    #[test]
    fn seed_determinism_state_zero_epoch_leaves_determinism_none() {
        let mut ctx = mk_ctx();
        ctx.set_env_source(MapEnvSource::new());
        let log = mk_log();
        seed_determinism_state(&mut ctx, "0", &log).unwrap();
        assert!(
            ctx.determinism.is_none(),
            "epoch=0 must not seed determinism state"
        );
    }

    // -------------------------------------------------------------------------
    // run_dry_run (lines 322-370)
    // -------------------------------------------------------------------------

    fn mk_exec<'a>(
        log: &'a StageLogger,
        tvars: &'a TemplateVars,
        dist_dir: &'a std::path::Path,
    ) -> BuildExec<'a> {
        BuildExec {
            log,
            template_vars: tvars,
            dist_dir,
            dry_run: true,
            commit_timestamp: "0",
        }
    }

    fn mk_build_job(bin_path: std::path::PathBuf, target: &str) -> BuildJob {
        BuildJob {
            cmd: Some(BuildCommand {
                program: "true".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: std::path::PathBuf::from("."),
            }),
            copy_from: None,
            bin_path,
            artifact_kind: ArtifactKind::Binary,
            target: target.to_string(),
            crate_name: "myproj".to_string(),
            binary_name: "myproj".to_string(),
            build_id: None,
            reproducible: false,
            pre_hooks: vec![],
            post_hooks: vec![],
            no_unique_dist_dir: false,
            crate_path: ".".to_string(),
            mod_timestamp: None,
            amd64_variant: None,
            build_env: HashMap::new(),
        }
    }

    /// run_dry_run with a cmd job: artifact is registered without spawning the
    /// compiler. The artifact appears in ctx.artifacts.
    #[test]
    fn run_dry_run_registers_artifact_without_spawning() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let bin = tmp.path().join("myproj");

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist);

        let job = mk_build_job(bin.clone(), "x86_64-unknown-linux-gnu");
        let mut ctx = mk_ctx();
        run_dry_run(&mut ctx, &exec, &[job], &[]).unwrap();

        let arts = ctx.artifacts.by_kind(ArtifactKind::Binary);
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].target.as_deref(), Some("x86_64-unknown-linux-gnu"));
    }

    /// run_dry_run with a copy_from job (cmd=None, copy_from set): the copy_from
    /// log branch fires instead of the cmd branch.
    #[test]
    fn run_dry_run_logs_copy_from_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let src = tmp.path().join("src_bin");
        let dst = tmp.path().join("dst_bin");

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist);

        let job = BuildJob {
            cmd: None,
            copy_from: Some((src.clone(), dst.clone())),
            bin_path: src.clone(),
            artifact_kind: ArtifactKind::Binary,
            target: "aarch64-apple-darwin".to_string(),
            crate_name: "myproj".to_string(),
            binary_name: "myproj".to_string(),
            build_id: None,
            reproducible: false,
            pre_hooks: vec![],
            post_hooks: vec![],
            no_unique_dist_dir: false,
            crate_path: ".".to_string(),
            mod_timestamp: None,
            amd64_variant: None,
            build_env: HashMap::new(),
        };
        let mut ctx = mk_ctx();
        run_dry_run(&mut ctx, &exec, &[job], &[]).unwrap();

        let arts = ctx.artifacts.by_kind(ArtifactKind::Binary);
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].target.as_deref(), Some("aarch64-apple-darwin"));
    }

    /// run_dry_run with multiple jobs across build_jobs and copy_jobs: all
    /// artifacts are registered.
    #[test]
    fn run_dry_run_processes_both_build_and_copy_job_slices() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist);

        let build_job = mk_build_job(tmp.path().join("bin1"), "x86_64-unknown-linux-gnu");
        let copy_job = BuildJob {
            cmd: None,
            copy_from: Some((tmp.path().join("bin1"), tmp.path().join("bin1-copy"))),
            bin_path: tmp.path().join("bin1"),
            artifact_kind: ArtifactKind::Binary,
            target: "aarch64-unknown-linux-gnu".to_string(),
            crate_name: "myproj".to_string(),
            binary_name: "myproj".to_string(),
            build_id: None,
            reproducible: false,
            pre_hooks: vec![],
            post_hooks: vec![],
            no_unique_dist_dir: false,
            crate_path: ".".to_string(),
            mod_timestamp: None,
            amd64_variant: None,
            build_env: HashMap::new(),
        };
        let mut ctx = mk_ctx();
        run_dry_run(&mut ctx, &exec, &[build_job], &[copy_job]).unwrap();

        // Both jobs processed: 2 artifact registrations.
        let arts = ctx.artifacts.by_kind(ArtifactKind::Binary);
        assert_eq!(arts.len(), 2);
    }

    // -------------------------------------------------------------------------
    // run_dry_run with no_unique_dist_dir=true in a copy_jobs slot (line 367)
    // -------------------------------------------------------------------------

    /// run_dry_run with no_unique_dist_dir=true on a copy_job: flat-path
    /// registration fires. dry_run=true so no copy happens.
    #[test]
    fn run_dry_run_no_unique_dist_dir_registers_flat_path_no_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let src = tmp.path().join("mybin");

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist);

        let job = BuildJob {
            cmd: Some(BuildCommand {
                program: "true".to_string(),
                args: vec![],
                env: HashMap::new(),
                cwd: std::path::PathBuf::from("."),
            }),
            copy_from: None,
            bin_path: src.clone(),
            artifact_kind: ArtifactKind::Binary,
            target: "x86_64-apple-darwin".to_string(),
            crate_name: "myproj".to_string(),
            binary_name: "mybin".to_string(),
            build_id: None,
            reproducible: false,
            pre_hooks: vec![],
            post_hooks: vec![],
            no_unique_dist_dir: true,
            crate_path: ".".to_string(),
            mod_timestamp: None,
            amd64_variant: None,
            build_env: HashMap::new(),
        };

        let mut ctx = mk_ctx();
        run_dry_run(&mut ctx, &exec, &[job], &[]).unwrap();

        let arts = ctx.artifacts.by_kind(ArtifactKind::Binary);
        assert_eq!(arts.len(), 1);
        // Flat path: dist_dir/mybin, not the original src path.
        assert_eq!(arts[0].path, dist.join("mybin"));
        // Dry-run: file must NOT have been written.
        assert!(!dist.join("mybin").exists());
    }

    // -------------------------------------------------------------------------
    // process_universal_binaries — duplicate output path error (lines 736-746)
    // -------------------------------------------------------------------------

    /// Two universal_binaries entries resolving to the same output path must
    /// produce a fail-loud error naming the conflicting path.
    #[test]
    fn process_universal_binaries_duplicate_output_path_errors() {
        let tmp = tempfile::tempdir().unwrap();
        // Build a context that has arm64 + x86_64 apple-darwin binaries
        // registered so project_universal_out_path returns Some for both entries.
        let mut ctx = Context::new(
            Config {
                project_name: "myapp".to_string(),
                dist: tmp.path().to_path_buf(),
                ..Default::default()
            },
            ContextOptions::default(),
        );
        ctx.template_vars_mut().set("ProjectName", "myapp");
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register the two arch binaries so the universal path projection resolves.
        use anodizer_core::artifact::Artifact;
        let fake_arm = tmp.path().join("myapp-arm64");
        let fake_x86 = tmp.path().join("myapp-x86_64");
        std::fs::write(&fake_arm, b"arm").unwrap();
        std::fs::write(&fake_x86, b"x86").unwrap();

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: fake_arm,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("binary".to_string(), "myapp".to_string()),
                ("id".to_string(), "myapp".to_string()),
            ]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: fake_x86,
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([
                ("binary".to_string(), "myapp".to_string()),
                ("id".to_string(), "myapp".to_string()),
            ]),
            size: None,
        });

        // Two entries with the SAME (default) name_template -> same output path.
        let ub1 = UniversalBinaryConfig::default();
        let ub2 = UniversalBinaryConfig::default();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            universal_binaries: Some(vec![ub1, ub2]),
            ..Default::default()
        };

        let err = process_universal_binaries(&mut ctx, &[crate_cfg], true).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("same output path") || msg.contains("disambiguate"),
            "error must mention collision/disambiguation, got: {msg}"
        );
    }

    // -------------------------------------------------------------------------
    // run_copy_jobs (lines 711-723) — missing copy_from pair error
    // -------------------------------------------------------------------------

    /// A copy_job with copy_from=None is a programmer bug; the function must
    /// return an error rather than panicking.
    #[test]
    fn run_copy_jobs_missing_copy_from_pair_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        let log = mk_log();
        let tvars = TemplateVars::default();
        // Not dry_run so copy_jobs path triggers real copy
        let exec = BuildExec {
            log: &log,
            template_vars: &tvars,
            dist_dir: &dist,
            dry_run: false,
            commit_timestamp: "0",
        };

        // copy_from is None — this is the programmer-bug path.
        let bad_job = BuildJob {
            cmd: None,
            copy_from: None,
            bin_path: tmp.path().join("bin"),
            artifact_kind: ArtifactKind::Binary,
            target: "x86_64-unknown-linux-gnu".to_string(),
            crate_name: "myproj".to_string(),
            binary_name: "myproj".to_string(),
            build_id: None,
            reproducible: false,
            pre_hooks: vec![],
            post_hooks: vec![],
            no_unique_dist_dir: false,
            crate_path: ".".to_string(),
            mod_timestamp: None,
            amd64_variant: None,
            build_env: HashMap::new(),
        };
        let mut ctx = mk_ctx();
        let err = run_copy_jobs(&mut ctx, &exec, &[bad_job]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("copy_from job without copy_from pair"),
            "error must describe the missing copy_from pair, got: {msg}"
        );
    }

    // -------------------------------------------------------------------------
    // apply_source_mutations — version_sync only (no binstall) path (line 224)
    // -------------------------------------------------------------------------

    /// A crate with version_sync enabled but binstall disabled: only Cargo.toml
    /// version is mutated, no binstall metadata is injected.
    #[test]
    fn apply_source_mutations_version_sync_only_no_binstall_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("core");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"core\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        let crate_cfg = CrateConfig {
            name: "core".to_string(),
            path: dir.to_str().unwrap().to_string(),
            tag_template: Some("core-v{{ .Version }}".to_string()),
            version_sync: Some(anodizer_core::config::VersionSyncConfig {
                enabled: Some(true),
                mode: None,
            }),
            binstall: None, // binstall not enabled
            ..Default::default()
        };

        let config = Config {
            project_name: "core".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "7.8.9");
        ctx.template_vars_mut().set("RawVersion", "7.8.9");
        ctx.template_vars_mut().set("Tag", "core-v7.8.9");
        ctx.template_vars_mut().set("ProjectName", "core");

        let resolver = |_: &Context, _: &CrateConfig| Some("core-v7.8.9".to_string());
        let log = mk_log();
        apply_source_mutations_with_resolver(&mut ctx, &[crate_cfg], &[], false, &log, &resolver)
            .unwrap();

        let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        let doc = toml.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(doc["package"]["version"].as_str().unwrap(), "7.8.9");
        assert!(
            doc["package"].get("metadata").is_none(),
            "no binstall metadata should be injected when binstall is None"
        );
    }

    // -------------------------------------------------------------------------
    // apply_source_mutations — needs_mutation=false skips entirely (line 179)
    // -------------------------------------------------------------------------

    /// A crate with neither version_sync nor binstall enabled must be skipped
    /// entirely (needs_mutation=false). The resolver must not be called and the
    /// Cargo.toml stays at 0.0.0.
    #[test]
    fn apply_source_mutations_skips_when_needs_mutation_false() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("noop");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"noop\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();

        let crate_cfg = CrateConfig {
            name: "noop".to_string(),
            path: dir.to_str().unwrap().to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            version_sync: None,
            binstall: None,
            ..Default::default()
        };

        let config = Config {
            project_name: "noop".to_string(),
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");

        let resolver = |_: &Context, _: &CrateConfig| -> Option<String> {
            panic!("resolver must not be called when needs_mutation=false")
        };
        let log = mk_log();
        apply_source_mutations_with_resolver(&mut ctx, &[crate_cfg], &[], false, &log, &resolver)
            .unwrap();

        let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(
            toml.contains("version = \"0.0.0\""),
            "noop crate must remain untouched, got:\n{toml}"
        );
    }
}

/// Tests for the live compile paths (`run_sequential` / `run_parallel` /
/// `run_copy_jobs`) that actually spawn `BuildJob::cmd`. Rather than invoke a
/// real toolchain, each job's `program` points at a `FakeToolDir` stub that
/// records argv and (where the post-build `exists()` check matters) materialises
/// the binary at `bin_path` via `.creates()`. Asserts the produced artifact's
/// path/target, the applied mtime (reproducible + mod_timestamp), and the exact
/// error text on the failure paths — never just "no panic".
#[cfg(unix)]
#[cfg(test)]
mod run_exec_tests {
    use super::*;
    use anodizer_core::MapEnvSource;
    use anodizer_core::artifact::ArtifactKind;
    use anodizer_core::config::{Config, HookEntry, StructuredHook};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::Verbosity;
    use anodizer_core::template::TemplateVars;
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use std::collections::HashMap;
    use std::time::SystemTime;

    fn mk_ctx() -> Context {
        let mut ctx = Context::new(
            Config {
                project_name: "myproj".to_string(),
                ..Default::default()
            },
            ContextOptions::default(),
        );
        // Clean env source so SOURCE_DATE_EPOCH from the host never leaks into
        // the reproducible-epoch resolution under test.
        ctx.set_env_source(MapEnvSource::new());
        ctx
    }

    fn mk_log() -> StageLogger {
        StageLogger::new("test", Verbosity::Quiet)
    }

    /// A `BuildJob` whose `cmd` runs `stub` in `cwd`, producing a binary at
    /// `cwd/<binary_name>` (matching `bin_path`) so the post-build existence
    /// check passes.
    #[allow(clippy::too_many_arguments)]
    fn building_job(
        stub: &std::path::Path,
        cwd: &std::path::Path,
        binary_name: &str,
        target: &str,
    ) -> BuildJob {
        let bin_path = cwd.join(binary_name);
        BuildJob {
            cmd: Some(BuildCommand {
                program: stub.to_string_lossy().into_owned(),
                args: vec!["build".to_string(), "--release".to_string()],
                env: HashMap::new(),
                cwd: cwd.to_path_buf(),
            }),
            copy_from: None,
            bin_path,
            artifact_kind: ArtifactKind::Binary,
            target: target.to_string(),
            crate_name: "myproj".to_string(),
            binary_name: binary_name.to_string(),
            build_id: None,
            reproducible: false,
            pre_hooks: vec![],
            post_hooks: vec![],
            no_unique_dist_dir: false,
            crate_path: cwd.to_string_lossy().into_owned(),
            mod_timestamp: None,
            amd64_variant: None,
            build_env: HashMap::new(),
        }
    }

    fn mk_exec<'a>(
        log: &'a StageLogger,
        tvars: &'a TemplateVars,
        dist: &'a std::path::Path,
        commit_timestamp: &'a str,
    ) -> BuildExec<'a> {
        BuildExec {
            log,
            template_vars: tvars,
            dist_dir: dist,
            dry_run: false,
            commit_timestamp,
        }
    }

    fn mtime_epoch(path: &std::path::Path) -> u64 {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    // -------------------------------------------------------------------------
    // run_sequential
    // -------------------------------------------------------------------------

    /// Happy path: the stub compiler is spawned with the job's argv, the
    /// resulting binary is registered as an artifact at its resolved path, and
    /// the exact argv reaches the tool.
    #[test]
    fn run_sequential_spawns_cmd_and_registers_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        let tools = FakeToolDir::new();
        tools
            .tool("cargo")
            .creates("myproj", "\x7fELF-fake")
            .install();
        let stub = tools.tool_path("cargo");

        let job = building_job(&stub, &work, "myproj", "x86_64-unknown-linux-gnu");

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        run_sequential(&mut ctx, &exec, &[job], &[]).unwrap();

        // The stub was invoked with exactly the job's argv.
        assert_eq!(tools.calls("cargo"), vec![vec!["build", "--release"]]);

        let arts = ctx.artifacts.by_kind(ArtifactKind::Binary);
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].path, work.join("myproj"));
        assert_eq!(arts[0].target.as_deref(), Some("x86_64-unknown-linux-gnu"));
    }

    /// reproducible=true with a usable commit timestamp: the produced binary's
    /// mtime is stamped to that epoch.
    #[test]
    fn run_sequential_reproducible_sets_mtime_to_commit_epoch() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        let tools = FakeToolDir::new();
        tools.tool("cargo").creates("app", "x").install();
        let mut job = building_job(
            &tools.tool_path("cargo"),
            &work,
            "app",
            "x86_64-unknown-linux-gnu",
        );
        job.reproducible = true;

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "1700000000");
        let mut ctx = mk_ctx();
        run_sequential(&mut ctx, &exec, &[job], &[]).unwrap();

        assert_eq!(
            mtime_epoch(&work.join("app")),
            1_700_000_000,
            "reproducible build must stamp the binary mtime to the commit epoch"
        );
    }

    /// mod_timestamp renders to a Unix epoch and the binary mtime is set to it,
    /// overriding the default filesystem mtime.
    #[test]
    fn run_sequential_mod_timestamp_sets_explicit_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        let tools = FakeToolDir::new();
        tools.tool("cargo").creates("app", "x").install();
        let mut job = building_job(
            &tools.tool_path("cargo"),
            &work,
            "app",
            "x86_64-unknown-linux-gnu",
        );
        // 2024-01-01T00:00:00Z = 1704067200.
        job.mod_timestamp = Some("1704067200".to_string());

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        run_sequential(&mut ctx, &exec, &[job], &[]).unwrap();

        assert_eq!(mtime_epoch(&work.join("app")), 1_704_067_200);
    }

    /// pre_hooks and post_hooks both fire around the build: each hook touches a
    /// sentinel file, proving the bracketing runs.
    #[test]
    fn run_sequential_runs_pre_and_post_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        let tools = FakeToolDir::new();
        tools.tool("cargo").creates("app", "x").install();
        let mut job = building_job(
            &tools.tool_path("cargo"),
            &work,
            "app",
            "x86_64-unknown-linux-gnu",
        );

        let pre_marker = tmp.path().join("pre.done");
        let post_marker = tmp.path().join("post.done");
        job.pre_hooks = vec![HookEntry::Structured(StructuredHook {
            cmd: format!("touch {}", pre_marker.display()),
            dir: Some(work.to_string_lossy().into_owned()),
            ..Default::default()
        })];
        job.post_hooks = vec![HookEntry::Structured(StructuredHook {
            cmd: format!("touch {}", post_marker.display()),
            dir: Some(work.to_string_lossy().into_owned()),
            ..Default::default()
        })];

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        run_sequential(&mut ctx, &exec, &[job], &[]).unwrap();

        assert!(pre_marker.exists(), "pre-build hook must run");
        assert!(post_marker.exists(), "post-build hook must run");
    }

    /// The compile succeeds but no binary appears at bin_path: a fail-loud error
    /// pointing at the missing path / Cargo.toml [bin] mismatch.
    #[test]
    fn run_sequential_missing_binary_after_success_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        // Stub exits 0 but creates NOTHING — the binary will be absent.
        let tools = FakeToolDir::new();
        tools.tool("cargo").install();
        let job = building_job(
            &tools.tool_path("cargo"),
            &work,
            "ghost",
            "x86_64-unknown-linux-gnu",
        );

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        let err = run_sequential(&mut ctx, &exec, &[job], &[]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("build succeeded but binary not found") && msg.contains("ghost"),
            "error must name the missing binary path, got: {msg}"
        );
    }

    /// The compiler exits non-zero: `check_output` surfaces a failure naming the
    /// program. No artifact is registered.
    #[test]
    fn run_sequential_compile_failure_propagates() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        let tools = FakeToolDir::new();
        tools
            .tool("cargo")
            .stderr("error[E0001]: boom\n")
            .exit(101)
            .install();
        let job = building_job(
            &tools.tool_path("cargo"),
            &work,
            "app",
            "x86_64-unknown-linux-gnu",
        );

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        let err = run_sequential(&mut ctx, &exec, &[job], &[]).unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("cargo") && (msg.contains("exit") || msg.contains("fail")),
            "compile failure must surface the program + failure, got: {msg}"
        );
        assert!(
            ctx.artifacts.by_kind(ArtifactKind::Binary).is_empty(),
            "no artifact may be registered for a failed build"
        );
    }

    /// A build job with `cmd: None` reaching the sequential path is a planner
    /// invariant violation surfaced as an error, not a panic.
    #[test]
    fn run_sequential_missing_cmd_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        let mut job = building_job(
            std::path::Path::new("/nonexistent"),
            tmp.path(),
            "app",
            "x86_64-unknown-linux-gnu",
        );
        job.cmd = None;

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        let err = run_sequential(&mut ctx, &exec, &[job], &[]).unwrap_err();
        assert!(
            format!("{err:#}").contains("build job has no cmd"),
            "missing cmd must be a fail-loud planner-bug error"
        );
    }

    /// no_unique_dist_dir=true: after building, `add_artifact` copies the binary
    /// flat into dist_dir and tags the metadata.
    #[test]
    fn run_sequential_no_unique_dist_dir_flattens_into_dist() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        let tools = FakeToolDir::new();
        tools.tool("cargo").creates("app", "BINARY").install();
        let mut job = building_job(
            &tools.tool_path("cargo"),
            &work,
            "app",
            "x86_64-unknown-linux-gnu",
        );
        job.no_unique_dist_dir = true;

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        run_sequential(&mut ctx, &exec, &[job], &[]).unwrap();

        let arts = ctx.artifacts.by_kind(ArtifactKind::Binary);
        assert_eq!(arts.len(), 1);
        assert_eq!(arts[0].path, dist.join("app"), "binary flattened into dist");
        assert_eq!(
            arts[0]
                .metadata
                .get("no_unique_dist_dir")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(std::fs::read(dist.join("app")).unwrap(), b"BINARY");
    }

    // -------------------------------------------------------------------------
    // run_parallel
    // -------------------------------------------------------------------------

    /// Parallel path across two jobs (parallelism=2): both stubs run, both
    /// binaries are registered with their respective targets.
    #[test]
    fn run_parallel_builds_all_jobs_and_registers_each() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work_a = tmp.path().join("a");
        let work_b = tmp.path().join("b");
        std::fs::create_dir_all(&work_a).unwrap();
        std::fs::create_dir_all(&work_b).unwrap();

        let tools = FakeToolDir::new();
        tools.tool("cargo").creates("app", "x").install();
        let stub = tools.tool_path("cargo");

        let job_a = building_job(&stub, &work_a, "app", "x86_64-unknown-linux-gnu");
        let job_b = building_job(&stub, &work_b, "app", "aarch64-unknown-linux-gnu");

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        run_parallel(&mut ctx, &exec, &[job_a, job_b], &[], 2).unwrap();

        let arts = ctx.artifacts.by_kind(ArtifactKind::Binary);
        assert_eq!(arts.len(), 2);
        let targets: HashSet<&str> = arts.iter().filter_map(|a| a.target.as_deref()).collect();
        assert!(targets.contains("x86_64-unknown-linux-gnu"));
        assert!(targets.contains("aarch64-unknown-linux-gnu"));
        assert_eq!(tools.call_count("cargo"), 2);
    }

    /// Parallel chunking: 3 jobs with parallelism=1 still build every job (the
    /// chunk loop iterates three times).
    #[test]
    fn run_parallel_chunks_smaller_than_job_count_build_all() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        let tools = FakeToolDir::new();
        tools.tool("cargo").creates("app", "x").install();
        let stub = tools.tool_path("cargo");

        let mut jobs = Vec::new();
        for (i, target) in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
        ]
        .iter()
        .enumerate()
        {
            let work = tmp.path().join(format!("c{i}"));
            std::fs::create_dir_all(&work).unwrap();
            jobs.push(building_job(&stub, &work, "app", target));
        }

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        run_parallel(&mut ctx, &exec, &jobs, &[], 1).unwrap();

        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Binary).len(), 3);
        assert_eq!(tools.call_count("cargo"), 3);
    }

    /// A failing job in the parallel path unwinds through the Result channel
    /// (not a process abort) and the redacted exit-code message surfaces.
    #[test]
    fn run_parallel_compile_failure_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        let tools = FakeToolDir::new();
        tools
            .tool("cargo")
            .stderr("linker error\n")
            .exit(1)
            .install();
        let job = building_job(
            &tools.tool_path("cargo"),
            &work,
            "app",
            "x86_64-unknown-linux-gnu",
        );

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        let err = run_parallel(&mut ctx, &exec, &[job], &[], 1).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed with exit code") && msg.contains("cargo"),
            "parallel failure must surface the exit-code message, got: {msg}"
        );
    }

    /// reproducible=true on the parallel path stamps the binary mtime from the
    /// commit epoch, same as the sequential path.
    #[test]
    fn run_parallel_reproducible_sets_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        let tools = FakeToolDir::new();
        tools.tool("cargo").creates("app", "x").install();
        let mut job = building_job(
            &tools.tool_path("cargo"),
            &work,
            "app",
            "x86_64-unknown-linux-gnu",
        );
        job.reproducible = true;

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "1600000000");
        let mut ctx = mk_ctx();
        run_parallel(&mut ctx, &exec, &[job], &[], 1).unwrap();

        assert_eq!(mtime_epoch(&work.join("app")), 1_600_000_000);
    }

    /// mod_timestamp on the parallel path (rendered via thread-local Tera, not
    /// ctx) stamps the binary mtime.
    #[test]
    fn run_parallel_mod_timestamp_renders_and_sets_mtime() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        let tools = FakeToolDir::new();
        tools.tool("cargo").creates("app", "x").install();
        let mut job = building_job(
            &tools.tool_path("cargo"),
            &work,
            "app",
            "x86_64-unknown-linux-gnu",
        );
        job.mod_timestamp = Some("1704067200".to_string());

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        run_parallel(&mut ctx, &exec, &[job], &[], 1).unwrap();

        assert_eq!(mtime_epoch(&work.join("app")), 1_704_067_200);
    }

    /// A `cmd: None` job reaching the parallel worker unwinds as an error naming
    /// the planner-invariant violation, not a panic that aborts the process.
    #[test]
    fn run_parallel_missing_cmd_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        let mut job = building_job(
            std::path::Path::new("/nonexistent"),
            tmp.path(),
            "app",
            "x86_64-unknown-linux-gnu",
        );
        job.cmd = None;

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        let err = run_parallel(&mut ctx, &exec, &[job], &[], 1).unwrap_err();
        assert!(
            format!("{err:#}").contains("planner invariant violation"),
            "missing cmd in the parallel worker must surface the invariant error"
        );
    }

    // -------------------------------------------------------------------------
    // run_copy_jobs — happy path (real copy)
    // -------------------------------------------------------------------------

    /// A copy_from job copies the registered source binary to its destination
    /// and registers a new artifact at the copy destination.
    #[test]
    fn run_sequential_drains_copy_jobs_after_builds() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        // A source binary already registered as an artifact (e.g. a sibling
        // build) that the copy job points at.
        let src = tmp.path().join("src-bin");
        std::fs::write(&src, b"SRC").unwrap();
        let dst = tmp.path().join("dst-bin");

        let mut ctx = mk_ctx();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "src-bin".to_string(),
            path: src.clone(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myproj".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let copy_job = BuildJob {
            cmd: None,
            copy_from: Some((src.clone(), dst.clone())),
            bin_path: dst.clone(),
            artifact_kind: ArtifactKind::Binary,
            target: "x86_64-unknown-linux-gnu".to_string(),
            crate_name: "myproj".to_string(),
            binary_name: "dst-bin".to_string(),
            build_id: None,
            reproducible: false,
            pre_hooks: vec![],
            post_hooks: vec![],
            no_unique_dist_dir: false,
            crate_path: ".".to_string(),
            mod_timestamp: None,
            amd64_variant: None,
            build_env: HashMap::new(),
        };

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        run_sequential(&mut ctx, &exec, &[], &[copy_job]).unwrap();

        assert_eq!(std::fs::read(&dst).unwrap(), b"SRC", "copy must reach dst");
        let arts = ctx.artifacts.by_kind(ArtifactKind::Binary);
        // src + copied dst.
        assert!(arts.iter().any(|a| a.path == dst));
    }

    // -------------------------------------------------------------------------
    // process_universal_binaries — non-error paths
    // -------------------------------------------------------------------------

    /// A crate with no universal_binaries config is a no-op (the `if let Some`
    /// guard is skipped); returns Ok with no artifacts added.
    #[test]
    fn process_universal_binaries_none_config_is_noop() {
        let mut ctx = mk_ctx();
        let crate_cfg = anodizer_core::config::CrateConfig {
            name: "myproj".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            universal_binaries: None,
            ..Default::default()
        };
        let before = ctx.artifacts.by_kind(ArtifactKind::Binary).len();
        process_universal_binaries(&mut ctx, &[crate_cfg], true).unwrap();
        assert_eq!(
            ctx.artifacts.by_kind(ArtifactKind::Binary).len(),
            before,
            "no universal_binaries config must add no artifacts"
        );
    }

    // -------------------------------------------------------------------------
    // run_dry_run — neither the compiler nor hook bodies execute
    // -------------------------------------------------------------------------

    /// In dry-run neither the compiler nor a hook's command body is executed
    /// (run_dry_run passes dry_run=true through to run_hooks), yet the planned
    /// artifact is still registered so downstream stages see the build output.
    #[test]
    fn run_dry_run_skips_compiler_and_hook_bodies_but_registers_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let work = tmp.path().join("crate");
        std::fs::create_dir_all(&work).unwrap();

        // Stub that, if ever spawned, would create a sentinel — proving the
        // compiler is NOT run in dry-run.
        let tools = FakeToolDir::new();
        tools.tool("cargo").creates("spawned.marker", "x").install();
        let mut job = building_job(
            &tools.tool_path("cargo"),
            &work,
            "app",
            "x86_64-unknown-linux-gnu",
        );

        // A dry-run hook: run_hooks with dry_run=true must NOT execute the body,
        // so this sentinel must remain absent.
        let hook_marker = tmp.path().join("hook.ran");
        job.pre_hooks = vec![HookEntry::Structured(StructuredHook {
            cmd: format!("touch {}", hook_marker.display()),
            ..Default::default()
        })];

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = BuildExec {
            log: &log,
            template_vars: &tvars,
            dist_dir: &dist,
            dry_run: true,
            commit_timestamp: "0",
        };
        let mut ctx = mk_ctx();
        run_dry_run(&mut ctx, &exec, &[job], &[]).unwrap();

        assert!(
            !work.join("spawned.marker").exists(),
            "dry-run must not spawn the compiler"
        );
        assert!(
            !hook_marker.exists(),
            "dry-run hooks must not execute their command body"
        );
        // Artifact still registered (planning output).
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Binary).len(), 1);
    }

    // -------------------------------------------------------------------------
    // Harness-gated cargo-intermediate pruning
    // -------------------------------------------------------------------------

    /// Scaffold the four cargo-scratch subdirs under `profile_dir` so a prune
    /// (or retention) is observable after the stub build runs.
    fn seed_intermediates(profile_dir: &std::path::Path) {
        for sub in ["deps", "build", "incremental", ".fingerprint"] {
            let d = profile_dir.join(sub);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("scratch"), "x").unwrap();
        }
    }

    fn intermediates_present(profile_dir: &std::path::Path) -> bool {
        ["deps", "build", "incremental", ".fingerprint"]
            .iter()
            .any(|s| profile_dir.join(s).exists())
    }

    /// A real `<root>/target/<triple>/release/` profile dir, seeded with cargo
    /// scratch — the layout the prune guard (basename `release`) requires.
    fn mk_profile(root: &std::path::Path, triple: &str) -> std::path::PathBuf {
        let profile = root.join("target").join(triple).join("release");
        std::fs::create_dir_all(&profile).unwrap();
        seed_intermediates(&profile);
        profile
    }

    /// A build job whose binary lands IN-PLACE at the planned
    /// `<root>/target/<triple>/release/<binary>` (planned == resolved).
    ///
    /// Each job gets its OWN stub tool (named after `binary`) that `creates`
    /// the binary there via an absolute path — distinct tool names so multiple
    /// jobs in one test don't overwrite each other's stub.
    fn release_job(
        tools: &FakeToolDir,
        root: &std::path::Path,
        triple: &str,
        binary: &str,
    ) -> BuildJob {
        let profile = root.join("target").join(triple).join("release");
        let bin_abs = profile.join(binary);
        let tool_name = format!("cargo-{binary}");
        tools
            .tool(&tool_name)
            .creates(bin_abs.to_string_lossy().into_owned(), "x")
            .install();
        let mut job = building_job(&tools.tool_path(&tool_name), root, binary, triple);
        // bin_path is the planned absolute profile-dir path; cwd is the root.
        job.bin_path = bin_abs;
        job.crate_path = root.to_string_lossy().into_owned();
        job
    }

    /// Without the `ANODIZER_IN_DETERMINISM_HARNESS` marker (a plain local
    /// `--snapshot`), the cargo cache MUST be preserved — no pruning.
    #[test]
    fn run_sequential_retains_intermediates_without_harness_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let root = tmp.path();
        let profile = mk_profile(root, "x86_64-unknown-linux-gnu");

        let tools = FakeToolDir::new();
        let job = release_job(&tools, root, "x86_64-unknown-linux-gnu", "app");

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx(); // MapEnvSource without the marker
        run_sequential(&mut ctx, &exec, &[job], &[]).unwrap();

        assert!(
            intermediates_present(&profile),
            "no harness marker → cargo intermediates must be retained"
        );
        assert!(profile.join("app").exists(), "binary must survive");
    }

    /// With the marker present (the hermetic harness rebuild), each triple's
    /// cargo intermediates are freed once its build lands; the binary stays.
    #[test]
    fn run_sequential_prunes_intermediates_under_harness_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let root = tmp.path();
        let profile = mk_profile(root, "x86_64-unknown-linux-gnu");

        let tools = FakeToolDir::new();
        let job = release_job(&tools, root, "x86_64-unknown-linux-gnu", "app");

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        ctx.set_env_source(MapEnvSource::new().with("ANODIZER_IN_DETERMINISM_HARNESS", "1"));
        run_sequential(&mut ctx, &exec, &[job], &[]).unwrap();

        assert!(
            !intermediates_present(&profile),
            "harness marker → cargo intermediates must be freed"
        );
        assert!(
            profile.join("app").exists(),
            "the produced binary must never be removed by the prune"
        );
        assert_eq!(
            ctx.artifacts.by_kind(ArtifactKind::Binary).len(),
            1,
            "artifact still registered after prune"
        );
    }

    /// BLOCKER regression: a workspace-root layout where the per-crate `cwd`
    /// has NO local `target/` so the binary resolves to the workspace-root
    /// `target/<triple>/release/<bin>` (planned RELATIVE ≠ resolved ABSOLUTE).
    /// The reaper must key seed and completion identically (both through
    /// `resolve_binary_path`), so the prune fires. Against the pre-fix code
    /// (seed keyed on planned-relative parent, lookup on resolved-absolute
    /// parent) this lookup MISSES and nothing is freed.
    #[test]
    fn run_sequential_prunes_when_binary_resolves_to_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();

        // Workspace root with a `[workspace]` Cargo.toml and the real
        // target/<triple>/release/ tree (where cargo actually writes).
        let ws_root = tmp.path().join("ws");
        std::fs::create_dir_all(&ws_root).unwrap();
        std::fs::write(
            ws_root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"member\"]\n",
        )
        .unwrap();
        let triple = "x86_64-unknown-linux-gnu";
        let ws_profile = mk_profile(&ws_root, triple);

        // The per-crate dir (member) — its `cwd` has NO local target/, so the
        // planned RELATIVE `target/<triple>/release/app` does not exist here
        // and resolution walks to the workspace-root absolute path.
        let member = ws_root.join("member");
        std::fs::create_dir_all(&member).unwrap();

        // Stub writes the binary at the ABSOLUTE workspace-root location.
        let bin_abs = ws_profile.join("app");
        let tools = FakeToolDir::new();
        tools
            .tool("cargo")
            .creates(bin_abs.to_string_lossy().into_owned(), "x")
            .install();

        // Planned bin_path is RELATIVE (`target/<triple>/release/app`); the cwd
        // for the stub is the member dir. crate_path = member so
        // find_workspace_root walks up to ws_root.
        let mut job = building_job(&tools.tool_path("cargo"), &member, "app", triple);
        job.bin_path = std::path::PathBuf::from("target")
            .join(triple)
            .join("release")
            .join("app");
        job.crate_path = member.to_string_lossy().into_owned();

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        ctx.set_env_source(MapEnvSource::new().with("ANODIZER_IN_DETERMINISM_HARNESS", "1"));
        run_sequential(&mut ctx, &exec, &[job], &[]).unwrap();

        assert!(
            !intermediates_present(&ws_profile),
            "planned≠resolved must still prune the resolved workspace-root profile dir"
        );
        assert!(
            bin_abs.exists(),
            "the resolved binary must survive the prune"
        );
    }

    /// Two jobs share one profile dir (two crates → same triple): the prune
    /// must wait for the LAST job, never freeing `deps/` while a sibling job
    /// still reads it. NON-tautological: a `before-build` hook on job 2 records
    /// whether `deps/` still exists at the moment job 2 begins — if the reaper
    /// wrongly freed after job 1, the sentinel is ABSENT and the assert fires.
    #[test]
    fn run_sequential_shared_triple_prunes_only_after_last_job() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let root = tmp.path();
        let triple = "x86_64-unknown-linux-gnu";
        let profile = mk_profile(root, triple);

        let tools = FakeToolDir::new();
        let job1 = release_job(&tools, root, triple, "app1");
        let mut job2 = release_job(&tools, root, triple, "app2");

        // job2 pre-build hook: assert deps/ is STILL present when job 2 starts.
        // Records a sentinel iff deps/ exists at that instant; if the reaper
        // freed after job1, deps/ is gone and the sentinel is never written.
        let deps_dir = profile.join("deps");
        let sentinel = tmp.path().join("deps_present_at_job2.flag");
        job2.pre_hooks = vec![HookEntry::Structured(StructuredHook {
            cmd: format!(
                "test -d {} && touch {}",
                deps_dir.display(),
                sentinel.display()
            ),
            ..Default::default()
        })];

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        ctx.set_env_source(MapEnvSource::new().with("ANODIZER_IN_DETERMINISM_HARNESS", "1"));
        run_sequential(&mut ctx, &exec, &[job1, job2], &[]).unwrap();

        assert!(
            sentinel.exists(),
            "deps/ must still exist when the second (sibling) job begins — \
             the reaper must NOT free a shared triple after job 1"
        );
        assert!(
            !intermediates_present(&profile),
            "shared triple freed after its last job"
        );
        assert!(profile.join("app1").exists() && profile.join("app2").exists());
    }

    // -------------------------------------------------------------------------
    // run_parallel analogs — exercise the post-chunk prune path
    // -------------------------------------------------------------------------

    /// Without the marker, run_parallel retains the cargo cache.
    #[test]
    fn run_parallel_retains_intermediates_without_harness_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let root = tmp.path();
        let profile = mk_profile(root, "x86_64-unknown-linux-gnu");

        let tools = FakeToolDir::new();
        let job = release_job(&tools, root, "x86_64-unknown-linux-gnu", "app");

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        run_parallel(&mut ctx, &exec, &[job], &[], 2).unwrap();

        assert!(
            intermediates_present(&profile),
            "no harness marker → run_parallel must retain intermediates"
        );
        assert!(profile.join("app").exists());
    }

    /// With the marker, run_parallel prunes each triple after its build lands.
    #[test]
    fn run_parallel_prunes_intermediates_under_harness_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let root = tmp.path();
        let profile = mk_profile(root, "x86_64-unknown-linux-gnu");

        let tools = FakeToolDir::new();
        let job = release_job(&tools, root, "x86_64-unknown-linux-gnu", "app");

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        ctx.set_env_source(MapEnvSource::new().with("ANODIZER_IN_DETERMINISM_HARNESS", "1"));
        run_parallel(&mut ctx, &exec, &[job], &[], 2).unwrap();

        assert!(
            !intermediates_present(&profile),
            "harness marker → run_parallel must free intermediates"
        );
        assert!(profile.join("app").exists());
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Binary).len(), 1);
    }

    /// run_parallel shared triple: two distinct profile dirs (two triples) so
    /// each is freed independently after its own job. (Sharing one dir within
    /// a single parallel chunk would have both jobs racing the same `deps/`;
    /// the per-dir count still guards the prune to the last job, which here is
    /// each dir's sole job.) Both binaries survive; both triples are pruned.
    #[test]
    fn run_parallel_prunes_each_triple_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        let root = tmp.path();
        let t1 = "x86_64-unknown-linux-gnu";
        let t2 = "aarch64-unknown-linux-gnu";
        let p1 = mk_profile(root, t1);
        let p2 = mk_profile(root, t2);

        let tools = FakeToolDir::new();
        let job1 = release_job(&tools, root, t1, "app1");
        let job2 = release_job(&tools, root, t2, "app2");

        let log = mk_log();
        let tvars = TemplateVars::default();
        let exec = mk_exec(&log, &tvars, &dist, "0");
        let mut ctx = mk_ctx();
        ctx.set_env_source(MapEnvSource::new().with("ANODIZER_IN_DETERMINISM_HARNESS", "1"));
        run_parallel(&mut ctx, &exec, &[job1, job2], &[], 2).unwrap();

        assert!(!intermediates_present(&p1), "triple 1 pruned");
        assert!(!intermediates_present(&p2), "triple 2 pruned");
        assert!(p1.join("app1").exists() && p2.join("app2").exists());
    }
}
