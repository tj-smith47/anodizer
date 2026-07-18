use std::ops::ControlFlow;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::Artifact;
use anodizer_core::config::CrateConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{RetryLog, is_retriable, retry_sync_deadline};
use anodizer_core::run::run_capture_timeout;

use crate::arch::triple_to_snap_arch;
use crate::command::{
    first_channel_rejected_for_prerelease_snap, is_content_dedup_rejection, is_retriable_snap_push,
    resolve_effective_channels, snapcraft_upload_command,
};
use crate::targets::SnapcraftTarget;

use super::*;

// ---------------------------------------------------------------------------
// Upload loop, extracted from `Stage::run` so the
// (attempted, exec_result) seam is testable in isolation.
// ---------------------------------------------------------------------------

/// Run the snapcraft upload loop and report
/// `(attempted_any_upload, Result<()>)`.
///
/// `attempted_any_upload` becomes `true` the first time we materialize a
/// non-dry-run upload (i.e. shell out to `snapcraft upload`). Callers
/// use it to decide whether to record a `PublisherResult`:
/// - `attempted = false` → nothing to report (BlobStage parity); the
///   `Result<()>` is bubbled as a stage error.
/// - `attempted = true`  → fold the `Result<()>` into a
///   `Succeeded`/`Failed(_)` outcome.
///
/// `skip_decisions` is the pre-pass `publish.skip` flag per
/// `(crate, snap_cfg)` tuple in iteration order — keeps this loop free
/// of template-render side effects.
pub(crate) fn run_uploads(
    ctx: &Context,
    crates: &[CrateConfig],
    snap_artifacts: &[Artifact],
    skip_decisions: &[bool],
    log: &StageLogger,
    targets: &mut [SnapcraftTarget],
) -> SnapUploadOutcome {
    let dry_run = ctx.options.dry_run;
    // Wrap snapcraft upload in retry.
    // commit eb944f9 (`isRetriableSnapPush`): 5xx Store responses
    // (500/502/503/504) are transient, every other failure is fatal.
    let retry_policy = ctx.retry_policy();
    let retry_policy = &retry_policy;
    let mut attempted_upload = false;
    let mut skipped_already_published = 0usize;
    let mut held_for_review: Vec<String> = Vec::new();
    let version = ctx.version();
    let project_name = ctx.config.project_name.clone();
    // IIFE captures `attempted_upload` by mutable reference so the
    // `?`-early-exit on per-target failure preserves the "anything
    // attempted before this point?" answer for the caller.
    let result: Result<()> = (|| -> Result<()> {
        let mut decision_idx = 0usize;
        for krate in crates {
            let Some(snap_configs) = krate.snapcrafts.as_ref() else {
                continue;
            };

            for snap_cfg in snap_configs {
                // Pull the pre-rendered skip flag in parallel with the
                // iteration — every `(crate, snap_cfg)` tuple contributes
                // exactly one entry to `skip_decisions`.
                let cfg_skip = skip_decisions.get(decision_idx).copied().unwrap_or(false);
                decision_idx += 1;

                // Only publish configs that opt in
                if !snap_cfg.publish.unwrap_or(false) {
                    continue;
                }
                if cfg_skip {
                    continue;
                }

                // Find snap artifacts for this crate (optionally filtered by id)
                let matching: Vec<&Artifact> = snap_artifacts
                    .iter()
                    .filter(|a| a.crate_name == krate.name)
                    .filter(|a| {
                        if let Some(ref filter_id) = snap_cfg.id {
                            a.metadata
                                .get("id")
                                .map(|id| id == filter_id)
                                .unwrap_or(false)
                        } else {
                            true
                        }
                    })
                    .collect();

                for artifact in &matching {
                    let snap_path = artifact.path.to_string_lossy();
                    // Scopes the idempotency/dedup probes below to this
                    // artifact's own architecture — a dual-arch snap config
                    // (`crates:` targeting both x86_64 and aarch64) mints one
                    // `list-revisions` row per arch per version, and matching
                    // on version alone would let one arch's already-released
                    // revision falsely skip a sibling arch's upload.
                    let arch = triple_to_snap_arch(artifact.target.as_deref().unwrap_or(""));

                    // Each channel template is rendered through the template
                    // engine, dropping only empty results. A render error
                    // propagates rather than silently vanishing a channel from
                    // the --release set (which would mis-route the upload).
                    let rendered_channels: Option<Vec<String>> =
                        match snap_cfg.channel_templates.as_ref() {
                            Some(templates) => {
                                let mut out = Vec::new();
                                for tmpl in templates {
                                    let rendered =
                                        ctx.render_template(tmpl).with_context(|| {
                                            format!("snapcraft: render channel template '{tmpl}'")
                                        })?;
                                    if !rendered.is_empty() {
                                        out.push(rendered);
                                    }
                                }
                                Some(out)
                            }
                            None => None,
                        };
                    // grade is also rendered through the template engine; a
                    // malformed grade must fail, not feed a raw `{{...}}` string
                    // into channel resolution.
                    let rendered_grade: Option<String> = snap_cfg
                        .grade
                        .as_deref()
                        .map(|g| {
                            ctx.render_template(g)
                                .with_context(|| format!("snapcraft: render grade template '{g}'"))
                        })
                        .transpose()?;
                    let effective_channels = resolve_effective_channels(
                        rendered_channels.as_deref(),
                        rendered_grade.as_deref(),
                        snap_cfg.confinement.as_deref(),
                    );

                    // Re-check the RENDERED channels/grade against the same
                    // pre-release restriction the build stage validates on
                    // the raw, unrendered `channel_templates`. A template
                    // that resolves to a forbidden channel only at publish
                    // time (or a `--publish-only` run, which never executes
                    // the build stage's preflight at all) must still be
                    // caught here, before the Snap Store ever sees the
                    // upload.
                    let confinement_is_devmode = snap_cfg.confinement.as_deref() == Some("devmode");
                    let grade_is_devel = rendered_grade.as_deref() == Some("devel");
                    if let Some(channels) = effective_channels.as_deref()
                        && (confinement_is_devmode || grade_is_devel)
                        && let Some(rejected) = first_channel_rejected_for_prerelease_snap(channels)
                    {
                        let reason = match (confinement_is_devmode, grade_is_devel) {
                            (true, true) => "devmode confinement and devel grade",
                            (true, false) => "devmode confinement",
                            (false, true) => "devel grade",
                            (false, false) => unreachable!("guarded by the outer if"),
                        };
                        anyhow::bail!(
                            "snapcraft: crate '{}' configures {reason} together with channel \
                             '{rejected}', which the Snap Store rejects — a snap with {reason} \
                             may only be pushed to pre-release channels (edge, beta). Remove \
                             '{rejected}' from channel_templates or drop the setting that \
                             produces {reason}.",
                            krate.name
                        );
                    }

                    let upload_args =
                        snapcraft_upload_command(&snap_path, effective_channels.as_deref());

                    if dry_run {
                        log.status(&format!("(dry-run) would run: {}", upload_args.join(" "),));
                        continue;
                    }

                    // Idempotency probe: the Snap Store mints a fresh revision
                    // on every upload, so re-running a release at an
                    // already-published version would create a duplicate
                    // revision (and, since the Store dedups on content, the
                    // re-upload would only hit a content-dedup rejection
                    // anyway). A revision uploaded but never released to a
                    // configured channel — orphaned by an earlier
                    // interrupted run — must be promoted, not silently
                    // reported as already published while nothing is
                    // actually live in that channel.
                    let snap_name = resolve_snap_name(
                        snap_cfg,
                        &project_name,
                        &crate::targets::crate_primary_binary(krate),
                    );
                    let probe_channels = effective_channels.clone().unwrap_or_default();
                    match revision_missing_channels(
                        &snap_name,
                        &version,
                        arch,
                        &probe_channels,
                        log,
                    ) {
                        None => {}
                        Some((_, missing)) if missing.is_empty() => {
                            log.status(&format!(
                                "skipped snapcraft '{}' {} — revision already published in the Snap Store",
                                snap_name, version
                            ));
                            skipped_already_published += 1;
                            continue;
                        }
                        Some((revision, missing)) => {
                            attempted_upload = true;
                            promote_revision(&snap_name, &revision, &missing, log).with_context(
                                || {
                                    format!(
                                        "promoting existing snap '{snap_name}' revision {revision} \
                                         to {missing:?} — uploaded by a prior run but never released"
                                    )
                                },
                            )?;
                            log.status(&format!(
                                "promoted snap '{}' {} (revision {}) to {} — recovered an \
                                 already-uploaded revision from a prior run that was never released",
                                snap_name,
                                version,
                                revision,
                                missing.join(", ")
                            ));
                            continue;
                        }
                    }

                    attempted_upload = true;
                    // Set from inside the retry closure when the store answers
                    // with a manual-review hold (Cell because the closure is
                    // re-entered per attempt while the outer scope still reads
                    // the flag afterwards).
                    let review_hold = std::cell::Cell::new(false);
                    // Set from inside the retry closure when a content-dedup
                    // rejection was recovered via promotion rather than a
                    // fresh upload — the final default status line below
                    // must say "promoted", not "uploaded", or the operator
                    // reads a fabricated upload that never happened.
                    let promoted = std::cell::Cell::new(false);
                    // A snapcraft push is idempotent for retry purposes: the
                    // revision-already-published probe above skips a re-run, so
                    // re-issuing the upload after a transient 5xx/network blip
                    // is safe. Keep a transient-error retry floor even when a
                    // stateful mode (`--publish-only`) resolves `attempts: 1`,
                    // mirroring the HTTP-upload, blob, and GitHub asset floors.
                    let upload_policy = retry_policy.with_idempotent_floor();
                    let max_attempts = upload_policy.max_attempts;
                    let retry_desc = format!("snapcraft upload '{snap_name}'");
                    retry_sync_deadline(
                        RetryLog::new(&retry_desc, log),
                        &upload_policy,
                        ctx.retry_deadline(),
                        |attempt| {
                            // Start-of-attempt marker at default visibility: the
                            // upload is a captured subprocess that can run for many
                            // minutes with no other output, so without this line the
                            // stage looks hung until the first attempt ENDS.
                            log.status(&format!(
                                "uploading snap '{}' to the Snap Store (attempt {}/{})",
                                snap_name, attempt, max_attempts
                            )); // status-ok: per-attempt start of a multi-minute captured upload
                            log.verbose(&format!("running {}", upload_args.join(" ")));
                            let mut cmd = Command::new(&upload_args[0]);
                            cmd.args(&upload_args[1..]);
                            let upload_output = match run_capture_timeout(
                                &mut cmd,
                                log,
                                "snapcraft upload",
                                SNAPCRAFT_UPLOAD_TIMEOUT,
                            ) {
                                Ok(o) => o,
                                Err(e) => {
                                    let e = e.context(format!(
                                        "execute snapcraft upload for crate {} snap {}",
                                        krate.name, snap_path
                                    ));
                                    // A deadline kill (upload stalled) is wrapped
                                    // Retriable → retry within budget rather than
                                    // hang forever. A spawn failure (binary missing,
                                    // permission denied) is not transient → break
                                    // without burning the retry budget.
                                    if is_retriable(e.as_ref()) {
                                        return Err(ControlFlow::Continue(e));
                                    }
                                    return Err(ControlFlow::Break(e));
                                }
                            };

                            if upload_output.status.success() {
                                return Ok(());
                            }

                            // Review-pending responses from the Snap Store should be
                            // warnings, not fatal errors — the snap was uploaded
                            // successfully but needs human review. Only the two
                            // genuine review markers flip the hold flag;
                            // "Waiting for previous upload" is a transient
                            // store-processing conflict with no review queued,
                            // so stamping it as HELD would send the operator to
                            // an empty dashboard review queue (the landing
                            // check still reports it honestly if the version
                            // never goes live).
                            const REVIEW_HOLD_STRINGS: &[&str] =
                                &["A human will soon review your snap", "(NEEDS REVIEW)"];
                            const REVIEW_PENDING_STRINGS: &[&str] = &[
                                "Waiting for previous upload",
                                "A human will soon review your snap",
                                "(NEEDS REVIEW)",
                            ];
                            // Redact before any warn-logging — `log.warn` ships
                            // straight to the operator, whereas `log.check_output`
                            // (called below on the fall-through path) already
                            // redacts internally. Without this hop the
                            // review-pending branch would leak any auth tokens
                            // snapcraft happens to echo on stderr.
                            let combined = log.redact(&format!(
                                "{}{}",
                                String::from_utf8_lossy(&upload_output.stdout),
                                String::from_utf8_lossy(&upload_output.stderr),
                            ));
                            if REVIEW_PENDING_STRINGS.iter().any(|s| combined.contains(s)) {
                                log.warn(&format!(
                                    "snap upload pending review — {}",
                                    combined.trim()
                                ));
                                if REVIEW_HOLD_STRINGS.iter().any(|s| combined.contains(s)) {
                                    review_hold.set(true);
                                }
                                return Ok(());
                            }

                            let err = match log.check_output(upload_output, "snapcraft upload") {
                                Ok(_) => return Ok(()),
                                Err(e) => e,
                            };
                            // The 5xx-retriable classifier is checked FIRST,
                            // ahead of the dedup classifier: the two are not
                            // mutually exclusive. Observed in the wild — the
                            // attempt that returns a 5xx can still have
                            // landed the bytes server-side, so the very next
                            // (automatic) retry gets rejected as a duplicate
                            // of what it already ingested, and its output
                            // carries both markers. Checking dedup first
                            // would permanently fast-fail a purely transient
                            // failure.
                            if is_retriable_snap_push(&combined) {
                                return Err(ControlFlow::Continue(err));
                            }
                            if is_content_dedup_rejection(&combined) {
                                // The Store deduplicates on the uploaded
                                // bytes' content hash, not on the
                                // caller-supplied version string. Two
                                // distinct situations produce this
                                // rejection:
                                //
                                // 1. An EARLIER attempt (this run's own retry
                                //    loop, or a prior failed run) already
                                //    landed these exact bytes server-side as
                                //    an orphaned revision that was never
                                //    released to any channel — the fix is to
                                //    PROMOTE that revision, not repack.
                                // 2. The `.snap` genuinely collides with a
                                //    DIFFERENT already-released version's
                                //    bytes — no revision exists at the
                                //    CURRENT version, so there is nothing to
                                //    promote and the package contents must
                                //    change.
                                //
                                // `snapcraft list-revisions` disambiguates:
                                // a revision whose Version column equals the
                                // version being published names case 1.
                                if let Some(revision) =
                                    find_colliding_revision(&snap_name, &version, arch, log)
                                {
                                    let promote_channels =
                                        effective_channels.clone().unwrap_or_default();
                                    if promote_channels.is_empty() {
                                        return Err(ControlFlow::Break(err.context(format!(
                                            "snap upload rejected: content-identical to \
                                             revision {revision} of {snap_name} {version} \
                                             already in the Snap Store, but no channel is \
                                             configured to promote it into — raw snapcraft \
                                             output: {combined}"
                                        ))));
                                    }
                                    return match promote_revision(
                                        &snap_name,
                                        &revision,
                                        &promote_channels,
                                        log,
                                    ) {
                                        Ok(()) => {
                                            promoted.set(true);
                                            log.status(&format!(
                                                "promoted snap {snap_name} {version} \
                                                 (revision {revision}) to {} — recovered from \
                                                 a content-dedup rejection: an earlier attempt \
                                                 had already landed this version's bytes \
                                                 without releasing them",
                                                promote_channels.join(",")
                                            ));
                                            Ok(())
                                        }
                                        Err(promote_err) => {
                                            Err(ControlFlow::Break(err.context(format!(
                                                "snap upload rejected as content-identical to \
                                                 revision {revision} (already-uploaded bytes \
                                                 for {snap_name} {version}), and promoting \
                                                 that revision to {} also failed: \
                                                 {promote_err:#} — raw snapcraft output: \
                                                 {combined}",
                                                promote_channels.join(",")
                                            ))))
                                        }
                                    };
                                }
                                log.warn(&format!(
                                    "snap {snap_name} {version} rejected by Snap Store: \
                                     content-identical to an already-uploaded revision, but \
                                     `snapcraft list-revisions` found no revision at version \
                                     {version} — the collision is against a DIFFERENT \
                                     already-released version's bytes; retrying will not \
                                     help, the package contents must change"
                                ));
                                return Err(ControlFlow::Break(err.context(format!(
                                    "snap upload rejected: content-identical to an \
                                     already-uploaded revision (Snap Store binary_sha3_384 \
                                     dedup) at a version OTHER than {version} — retry will \
                                     not help, the .snap contents must change — raw \
                                     snapcraft output: {combined}"
                                ))));
                            }
                            // Auth failures, malformed snap, quota errors, etc.
                            // fast-fail without burning retry budget.
                            Err(ControlFlow::Break(
                                err.context(format!("raw snapcraft output: {combined}")),
                            ))
                        },
                    )?;
                    if review_hold.get() {
                        // A held upload is NOT a delivered release: the store
                        // accepted the binary but ships nothing until a human
                        // approves it, and a rejection arrives only by email.
                        // Say so at default visibility instead of the
                        // "uploaded" wording, and record the hold on the
                        // evidence snapshot so verify-release and
                        // `--rollback-only --from-run` see the open state.
                        log.warn(&format!(
                            "snap {snap_name} {version} HELD for Snap Store manual review — \
                             not live in any channel until review approves \
                             (https://dashboard.snapcraft.io/snaps/{snap_name}/)"
                        ));
                        held_for_review.push(format!("{snap_name} {version}"));
                        for t in targets
                            .iter_mut()
                            .filter(|t| t.crate_name == krate.name && t.package_name == snap_name)
                        {
                            t.held_for_review = true;
                        }
                    } else if !promoted.get() {
                        log.status(&format!("uploaded snap {} {}", snap_name, version));
                    }
                }
            }
        }
        Ok(())
    })();

    SnapUploadOutcome {
        attempted: attempted_upload,
        skipped_already_published,
        held_for_review,
        result,
    }
}

/// Result of [`run_uploads`]: whether any upload was attempted, how many snaps
/// were skipped because their version was already published, and the
/// upload-phase result.
pub(crate) struct SnapUploadOutcome {
    pub(crate) attempted: bool,
    pub(crate) skipped_already_published: usize,
    /// `"<snap> <version>"` per upload the store answered with a
    /// manual-review hold — accepted but not live until a human approves.
    pub(crate) held_for_review: Vec<String>,
    pub(crate) result: Result<()>,
}
