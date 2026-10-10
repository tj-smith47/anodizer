//! Publish run-report persistence: `run_id` derivation, `<dist>/run-<id>/`
//! path helpers, prior-report load, the end-of-pipeline report writer, and the
//! rerun-refusal guard.

use std::path::PathBuf;

use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::Result;

use crate::rollback;

/// Derive a stable per-run identifier suitable for the
/// `<dist>/run-<id>/` directory written by [`write_report_to_run_dir`]
/// and read back by [`rollback::run`].
///
/// Priority order:
/// 1. `ctx.git_info.tag` — what the operator typed (e.g. `v0.2.1`).
///    Every tag shape anodizer cuts — including a semver build-metadata
///    suffix like `v1.2.3+build.1` — fits the `[A-Za-z0-9._+-]` charset
///    [`rollback::validate_run_id`] enforces, so the tag branch is what
///    real releases take. That matters beyond tidiness: `anodizer tag
///    rollback` locates a run's recorded publisher state by probing
///    `run-<tag>/`, so a tag that fell through to the commit fallback here
///    would leave published state the unwind can never find.
/// 2. `ctx.git_info.short_commit` — fallback for snapshot / dry-run /
///    detached-HEAD scenarios where there's no tag.
/// 3. The literal `"local"` — final fallback for genuinely-no-git
///    contexts (e.g. some integration tests).
///
/// All three branches return a string that satisfies
/// [`rollback::validate_run_id`]; the candidates from `git_info`
/// are pre-filtered against the validator so a malformed value (e.g. a
/// short_commit somehow containing slashes) falls through to the next
/// step instead of producing an invalid path. The `"local"` literal is
/// fixed and always valid.
///
/// The `"local"` branch is the no-git fallback; for production releases
/// (which always have a tag or short_commit) it should never fire. Seeing
/// `dist/run-local/` in a real release is a signal that
/// `ctx.git_info` was not populated upstream and is worth
/// investigating before invoking `anodizer tag rollback`.
///
/// The derived id names the `<dist>/run-<id>/report.json` path. That
/// report is written only outside snapshot / dry-run mode, so any
/// code deriving the id independently of the write path should also
/// gate on `ctx.is_snapshot()` / `ctx.is_dry_run()` to match that
/// behavior.
pub fn derive_run_id(ctx: &Context) -> String {
    if let Some(info) = ctx.git_info.as_ref() {
        if !info.tag.is_empty() && rollback::validate_run_id(&info.tag).is_ok() {
            return info.tag.clone();
        }
        if !info.short_commit.is_empty() && rollback::validate_run_id(&info.short_commit).is_ok() {
            return info.short_commit.clone();
        }
    }
    NO_GIT_RUN_ID.to_string()
}

/// The run id a context with no git information falls back to. Two runs
/// under it need not belong to one release, so the report written under
/// it carries nothing from an earlier one and `release --clean` does not
/// keep it.
pub const NO_GIT_RUN_ID: &str = "local";

/// Resolve `<dist>/run-<id>/` for a derived `run_id` — formatted with
/// [`anodizer_core::dist::RUN_DIR_PREFIX`], the same constant the
/// run-summary scanner matches on. Shared by [`report_path_for`],
/// [`summary_path`](crate::run_summary::summary_path), [`rollback`], and the writer in
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
/// [`rollback::run`].
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
    rollback::validate_run_id(&run_id).ok()?;
    let path = report_path_for(ctx, &run_id);
    path.exists().then_some(path)
}

/// Load the prior run's `<dist>/run-<id>/report.json` into a
/// [`anodizer_core::publish_report::PublishReport`].
///
/// Errors when the file is missing or unparseable. The recovery hint
/// mirrors the message [`rollback::run`] produces because the two
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
             release run; re-run `anodizer release` end-to-end first so the \
             run dir exists.",
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

/// Fold into `report` what an earlier run of the same release recorded and
/// has not withdrawn, so `anodizer tag rollback` sees the whole release in
/// the one file it reads.
///
/// A release finished by re-running the command leaves its work spread
/// over several runs: the first uploads two objects and fails, the second
/// skips those two as identical and uploads the third. Each run records
/// only what it did, and each write replaces the file, so without this
/// the withdrawal would remove the third object and leave the first two.
///
/// Per earlier row that is still a rollback candidate with evidence:
///
/// - this run's row for the publisher recorded work of its own
///   (a candidate whose evidence
///   [`records_published_work`](anodizer_core::PublishEvidence::records_published_work))
///   and the two lists join
///   ([`PublishEvidence::absorb_prior`](anodizer_core::PublishEvidence::absorb_prior))
///   — one row names everything the release did;
/// - this run's row recorded work the earlier evidence cannot join (a
///   different evidence kind) — both stand, the earlier one in
///   `carried_forward`;
/// - this run did no work for the publisher (skipped as already
///   published, deselected, failed before committing anything, or absent)
///   — the earlier row rides in `carried_forward`.
///
/// A Submitter row is carried unless this run ran that publisher itself
/// (succeeded, failed, or skipped it as already published): its rollback
/// exists to undo a partial submission, and a run that reached the
/// publisher again decides afresh what that submission is.
pub(crate) fn carry_prior_work(
    report: &mut anodizer_core::publish_report::PublishReport,
    prior: &anodizer_core::publish_report::PublishReport,
) {
    use anodizer_core::{PublisherGroup, PublisherOutcome, SkipReason};
    for rows in [&prior.carried_forward, &prior.results] {
        for i in rollback::candidate_rows(rows) {
            let earlier = &rows[i];
            let Some(earlier_evidence) = earlier.evidence.as_ref() else {
                continue;
            };
            if earlier.group == PublisherGroup::Submitter {
                let ran_again = report.results.iter().any(|r| {
                    r.name == earlier.name
                        && matches!(
                            r.outcome,
                            PublisherOutcome::Succeeded
                                | PublisherOutcome::Failed(_)
                                | PublisherOutcome::Skipped(SkipReason::AlreadyPublished)
                        )
                });
                if !ran_again {
                    carry_row(report, earlier, earlier_evidence);
                }
                continue;
            }
            let mine = rollback::rollback_candidates(report)
                .into_iter()
                .find(|&c| report.results[c].name == earlier.name)
                .and_then(|c| report.results[c].evidence.as_mut())
                .filter(|e| e.records_published_work());
            let joined = mine.is_some_and(|mine| mine.absorb_prior(earlier_evidence));
            if !joined {
                carry_row(report, earlier, earlier_evidence);
            }
        }
    }
}

/// Put `earlier` in `carried_forward`, joining its evidence into a row
/// already carried for the same publisher when the two can join.
fn carry_row(
    report: &mut anodizer_core::publish_report::PublishReport,
    earlier: &anodizer_core::PublisherResult,
    earlier_evidence: &anodizer_core::PublishEvidence,
) {
    match report
        .carried_forward
        .iter_mut()
        .find(|r| r.name == earlier.name)
    {
        Some(carried) => {
            let joined = carried
                .evidence
                .as_mut()
                .is_some_and(|e| e.absorb_prior(earlier_evidence));
            if !joined {
                *carried = earlier.clone();
            }
        }
        None => report.carried_forward.push(earlier.clone()),
    }
}

/// Where an unreadable `report.json` is moved before a new one is written:
/// `report.json.unreadable-<unix seconds>` beside it.
fn unreadable_aside_path(path: &std::path::Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".unreadable-{stamp}"));
    path.with_file_name(name)
}

/// Persist `ctx.publish_report` to `<config.dist>/run-<run_id>/report.json`
/// so a later `anodizer tag rollback` unwind can
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
/// Mirrors the path-derivation `rollback.rs` uses internally to locate
/// this same file — the two form the writer/reader contract that
/// [`rollback::run`] replays against.
///
/// Pretty-prints so operators reading the file directly do not have
/// to pipe through `jq`. A future contributor tempted to switch to
/// compact JSON for byte-size should know the format is part of the
/// operator-facing contract — `anodizer tag rollback` consumers may read it
/// by hand to triage which step failed before invoking the replay.
///
/// # Atomicity
///
/// Serializes to a `String` first via [`serde_json::to_string_pretty`]
/// and then commits with [`std::fs::write`]. A serialize-failure
/// (closed sums, malformed Unicode in a publisher name, ...) happens
/// BEFORE the target file is touched, so a partially-written /
/// truncated `report.json` cannot leak onto disk and trip a later
/// `anodizer tag rollback` parse error. Matches the sibling
/// pattern in `crates/stage-publish/src/run_summary.rs::write_summary_json`.
///
/// No explicit `fsync`: `fs::write` does not expose a sync hook, and
/// the in-repo convention (run_summary, every other JSON-emit site
/// across the stages) does not fsync either. The atomic-rename-style
/// safety `fs::write` provides covers the truncation hazard that
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
/// only on-disk state needed for `anodizer tag rollback`
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
    if let Err(e) = rollback::validate_run_id(&run_id) {
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

    let mut report = report.clone();
    let had_prior = run_id != NO_GIT_RUN_ID && rollback::prior_state_exists(ctx, &run_id);
    if had_prior {
        match rollback::load_prior_state(ctx, &run_id, None) {
            Ok(prior) => carry_prior_work(&mut report, &prior),
            // The file is the only record of what the earlier run published,
            // so it is moved aside for the operator rather than overwritten;
            // this run's own record is still written.
            Err(e) => {
                let aside = unreadable_aside_path(&path);
                match std::fs::rename(&path, &aside) {
                    Ok(()) => log.warn(&format!(
                        "moved the earlier run-report for '{run_id}' to {} — it could not be \
                         read to carry its published work forward: {e:#}",
                        aside.display(),
                    )),
                    Err(rename_err) => {
                        log.warn(&format!(
                            "kept the earlier run-report for '{run_id}' — it could not be read \
                             to carry its published work forward ({e:#}) and could not be moved \
                             to {}: {rename_err}",
                            aside.display(),
                        ));
                        return;
                    }
                }
            }
        }
    }
    let report = &report;

    // Serialize first, then write — so a serialize-failure cannot
    // leave a truncated/corrupt file on disk for the rollback replay
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

    // The pipeline end writes the report too, for the stages that record
    // rows without the publish stage (`--skip=publish`); after a write by
    // the publish stage the two are the same bytes, so the second is not
    // made and not announced.
    if had_prior && std::fs::read_to_string(&path).is_ok_and(|on_disk| on_disk == text) {
        return;
    }

    if let Err(e) = anodizer_core::fs_atomic::atomic_write_str(&path, &text) {
        log.warn(&format!(
            "failed to write run-report to {}: {}",
            path.display(),
            e,
        ));
        return;
    }

    // The replay state of an earlier withdrawal is folded into the report
    // just written; left behind, the next withdrawal would read it in
    // preference and never see this run.
    let stale = rollback::rollback_path(ctx, &run_id);
    if had_prior
        && stale.exists()
        && let Err(e) = std::fs::remove_file(&stale)
    {
        log.warn(&format!(
            "failed to remove superseded rollback state {}: {}; delete it before the next \
             `anodizer tag rollback`",
            stale.display(),
            e,
        ));
    }

    log.status(&format!("wrote run-report to {}", path.display()));
}

#[cfg(test)]
mod tests {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    /// The recovery hint must name the binary the operator actually has on
    /// PATH: the tool's former name resolves to nothing.
    #[test]
    fn missing_prior_report_names_the_real_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config {
            dist: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let ctx = Context::new(config, ContextOptions::default());
        let err = super::load_prior_report(&ctx, "nope")
            .expect_err("a missing report must bail with the recovery hint");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("`anodizer release`"),
            "the hint must name the anodizer binary; got {msg}"
        );
    }

    use anodizer_core::publish_evidence::{BlobExtra, BlobTargetSnapshot, PublishEvidenceExtra};
    use anodizer_core::publish_evidence::{HomebrewExtra, HomebrewTargetSnapshot};
    use anodizer_core::publish_report::PublishReport;
    use anodizer_core::{
        PublishEvidence, PublisherGroup, PublisherOutcome, PublisherResult, SkipReason,
    };

    fn blob_evidence(keys: &[&str]) -> PublishEvidence {
        let mut e = PublishEvidence::new("blob");
        e.artifact_paths = keys.iter().map(|k| format!("s3://b/{k}").into()).collect();
        e.extra = PublishEvidenceExtra::Blob(BlobExtra {
            blob_targets: keys
                .iter()
                .map(|k| BlobTargetSnapshot {
                    provider: "s3".into(),
                    bucket: "b".into(),
                    key: (*k).into(),
                    region: None,
                    endpoint: None,
                    overwrote: false,
                })
                .collect(),
        });
        e
    }

    fn row(
        name: &str,
        group: PublisherGroup,
        outcome: PublisherOutcome,
        evidence: Option<PublishEvidence>,
    ) -> PublisherResult {
        PublisherResult {
            name: name.into(),
            group,
            required: false,
            outcome,
            evidence,
            entry_skips: Vec::new(),
        }
    }

    fn report_of(rows: Vec<PublisherResult>) -> PublishReport {
        PublishReport {
            results: rows,
            ..Default::default()
        }
    }

    fn blob_keys(evidence: &PublishEvidence) -> Vec<String> {
        match &evidence.extra {
            PublishEvidenceExtra::Blob(b) => b.blob_targets.iter().map(|t| t.key.clone()).collect(),
            other => panic!("expected blob targets, got {other:?}"),
        }
    }

    fn tap_evidence(commits: &[&str]) -> PublishEvidence {
        let mut e = PublishEvidence::new("homebrew");
        e.extra = PublishEvidenceExtra::Homebrew(HomebrewExtra {
            homebrew_targets: commits
                .iter()
                .map(|c| HomebrewTargetSnapshot {
                    target: "app".into(),
                    repo_url: "https://github.com/o/tap.git".into(),
                    commit: Some((*c).into()),
                    ..Default::default()
                })
                .collect(),
        });
        e
    }

    fn tap_commits(evidence: &PublishEvidence) -> Vec<String> {
        match &evidence.extra {
            PublishEvidenceExtra::Homebrew(h) => h
                .homebrew_targets
                .iter()
                .map(|t| t.commit.clone().unwrap())
                .collect(),
            other => panic!("expected tap targets, got {other:?}"),
        }
    }

    /// The partial-failure sequence: two objects are uploaded and the third fails,
    /// then the re-run uploads only the third. The record after the re-run
    /// names all three, each once.
    #[test]
    fn a_rerun_joins_the_objects_the_failed_run_committed() {
        let prior = report_of(vec![row(
            "blob",
            PublisherGroup::Assets,
            PublisherOutcome::Failed("boom".into()),
            Some(blob_evidence(&["a", "b"])),
        )]);
        let mut current = report_of(vec![row(
            "blob",
            PublisherGroup::Assets,
            PublisherOutcome::Succeeded,
            Some(blob_evidence(&["c", "b"])),
        )]);

        super::carry_prior_work(&mut current, &prior);

        let evidence = current.results[0].evidence.as_ref().unwrap();
        assert_eq!(blob_keys(evidence), ["c", "b", "a"]);
        assert_eq!(evidence.artifact_paths.len(), 3);
        assert!(current.carried_forward.is_empty());
    }

    /// A re-run that found every object in place wrote nothing, so the
    /// earlier run's row is the only record of them. It survives a third
    /// run the same way.
    #[test]
    fn a_rerun_that_did_no_work_carries_the_earlier_row() {
        let prior = report_of(vec![row(
            "blob",
            PublisherGroup::Assets,
            PublisherOutcome::Succeeded,
            Some(blob_evidence(&["a", "b"])),
        )]);
        let skipped = || {
            report_of(vec![row(
                "blob",
                PublisherGroup::Assets,
                PublisherOutcome::Skipped(SkipReason::AlreadyPublished),
                None,
            )])
        };
        let mut second = skipped();
        super::carry_prior_work(&mut second, &prior);
        assert_eq!(second.carried_forward, prior.results);

        let mut third = skipped();
        super::carry_prior_work(&mut third, &second);
        assert_eq!(third.carried_forward, prior.results);

        // A run that then uploads one more object takes the carried ones in.
        let mut fourth = report_of(vec![row(
            "blob",
            PublisherGroup::Assets,
            PublisherOutcome::Succeeded,
            Some(blob_evidence(&["c"])),
        )]);
        super::carry_prior_work(&mut fourth, &third);
        assert_eq!(
            blob_keys(fourth.results[0].evidence.as_ref().unwrap()),
            ["c", "a", "b"]
        );
        assert!(fourth.carried_forward.is_empty());
    }

    /// A tap publisher's rollback reverts each commit the release pushed, so
    /// a second push joins the first under its own sha.
    #[test]
    fn a_tap_publisher_that_pushed_again_keeps_both_commits() {
        let prior = report_of(vec![row(
            "homebrew",
            PublisherGroup::Manager,
            PublisherOutcome::Succeeded,
            Some(tap_evidence(&["first"])),
        )]);
        let mut current = report_of(vec![row(
            "homebrew",
            PublisherGroup::Manager,
            PublisherOutcome::Succeeded,
            Some(tap_evidence(&["second"])),
        )]);

        super::carry_prior_work(&mut current, &prior);

        assert_eq!(
            tap_commits(current.results[0].evidence.as_ref().unwrap()),
            ["second", "first"]
        );
        assert!(current.carried_forward.is_empty());
    }

    /// A row of this run whose evidence names nothing it did — a failure
    /// before the first push, or a success whose record is empty — is no
    /// record of the earlier work, so the earlier row is carried beside it.
    #[test]
    fn a_rerun_row_that_records_no_work_does_not_replace_the_earlier_row() {
        let prior = report_of(vec![row(
            "homebrew",
            PublisherGroup::Manager,
            PublisherOutcome::Succeeded,
            Some(tap_evidence(&["first"])),
        )]);
        for outcome in [
            PublisherOutcome::Failed("clone failed".into()),
            PublisherOutcome::Succeeded,
        ] {
            let mut current = report_of(vec![row(
                "homebrew",
                PublisherGroup::Manager,
                outcome,
                Some(tap_evidence(&[])),
            )]);
            super::carry_prior_work(&mut current, &prior);
            assert_eq!(current.carried_forward, prior.results);
            assert_eq!(
                tap_commits(current.results[0].evidence.as_ref().unwrap()),
                Vec::<String>::new()
            );
        }
    }

    /// Evidence of another kind cannot join; the earlier row stays on
    /// record in `carried_forward` beside this run's.
    #[test]
    fn a_rerun_whose_evidence_cannot_join_keeps_the_earlier_row() {
        let prior = report_of(vec![row(
            "blob",
            PublisherGroup::Assets,
            PublisherOutcome::Succeeded,
            Some(blob_evidence(&["a"])),
        )]);
        let mut current = report_of(vec![row(
            "blob",
            PublisherGroup::Assets,
            PublisherOutcome::Succeeded,
            Some(tap_evidence(&["x"])),
        )]);

        super::carry_prior_work(&mut current, &prior);

        assert_eq!(current.carried_forward, prior.results);
        assert_eq!(current.results.len(), 1);
    }

    /// A re-run narrowed to one crate publishes that crate only; the other
    /// crate's push from the earlier run is still part of the release.
    #[test]
    fn a_rerun_for_one_crate_keeps_the_other_crates_pushes() {
        let mut first = tap_evidence(&["sha-a"]);
        match &mut first.extra {
            PublishEvidenceExtra::Homebrew(h) => h.homebrew_targets[0].target = "crate-a".into(),
            _ => unreachable!(),
        }
        let prior = report_of(vec![row(
            "homebrew",
            PublisherGroup::Manager,
            PublisherOutcome::Failed("crate-b failed".into()),
            Some(first),
        )]);
        let mut second = tap_evidence(&["sha-b"]);
        match &mut second.extra {
            PublishEvidenceExtra::Homebrew(h) => h.homebrew_targets[0].target = "crate-b".into(),
            _ => unreachable!(),
        }
        let mut current = report_of(vec![row(
            "homebrew",
            PublisherGroup::Manager,
            PublisherOutcome::Succeeded,
            Some(second),
        )]);

        super::carry_prior_work(&mut current, &prior);

        assert_eq!(
            tap_commits(current.results[0].evidence.as_ref().unwrap()),
            ["sha-b", "sha-a"]
        );
        assert!(current.carried_forward.is_empty());
    }

    /// A Submitter's partial publish stays on record until a run reaches
    /// that publisher again; a run that only republished something else
    /// has not decided anything about it.
    #[test]
    fn a_submitter_row_is_carried_until_the_publisher_runs_again() {
        let mut crate_evidence = PublishEvidence::new("cargo");
        crate_evidence.primary_ref = Some("crate-a".into());
        let prior = report_of(vec![row(
            "cargo",
            PublisherGroup::Submitter,
            PublisherOutcome::Failed("crate-b failed".into()),
            Some(crate_evidence),
        )]);

        let mut current = report_of(vec![row(
            "blob",
            PublisherGroup::Assets,
            PublisherOutcome::Succeeded,
            Some(blob_evidence(&["a"])),
        )]);
        super::carry_prior_work(&mut current, &prior);
        assert_eq!(current.carried_forward, prior.results);

        let mut deselected = report_of(vec![row(
            "cargo",
            PublisherGroup::Submitter,
            PublisherOutcome::Skipped(SkipReason::Deselected),
            None,
        )]);
        super::carry_prior_work(&mut deselected, &prior);
        assert_eq!(deselected.carried_forward, prior.results);

        for outcome in [
            PublisherOutcome::Succeeded,
            PublisherOutcome::Failed("again".into()),
            PublisherOutcome::Skipped(SkipReason::AlreadyPublished),
        ] {
            let mut ran = report_of(vec![row("cargo", PublisherGroup::Submitter, outcome, None)]);
            super::carry_prior_work(&mut ran, &prior);
            assert!(ran.carried_forward.is_empty());
        }
    }

    #[test]
    fn withdrawn_rows_and_rows_without_evidence_are_not_carried() {
        let prior = report_of(vec![
            row(
                "blob",
                PublisherGroup::Assets,
                PublisherOutcome::RolledBack,
                Some(blob_evidence(&["a"])),
            ),
            row(
                "uploads",
                PublisherGroup::Assets,
                PublisherOutcome::Failed("boom".into()),
                None,
            ),
        ]);
        let mut current = report_of(Vec::new());

        super::carry_prior_work(&mut current, &prior);

        assert!(current.carried_forward.is_empty());
    }

    fn ctx_reporting(dist: &std::path::Path, report: PublishReport) -> Context {
        let mut ctx = anodizer_core::test_helpers::TestContextBuilder::new()
            .tag("v0.0.0-test")
            .dist(dist.to_path_buf())
            .build();
        ctx.set_publish_report(report);
        ctx
    }

    /// The whole sequence through the files a withdrawal reads: a failed
    /// run, a re-run, a withdrawal, then a third run and its withdrawal.
    #[test]
    fn the_report_on_disk_keeps_every_object_across_runs_and_withdrawals() {
        let tmp = tempfile::tempdir().unwrap();
        let read = |name: &str| -> PublishReport {
            let path = tmp.path().join("run-v0.0.0-test").join(name);
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let blob_run = |outcome, keys: &[&str]| {
            let ctx = ctx_reporting(
                tmp.path(),
                report_of(vec![row(
                    "blob",
                    PublisherGroup::Assets,
                    outcome,
                    Some(blob_evidence(keys)),
                )]),
            );
            super::write_report_to_run_dir(&ctx, &ctx.logger("publish"));
            ctx
        };

        blob_run(PublisherOutcome::Failed("boom".into()), &["a", "b"]);
        assert_eq!(
            blob_keys(read("report.json").results[0].evidence.as_ref().unwrap()),
            ["a", "b"]
        );

        let mut ctx = blob_run(PublisherOutcome::Succeeded, &["c"]);
        assert_eq!(
            blob_keys(read("report.json").results[0].evidence.as_ref().unwrap()),
            ["c", "a", "b"]
        );

        let (blob, calls) = crate::testing::fake_counting("blob", PublisherGroup::Assets, false);
        let publishers: Vec<Box<dyn anodizer_core::Publisher>> = vec![blob];
        crate::rollback::run_with_publishers(&mut ctx, "v0.0.0-test", &publishers).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            read("rollback.json").results[0].outcome,
            PublisherOutcome::RolledBack
        );

        // A release after the withdrawal starts a new record: the withdrawn
        // objects are not carried, and the old replay state would otherwise
        // be read in preference to it.
        let mut ctx = blob_run(PublisherOutcome::Succeeded, &["d"]);
        assert!(!tmp.path().join("run-v0.0.0-test/rollback.json").exists());
        assert_eq!(
            blob_keys(read("report.json").results[0].evidence.as_ref().unwrap()),
            ["d"]
        );
        crate::rollback::run_with_publishers(&mut ctx, "v0.0.0-test", &publishers).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn an_unreadable_earlier_report_is_moved_aside_and_the_new_one_written() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run-v0.0.0-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("report.json"), "{ not json").unwrap();
        let ctx = ctx_reporting(
            tmp.path(),
            report_of(vec![row(
                "blob",
                PublisherGroup::Assets,
                PublisherOutcome::Succeeded,
                Some(blob_evidence(&["a"])),
            )]),
        );

        super::write_report_to_run_dir(&ctx, &ctx.logger("publish"));

        let written: PublishReport =
            serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(
            blob_keys(written.results[0].evidence.as_ref().unwrap()),
            ["a"]
        );
        assert!(written.carried_forward.is_empty());
        let aside: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("report.json.unreadable-"))
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join(&aside[0])).unwrap(),
            "{ not json"
        );
    }

    /// Without git information every run writes under one id, so the
    /// report written there is this run's alone.
    #[test]
    fn the_no_git_run_id_carries_nothing_from_an_earlier_report() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run-local");
        std::fs::create_dir_all(&dir).unwrap();
        let prior = report_of(vec![row(
            "blob",
            PublisherGroup::Assets,
            PublisherOutcome::Succeeded,
            Some(blob_evidence(&["a"])),
        )]);
        std::fs::write(
            dir.join("report.json"),
            serde_json::to_string(&prior).unwrap(),
        )
        .unwrap();
        let mut ctx = ctx_reporting(
            tmp.path(),
            report_of(vec![row(
                "blob",
                PublisherGroup::Assets,
                PublisherOutcome::Succeeded,
                Some(blob_evidence(&["b"])),
            )]),
        );
        ctx.git_info = None;
        assert_eq!(super::derive_run_id(&ctx), super::NO_GIT_RUN_ID);

        super::write_report_to_run_dir(&ctx, &ctx.logger("publish"));

        let written: PublishReport =
            serde_json::from_str(&std::fs::read_to_string(dir.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(
            blob_keys(written.results[0].evidence.as_ref().unwrap()),
            ["b"]
        );
        assert!(written.carried_forward.is_empty());
    }
}
