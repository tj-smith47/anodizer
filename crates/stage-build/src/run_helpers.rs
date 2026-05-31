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
    /// Fully-rendered per-(crate, target) `builds[].env` map for THIS job.
    /// Layered into this job's build hooks beneath each hook's own `env:`,
    /// matching GoReleaser's build-hook env precedence. Empty when the build
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
    let is_snapshot = ctx.is_snapshot();
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
        // the next. Snapshot mode skips mutation entirely, so no git lookup
        // (and no re-scope) happens there.
        //
        // A mutation-enabled crate with no resolvable per-crate tag/version is
        // a fail-loud error, never a silent fall-back to the first crate's
        // vars: stamping crate B with crate A's version (or rendering B's
        // binstall URL with A's version) ships a wrong, hard-to-spot artifact.
        let saved: Vec<(&'static str, Option<String>)> = if is_snapshot {
            Vec::new()
        } else {
            let tag = resolve_tag(ctx, crate_cfg).with_context(|| {
                format!(
                    "crate '{}' is selected for version-sync/binstall but has no \
                     release tag matching its tag_template; cannot derive its version",
                    crate_cfg.name
                )
            })?;
            let overrides = crate_template_overrides(&crate_cfg.name, &tag)?;
            apply_var_overrides(ctx, &overrides)
        };

        let result = (|| -> Result<()> {
            if let Some(ref vs) = crate_cfg.version_sync
                && vs.enabled.unwrap_or(false)
            {
                if is_snapshot {
                    log.verbose(&format!(
                        "version-sync: skipping {} (snapshot mode does not mutate source files)",
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
                if is_snapshot {
                    log.verbose(&format!(
                        "binstall: skipping {} (snapshot mode does not mutate source files)",
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
                true,
                exec.log,
                Some(exec.template_vars),
                Some(&job.build_env),
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
                Some(&job.build_env),
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
                Some(&job.build_env),
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
                Some(&job.build_env),
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
                            run_hooks(&pre_hooks, "pre-build", false, &thread_log, Some(&thread_tvars), Some(&build_env))?;
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
                            run_hooks(&post_hooks, "post-build", false, &thread_log, Some(&thread_tvars), Some(&build_env))?;
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

#[cfg(test)]
mod source_mutation_tests {
    use super::*;
    use anodizer_core::config::{BinstallConfig, Config, CrateConfig, VersionSyncConfig};
    use anodizer_core::context::{Context, ContextOptions};

    fn crate_cfg(name: &str, path: &str, tag_template: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: path.to_string(),
            tag_template: tag_template.to_string(),
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
        let mut ctx = Context::new(config, ContextOptions::default());
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
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?} failed to spawn: {e}"));
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
