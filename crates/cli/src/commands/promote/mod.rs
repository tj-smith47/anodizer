//! `anodizer promote` command.
//!
//! Moves an already-published artifact from a pre-release track to a stable
//! track without rebuilding — a cross-publisher capability (snapcraft channels,
//! npm dist-tags, OCI floating tags, GitHub prerelease flips). Promotion is
//! CLI-driven with **no config block**: a static `promote:{from,to}` field would
//! auto-promote the just-uploaded revision on every release run, defeating the
//! candidate gate. Existing publisher config is read only to learn each
//! publisher's native track vocabulary.
//!
//! ```text
//! anodizer promote --to <track> [--from <track>] [--publishers a,b,c] \
//!                  [--version <X> | --from-run <run-id>] [--dry-run]
//! ```

use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::promote::{
    DEFAULT_FROM_TRACK, PROMOTABLE_PUBLISHERS, Promotable, PromoteSelector, dispatch_promotions,
    is_promotion_capable,
};
use anyhow::{Result, bail};
use std::collections::HashSet;
use std::path::PathBuf;

/// Options for `anodizer promote`, one field per CLI flag.
pub struct PromoteOpts {
    /// Destination track (canonical or native). Required.
    pub to: String,
    /// Source track (canonical or native). Defaults to the pre-stable track.
    pub from: Option<String>,
    /// Publisher allowlist (empty = all configured promotion-capable publishers).
    pub publishers: Vec<String>,
    /// Promote this explicit version/tag. Mutually exclusive with `from_run`.
    pub version: Option<String>,
    /// Promote what a prior run recorded. Mutually exclusive with `version`.
    pub from_run: Option<String>,
    /// Resolve and print the plan without running any external command.
    pub dry_run: bool,
    pub config_override: Option<PathBuf>,
    pub verbose: bool,
    pub debug: bool,
    pub quiet: bool,
}

pub fn run(opts: PromoteOpts) -> Result<()> {
    let log = StageLogger::new(
        "promote",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );

    let config_path =
        crate::pipeline::find_config_with_logger(opts.config_override.as_deref(), Some(&log))?;
    let config = crate::pipeline::load_config(&config_path)?;

    let ctx_opts = ContextOptions {
        dry_run: opts.dry_run,
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        ..Default::default()
    };
    let ctx = Context::new(config, ctx_opts);

    // Resolve the selector. `--version` and `--from-run` are mutually exclusive
    // (enforced by clap); default is the newest artifact in the from-track.
    let selector = match (&opts.version, &opts.from_run) {
        (Some(v), _) => PromoteSelector::Version(v.clone()),
        (_, Some(run_id)) => {
            let report = anodizer_stage_publish::load_prior_report(&ctx, run_id)?;
            PromoteSelector::FromRun {
                run_id: run_id.clone(),
                report,
            }
        }
        _ => PromoteSelector::Newest,
    };

    let canonical_from = opts.from.as_deref().unwrap_or(DEFAULT_FROM_TRACK);
    let canonical_to = opts.to.as_str();

    let selected = select_publishers(&ctx, &opts.publishers)?;
    if selected.is_empty() {
        log.status("no promotion-capable publishers configured; nothing to promote");
        return Ok(());
    }

    // Preflight the selected publishers' external tools. Skipped in dry-run,
    // which must run no external command and need no tool installed.
    if !opts.dry_run {
        for p in &selected {
            match p.name() {
                "snapcraft" => anodizer_stage_snapcraft::snapcraft_promote_preflight()?,
                "npm" => anodizer_stage_publish::npm_promote_preflight(&ctx)?,
                "docker" => anodizer_stage_docker::docker_promote_preflight()?,
                "github" => anodizer_stage_release::github_promote_preflight(&ctx)?,
                _ => {}
            }
        }
    }

    let report = dispatch_promotions(&selected, canonical_from, canonical_to, &selector, &ctx);

    for result in &report.results {
        log.status(&result.summary_line());
    }

    if report.any_failure() {
        bail!(
            "{} publisher(s) failed to promote: {}",
            report.failure_names().len(),
            report.failure_names().join(", ")
        );
    }
    Ok(())
}

/// Assemble every configured, promotion-capable publisher as a `Promotable`.
///
/// Each promoter is gated on its publisher being configured, so a project only
/// contributes the promoters it can actually run. Adding a promoter is a config
/// check plus a `Box::new(...)` here (and the publisher's name in
/// [`anodizer_core::promote::PROMOTABLE_PUBLISHERS`]).
fn configured_promotable(ctx: &Context) -> Vec<Box<dyn Promotable>> {
    let mut out: Vec<Box<dyn Promotable>> = Vec::new();
    let crates = ctx.config.crate_universe();

    let has_snap = crates
        .iter()
        .any(|c| c.snapcrafts.as_ref().is_some_and(|s| !s.is_empty()));
    if has_snap {
        out.push(Box::new(anodizer_stage_snapcraft::SnapcraftPromoter));
    }

    // npm is a workspace-level `npms:` block, not per-crate.
    if ctx.config.npms.as_ref().is_some_and(|n| !n.is_empty()) {
        out.push(Box::new(anodizer_stage_publish::NpmPromoter::new(
            npm_pre_dist_tag(ctx),
        )));
    }

    let has_docker = crates
        .iter()
        .any(|c| c.dockers_v2.as_ref().is_some_and(|d| !d.is_empty()));
    if has_docker {
        out.push(Box::new(anodizer_stage_docker::DockerPromoter));
    }

    // The github release promoter is contributed when any crate has a
    // `release.github` block; the verb's preflight re-checks token resolution.
    let has_github_release = crates
        .iter()
        .any(|c| c.release.as_ref().is_some_and(|r| r.github.is_some()));
    if has_github_release {
        out.push(Box::new(anodizer_stage_release::GithubReleasePromoter));
    }

    out
}

/// The npm dist-tag the pre-stable canonical aliases resolve to: the project's
/// configured `npms[].tag` when it names a non-`latest` pre-tag, else `next`.
fn npm_pre_dist_tag(ctx: &Context) -> String {
    ctx.config
        .npms
        .iter()
        .flatten()
        .find_map(|cfg| {
            cfg.tag
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case("latest"))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "next".to_string())
}

/// Resolve the publisher set to promote: all configured+capable by default, or
/// exactly those named in `requested`. A requested name that is not
/// promotion-capable, or is capable but not configured, is a hard error.
fn select_publishers(ctx: &Context, requested: &[String]) -> Result<Vec<Box<dyn Promotable>>> {
    let all = configured_promotable(ctx);
    if requested.is_empty() {
        return Ok(all);
    }

    let configured: HashSet<&str> = all.iter().map(|p| p.name()).collect();
    for name in requested {
        if !is_promotion_capable(name) {
            bail!(
                "publisher '{}' does not support promotion (promotable: {})",
                name,
                PROMOTABLE_PUBLISHERS.join(", ")
            );
        }
        if !configured.contains(name.as_str()) {
            bail!(
                "publisher '{}' supports promotion but is not configured in this project",
                name
            );
        }
    }

    let keep: HashSet<&str> = requested.iter().map(String::as_str).collect();
    Ok(all
        .into_iter()
        .filter(|p| keep.contains(p.name()))
        .collect())
}

#[cfg(test)]
mod tests;
