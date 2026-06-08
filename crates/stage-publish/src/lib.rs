// Must appear before any module that uses `simple_publisher!` because
// `#[macro_use]` imports macros from this module into the crate-root
// namespace only for siblings that come AFTER it textually.
#[macro_use]
pub(crate) mod publisher_helpers;

pub mod artifactory;
pub mod aur;
pub mod aur_source;
pub mod cargo;
pub mod chocolatey;
pub mod cloudsmith;
pub mod dispatch;
pub mod dockerhub;
pub mod gemfury;
pub mod homebrew;
pub(crate) mod http_upload;
pub mod krew;
pub mod mcp;
pub mod nix;
pub mod npm;
pub mod post_publish;
pub mod preflight;
pub mod registry;
pub mod rollback;
pub mod rollback_only;
pub mod run_summary;
pub mod schema_validation;
pub(crate) mod schemastore;
pub mod scoop;
pub(crate) mod scope;
pub(crate) mod snapshot_validation;
pub(crate) mod util;
pub mod winget;

/// Test-support module: `FakePublisher`, `FakeOutcome`, etc.
///
/// Gated behind the `test-support` Cargo feature (and `cfg(test)` for
/// the in-crate unit tests). The feature is enabled by this crate's own
/// `[dev-dependencies]` so integration tests under `tests/` can drive
/// the same fakes the in-crate unit tests use.
///
/// NOT a stable public API — shape may change without notice. External
/// consumers outside this workspace MUST NOT rely on it.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod testing;

pub use dispatch::{DispatchOptions, dispatch};
pub use registry::{configured_publishers, group_dispatch_order};
pub use schema_validation::{TagResolver, validate_publisher_schemas};

use anodizer_core::config::PublishConfig;
use anodizer_core::context::{Context, RollbackMode};
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;
use anodizer_core::{Publisher, PublisherGroup, PublisherOutcome, SkipReason};
use anyhow::Result;
use std::path::PathBuf;

/// Derive a stable per-run identifier suitable for the
/// `<dist>/run-<id>/` directory written by [`write_report_to_run_dir`]
/// and read back by [`rollback_only::run`].
///
/// Priority order:
/// 1. `ctx.git_info.tag` — what the operator typed (e.g. `v0.2.1`).
///    Naturally fits the `[A-Za-z0-9._-]` shape expected by
///    [`rollback_only::validate_run_id`].
/// 2. `ctx.git_info.short_commit` — fallback for snapshot / dry-run /
///    detached-HEAD scenarios where there's no tag.
/// 3. The literal `"local"` — final fallback for genuinely-no-git
///    contexts (e.g. some integration tests).
///
/// All three branches return a string that satisfies
/// [`rollback_only::validate_run_id`]; the candidates from `git_info`
/// are pre-filtered against the validator so a malformed value (e.g. a
/// short_commit somehow containing slashes) falls through to the next
/// step instead of producing an invalid path. The `"local"` literal is
/// fixed and always valid.
///
/// Operators replaying a `--from-run=local` invocation are addressing
/// the no-git fallback path; for production releases (which always
/// have a tag or short_commit), this branch should never fire. Seeing
/// `dist/run-local/` in a real release is a signal that
/// `ctx.git_info` was not populated upstream and is worth
/// investigating before invoking `--rollback-only`.
///
/// Used by [`PublishStage::run`] before writing
/// `<dist>/run-<id>/report.json`. The write is skipped entirely in
/// snapshot / dry-run mode, so callers that derive the id outside of
/// the write path should also gate on `ctx.is_snapshot()` /
/// `ctx.is_dry_run()` if they want the same behavior.
pub fn derive_run_id(ctx: &Context) -> String {
    if let Some(info) = ctx.git_info.as_ref() {
        if !info.tag.is_empty() && rollback_only::validate_run_id(&info.tag).is_ok() {
            return info.tag.clone();
        }
        if !info.short_commit.is_empty()
            && rollback_only::validate_run_id(&info.short_commit).is_ok()
        {
            return info.short_commit.clone();
        }
    }
    "local".to_string()
}

/// Resolve `<dist>/run-<id>/report.json` for a derived `run_id`. Pure
/// path helper kept alongside [`derive_run_id`] so consumers driving
/// the announce-only flow share the same path-shape contract as the
/// writer in [`write_report_to_run_dir`] and the reader in
/// [`rollback_only::run`].
pub fn report_path_for(ctx: &Context, run_id: &str) -> PathBuf {
    ctx.config
        .dist
        .join(format!("run-{}", run_id))
        .join("report.json")
}

/// Load the prior run's `<dist>/run-<id>/report.json` into a
/// [`anodizer_core::publish_report::PublishReport`].
///
/// Errors when the file is missing or unparseable. The recovery hint
/// mirrors the message [`rollback_only::run`] produces because the two
/// share the same `dist/run-<id>/` contract.
pub fn load_prior_report(
    ctx: &Context,
    run_id: &str,
) -> Result<anodizer_core::publish_report::PublishReport> {
    use anyhow::Context as _;
    let path = report_path_for(ctx, run_id);
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "no prior report found at {} (run_id={}). The announce-only \
             flow consumes a `report.json` written by a successful prior \
             release run; re-run `anodize release` end-to-end first or \
             pass `--from-run=<id>` to point at an existing run dir.",
            path.display(),
            run_id,
        )
    })?;
    serde_json::from_str(&raw).with_context(|| {
        format!(
            "failed to parse prior report at {} (run_id={})",
            path.display(),
            run_id,
        )
    })
}

/// Persist `ctx.publish_report` to `<config.dist>/run-<run_id>/report.json`
/// so a later `--rollback-only --from-run=<run_id>` invocation can
/// re-attempt rollback against the same run.
///
/// Best-effort: any IO / serialization failure is logged as a warn and
/// returns `Ok(())`. The write does NOT fail the pipeline — the release
/// itself isn't affected by a missing on-disk replay surface.
///
/// Skipped (no-op) when:
/// - `ctx.is_snapshot()` or `ctx.is_dry_run()` — these modes are not
///   real releases and shouldn't pollute `dist/run-*/`.
/// - `report.results.is_empty()` — no work was done; an empty file
///   just clutters `dist/`. BlobStage / SnapcraftPublishStage append to
///   `publish_report` independently, so the empty-check correctly
///   covers "no work done at all."
///
/// Mirrors the path-derivation in
/// [`rollback_only::report_path`] — the two helpers form the
/// writer/reader contract for the `--rollback-only` flow.
///
/// Pretty-prints so operators reading the file directly do not have
/// to pipe through `jq`. A future contributor tempted to switch to
/// compact JSON for byte-size should know the format is part of the
/// operator-facing contract — `--rollback-only` consumers may read it
/// by hand to triage which step failed before invoking the replay.
///
/// # Atomicity
///
/// Serializes to a `String` first via [`serde_json::to_string_pretty`]
/// and then commits with [`std::fs::write`]. A serialize-failure
/// (closed sums, malformed Unicode in a publisher name, ...) happens
/// BEFORE the target file is touched, so a partially-written /
/// truncated `report.json` cannot leak onto disk and trip a later
/// `--rollback-only --from-run=<id>` parse error. Matches the sibling
/// pattern in `crates/stage-publish/src/run_summary.rs::write_summary_json`.
///
/// No explicit `fsync`: `fs::write` does not expose a sync hook, and
/// the in-repo convention (run_summary, every other JSON-emit site
/// across the stages) does not fsync either. The atomic-rename-style
/// safety we get from `fs::write` covers the truncation hazard that
/// motivated the change; a crash mid-`fs::write` is rare enough on
/// modern filesystems that the divergence from in-repo convention
/// isn't worth carrying.
///
/// # Single-`()`-return shape
///
/// Combines serialize + observe into a single `()`-returning helper;
/// the sibling `stage-announce::emit_summary` (writer of
/// `summary.json`) splits these for testability. The shape is
/// preserved single-call-site here intentionally — the only consumer
/// is `PublishStage::run`, and the warn-don't-fail policy means there
/// is no error to thread up.
///
/// # Retention
///
/// This writer creates one `dist/run-<id>/` directory per release
/// run; the pipeline does NOT auto-prune. Operators own retention and
/// should periodically clean stale run directories — they hold the
/// only on-disk state needed for `--rollback-only --from-run=<id>`
/// replay, so a deletion is recoverable only by re-running the
/// release.
///
/// # Stability
///
/// This function is `pub` + `#[doc(hidden)]` only so the in-crate
/// integration test (`tests/run_report_persistence.rs`) can drive the
/// production writer without re-implementing it. It is **not** part of
/// the public API surface — downstream crates must invoke
/// `PublishStage` via the `Stage` trait, which calls this writer
/// internally at end-of-pipeline.
#[doc(hidden)]
pub fn write_report_to_run_dir(ctx: &Context, log: &StageLogger) {
    if ctx.is_snapshot() || ctx.is_dry_run() {
        return;
    }
    let Some(report) = ctx.publish_report() else {
        return;
    };
    if report.results.is_empty() {
        return;
    }

    let run_id = derive_run_id(ctx);
    // Defense-in-depth: derive_run_id is supposed to always return a
    // valid id, but a future refactor could regress that invariant. If
    // the id is bad, skip the write rather than write to an invalid
    // path — the operator loses replay; the release is unaffected.
    if let Err(e) = rollback_only::validate_run_id(&run_id) {
        log.warn(&format!(
            "publish: skipped run-report write — derived run_id '{}' failed validation: {}",
            run_id, e,
        ));
        return;
    }

    let path = report_path_for(ctx, &run_id);
    let dir: PathBuf = ctx.config.dist.join(format!("run-{}", run_id));

    if let Err(e) = std::fs::create_dir_all(&dir) {
        log.warn(&format!(
            "publish: failed to create run-report dir {}: {}",
            dir.display(),
            e,
        ));
        return;
    }

    // Serialize first, then write — so a serialize-failure cannot
    // leave a truncated/corrupt file on disk for the rollback_only
    // reader to choke on. Matches `run_summary::write_summary_json`.
    let text = match serde_json::to_string_pretty(report) {
        Ok(t) => t,
        Err(e) => {
            log.warn(&format!(
                "publish: failed to serialize run-report for {}: {}",
                path.display(),
                e,
            ));
            return;
        }
    };

    if let Err(e) = anodizer_core::fs_atomic::atomic_write_str(&path, &text) {
        log.warn(&format!(
            "publish: failed to write run-report to {}: {}",
            path.display(),
            e,
        ));
        return;
    }

    log.status(&format!("publish: wrote run-report to {}", path.display()));
}

/// Collect crate names that match the selection filter and have a specific
/// publisher configured (as determined by the predicate `has_config`).
///
/// Walks the same crate universe as `cargo.rs::publish_to_cargo` —
/// `ctx.config.crates` plus every `ctx.config.workspaces[].crates` —
/// so a workspace-only crate carrying a non-cargo publisher block
/// (`homebrew:`, `scoop:`, `aur:`, ...) is dispatched alongside the
/// crates from the top-level list. Without this, cargo would publish
/// the workspace crate but every other publisher would silently skip
/// it. See `util::all_crates` for the dedup rule.
fn crates_with_publisher<F>(ctx: &Context, selected: &[String], has_config: F) -> Vec<String>
where
    F: Fn(&PublishConfig) -> bool,
{
    util::all_crates(ctx)
        .into_iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.name))
        .filter(|c| c.publish.as_ref().is_some_and(&has_config))
        .map(|c| c.name)
        .collect()
}

/// Build the post-publish polling job list from the active context and run
/// every job in parallel. Writes typed `PostPublishResult` entries (as JSON
/// values) into `ctx.stage_outputs.post_publish_results` for the deferred
/// release-summary renderer to consume.
///
/// Eligibility rules:
///
/// - The publish stage must NOT be in dry-run / snapshot mode (gated at
///   the call site — nothing was actually pushed in those modes).
/// - Chocolatey jobs require `--skip=choco` to be absent AND a per-crate
///   `chocolatey:` block with `post_publish_poll.enabled != false`.
/// - WinGet jobs require `--skip=winget` to be absent AND a per-crate
///   `winget:` block with `post_publish_poll.enabled != false`.
/// - `--no-post-publish-poll` short-circuits to a `NotPolled` result per
///   eligible publisher (so the release summary can render "skipped"
///   distinctly from "no publishers configured").
///
/// All polling is non-fatal; any worker error becomes a
/// `PostPublishStatus::Error` in the results vec rather than failing the
/// publish stage.
fn run_post_publish_pollers(ctx: &mut Context, selected: &[String], log: &StageLogger) {
    let version = ctx.version();
    let mut jobs: Vec<post_publish::PollJob> = Vec::new();
    // Mirrors `jobs` for the skip-path: when the CLI flag is set we
    // never construct a `PollJob` (no cfg / no URL / no token needed),
    // but we DO want to emit a `NotPolled` result per configured
    // publisher so summaries can render "skipped via flag" vs. "no
    // publishers configured" distinctly. `(publisher, package, version)`
    // triples are collected in dispatch order to match the result vec
    // ordering invariant.
    let mut skipped: Vec<(&'static str, String, String)> = Vec::new();
    let skip_via_cli = ctx.options.skip_post_publish_poll;

    // Chocolatey eligibility — collect a job per per-crate `chocolatey:`
    // block when the `choco` skip isn't engaged.
    if !ctx.should_skip("choco") {
        for crate_name in
            &crates_with_publisher(ctx, selected, |p: &PublishConfig| p.chocolatey.is_some())
        {
            let cfg_opt = util::all_crates(ctx)
                .into_iter()
                .find(|c| &c.name == crate_name)
                .and_then(|c| c.publish)
                .and_then(|p| p.chocolatey);
            let Some(choco) = cfg_opt else {
                continue;
            };
            // Per-publisher `enabled: false` opts a publisher out
            // *entirely* (not the same surface as `--no-post-publish-poll`,
            // which is a global skip). Detect that here so the skip-path
            // doesn't emit `NotPolled` for a publisher the operator
            // explicitly turned off in config (which the renderer would
            // otherwise misreport as "skipped via flag").
            let per_pub_cfg = choco.post_publish_poll.unwrap_or_default();
            if !per_pub_cfg.enabled {
                continue;
            }
            let pkg_name = choco.name.unwrap_or_else(|| crate_name.clone());
            if skip_via_cli {
                skipped.push(("chocolatey", pkg_name, version.clone()));
                continue;
            }
            // `resolve_poll_config` collapses both gates (CLI + per-pub)
            // into one `Option`. We've already filtered the per-pub
            // `enabled` case, so a `None` here can only mean the CLI
            // flag — caught by the `skip_via_cli` branch above.
            let Some(poll_cfg) = post_publish::resolve_poll_config(ctx, choco.post_publish_poll)
            else {
                continue;
            };
            jobs.push(post_publish::PollJob::Chocolatey {
                package: pkg_name,
                version: version.clone(),
                page_base_url: "https://community.chocolatey.org".to_string(),
                cfg: poll_cfg,
            });
        }
    }

    // WinGet eligibility — same pattern. The PR is rediscovered via the
    // GitHub search API (mirroring `preflight::Winget`), so we don't need
    // to thread a PR URL through from the publish step.
    if !ctx.should_skip("winget") {
        for crate_name in
            &crates_with_publisher(ctx, selected, |p: &PublishConfig| p.winget.is_some())
        {
            let cfg_opt = util::all_crates(ctx)
                .into_iter()
                .find(|c| &c.name == crate_name)
                .and_then(|c| c.publish)
                .and_then(|p| p.winget);
            let Some(winget) = cfg_opt else {
                continue;
            };
            // Per-publisher disable check — same rationale as the
            // chocolatey arm above.
            let per_pub_cfg = winget.post_publish_poll.unwrap_or_default();
            if !per_pub_cfg.enabled {
                continue;
            }
            // PackageIdentifier resolution: prefer explicit
            // `package_identifier`, fall back to `<publisher>.<name>`
            // (the upstream convention enforced by winget validation),
            // then to the crate name as a last resort.
            let pkg_id = winget.package_identifier.clone().unwrap_or_else(|| {
                let publisher = winget.publisher.as_deref().unwrap_or("");
                let name = winget
                    .name
                    .as_deref()
                    .or(winget.package_name.as_deref())
                    .unwrap_or(crate_name);
                if publisher.is_empty() {
                    name.to_string()
                } else {
                    format!("{}.{}", publisher, name)
                }
            });
            if skip_via_cli {
                skipped.push(("winget", pkg_id, version.clone()));
                continue;
            }
            let Some(poll_cfg) = post_publish::resolve_poll_config(ctx, winget.post_publish_poll)
            else {
                continue;
            };
            let token = winget
                .repository
                .as_ref()
                .and_then(|r| r.token.clone())
                .or_else(|| ctx.env_var("ANODIZER_GITHUB_TOKEN"))
                .or_else(|| ctx.env_var("GITHUB_TOKEN"));
            jobs.push(post_publish::PollJob::Winget {
                package_identifier: pkg_id,
                version: version.clone(),
                api_base_url: "https://api.github.com".to_string(),
                token,
                cfg: poll_cfg,
            });
        }
    }

    // Skip-path: emit one `NotPolled` per eligible publisher so the
    // release summary distinguishes "skipped via --no-post-publish-poll"
    // from "no eligible publishers". Short-circuits without running any
    // pollers.
    if skip_via_cli {
        if skipped.is_empty() {
            log.verbose(
                "post-publish polling: skipped via --no-post-publish-poll (no eligible publishers)",
            );
            return;
        }
        log.verbose(&format!(
            "post-publish polling: skipped via --no-post-publish-poll ({} publisher(s) recorded as NotPolled)",
            skipped.len()
        ));
        let not_polled: Vec<post_publish::PostPublishResult> = skipped
            .into_iter()
            .map(
                |(publisher, package, version)| post_publish::PostPublishResult {
                    publisher: publisher.to_string(),
                    package,
                    version,
                    status: post_publish::PostPublishStatus::NotPolled,
                },
            )
            .collect();
        ctx.stage_outputs.post_publish_results = not_polled
            .iter()
            .map(|r| {
                serde_json::to_value(r).expect(
                    "PostPublishResult is always serializable — schema is derived from a string + enum struct",
                )
            })
            .collect();
        return;
    }

    if jobs.is_empty() {
        log.verbose("post-publish polling: no eligible publishers");
        return;
    }
    log.status(&format!(
        "post-publish polling: starting {} parallel poller(s)",
        jobs.len()
    ));
    let results = post_publish::run_post_publish_polls(jobs, log);
    for r in &results {
        match &r.status {
            post_publish::PostPublishStatus::Approved { detail } => log.status(&format!(
                "post-publish: {} {} {} approved: {}",
                r.publisher, r.package, r.version, detail
            )),
            post_publish::PostPublishStatus::Rejected { detail } => log.warn(&format!(
                "post-publish: {} {} {} rejected: {}",
                r.publisher, r.package, r.version, detail
            )),
            post_publish::PostPublishStatus::Timeout { last_state, .. } => log.warn(&format!(
                "post-publish: {} {} {} polling timed out (last state: {})",
                r.publisher, r.package, r.version, last_state
            )),
            post_publish::PostPublishStatus::Error { reason } => log.warn(&format!(
                "post-publish: {} {} {} polling error: {}",
                r.publisher, r.package, r.version, reason
            )),
            post_publish::PostPublishStatus::Pending { .. }
            | post_publish::PostPublishStatus::NotPolled => {
                // Pending shouldn't reach this path (poller loops until
                // terminal). NotPolled is built by callers that explicitly
                // opt out — silent is fine.
            }
        }
    }
    ctx.stage_outputs.post_publish_results = results
        .into_iter()
        .map(|r| {
            serde_json::to_value(&r).expect(
                "PostPublishResult is always serializable — schema is derived from a string + enum struct",
            )
        })
        .collect();
}

/// Run best-effort rollback when the trigger conditions are met:
///
/// 1. At least one required Assets or Manager publisher failed, AND
/// 2. `ctx.options.rollback_mode != Some(RollbackMode::None)`.
///
/// When `ctx.options.rollback_mode` is `None`, this defaults to
/// `RollbackMode::BestEffort` so the rollback path engages by default.
/// The `--rollback=none` CLI flag plumbs `Some(RollbackMode::None)` here
/// to suppress rollback entirely.
///
/// The function takes `ctx.publish_report` out, mutates it via
/// `rollback::run`, and writes it back. Doing the dance here keeps the
/// `&mut Context` borrow scope tight: `rollback::run` needs mutable
/// access to `ctx` (each publisher's `rollback()` is `&mut Context`),
/// which would otherwise conflict with an active `&mut PublishReport`
/// borrow through `ctx.publish_report`.
/// Whether any required Submitter publisher *failed* yet still has a
/// non-empty programmatic rollback to perform (per
/// [`Publisher::programmatic_rollback_on_failure`]).
///
/// The motivating case is cargo: a multi-crate `cargo publish` that
/// succeeds on crate A then fails on crate B records A in its evidence and
/// lands on a `Failed` Submitter row. The default rollback trigger only
/// considers Assets/Manager failures, so this predicate is what arms the
/// machinery to yank A. A failed Submitter with an empty record (nothing
/// went live) does not arm rollback.
fn has_failed_submitter_with_programmatic_rollback(
    report: &anodizer_core::PublishReport,
    publishers: &[Box<dyn Publisher>],
) -> bool {
    report.results.iter().any(|r| {
        r.group == PublisherGroup::Submitter
            && r.required
            && matches!(r.outcome, PublisherOutcome::Failed(_))
            && r.evidence.as_ref().is_some_and(|ev| {
                publishers
                    .iter()
                    .find(|p| p.name() == r.name)
                    .is_some_and(|p| p.programmatic_rollback_on_failure(ev))
            })
    })
}

fn run_rollback_if_needed(ctx: &mut Context, publishers: &[Box<dyn Publisher>], log: &StageLogger) {
    let mode = ctx
        .options
        .rollback_mode
        .unwrap_or(RollbackMode::BestEffort);
    if mode == RollbackMode::None {
        return;
    }

    let needs_rollback = ctx.publish_report.as_ref().is_some_and(|r| {
        r.any_failed(PublisherGroup::Assets, true)
            || r.any_failed(PublisherGroup::Manager, true)
            // A failed required Submitter (cargo) that already pushed one or
            // more crates to crates.io carries a non-empty programmatic
            // rollback set in its evidence. Arm rollback so those live crates
            // get yanked — the Assets/Manager-only gate would otherwise leave
            // a partial multi-crate publish stranded on the registry.
            || has_failed_submitter_with_programmatic_rollback(r, publishers)
    });
    if !needs_rollback {
        return;
    }

    log.status("rollback: required failure(s) detected; invoking best-effort rollback");

    // Take the report out so `rollback::run` can mutate it while
    // calling `publisher.rollback(ctx, ...)` (which itself needs
    // `&mut Context`). We unconditionally write it back below.
    let Some(mut report) = ctx.publish_report.take() else {
        // Defensive: `needs_rollback` was true above, so the Option
        // must have been `Some`. If a future refactor changes that
        // invariant, log and bail rather than panic.
        log.warn("rollback: publish_report missing; nothing to dispatch");
        return;
    };
    rollback::run(publishers, &mut report, ctx, mode);
    ctx.set_publish_report(report);
}

/// Refuse to re-run `PublishStage::run` when a prior end-of-pipeline
/// `report.json` already exists for the current `run_id`. Returns
/// `Ok(())` on first run (no report yet), when `--allow-rerun` is
/// set, or under any of the skip conditions documented at the call
/// site (snapshot / dry-run / rollback-only / run_id == "local").
fn refuse_rerun_if_report_exists(ctx: &Context) -> Result<()> {
    if ctx.is_snapshot() || ctx.is_dry_run() {
        return Ok(());
    }
    if ctx.options.rollback_only {
        return Ok(());
    }
    if ctx.options.allow_rerun {
        return Ok(());
    }

    let run_id = derive_run_id(ctx);
    // The "local" id is the no-git fallback; enforcing the guard
    // against it would create false positives across unrelated
    // `cargo test` runs in CI (they all derive the same id). Only
    // enforce when the id is a real tag or short_commit.
    if run_id == "local" {
        return Ok(());
    }

    let report_path = report_path_for(ctx, &run_id);
    if report_path.exists() {
        anyhow::bail!(
            "publish refusing to run: a prior report.json exists at {} (run_id={}). \
             To recover from a partial failure, run \
             `anodizer release --rollback-only --from-run={}` first (this reverts \
             reversible publishers and is idempotent). Pass --allow-rerun to force \
             re-publish anyway — WARNING: PR-based publishers (homebrew, scoop, nix, \
             krew, MCP) will open DUPLICATE pull requests against the same tag.",
            report_path.display(),
            run_id,
            run_id,
        );
    }
    Ok(())
}

/// Verify every `--allow-nondeterministic <name>=<reason>` entry
/// matches at least one artifact emitted by the build-side pipeline.
/// Glob entries (`*.ext`) match by suffix; bare names match exactly.
///
/// Called at the top of [`PublishStage::run`] so the run errors out
/// BEFORE any publisher fires. An unmatched name almost always
/// signifies an operator typo — silently letting it through would
/// produce a release with an exemption notice that doesn't apply to
/// anything, undermining the audit trail.
fn validate_runtime_allowlist(ctx: &Context) -> Result<()> {
    let entries = &ctx.options.runtime_nondeterministic_allowlist;
    if entries.is_empty() {
        return Ok(());
    }
    let artifact_names: Vec<&str> = ctx
        .artifacts
        .all()
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    // Also match against the basename of `artifact.path`: the spec
    // encourages operators to type `*.crate` / `*.deb` (file-extension
    // patterns), but `artifact.name` is whatever the build stage
    // recorded and is not always the on-disk filename. Matching both
    // surfaces means a `*.crate` glob hits whichever of
    // `artifact.name` ("anodize-v0.2.1") or
    // `basename(artifact.path)` ("anodize-v0.2.1.crate") satisfies
    // the pattern.
    let artifact_pathnames: Vec<String> = ctx
        .artifacts
        .all()
        .iter()
        .filter_map(|a| a.path.file_name().map(|f| f.to_string_lossy().into_owned()))
        .collect();
    let mut unmatched: Vec<&str> = Vec::new();
    for (name, _reason) in entries {
        let matched = artifact_names
            .iter()
            .any(|n| matches_artifact_pattern(name, n))
            || artifact_pathnames
                .iter()
                .any(|n| matches_artifact_pattern(name, n.as_str()));
        if !matched {
            unmatched.push(name.as_str());
        }
    }
    if !unmatched.is_empty() {
        anyhow::bail!(
            "--allow-nondeterministic name(s) did not match any emitted artifact: {} \
             (check spelling; use `*.ext` glob for suffix match)",
            unmatched.join(", ")
        );
    }
    Ok(())
}

/// Glob match: `*.ext` is suffix-match; anything else is exact-match.
/// Same semantics as `DeterminismState::resolve_reason` (kept local
/// here to avoid exposing the core helper publicly).
fn matches_artifact_pattern(pattern: &str, artifact: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        return artifact.ends_with(suffix);
    }
    pattern == artifact
}

/// Validates the anodize-only emissions (binstall, nix, version-sync) in
/// snapshot / dry-run mode, where the real-release stages skip their source
/// mutations and remote pushes.
///
/// In a real release this stage is a no-op — the actual emission stages run
/// and the published output is the source of truth. In snapshot/dry-run it
/// renders each emission in-memory and cross-checks it against the assets the
/// run produced, so a broken emission (a 404-class binstall `pkg_url`, a nix
/// system mapped to a missing asset, a crate with no resolvable version) fails
/// LOCALLY instead of on a consumer's `cargo binstall` / `nix build`.
///
/// Placed after the packaging + checksum stages so `ctx.artifacts` carries the
/// archive set the cross-checks compare against, and before the publishers so
/// a broken emission aborts the snapshot before any (skipped-anyway) publish
/// work is reported as green.
pub struct EmissionValidateStage;

impl Stage for EmissionValidateStage {
    fn name(&self) -> &str {
        "emission-validate"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("publish");
        snapshot_validation::validate_snapshot_emissions(ctx, &log)
    }
}

pub struct PublishStage;

impl PublishStage {
    /// Core of `Stage::run`, factored out so tests can substitute an
    /// arbitrary `&[Box<dyn Publisher>]` registry. `Stage::run` calls
    /// this with `registry::configured_publishers(ctx)`.
    ///
    /// The body invokes the group-aware dispatcher (Assets -> Manager
    /// -> Submitter, with Submitter gating), writes the resulting
    /// `PublishReport` to `ctx.publish_report`, and returns `Ok(())`
    /// even on per-publisher failure — those failures are recorded in
    /// the report. `Err` is reserved for catastrophic non-publisher
    /// errors (impossible IO, malformed config); for now `dispatch`
    /// itself never returns `Err`.
    ///
    /// # Stability
    ///
    /// This function is `pub` + `#[doc(hidden)]` so the in-crate
    /// `#[cfg(test)] mod tests` block AND the cross-crate integration
    /// test `crates/stage-publish/tests/run_report_persistence.rs` can
    /// substitute a synthetic publisher slice. It is **not** part of
    /// the public API surface: `#[doc(hidden)]` marks that downstream
    /// crates must not couple to this signature; production consumers
    /// should invoke `<PublishStage as Stage>::run` instead. The
    /// integration test depends on this seam by design (writer/reader
    /// contract for `report.json`), so visibility cannot tighten to
    /// `pub(crate)` without breaking that test.
    #[doc(hidden)]
    pub fn run_with_publishers(
        ctx: &mut Context,
        log: &StageLogger,
        publishers: &[Box<dyn anodizer_core::Publisher>],
    ) -> Result<()> {
        let opts = dispatch::DispatchOptions {
            fail_fast: ctx.options.fail_fast,
            gate_submitter: ctx.options.gate_submitter.unwrap_or(true),
        };
        let report = dispatch::dispatch(publishers, ctx, &opts)?;

        // Summary line — operators see succeeded/failed/skipped counts
        // and whether the submitter gate fired without grepping the
        // per-publisher log noise.
        let succeeded = report
            .results
            .iter()
            .filter(|r| matches!(r.outcome, PublisherOutcome::Succeeded))
            .count();
        let failed = report
            .results
            .iter()
            .filter(|r| matches!(r.outcome, PublisherOutcome::Failed(_)))
            .count();
        let skipped = report
            .results
            .iter()
            .filter(|r| matches!(r.outcome, PublisherOutcome::Skipped(_)))
            .count();
        log.status(&format!(
            "publish: {} succeeded, {} failed, {} skipped, submitter_gated={}",
            succeeded, failed, skipped, report.submitter_gated,
        ));
        // Per-publisher failure detail — surface error strings so
        // operators see which publisher failed without re-reading the
        // dispatcher's interleaved log output.
        for r in &report.results {
            if let PublisherOutcome::Failed(msg) = &r.outcome {
                log.warn(&format!("{}: {}", r.name, msg));
            } else if let PublisherOutcome::Skipped(SkipReason::SubmitterGated) = &r.outcome {
                log.status(&format!("{}: skipped via submitter-gate", r.name));
            }
        }

        ctx.set_publish_report(report);
        Ok(())
    }
}

impl Stage for PublishStage {
    fn name(&self) -> &str {
        "publish"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("publish");
        if ctx.skip_in_snapshot(&log, "publish") {
            return Ok(());
        }

        // Refuse to re-run publish when a prior `report.json` exists
        // for the current `run_id` unless the operator explicitly
        // opts in via `--allow-rerun`. The guard exists because
        // PR-based publishers (homebrew / scoop / nix / krew / MCP)
        // open a fresh pull request on each publish — re-running
        // them against the same tag duplicates the PR with no
        // safeguard. Operators recovering from a partial failure
        // should use `--rollback-only --from-run=<id>` first (which
        // has its own idempotency via `dist/run-<id>/rollback.json`).
        //
        // Skip the check in:
        //   - snapshot / dry-run (no report.json gets written in
        //     those modes — the file's existence is meaningless);
        //   - rollback-only (the CLI dispatches directly to
        //     `rollback_only::run` and never enters PublishStage,
        //     but defense-in-depth: refuse here too if a future
        //     refactor wires the path differently);
        //   - run_id == "local" (the no-git fallback produces
        //     false positives across unrelated `cargo test` runs
        //     in CI; only enforce when the id is derived from a
        //     real tag or commit).
        refuse_rerun_if_report_exists(ctx)?;

        // Preflight: every `--allow-nondeterministic <name>=<reason>`
        // entry must match at least one artifact emitted by the
        // build-side pipeline. Fail hard BEFORE the first publisher
        // fires so an operator typo can't ship as a silent exemption.
        validate_runtime_allowlist(ctx)?;

        // Build the publisher list from the active context and hand off
        // to the group-aware dispatcher via `run_with_publishers`.
        // `configured_publishers` is the single source of truth for
        // which publishers run.
        let publishers = registry::configured_publishers(ctx);
        // Surface the release-optional + dependent-manifest-publisher coupling
        // before any publisher fires (a manifest pointing at a 404 release URL
        // ships silently otherwise).
        registry::warn_release_optional_with_dependent_publisher(ctx, &log);
        Self::run_with_publishers(ctx, &log, &publishers)?;

        // ---- Best-effort rollback dispatch ----
        //
        // Runs (unless `--rollback=none`) when a required Assets/Manager
        // publisher failed, OR a required Submitter that opts into a
        // programmatic rollback failed with live state recorded (cargo:
        // crate A published, crate B failed). Reversible Assets/Manager
        // publishers that recorded `Succeeded` get their
        // `Publisher::rollback` invoked and flip to `RolledBack` /
        // `RollbackFailed` / `RollbackSkippedNoScope`; a failed cargo
        // Submitter gets its recorded crates yanked while KEEPING its
        // `Failed` outcome on a successful yank. Every other Submitter has
        // no programmatic rollback and is left untouched (protected by the
        // dispatch-time submitter gate).
        run_rollback_if_needed(ctx, &publishers, &log);

        // ---- Persist end-of-pipeline state to dist/run-<id>/report.json ----
        //
        // Writer half of the `--rollback-only --from-run=<id>` contract
        // (`rollback_only::run` is the reader). Runs AFTER
        // `run_rollback_if_needed` so per-publisher rollback outcomes
        // (`RolledBack` / `RollbackFailed`) are captured — the file
        // represents END-OF-PIPELINE state, not mid-pipeline. Snapshot /
        // dry-run modes and empty-result reports are no-ops; IO failure
        // is best-effort (warn + continue, never fail the pipeline).
        write_report_to_run_dir(ctx, &log);

        // ---- Post-publish polling fan-out (Chocolatey moderation + WinGet PR) ----
        //
        // Runs AFTER every publisher has completed so polling isn't gated
        // on a failed unrelated publisher (e.g. krew). The fan-out is
        // gated by `--no-post-publish-poll` and by each publisher's
        // `post_publish_poll.enabled` block. Skipping `choco` /
        // `winget` skips their poll automatically (no submission =
        // nothing to poll for).
        if !ctx.is_dry_run() && !ctx.is_snapshot() {
            let selected = ctx.options.selected_crates.clone();
            run_post_publish_pollers(ctx, &selected, &log);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::config::{
        AurConfig, CargoPublishConfig, Config, CrateConfig, HomebrewConfig, PublishConfig,
        WorkspaceConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};

    fn dry_run_ctx(config: Config) -> Context {
        Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_stage_name() {
        assert_eq!(PublishStage.name(), "publish");
    }

    #[test]
    fn test_run_no_crates_configured() {
        let config = Config::default();
        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // PublishStage::run swap — trait-based dispatch sets ctx.publish_report,
    // returns Ok(()) on per-publisher failure, applies the Submitter gate.
    // -----------------------------------------------------------------------

    #[test]
    fn publish_stage_returns_ok_and_sets_context_publish_report() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::test_fixture();
        let publishers = vec![fake(
            "manager-only",
            PublisherGroup::Manager,
            false,
            FakeOutcome::Succeed,
        )];
        let log = ctx.logger("publish-test");
        PublishStage::run_with_publishers(&mut ctx, &log, &publishers)
            .expect("run_with_publishers returns Ok on per-publisher success");

        let report = ctx.publish_report().expect("publish_report set on Context");
        assert_eq!(report.results.len(), 1);
        assert!(matches!(
            report.results[0].outcome,
            anodizer_core::PublisherOutcome::Succeeded
        ));
        assert!(!report.submitter_gated);
    }

    // -----------------------------------------------------------------------
    // validate_runtime_allowlist — operator-typo guard before publishers fire
    // -----------------------------------------------------------------------

    fn add_artifact(ctx: &mut Context, name: &str) {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("dist/{name}")),
            name: name.to_string(),
            target: None,
            crate_name: "test".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }

    #[test]
    fn allow_nondeterministic_validates_matching_name() {
        let mut ctx = Context::test_fixture();
        add_artifact(&mut ctx, "anodizer-0.1.0.tar.gz");
        ctx.options.runtime_nondeterministic_allowlist = vec![(
            "anodizer-0.1.0.tar.gz".to_string(),
            "embedded build date".to_string(),
        )];
        validate_runtime_allowlist(&ctx).expect("matching name must pass validation");
    }

    #[test]
    fn allow_nondeterministic_validates_matching_glob() {
        let mut ctx = Context::test_fixture();
        add_artifact(&mut ctx, "anodizer-0.1.0.rpm");
        ctx.options.runtime_nondeterministic_allowlist =
            vec![("*.rpm".to_string(), "rpm metadata".to_string())];
        validate_runtime_allowlist(&ctx).expect("matching glob must pass validation");
    }

    #[test]
    fn allow_nondeterministic_unmatched_name_errors_before_publish() {
        let mut ctx = Context::test_fixture();
        add_artifact(&mut ctx, "anodizer-0.1.0.tar.gz");
        ctx.options.runtime_nondeterministic_allowlist = vec![(
            "anodizer-0.1.0.deb".to_string(),
            "typo - meant tar.gz".to_string(),
        )];
        let err =
            validate_runtime_allowlist(&ctx).expect_err("unmatched name must error before publish");
        let msg = err.to_string();
        assert!(
            msg.contains("anodizer-0.1.0.deb"),
            "error must name the unmatched entry: {msg}",
        );
        assert!(
            msg.contains("--allow-nondeterministic"),
            "error must cite the flag for operator orientation: {msg}",
        );
    }

    #[test]
    fn allow_nondeterministic_empty_list_is_noop() {
        let ctx = Context::test_fixture();
        // No allowlist entries, no artifacts — must not error.
        validate_runtime_allowlist(&ctx).expect("empty allowlist must be a no-op");
    }

    /// Helper for tests that need to control `artifact.name` and
    /// `artifact.path` independently — exercising the basename-match
    /// path in `validate_runtime_allowlist`.
    fn add_artifact_with_path(ctx: &mut Context, name: &str, path: &str) {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(path),
            name: name.to_string(),
            target: None,
            crate_name: "test".to_string(),
            metadata: std::collections::HashMap::new(),
            size: None,
        });
    }

    #[test]
    fn allow_nondeterministic_matches_file_extension_against_path_basename() {
        // Build stage recorded `artifact.name = "anodize-v0.2.1"` (no
        // extension), while the actual file on disk is
        // `dist/anodize-v0.2.1.crate`. A `*.crate` glob must match via
        // the path-basename surface even though the name alone won't.
        let mut ctx = Context::test_fixture();
        add_artifact_with_path(&mut ctx, "anodize-v0.2.1", "dist/anodize-v0.2.1.crate");
        ctx.options.runtime_nondeterministic_allowlist =
            vec![("*.crate".to_string(), "cargo embeds mtime".to_string())];
        validate_runtime_allowlist(&ctx)
            .expect("*.crate glob must match path basename when name lacks extension");
    }

    #[test]
    fn allow_nondeterministic_matches_exact_basename_against_path() {
        // Exact-match form: operator types the full filename. `name`
        // is the bare crate identifier; `path` is the real file.
        let mut ctx = Context::test_fixture();
        add_artifact_with_path(&mut ctx, "core", "dist/core-aarch64.tar.gz");
        ctx.options.runtime_nondeterministic_allowlist = vec![(
            "core-aarch64.tar.gz".to_string(),
            "tar metadata".to_string(),
        )];
        validate_runtime_allowlist(&ctx).expect("exact basename must match path filename");
    }

    #[test]
    fn allow_nondeterministic_typo_still_errors() {
        // Negative case: a real typo against the same artifact above
        // must still fall through to the unmatched error path — the
        // basename surface widens what *can* match but does not
        // suppress typo detection.
        let mut ctx = Context::test_fixture();
        add_artifact_with_path(&mut ctx, "core", "dist/core-aarch64.tar.gz");
        ctx.options.runtime_nondeterministic_allowlist =
            vec![("corre.tar.gz".to_string(), "typo".to_string())];
        let err = validate_runtime_allowlist(&ctx)
            .expect_err("typo must still error even with basename match enabled");
        let msg = err.to_string();
        assert!(
            msg.contains("corre.tar.gz"),
            "error must name the unmatched entry: {msg}",
        );
    }

    // -----------------------------------------------------------------------
    // Rollback dispatch integration - end-to-end PublishStage::run path
    // through `run_with_publishers` + `run_rollback_if_needed`.
    // -----------------------------------------------------------------------

    /// Helper to drive the same end-to-end shape `Stage::run` exercises
    /// (dispatch -> rollback) but with a synthetic publisher slice.
    /// Skips the post-publish polling fan-out because the fan-out only
    /// reads per-crate config blocks; with no chocolatey/winget blocks
    /// configured, the helper is a no-op.
    fn run_dispatch_and_rollback(
        ctx: &mut Context,
        publishers: &[Box<dyn anodizer_core::Publisher>],
    ) -> Result<()> {
        let log = ctx.logger("publish-test");
        PublishStage::run_with_publishers(ctx, &log, publishers)?;
        run_rollback_if_needed(ctx, publishers, &log);
        Ok(())
    }

    #[test]
    fn publish_stage_invokes_rollback_after_required_failure() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::test_fixture();
        // Required Manager publisher fails; Assets publisher succeeds.
        // Rollback dispatch should flip the Assets entry to RolledBack.
        let publishers = vec![
            fake(
                "assets",
                PublisherGroup::Assets,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "manager",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Fail("manager boom".into()),
            ),
        ];
        run_dispatch_and_rollback(&mut ctx, &publishers)
            .expect("stage run returns Ok even when required publisher fails");

        let report = ctx.publish_report().expect("publish_report set");
        let assets = report
            .results
            .iter()
            .find(|r| r.name == "assets")
            .expect("assets entry present");
        assert!(
            matches!(assets.outcome, anodizer_core::PublisherOutcome::RolledBack),
            "expected Assets publisher to be rolled back, got {:?}",
            assets.outcome
        );
        // Manager remains Failed (rollback doesn't touch failed entries).
        let manager = report
            .results
            .iter()
            .find(|r| r.name == "manager")
            .expect("manager entry present");
        assert!(matches!(
            manager.outcome,
            anodizer_core::PublisherOutcome::Failed(_)
        ));
    }

    /// END-TO-END partial cargo failure: a required Submitter named
    /// `cargo` records crate-a as published, then fails (crate-b). The
    /// REAL dispatcher builds the report (failed row + partial evidence),
    /// then the REAL `run_rollback_if_needed` must ARM rollback on the
    /// failed Submitter and `rollback::run` must invoke the publisher's
    /// programmatic rollback — issuing `cargo yank` for crate-a ONLY.
    ///
    /// This exercises the full orchestration a direct `rollback()` call
    /// cannot: dispatch -> report -> run_rollback_if_needed -> rollback::run
    /// -> Publisher::rollback. A `cargo` PATH stub records the yank argv.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(cargo_stub_path)]
    fn end_to_end_failed_cargo_submitter_yanks_only_succeeded_crate() {
        use crate::cargo::{CargoPublisher, CargoYankTarget, encode_cargo_yank_targets};
        use anodizer_core::{PublishEvidence, Publisher, PublisherGroup, PublisherOutcome};
        use std::os::unix::fs::PermissionsExt;

        // A `cargo`-named Submitter whose run() records crate-a as
        // published then fails on crate-b — exactly the partial-failure
        // shape `CargoPublisher::run` produces, but without the network
        // (no `cargo publish` / index GET). rollback + the opt-in
        // predicate delegate to a REAL CargoPublisher so the actual decode
        // + `cargo yank` spawn surface is what runs.
        struct PartialCargo {
            inner: CargoPublisher,
        }
        impl Publisher for PartialCargo {
            fn name(&self) -> &str {
                "cargo"
            }
            fn group(&self) -> PublisherGroup {
                PublisherGroup::Submitter
            }
            fn required(&self) -> bool {
                true
            }
            fn skips_on_nightly(&self) -> bool {
                true
            }
            fn run(&self, ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
                // crate-a went live; crate-b then failed.
                let mut ev = PublishEvidence::new("cargo");
                ev.extra = encode_cargo_yank_targets(&[CargoYankTarget {
                    name: "crate-a".into(),
                    version: "1.0.0".into(),
                    registry: Some("my-registry".into()),
                    index: None,
                }]);
                ctx.record_pending_evidence(ev);
                anyhow::bail!("crate-b publish failed after crate-a succeeded")
            }
            fn rollback(
                &self,
                ctx: &mut Context,
                evidence: &PublishEvidence,
            ) -> anyhow::Result<()> {
                self.inner.rollback(ctx, evidence)
            }
            fn programmatic_rollback_on_failure(&self, evidence: &PublishEvidence) -> bool {
                self.inner.programmatic_rollback_on_failure(evidence)
            }
            fn rollback_scope_needed(&self) -> Option<&'static str> {
                // Drop the scope gate for this test so the rollback path
                // isn't short-circuited by a missing CARGO_REGISTRY_TOKEN.
                None
            }
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let argv_log = tmp.path().join("argv.log");
        let stub = tmp.path().join("cargo");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
                argv_log.display()
            ),
        )
        .expect("write cargo stub");
        let mut perms = std::fs::metadata(&stub).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).expect("chmod");

        let mut ctx = Context::test_fixture();
        let publishers: Vec<Box<dyn anodizer_core::Publisher>> = vec![Box::new(PartialCargo {
            inner: CargoPublisher::new(),
        })];

        let prev_path = std::env::var("PATH").ok();
        let new_path = format!(
            "{}:{}",
            tmp.path().display(),
            prev_path.clone().unwrap_or_default()
        );
        // SAFETY: env mutation single-threaded within this serial group.
        unsafe { std::env::set_var("PATH", &new_path) };
        let res = run_dispatch_and_rollback(&mut ctx, &publishers);
        // SAFETY: restore PATH within the same serial group.
        unsafe {
            match prev_path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
        }
        res.expect("stage run returns Ok even when cargo fails");

        // The yank ran for crate-a ONLY, on its configured registry.
        let yanks: Vec<String> = std::fs::read_to_string(&argv_log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("yank"))
            .map(str::to_string)
            .collect();
        assert_eq!(yanks.len(), 1, "exactly one yank, for crate-a: {yanks:?}");
        let line = &yanks[0];
        assert!(
            line.contains("--version 1.0.0"),
            "yank carries version: {line}"
        );
        assert!(line.contains("crate-a"), "yank targets crate-a: {line}");
        assert!(
            line.contains("--registry my-registry"),
            "yank targets the recorded registry: {line}"
        );
        assert!(
            !line.contains("crate-b"),
            "crate-b never published; must not be yanked: {line}"
        );

        // The cargo row stays Failed on a successful yank — the release
        // genuinely failed and must not masquerade as RolledBack.
        let report = ctx.publish_report().expect("publish_report set");
        let cargo_row = report
            .results
            .iter()
            .find(|r| r.name == "cargo")
            .expect("cargo entry present");
        assert!(
            matches!(cargo_row.outcome, PublisherOutcome::Failed(_)),
            "cargo row stays Failed after a successful partial yank, got {:?}",
            cargo_row.outcome
        );
    }

    #[test]
    fn publish_stage_skips_rollback_when_mode_is_none() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::new(
            anodizer_core::config::Config::default(),
            ContextOptions {
                rollback_mode: Some(RollbackMode::None),
                ..Default::default()
            },
        );
        // Same fixture as the prior test, but `--rollback=none`
        // (Some(None)) suppresses the rollback dispatch.
        let publishers = vec![
            fake(
                "assets",
                PublisherGroup::Assets,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "manager",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Fail("manager boom".into()),
            ),
        ];
        run_dispatch_and_rollback(&mut ctx, &publishers)
            .expect("stage run returns Ok in rollback=none mode");

        let report = ctx.publish_report().expect("publish_report set");
        let assets = report
            .results
            .iter()
            .find(|r| r.name == "assets")
            .expect("assets entry present");
        assert!(
            matches!(assets.outcome, anodizer_core::PublisherOutcome::Succeeded),
            "expected Assets publisher to remain Succeeded under rollback=none, got {:?}",
            assets.outcome
        );
    }

    #[test]
    fn publish_stage_skips_rollback_when_no_required_failure() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::test_fixture();
        // Optional Manager publisher fails - rollback should NOT fire
        // because no REQUIRED publisher failed.
        let publishers = vec![
            fake(
                "assets",
                PublisherGroup::Assets,
                false,
                FakeOutcome::Succeed,
            ),
            fake(
                "manager",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Fail("manager boom".into()),
            ),
        ];
        run_dispatch_and_rollback(&mut ctx, &publishers)
            .expect("stage run returns Ok on optional failure");

        let report = ctx.publish_report().expect("publish_report set");
        let assets = report
            .results
            .iter()
            .find(|r| r.name == "assets")
            .expect("assets entry present");
        assert!(
            matches!(assets.outcome, anodizer_core::PublisherOutcome::Succeeded),
            "expected Assets publisher to remain Succeeded when no required failure, got {:?}",
            assets.outcome
        );
    }

    #[test]
    fn publish_stage_records_publisher_failures_without_returning_err() {
        use crate::testing::*;
        use anodizer_core::PublisherGroup;

        let mut ctx = Context::test_fixture();
        // Three publishers in the Manager group; the middle one fails.
        // Dispatch must record every outcome and still return Ok so the
        // pipeline continues past PublishStage.
        let publishers = vec![
            fake("m1", PublisherGroup::Manager, false, FakeOutcome::Succeed),
            fake(
                "m2",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Fail("boom".into()),
            ),
            fake("m3", PublisherGroup::Manager, false, FakeOutcome::Succeed),
        ];
        let log = ctx.logger("publish-test");
        PublishStage::run_with_publishers(&mut ctx, &log, &publishers)
            .expect("per-publisher failure must not bail the stage");

        let report = ctx.publish_report().expect("publish_report set on Context");
        let names: Vec<&str> = report.results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["m1", "m2", "m3"]);
        assert!(matches!(
            report.results[0].outcome,
            anodizer_core::PublisherOutcome::Succeeded
        ));
        match &report.results[1].outcome {
            anodizer_core::PublisherOutcome::Failed(msg) => assert!(msg.contains("boom")),
            other => panic!("expected Failed for m2, got {:?}", other),
        }
        assert!(matches!(
            report.results[2].outcome,
            anodizer_core::PublisherOutcome::Succeeded
        ));
    }

    #[test]
    fn submitter_gate_records_skipped_when_required_manager_fails() {
        use crate::testing::*;
        use anodizer_core::{PublisherGroup, PublisherOutcome, SkipReason};

        let mut ctx = Context::test_fixture();
        // Required Manager publisher fails -> Submitter must be gated to
        // Skipped(SubmitterGated) (irreversible publish protected).
        let publishers = vec![
            fake(
                "manager",
                PublisherGroup::Manager,
                true,
                FakeOutcome::Fail("manager boom".into()),
            ),
            fake(
                "submitter",
                PublisherGroup::Submitter,
                false,
                FakeOutcome::Succeed,
            ),
        ];
        let log = ctx.logger("publish-test");
        PublishStage::run_with_publishers(&mut ctx, &log, &publishers)
            .expect("Submitter gating must record skipped, not Err");

        let report = ctx.publish_report().expect("publish_report set on Context");
        assert!(report.submitter_gated);
        let submitter = report
            .results
            .iter()
            .find(|r| r.name == "submitter")
            .expect("submitter entry present");
        assert!(matches!(
            submitter.outcome,
            PublisherOutcome::Skipped(SkipReason::SubmitterGated)
        ));
    }

    #[test]
    fn publish_stage_skips_under_snapshot() {
        // Snapshot mode short-circuits `Stage::run` before dispatch fires,
        // leaving `ctx.publish_report` as `None`. This pins the gate in
        // `ctx.skip_in_snapshot` so a future refactor can't silently
        // start running publishers under `--snapshot`.
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let mut ctx = Context::new(
            config,
            ContextOptions {
                snapshot: true,
                ..Default::default()
            },
        );
        assert!(PublishStage.run(&mut ctx).is_ok());
        assert!(
            ctx.publish_report().is_none(),
            "snapshot mode must short-circuit before dispatch fires"
        );
    }

    /// A workspace-only crate that carries a non-cargo publisher block
    /// (homebrew/scoop/aur/...) must be visible to `crates_with_publisher`,
    /// matching the universe `cargo.rs::publish_to_cargo` walks. Before the
    /// shared `util::all_crates` lift, this crate would silently disappear
    /// from every non-cargo dispatcher even though cargo would still publish it.
    #[test]
    fn test_crates_with_publisher_includes_workspace_only_crates() {
        let mut config = Config::default();
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                name: "ws-only".to_string(),
                path: "crates/ws-only".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    homebrew: Some(HomebrewConfig::default()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }]);

        let ctx = dry_run_ctx(config);
        let names = crates_with_publisher(&ctx, &[], |p| p.homebrew.is_some());
        assert_eq!(names, vec!["ws-only".to_string()]);
    }

    /// Dedup rule: top-level `crates` wins on name collision with a
    /// workspace entry. Both walkers (cargo + non-cargo) must see exactly
    /// one entry per name so `expand_with_transitive_deps` and the
    /// publisher loops never double-publish.
    #[test]
    fn test_crates_with_publisher_dedupes_top_level_over_workspace() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "shared".to_string(),
            path: "top".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                homebrew: Some(HomebrewConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }];
        config.workspaces = Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![CrateConfig {
                // Same name as the top-level — top-level must win.
                name: "shared".to_string(),
                path: "ws/shared".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: None,
                ..Default::default()
            }],
            ..Default::default()
        }]);

        let ctx = dry_run_ctx(config);
        let names = crates_with_publisher(&ctx, &[], |p| p.homebrew.is_some());
        assert_eq!(
            names,
            vec!["shared".to_string()],
            "top-level entry must win on name collision and not be doubled"
        );
    }

    /// `--no-post-publish-poll` must emit one `PostPublishResult { status:
    /// NotPolled }` per eligible per-crate publisher block instead of silently
    /// short-circuiting. The release-summary renderer relies on the explicit
    /// `NotPolled` rows to distinguish "skipped via flag" from "no eligible
    /// publishers" — see `post_publish::status::PostPublishStatus::NotPolled`
    /// docs.
    #[test]
    fn skip_path_emits_not_polled_for_each_configured_publisher() {
        // Polling is opt-in per-publisher (PostPublishPollConfig default
        // is `enabled: false` because moderation queues take hours-to-
        // days). The skip-path test must therefore explicitly enable
        // polling on both publisher blocks before asserting that
        // `--no-post-publish-poll` overrides emit `NotPolled` rows for
        // each eligible publisher.
        use anodizer_core::config::{ChocolateyConfig, PostPublishPollConfig, WingetConfig};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                chocolatey: Some(ChocolateyConfig {
                    name: Some("mylib-choco".to_string()),
                    post_publish_poll: Some(PostPublishPollConfig {
                        enabled: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                winget: Some(WingetConfig {
                    publisher: Some("TJSmith".to_string()),
                    name: Some("MyLib".to_string()),
                    post_publish_poll: Some(PostPublishPollConfig {
                        enabled: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                // NOT dry_run — we want the skip-path inside
                // `run_post_publish_pollers` to engage and emit
                // `NotPolled`. dry-run gates the entire pipeline before
                // ever reaching the post-publish call site.
                skip_post_publish_poll: true,
                ..Default::default()
            },
        );

        let log = StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
        run_post_publish_pollers(&mut ctx, &[], &log);

        let results = &ctx.stage_outputs.post_publish_results;
        assert_eq!(
            results.len(),
            2,
            "skip path must emit one NotPolled per configured publisher (got {results:?})"
        );

        // Dispatch order in `run_post_publish_pollers`: chocolatey arm
        // runs before winget arm.
        assert_eq!(results[0]["publisher"], "chocolatey");
        assert_eq!(results[0]["package"], "mylib-choco");
        assert_eq!(results[0]["status"]["kind"], "not_polled");

        assert_eq!(results[1]["publisher"], "winget");
        assert_eq!(results[1]["package"], "TJSmith.MyLib");
        assert_eq!(results[1]["status"]["kind"], "not_polled");
    }

    #[test]
    fn test_run_dry_run_cargo() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mylib".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                cargo: Some(CargoPublishConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        // dry-run: should log but not actually shell out
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // ── Config-to-behavior wiring tests ──
    // -----------------------------------------------------------------------

    #[test]
    fn test_no_publish_config_is_noop() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "nopub".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: None, // No publish config
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        // Should succeed (no-op)
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    /// Document current behavior: the publish stage does NOT skip homebrew/scoop
    /// publishing for prerelease versions. It proceeds regardless of whether
    /// the version contains a prerelease suffix like -rc.1 or -beta.
    ///
    /// This is a known limitation: homebrew/scoop are skipped for prereleases
    /// by default. If this behavior is added in the future, this test should be
    /// updated to verify that skipping occurs.
    // -----------------------------------------------------------------------
    // Chocolatey integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // WinGet integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // AUR integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_aur() {
        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
                    description: Some("My tool".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Krew integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Top-level AUR sources integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_run_dry_run_top_level_aur_sources() {
        use anodizer_core::config::AurSourceConfig;

        let mut config = Config::default();
        config.aur_sources = Some(vec![AurSourceConfig {
            name: Some("myapp".to_string()),
            description: Some("My application".to_string()),
            license: Some("MIT".to_string()),
            git_url: Some("ssh://aur@aur.archlinux.org/myapp.git".to_string()),
            makedepends: Some(vec!["rust".to_string(), "cargo".to_string()]),
            ..Default::default()
        }]);
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx.template_vars_mut().set("ProjectName", "myapp");
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_top_level_aur_sources_empty_is_noop() {
        let mut config = Config::default();
        config.aur_sources = Some(vec![]);
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_top_level_aur_sources_none_is_noop() {
        let mut config = Config::default();
        config.aur_sources = None;

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // Nix integration tests
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // record_publisher_result tests removed when PublishStage swapped to
    // trait-based dispatch (see `crates/stage-publish/src/dispatch.rs`).
    // The collect-or-bail policy now lives in `DispatchOptions::fail_fast`
    // and is covered by tests in `crates/stage-publish/src/dispatch.rs`.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // derive_run_id + write_report_to_run_dir — B4 (writer for the
    // `--rollback-only --from-run=<id>` contract). The writer half was
    // missing in production; `rollback_only::run` was structurally
    // unreachable. Tests below pin: (a) the run_id fallback chain
    // (tag -> short_commit -> "local") with the validator gate, and
    // (b) the writer's no-op/IO behavior including snapshot/dry-run
    // skip and empty-results skip.
    // -----------------------------------------------------------------------

    mod run_report_persistence {
        use super::*;
        use crate::testing::*;
        use anodizer_core::test_helpers::TestContextBuilder;
        use anodizer_core::{
            PublishReport, PublisherGroup, PublisherOutcome, PublisherResult, context::Context,
        };

        fn synthetic_report(name: &str) -> PublishReport {
            let mut r = PublishReport::default();
            r.results.push(PublisherResult {
                name: name.to_string(),
                group: PublisherGroup::Manager,
                required: false,
                outcome: PublisherOutcome::Succeeded,
                evidence: None,
            });
            r
        }

        #[test]
        fn derive_run_id_prefers_tag_when_available() {
            let ctx = TestContextBuilder::new()
                .tag("v1.2.3")
                .commit("abc123def4567890")
                .build();
            assert_eq!(derive_run_id(&ctx), "v1.2.3");
        }

        #[test]
        fn derive_run_id_falls_back_to_short_commit_when_tag_empty() {
            let mut ctx = TestContextBuilder::new()
                .tag("v1.2.3")
                .commit("abc123def4567890")
                .build();
            // Force the tag empty post-build to exercise the fallback;
            // tag("") would still satisfy validation if non-empty rule
            // were the only check, so blank the field directly.
            ctx.git_info.as_mut().unwrap().tag = String::new();
            assert_eq!(derive_run_id(&ctx), "abc123d");
        }

        #[test]
        fn derive_run_id_falls_back_to_local_when_no_git_info() {
            let mut ctx = TestContextBuilder::new().build();
            ctx.git_info = None;
            assert_eq!(derive_run_id(&ctx), "local");
        }

        #[test]
        fn derive_run_id_falls_back_to_local_when_both_tag_and_short_commit_empty() {
            let mut ctx = TestContextBuilder::new().build();
            let info = ctx.git_info.as_mut().unwrap();
            info.tag = String::new();
            info.short_commit = String::new();
            assert_eq!(derive_run_id(&ctx), "local");
        }

        #[test]
        fn derive_run_id_skips_tag_with_invalid_chars_and_falls_through() {
            // A tag containing '/' (e.g. a malformed monorepo prefix
            // that bypassed earlier validation) must NOT propagate into
            // the run-dir path. Fall through to short_commit.
            let mut ctx = TestContextBuilder::new()
                .tag("v1.2.3")
                .commit("abc123def4567890")
                .build();
            ctx.git_info.as_mut().unwrap().tag = "bad/tag".to_string();
            assert_eq!(derive_run_id(&ctx), "abc123d");
        }

        #[test]
        fn derive_run_id_always_passes_validate_run_id() {
            // Table-driven: every branch of the fallback chain must
            // produce a string that satisfies the validator. A future
            // refactor that loosens an upstream check could regress
            // this — the validator is the single source of truth.
            type CaseFn = fn() -> Context;
            let cases: &[(&str, CaseFn)] = &[
                ("tag branch", || {
                    TestContextBuilder::new()
                        .tag("v0.0.0-test")
                        .commit("abc123def4567890")
                        .build()
                }),
                ("short_commit branch", || {
                    let mut ctx = TestContextBuilder::new()
                        .tag("v0.0.0-test")
                        .commit("abc123def4567890")
                        .build();
                    ctx.git_info.as_mut().unwrap().tag = String::new();
                    ctx
                }),
                ("local fallback (no git_info)", || {
                    let mut ctx = TestContextBuilder::new().build();
                    ctx.git_info = None;
                    ctx
                }),
                ("local fallback (both empty)", || {
                    let mut ctx = TestContextBuilder::new().build();
                    let info = ctx.git_info.as_mut().unwrap();
                    info.tag = String::new();
                    info.short_commit = String::new();
                    ctx
                }),
            ];
            for (label, make_ctx) in cases {
                let ctx = make_ctx();
                let id = derive_run_id(&ctx);
                rollback_only::validate_run_id(&id).unwrap_or_else(|e| {
                    panic!(
                        "case '{label}' produced invalid run_id '{id}': {e}",
                        label = label,
                        id = id,
                        e = e
                    )
                });
            }
        }

        #[test]
        fn write_report_creates_parent_directory_and_pretty_json() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .build();
            ctx.set_publish_report(synthetic_report("manager-only"));

            let log = ctx.logger("publish-test");
            write_report_to_run_dir(&ctx, &log);

            let path = tmp.path().join("run-v0.0.0-test").join("report.json");
            assert!(path.exists(), "expected report at {}", path.display());

            let body = std::fs::read_to_string(&path).expect("read");
            // Pretty-print includes newlines + 2-space indent. Crude
            // shape-check rather than full whitespace equality so a
            // future serde_json change doesn't break the test.
            assert!(body.contains('\n'), "expected pretty JSON, got: {body}");
            // Round-trip: same shape as PublishReport.
            let parsed: PublishReport = serde_json::from_str(&body).expect("round-trip");
            assert_eq!(parsed.results.len(), 1);
            assert_eq!(parsed.results[0].name, "manager-only");
            assert!(matches!(
                parsed.results[0].outcome,
                PublisherOutcome::Succeeded
            ));
        }

        #[test]
        fn write_report_is_noop_on_empty_results() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .build();
            // Default report = empty results.
            ctx.set_publish_report(PublishReport::default());

            let log = ctx.logger("publish-test");
            write_report_to_run_dir(&ctx, &log);

            let dir = tmp.path().join("run-v0.0.0-test");
            assert!(
                !dir.exists(),
                "no work done -> no dir written; found {}",
                dir.display(),
            );
        }

        #[test]
        fn write_report_is_noop_in_snapshot_mode() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .snapshot(true)
                .build();
            ctx.set_publish_report(synthetic_report("manager-only"));

            let log = ctx.logger("publish-test");
            write_report_to_run_dir(&ctx, &log);

            let dir = tmp.path().join("run-v0.0.0-test");
            assert!(
                !dir.exists(),
                "snapshot mode must not pollute dist/run-*/; found {}",
                dir.display(),
            );
        }

        #[test]
        fn write_report_is_noop_in_dry_run_mode() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .dry_run(true)
                .build();
            ctx.set_publish_report(synthetic_report("manager-only"));

            let log = ctx.logger("publish-test");
            write_report_to_run_dir(&ctx, &log);

            let dir = tmp.path().join("run-v0.0.0-test");
            assert!(
                !dir.exists(),
                "dry-run mode must not pollute dist/run-*/; found {}",
                dir.display(),
            );
        }

        #[test]
        fn write_report_is_noop_when_no_publish_report_set() {
            // Edge case: PublishStage::run only calls write after
            // dispatch sets publish_report, but write_report_to_run_dir
            // is defensive against being invoked with no report. Verify
            // the no-op path so a future refactor that moves the call
            // can't crash on None.
            let tmp = tempfile::tempdir().expect("tempdir");
            let ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .build();
            // No set_publish_report() call.
            let log = ctx.logger("publish-test");
            write_report_to_run_dir(&ctx, &log);
            assert!(!tmp.path().join("run-v0.0.0-test").exists());
        }

        #[test]
        fn publish_stage_run_writes_report_at_end_of_pipeline() {
            // End-to-end via run_with_publishers + run_rollback_if_needed
            // + write_report_to_run_dir, exercising the order
            // (rollback BEFORE write) so any rollback outcomes show up
            // in the on-disk file.
            let tmp = tempfile::tempdir().expect("tempdir");
            let mut ctx = TestContextBuilder::new()
                .tag("v0.0.0-test")
                .dist(tmp.path().to_path_buf())
                .build();
            let publishers = vec![fake(
                "manager-only",
                PublisherGroup::Manager,
                false,
                FakeOutcome::Succeed,
            )];
            let log = ctx.logger("publish-test");
            PublishStage::run_with_publishers(&mut ctx, &log, &publishers)
                .expect("run_with_publishers Ok");
            run_rollback_if_needed(&mut ctx, &publishers, &log);
            write_report_to_run_dir(&ctx, &log);

            let path = tmp.path().join("run-v0.0.0-test").join("report.json");
            assert!(path.exists(), "expected report at {}", path.display());
            let parsed: PublishReport =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(parsed.results.len(), 1);
            assert_eq!(parsed.results[0].name, "manager-only");
        }
    }

    #[test]
    fn test_run_dry_run_nix() {
        use anodizer_core::config::{NixConfig, RepositoryConfig};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                nix: Some(NixConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("myorg".to_string()),
                        name: Some("nixpkgs-overlay".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = dry_run_ctx(config);
        assert!(PublishStage.run(&mut ctx).is_ok());
    }

    // -----------------------------------------------------------------------
    // refuse_rerun_if_report_exists — guards PublishStage::run from
    // re-publishing when a prior run's report.json is on disk for the
    // same `run_id`. PR-based publishers (homebrew / scoop / nix /
    // krew / MCP) open a fresh PR on each invocation, so re-running
    // against the same tag would duplicate work with no safeguard.
    // -----------------------------------------------------------------------

    /// Build a Context whose `config.dist` is a real on-disk tempdir
    /// AND whose `git_info` has a stable tag so `derive_run_id` returns
    /// a deterministic non-"local" value.
    fn ctx_with_dist_and_tag(tag: &str) -> (Context, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).expect("mkdir dist");
        let ctx = anodizer_core::test_helpers::TestContextBuilder::new()
            .tag(tag)
            .dist(dist)
            .build();
        (ctx, tmp)
    }

    /// Pre-seed `dist/run-<tag>/report.json` so the guard sees a prior run.
    fn seed_prior_report(ctx: &Context, tag: &str) {
        let dir = ctx.config.dist.join(format!("run-{}", tag));
        std::fs::create_dir_all(&dir).expect("mkdir run dir");
        std::fs::write(dir.join("report.json"), "{}").expect("write fixture report.json");
    }

    #[test]
    fn refuse_rerun_passes_on_first_run_no_prior_report() {
        let (ctx, _tmp) = ctx_with_dist_and_tag("v1.0.0");
        // No prior report.json on disk — guard must allow proceeding.
        refuse_rerun_if_report_exists(&ctx).expect("first run must pass guard");
    }

    #[test]
    fn publish_stage_refuses_when_report_exists() {
        let (ctx, _tmp) = ctx_with_dist_and_tag("v1.0.0");
        seed_prior_report(&ctx, "v1.0.0");

        let err = refuse_rerun_if_report_exists(&ctx)
            .expect_err("guard must refuse when prior report.json exists");
        let msg = err.to_string();
        assert!(
            msg.contains("publish refusing to run"),
            "error must announce refusal: {msg}",
        );
        assert!(
            msg.contains("--rollback-only"),
            "error must point operators at the safer recovery flow: {msg}",
        );
        assert!(
            msg.contains("--allow-rerun"),
            "error must cite the override flag: {msg}",
        );
        assert!(
            msg.contains("DUPLICATE"),
            "error must warn loudly about duplicate-PR risk: {msg}",
        );
        assert!(
            msg.contains("v1.0.0"),
            "error must include the run_id so operators know which prior run blocked the re-run: {msg}",
        );
    }

    #[test]
    fn publish_stage_allows_rerun_when_flag_set() {
        let (mut ctx, _tmp) = ctx_with_dist_and_tag("v1.0.0");
        seed_prior_report(&ctx, "v1.0.0");
        ctx.options.allow_rerun = true;

        refuse_rerun_if_report_exists(&ctx).expect("allow_rerun must override the guard");
    }

    #[test]
    fn publish_stage_skips_check_in_snapshot_mode() {
        let (mut ctx, _tmp) = ctx_with_dist_and_tag("v1.0.0");
        seed_prior_report(&ctx, "v1.0.0");
        ctx.options.snapshot = true;

        refuse_rerun_if_report_exists(&ctx)
            .expect("snapshot mode must skip the guard (no report.json gets written there)");
    }

    #[test]
    fn publish_stage_skips_check_in_dry_run_mode() {
        let (mut ctx, _tmp) = ctx_with_dist_and_tag("v1.0.0");
        seed_prior_report(&ctx, "v1.0.0");
        ctx.options.dry_run = true;

        refuse_rerun_if_report_exists(&ctx).expect("dry-run mode must skip the guard");
    }

    #[test]
    fn publish_stage_skips_check_in_rollback_only_mode() {
        let (mut ctx, _tmp) = ctx_with_dist_and_tag("v1.0.0");
        seed_prior_report(&ctx, "v1.0.0");
        ctx.options.rollback_only = true;

        refuse_rerun_if_report_exists(&ctx).expect(
            "rollback-only mode bypasses PublishStage entirely; \
                defense-in-depth guard must also let it through here",
        );
    }

    #[test]
    fn publish_stage_skips_check_when_run_id_is_local() {
        // Build a context whose `derive_run_id` returns "local" — that
        // requires `git_info == None` (the no-git fallback path).
        // `TestContextBuilder` always populates git_info, so we drop
        // it explicitly here.
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let dist = tmp.path().join("dist");
        std::fs::create_dir_all(&dist).expect("mkdir dist");
        let mut ctx = anodizer_core::test_helpers::TestContextBuilder::new()
            .dist(dist)
            .build();
        ctx.git_info = None;
        // Pre-condition: the derived id is "local".
        assert_eq!(derive_run_id(&ctx), "local");

        // Seed a stale `run-local/report.json` (the kind of file an
        // earlier `cargo test` run might leave behind in shared CI).
        let dir = ctx.config.dist.join("run-local");
        std::fs::create_dir_all(&dir).expect("mkdir run-local");
        std::fs::write(dir.join("report.json"), "{}").expect("write");

        refuse_rerun_if_report_exists(&ctx).expect(
            "the 'local' run_id is the no-git fallback; the guard must \
                not produce false positives across unrelated CI runs",
        );
    }
}
