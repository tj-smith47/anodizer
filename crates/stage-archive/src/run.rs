use std::fs;
use std::path::PathBuf;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{ArchiveConfig, FormatOverride};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;
use anyhow::{Context as _, Result};

use crate::ArchiveStage;
use crate::archive_config::archive_one_config;
use crate::run_helpers::{clear_archive_template_vars, validate_archive_configs};

/// Artifact kinds eligible for archiving — bound to the shared selection
/// SSOT's list so this stage and the name-deriving consumers (binstall
/// `pkg_url`, remote installer) can never disagree on what counts.
const ARCHIVABLE_KINDS: &[ArtifactKind] = anodizer_core::archive_selection::ARCHIVABLE_KINDS;

impl Stage for ArchiveStage {
    fn name(&self) -> &str {
        "archive"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("archive");
        let selected = ctx.options.selected_crates.clone();
        let dist = ctx.config.dist.clone();
        let dry_run = ctx.options.dry_run;

        let (global_default_format, global_format_overrides) = resolve_global_archive_defaults(ctx);

        let work = collect_archivable_crates(ctx, &selected)?;

        validate_archive_configs(&work, &log)?;

        fs::create_dir_all(&dist)
            .with_context(|| format!("create dist dir: {}", dist.display()))?;

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let multi_crate = work.len() > 1;

        let original_project_name = ctx
            .template_vars()
            .get("ProjectName")
            .cloned()
            .unwrap_or_else(|| ctx.config.project_name.clone());

        // Capture the loop result rather than `?`-ing inside it: a per-crate
        // failure must still restore the rebound `ProjectName` below before
        // propagating, so the workspace value never leaks past this stage.
        let loop_result: Result<()> = (|| {
            for (crate_name, crate_dir, archive_cfgs) in &work {
                if multi_crate {
                    ctx.template_vars_mut().set("ProjectName", crate_name);
                }

                let all_binaries = collect_crate_archivable_artifacts(ctx, crate_name);

                let has_any_meta = archive_cfgs.iter().any(|cfg| cfg.meta.unwrap_or(false));

                if all_binaries.is_empty() && !has_any_meta {
                    log.skip_line(
                        ctx.options.show_skipped,
                        &format!("skipped archive for crate {crate_name} — no binaries"),
                    );
                    continue;
                }

                archive_one_config(
                    ctx,
                    &log,
                    &dist,
                    dry_run,
                    multi_crate,
                    &global_default_format,
                    &global_format_overrides,
                    archive_cfgs,
                    crate_name,
                    crate_dir,
                    &all_binaries,
                    &mut new_artifacts,
                )?;
            }
            Ok(())
        })();

        ctx.template_vars_mut()
            .set("ProjectName", &original_project_name);
        loop_result?;

        clear_archive_template_vars(ctx);

        // Remove the templated_files staging tree so the rendered scratch
        // files don't persist in dist/ after their contents have already
        // been packed into the archives. Best-effort: the archives are
        // written by now, so a cleanup failure must not fail the stage.
        let staging_root = dist.join(ARCHIVE_TEMPLATED_STAGING_DIR);
        if staging_root.exists()
            && let Err(e) = fs::remove_dir_all(&staging_root)
        {
            log.verbose(&format!(
                "could not remove templated_files staging dir '{}': {e}",
                staging_root.display()
            ));
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

/// Dist-relative directory under which `archives[].templated_files[]`
/// entries are rendered before being packed. Removed at the end of the
/// stage so the scratch files don't persist in dist/.
pub(crate) const ARCHIVE_TEMPLATED_STAGING_DIR: &str = ".archive-templated";

/// Resolve global archive defaults from `defaults.archives`.
/// Returns `(default_format, format_overrides)`.
fn resolve_global_archive_defaults(ctx: &Context) -> (String, Vec<FormatOverride>) {
    let global_default_format = ctx
        .config
        .defaults
        .as_ref()
        .and_then(|d| d.archives.as_ref())
        .and_then(|a| a.formats.as_ref())
        .and_then(|fmts| fmts.first().cloned())
        .unwrap_or_else(|| "tar.gz".to_string());
    let global_format_overrides: Vec<FormatOverride> = ctx
        .config
        .defaults
        .as_ref()
        .and_then(|d| d.archives.as_ref())
        .and_then(|a| a.format_overrides.clone())
        .unwrap_or_default();
    (global_default_format, global_format_overrides)
}

/// Build the list of `(crate_name, crate_dir, archive_configs)` for all
/// crates that have something to archive: configured builds, a meta-archive,
/// or already-registered binary artifacts.
fn collect_archivable_crates(
    ctx: &Context,
    selected: &[String],
) -> Result<Vec<(String, PathBuf, Vec<ArchiveConfig>)>> {
    let project_root = ctx
        .options
        .project_root
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    // Selection AND per-crate config shaping are delegated to the shared
    // SSOT so the name-deriving consumers (binstall `pkg_url`, remote
    // installer) count the exact same work list this stage processes, and so
    // this stage never re-encodes the SSOT's `Disabled` exclusion.
    Ok(anodizer_core::archive_selection::archive_producing_crates(
        &ctx.config,
        &ctx.artifacts,
        selected,
    )
    .into_iter()
    .map(|c| {
        (
            c.name.clone(),
            project_root.join(&c.path),
            anodizer_core::archive_selection::effective_archive_configs(c),
        )
    })
    .collect())
}

/// Pick the host-native binary artifact from a crate's binaries, for mode-A
/// completion/man generation (running the binary requires it execute on the
/// host). Matches by exact target triple against the detected host target.
///
/// Returns `None` when host detection fails (e.g. `rustc` unavailable) or no
/// built artifact targets the host — a pure cross build. The generation layer
/// turns that `None` into a clear, actionable error for mode A while leaving
/// modes B/C (which don't run the binary) unaffected.
pub(crate) fn resolve_host_binary(all_binaries: &[Artifact]) -> Option<&Artifact> {
    let host = anodizer_core::partial::detect_host_target().ok()?;
    all_binaries
        .iter()
        .find(|b| b.target.as_deref() == Some(host.as_str()))
}

/// Collect all archivable binary artifacts for a single crate.
fn collect_crate_archivable_artifacts(ctx: &Context, crate_name: &str) -> Vec<Artifact> {
    ctx.artifacts
        .by_kinds_and_crate(ARCHIVABLE_KINDS, crate_name)
        .into_iter()
        .cloned()
        .collect()
}
