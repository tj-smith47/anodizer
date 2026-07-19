//! Publish run-report persistence: `run_id` derivation, `<dist>/run-<id>/`
//! path helpers, prior-report load, the end-of-pipeline report writer, and the
//! rerun-refusal guard.

use std::path::PathBuf;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Result;

use crate::rollback_only;

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

/// Resolve `<dist>/run-<id>/` for a derived `run_id` — formatted with
/// [`anodizer_core::dist::RUN_DIR_PREFIX`], the same constant the
/// run-summary scanner matches on. Shared by [`report_path_for`],
/// [`run_summary::summary_path`], [`rollback_only`], and the writer in
/// [`write_report_to_run_dir`]. Anchors on
/// `ctx.config.dist`, which per-crate workspace mode re-anchors onto
/// `dist/<crate>/`, so the helper composes correctly across every config mode.
pub fn run_dir(ctx: &Context, run_id: &str) -> PathBuf {
    ctx.config
        .dist
        .join(format!("{}{run_id}", anodizer_core::dist::RUN_DIR_PREFIX))
}

/// Resolve `<dist>/run-<id>/report.json` for a derived `run_id`. Pure
/// path helper kept alongside [`derive_run_id`] so consumers driving
/// the announce-only flow share the same path-shape contract as the
/// writer in [`write_report_to_run_dir`] and the reader in
/// [`rollback_only::run`].
pub fn report_path_for(ctx: &Context, run_id: &str) -> PathBuf {
    run_dir(ctx, run_id).join(anodizer_core::dist::REPORT_JSON)
}

/// This run's `report.json` path, only when [`write_report_to_run_dir`]
/// can have written it: mirrors the writer's gates (snapshot / dry-run /
/// empty report / invalid run id) AND requires the file to exist on disk,
/// so a hook handed `$ANODIZER_RUN_REPORT` never reads a stale file left
/// by an earlier run of the same tag.
pub(crate) fn existing_run_report_path(ctx: &Context) -> Option<PathBuf> {
    if ctx.is_snapshot() || ctx.is_dry_run() {
        return None;
    }
    let report = ctx.publish_report()?;
    if report.results.is_empty() {
        return None;
    }
    let run_id = derive_run_id(ctx);
    rollback_only::validate_run_id(&run_id).ok()?;
    let path = report_path_for(ctx, &run_id);
    path.exists().then_some(path)
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
            "skipped run-report write — derived run_id '{}' failed validation: {}",
            run_id, e,
        ));
        return;
    }

    let dir: PathBuf = run_dir(ctx, &run_id);
    let path = dir.join(anodizer_core::dist::REPORT_JSON);

    if let Err(e) = std::fs::create_dir_all(&dir) {
        log.warn(&format!(
            "failed to create run-report dir {}: {}",
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
                "failed to serialize run-report for {}: {}",
                path.display(),
                e,
            ));
            return;
        }
    };

    if let Err(e) = anodizer_core::fs_atomic::atomic_write_str(&path, &text) {
        log.warn(&format!(
            "failed to write run-report to {}: {}",
            path.display(),
            e,
        ));
        return;
    }

    log.status(&format!("wrote run-report to {}", path.display()));
}

/// Refuse to re-run `PublishStage::run` when a prior end-of-pipeline
/// `report.json` already exists for the current `run_id`. Returns
/// `Ok(())` on first run (no report yet), when `--allow-rerun` is
/// set, or under any of the skip conditions documented at the call
/// site (snapshot / dry-run / rollback-only / run_id == "local").
pub(crate) fn refuse_rerun_if_report_exists(ctx: &Context) -> Result<()> {
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
