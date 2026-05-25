//! Helpers extracted from `run.rs` to reduce that file's god-function size
//! while keeping behavior identical.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{CrateConfig, HookEntry};
use anodizer_core::context::Context;
use anodizer_core::hooks::run_hooks;
use anodizer_core::log::StageLogger;
use anodizer_core::template::TemplateVars;

use crate::binstall;
use crate::command::BuildCommand;
use crate::universal::{build_universal_binary, project_universal_out_path};
use crate::validation::is_dynamically_linked;
use crate::version_sync;
use crate::workspace::{
    resolve_binary_path, resolve_copy_from, resolve_reproducible_epoch_with_env,
};

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
                        "no_unique_dist_dir: failed to copy {} -> {}",
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
    if artifact_path.exists() && is_dynamically_linked(&artifact_path) {
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
    dry_run: bool,
    log: &StageLogger,
) -> Result<()> {
    let version = ctx
        .template_vars()
        .get("RawVersion")
        .or_else(|| ctx.template_vars().get("Version"))
        .cloned()
        .unwrap_or_default();
    let is_snapshot = ctx.is_snapshot();
    for crate_cfg in crates {
        if let Some(ref vs) = crate_cfg.version_sync
            && vs.enabled.unwrap_or(false)
        {
            if is_snapshot {
                log.verbose(&format!(
                    "version-sync: skipping {} (snapshot mode does not mutate source files)",
                    crate_cfg.path
                ));
            } else if !version.is_empty() {
                version_sync::sync_version(&crate_cfg.path, &version, dry_run, log)?;
            }
        }
        if let Some(ref bs) = crate_cfg.binstall
            && bs.enabled.unwrap_or(false)
        {
            if is_snapshot {
                log.verbose(&format!(
                    "binstall: skipping {} (snapshot mode does not mutate source files)",
                    crate_cfg.path
                ));
            } else {
                binstall::generate_binstall_metadata(&crate_cfg.path, bs, ctx, dry_run)?;
            }
        }
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
                true,
                exec.log,
                Some(exec.template_vars),
            )?;
        }
        if let Some(ref cmd) = job.cmd {
            exec.log
                .status(&format!("(dry-run) {} {}", cmd.program, cmd.args.join(" ")));
        } else if let Some((ref src, ref dst)) = job.copy_from {
            exec.log.status(&format!(
                "(dry-run) copy {} -> {}",
                src.display(),
                dst.display()
            ));
        }
        if !job.post_hooks.is_empty() {
            run_hooks(
                &job.post_hooks,
                "post-build",
                true,
                exec.log,
                Some(exec.template_vars),
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

/// Sequential path: compile each job in-process, register the produced
/// artifact, then drain `copy_jobs` after all source builds complete.
pub(crate) fn run_sequential(
    ctx: &mut Context,
    exec: &BuildExec<'_>,
    build_jobs: &[BuildJob],
    copy_jobs: &[BuildJob],
) -> Result<()> {
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
                false,
                exec.log,
                Some(exec.template_vars),
            )?;
        }

        let cmd = job
            .cmd
            .as_ref()
            .context("build job has no cmd (programmer bug: planner should populate)")?;
        exec.log
            .status(&format!("running: {} {}", cmd.program, cmd.args.join(" ")));
        let output = Command::new(&cmd.program)
            .args(&cmd.args)
            .envs(&cmd.env)
            .current_dir(&cmd.cwd)
            .output()
            .with_context(|| format!("failed to spawn {}", cmd.program))?;
        exec.log.check_output(output, &cmd.program)?;

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
                false,
                exec.log,
                Some(exec.template_vars),
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
                            run_hooks(&pre_hooks, "pre-build", false, &thread_log, Some(&thread_tvars))?;
                        }

                        thread_log.status(&format!("running: {} {}", program, args.join(" ")));
                        let output = Command::new(&program)
                            .args(&args)
                            .envs(&env)
                            .current_dir(&cwd)
                            .output()
                            .with_context(|| format!("failed to spawn {}", program))?;

                        if !output.status.success() {
                            // Redact secrets in stderr/stdout before interpolating
                            // into the bail message.
                            let stderr = thread_log
                                .redact(&String::from_utf8_lossy(&output.stderr));
                            let stdout = thread_log
                                .redact(&String::from_utf8_lossy(&output.stdout));
                            let mut msg = format!(
                                "{} failed with exit code: {}",
                                program,
                                output.status.code().unwrap_or(-1)
                            );
                            if !stderr.is_empty() {
                                msg.push_str(&format!("\nstderr:\n{}", stderr));
                            }
                            if !stdout.is_empty() {
                                msg.push_str(&format!("\nstdout:\n{}", stdout));
                            }
                            anyhow::bail!("{}", msg);
                        }

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
                            run_hooks(&post_hooks, "post-build", false, &thread_log, Some(&thread_tvars))?;
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

        for result in results {
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
