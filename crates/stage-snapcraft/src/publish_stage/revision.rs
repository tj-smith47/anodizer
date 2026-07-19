use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::config::CrateConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::run::run_capture_timeout;

use crate::command::{
    missing_channels_for_version, snap_revision_for_version, snapcraft_list_revisions_command,
    snapcraft_release_command,
};

use super::*;

// ---------------------------------------------------------------------------
// Pre-pass: render every config's `publish.skip` template upfront so a
// template error fast-fails as a stage error before any upload begins.
// ---------------------------------------------------------------------------

/// Per-config skip flag, indexed parallel to the
/// `(crate_index, snap_cfg_index)` ordering of
/// `crates[].snapcrafts[]`. `run_uploads` indexes into this Vec rather
/// than re-rendering the template inside the upload loop.
pub(crate) fn render_skip_decisions(ctx: &Context, crates: &[CrateConfig]) -> Result<Vec<bool>> {
    let mut decisions = Vec::new();
    for krate in crates {
        let Some(snap_configs) = krate.snapcrafts.as_ref() else {
            continue;
        };
        for snap_cfg in snap_configs {
            let skip = if let Some(ref d) = snap_cfg.skip {
                d.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| {
                        format!(
                            "snapcraft: render publish.skip template for crate {}",
                            krate.name
                        )
                    })?
            } else {
                false
            };
            // Treat `if:` as another skip-decision: when the gate is falsy,
            // skip this snap from the publish phase too.
            let if_skip = !anodizer_core::config::evaluate_if_condition(
                snap_cfg.if_condition.as_deref(),
                &format!("snapcraft publish for crate '{}'", krate.name),
                |t| ctx.render_template(t),
            )?;
            decisions.push(skip || if_skip);
        }
    }
    Ok(decisions)
}

/// Resolve the Snap Store name for a config, mirroring
/// [`crate::generate::generate_snap_yaml`]: explicit `name`, else the project
/// name, else the primary binary. Used to address the `list-revisions` probe.
pub(crate) fn resolve_snap_name(
    snap_cfg: &anodizer_core::config::SnapcraftConfig,
    project_name: &str,
    primary_binary: &str,
) -> String {
    snap_cfg.name.clone().unwrap_or_else(|| {
        if project_name.is_empty() {
            primary_binary.to_string()
        } else {
            project_name.to_string()
        }
    })
}

/// Idempotency probe for a snap version: does a revision for `version`
/// already exist, and if so, which of `channels` is it NOT yet released to?
///
/// Runs `snapcraft list-revisions <name>` and parses its tabular output:
/// - `None`                    → either no revision exists for `version` yet
///   (genuine first upload) or the probe itself failed (snapcraft missing,
///   not logged in, snap not registered, network error) → proceed to
///   upload. Never falsely skip a genuine first publish just because the
///   existence check couldn't run; a true auth/network problem resurfaces
///   from the upload itself.
/// - `Some((revision, missing))` where `missing` is empty → the revision
///   already occupies every requested channel — a true re-run at an
///   already-published version → skip (re-uploading would only mint a
///   duplicate Snap Store revision, or hit the content-dedup rejection).
/// - `Some((revision, missing))` where `missing` is non-empty → the bytes
///   were already uploaded (by this run's own earlier attempt, or a prior
///   interrupted run) but never released to one or more configured
///   channels — an orphaned revision. The caller must promote it into
///   `missing` rather than skip (which would silently leave it unpublished)
///   or re-upload (which would only hit the Store's content-dedup
///   rejection).
pub(crate) fn revision_missing_channels(
    snap_name: &str,
    version: &str,
    arch: &str,
    channels: &[String],
    log: &StageLogger,
) -> Option<(String, Vec<String>)> {
    let args = snapcraft_list_revisions_command(snap_name);
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    let output = match run_capture_timeout(
        &mut cmd,
        log,
        "snapcraft list-revisions",
        SNAPCRAFT_PROBE_TIMEOUT,
    ) {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            // Non-zero exit: snap not registered, not logged in, etc. Can't
            // prove the revision exists, so let the upload proceed.
            log.debug(&format!(
                "snapcraft list-revisions for '{}' exited non-zero ({}); proceeding with upload",
                snap_name, o.status
            ));
            return None;
        }
        Err(e) => {
            // Spawn failure or a deadline kill (probe stalled). Either way the
            // existence check couldn't run — proceed to upload rather than
            // falsely skip; a real auth/network fault resurfaces (bounded) from
            // the upload itself.
            log.debug(&format!(
                "snapcraft list-revisions for '{}' could not run ({}); proceeding with upload",
                snap_name, e
            ));
            return None;
        }
    };
    let combined = log.redact(&String::from_utf8_lossy(&output.stdout));
    let (revision, missing) = missing_channels_for_version(&combined, version, arch, channels)?;
    Some((
        revision,
        missing.into_iter().map(|s| s.to_string()).collect(),
    ))
}

/// After a content-dedup upload rejection, re-query `snapcraft
/// list-revisions` for the revision whose `Version` column equals `version`
/// — the revision that collided with the bytes we just tried to upload.
///
/// A dedup rejection at the SAME version most commonly means an earlier
/// attempt (this run's own retry loop, or a prior failed run) already
/// landed those exact bytes server-side as an orphaned, unreleased
/// revision — the client observed a transient failure (e.g. a 5xx) and
/// retried, but the Store had already ingested the upload. Naming that
/// revision here is what lets the caller promote it instead of reporting a
/// permanent "repack" error for content that, from the operator's
/// perspective, never actually changed.
///
/// `arch` scopes the match to the artifact's own architecture-specific
/// revision — a dual-arch snap mints one revision per arch per version, so
/// matching on `version` alone could name a different arch's revision and
/// have the caller promote (or skip re-uploading) the wrong artifact.
///
/// Returns `None` when the probe itself fails (mirrors
/// [`revision_missing_channels`]'s fail-open stance) or when no revision
/// matches both `version` and `arch` — the latter means the collision is
/// against a DIFFERENT version's (or a different arch's) bytes, so there is
/// nothing to promote.
pub(crate) fn find_colliding_revision(
    snap_name: &str,
    version: &str,
    arch: &str,
    log: &StageLogger,
) -> Option<String> {
    probe_revision_for_version(snap_name, version, arch, log)
}

/// After a successful upload/promotion, resolve the revision the Snap Store now
/// lists for (`version`, `arch`) — the revision this run's artifact
/// corresponds to — so it can be recorded in the run evidence for a later
/// `promote --from-run`. Returns `None` when the probe cannot run or names no
/// matching revision; the evidence then records the arch without a revision
/// (the store version-map probe in verify-release still covers the landing
/// check).
pub(crate) fn resolve_recorded_revision(
    snap_name: &str,
    version: &str,
    arch: &str,
    log: &StageLogger,
) -> Option<String> {
    probe_revision_for_version(snap_name, version, arch, log)
}

/// Run `snapcraft list-revisions <name>` (bounded) and return the numerically
/// highest revision whose `Version` and `Arches` columns match. Shared by the
/// content-dedup collision lookup and the post-upload evidence capture. `None`
/// on a probe that could not run, a non-zero exit, or no matching row.
fn probe_revision_for_version(
    snap_name: &str,
    version: &str,
    arch: &str,
    log: &StageLogger,
) -> Option<String> {
    let args = snapcraft_list_revisions_command(snap_name);
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    let output = run_capture_timeout(
        &mut cmd,
        log,
        "snapcraft list-revisions",
        SNAPCRAFT_PROBE_TIMEOUT,
    )
    .ok()?;
    if !output.status.success() {
        log.debug(&format!(
            "snapcraft list-revisions for '{snap_name}' exited non-zero while resolving the \
             revision for {version} ({arch}); cannot identify it"
        ));
        return None;
    }
    let combined = log.redact(&String::from_utf8_lossy(&output.stdout));
    snap_revision_for_version(&combined, version, Some(arch))
}

/// Promote `revision` into every channel in `channels` via `snapcraft
/// release`, stopping at the first failure. Used to recover a dedup
/// rejection whose colliding revision matches the version being published
/// — the bytes already landed, they just were never released.
pub(crate) fn promote_revision(
    snap_name: &str,
    revision: &str,
    channels: &[String],
    log: &StageLogger,
) -> Result<()> {
    for channel in channels {
        let args = snapcraft_release_command(snap_name, revision, channel);
        log.verbose(&format!("running {}", args.join(" ")));
        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..]);
        let output = run_capture_timeout(
            &mut cmd,
            log,
            "snapcraft release",
            SNAPCRAFT_UPLOAD_TIMEOUT,
        )
        .with_context(|| {
            format!("execute snapcraft release for '{snap_name}' revision {revision} -> {channel}")
        })?;
        log.check_output(output, "snapcraft release")?;
    }
    Ok(())
}
