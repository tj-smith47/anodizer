//! Helpers extracted from `run.rs` to reduce that file's god-function size
//! while keeping behavior identical.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{CrateConfig, HookEntry};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;

use crate::binstall;
use crate::command::BuildCommand;
use crate::universal::{build_universal_binary, project_universal_out_path};
use crate::validation::is_dynamically_linked;
use crate::version_sync;
use crate::workspace::resolve_reproducible_epoch_with_env;

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
