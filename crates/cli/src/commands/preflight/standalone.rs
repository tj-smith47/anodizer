//! The standalone `anodizer preflight` command: seeds the version this tree
//! would release, runs the one engine, and prints the report as text or JSON.

use std::path::PathBuf;

use anodizer_core::context::Context;
use anodizer_core::env_preflight::EnvPreflightReport;
use anodizer_core::git::{TagPosition, TagSource};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_stage_publish::reconcile_report::ReconcileRowJson;
use anyhow::Result;

use super::{PreflightScope, run_engine};

pub struct PreflightOpts {
    pub config_override: Option<PathBuf>,
    pub json: bool,
    pub skip: Vec<String>,
    /// `--publishers` allowlist: mirrors `release --publishers` so the
    /// standalone canary can validate the exact publish-time stages a
    /// publisher-scoped release runs (e.g. the npm-provenance job's
    /// `--publishers npm`), including the stages that self-skip when a
    /// publisher is deselected.
    pub publishers: Vec<String>,
    pub publish_only: bool,
    pub token: Option<String>,
    pub quiet: bool,
    pub verbose: bool,
    pub debug: bool,
}

/// Point the context at the version this tree would release.
///
/// A tag the operator declared, or one HEAD carries, already names it. On any
/// other tree the context resolved the LAST release, so the publisher probes
/// would report on a version nobody will publish; the plan `anodizer tag`
/// would cut replaces it. With no release signal the current version stays
/// and the run says so.
///
/// A git failure while planning keeps the current version, the same way
/// [`super::reconcile_sweep`] keeps probing when it cannot place the tag: an
/// unanswerable question must not turn the report into an abort.
///
/// A shallow checkout whose history does not reach the last tag is the one
/// shape that is refused instead. The plan would start from no tag at all and
/// keep the manifest version — the one the LAST release published — and every
/// crate changed since then would probe as `diverged`. That answer is wrong
/// with certainty, so the run stops before any probe and names the remedy
/// ([`shallow_checkout_refusal`]). A shallow clone deep enough to hold the
/// last tag plans the same bump a full clone plans and is not refused, and
/// neither is a shallow checkout of a repository that has never been tagged:
/// the tag is missing from the truncated history, so the push remote's tag
/// listing decides ([`shallow_history_hides_a_tag`]) — a remote holding no
/// tag of this family means there is no last release to reach, and the plan
/// cuts the first version the way a full clone would.
fn seed_planned_version(
    ctx: &mut Context,
    config_override: Option<&std::path::Path>,
    log: &StageLogger,
) -> Result<()> {
    let Some(git_info) = ctx.git_info.as_ref() else {
        return Ok(());
    };
    if git_info.tag_source == TagSource::Declared {
        return Ok(());
    }
    let tag = git_info.tag.clone();
    let root = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let position = anodizer_core::git::tag_position_in(&root, &tag)
        .map_err(|e| format!("could not locate tag {tag} relative to HEAD: {e:#}"));
    // A tag the truncated history still reaches plans correctly; only a tag
    // the history does not reach (or cannot be placed) is the wrong plan.
    if anodizer_core::git::is_shallow_clone_in(&root)
        && shallow_history_hides_a_tag(&root, &tag, &position, log)
    {
        anyhow::bail!(shallow_checkout_refusal());
    }
    let plan = position.and_then(|position| match position {
        TagPosition::AtHead => Ok(None),
        _ => crate::commands::tag::plan_next_version(config_override, log)
            .map_err(|e| format!("could not plan the next version: {e:#}")),
    });
    match plan {
        Ok(None) => {}
        Ok(Some(plan)) => {
            let Ok(semver) = anodizer_core::git::parse_semver_tag(&plan.new_tag) else {
                log.verbose(&format!(
                    "planned tag {} is not semver; publisher probes use the current version {}",
                    plan.new_tag,
                    ctx.version()
                ));
                return Ok(());
            };
            log.verbose(&format!(
                "HEAD is not tagged; publisher probes use the planned version {} ({} → {})",
                plan.new_version,
                if plan.old_tag.is_empty() {
                    "(none)"
                } else {
                    plan.old_tag.as_str()
                },
                plan.new_tag
            ));
            if let Some(git_info) = ctx.git_info.as_mut() {
                git_info.tag = plan.new_tag;
                git_info.semver = semver;
                git_info.previous_tag = (!plan.old_tag.is_empty()).then_some(plan.old_tag);
            }
            ctx.planned_crate_versions = plan.crate_versions;
            ctx.populate_git_vars();
        }
        Err(reason) => log.verbose(&format!(
            "{reason}; publisher probes use the current version {}",
            ctx.version()
        )),
    }
    Ok(())
}

/// Whether a shallow checkout's truncated history is hiding the last tag
/// from the plan.
///
/// A tag at `HEAD` or behind it is reached and plans correctly. A tag that
/// resolves elsewhere, or cannot be placed, is not. A tag that does not
/// resolve at all is either cut off by the truncation or never cut: the
/// push remote's listing tells the two apart, so a remote holding any tag of
/// this tag's family hides one, a remote holding none has no last release to
/// reach, and a listing that fails is treated as hiding one — the plan is
/// refused rather than guessed. A checkout with no push remote plans from
/// its local tags the way `anodizer tag` would.
fn shallow_history_hides_a_tag(
    root: &std::path::Path,
    tag: &str,
    position: &std::result::Result<TagPosition, String>,
    log: &StageLogger,
) -> bool {
    match position {
        Ok(TagPosition::AtHead | TagPosition::AncestorOfHead) => false,
        Ok(TagPosition::UnrelatedToHead) | Err(_) => true,
        Ok(TagPosition::Missing) => {
            let remote = "origin";
            if !anodizer_core::git::has_remote_in(root, remote) {
                return false;
            }
            let family = anodizer_core::git::split_tag_family(tag).map(|(prefix, _)| prefix);
            match anodizer_core::git::list_remote_tag_names_in(root, remote) {
                Ok(names) => names.iter().any(|name| {
                    anodizer_core::git::split_tag_family(name).map(|(prefix, _)| prefix) == family
                }),
                Err(e) => {
                    log.verbose(&format!(
                        "could not list tags on remote '{remote}' ({e}); a shallow checkout                          cannot tell a never-tagged repository from a truncated one"
                    ));
                    true
                }
            }
        }
    }
}

/// The abort for a shallow checkout whose history does not reach the last
/// tag, quoted on `docs/site/content/docs/general/preflight.md`.
pub(crate) fn shallow_checkout_refusal() -> String {
    "preflight: this checkout is shallow and HEAD's history does not reach the last tag, so \
     the version this tree would release cannot be planned; fetch the whole history \
     (actions/checkout fetch-depth: 0, or git fetch --unshallow) and re-run"
        .to_string()
}

/// Standalone `anodizer preflight`: load the config, derive the version this
/// tree would release, run the engine, and exit non-zero when anything is
/// wrong. Same engine the release pipeline runs before any stage.
pub fn run(opts: PreflightOpts) -> Result<()> {
    let log = StageLogger::new(
        "preflight",
        Verbosity::from_flags(opts.quiet, opts.verbose, opts.debug),
    );
    // `observe` keeps the shared init from aborting on what the report
    // itself carries (a missing token) or on the tree's shape (an untagged
    // HEAD). The run mode stays live: a dry-run or snapshot context would
    // make the publisher half skip the crates.io probes and the
    // `cargo publish --dry-run` simulation, and a CI job that passes
    // `--skip=preflight` to the release relies on this command for them.
    let ctx_opts = anodizer_core::context::ContextOptions {
        skip_stages: opts.skip.clone(),
        publisher_allowlist: opts.publishers.clone(),
        token: opts.token.clone(),
        quiet: opts.quiet,
        verbose: opts.verbose,
        debug: opts.debug,
        observe: true,
        ..Default::default()
    };
    let (_config, mut ctx) = crate::commands::helpers::init_merge_stage_ctx(
        opts.config_override.as_deref(),
        ctx_opts,
        &log,
    )?;
    seed_planned_version(&mut ctx, opts.config_override.as_deref(), &log)?;

    let scope = if opts.publish_only {
        PreflightScope::PublishOnly
    } else {
        PreflightScope::Full
    };
    // `--json` owns stdout and keeps stderr to the failures: the engine's
    // status lines go quiet, the error register and any `-v` detail stay.
    let engine_log = if opts.json && !opts.verbose && !opts.debug {
        StageLogger::new("preflight", Verbosity::Quiet)
    } else {
        log.clone()
    };
    let outcome = run_engine(&mut ctx, scope, &engine_log)?;

    if opts.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&PreflightJson {
                environment: &outcome.environment,
                publishers: outcome.publishers.as_ref(),
                reconcile: outcome.reconcile.to_json_rows(),
            })?
        );
    }
    if !outcome.ok() {
        anyhow::bail!(outcome.failure_message());
    }
    Ok(())
}

/// `--json` shape: the environment report's keys stay at the top level (so
/// existing consumers keep reading the same fields) with the publisher
/// report and the reconcile table added alongside as their own keys.
#[derive(serde::Serialize)]
struct PreflightJson<'a> {
    #[serde(flatten)]
    environment: &'a EnvPreflightReport,
    publishers: Option<&'a anodizer_core::preflight::PreflightReport>,
    reconcile: Vec<ReconcileRowJson>,
}
