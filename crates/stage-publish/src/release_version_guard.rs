//! Non-release version publish guard.
//!
//! Refuses to ship a snapshot / dev / dirty / `0.0.0`-sentinel version to any
//! external publisher. Runs at the [`crate::PublishStage::run`] entrypoint —
//! before the first publisher fires — because several index publishers
//! (crates.io, Cloudsmith, Chocolatey, winget, AUR, …) are one-way doors and
//! a non-release version reaching them is essentially always a mistake.
//!
//! The canonical accident this prevents: a real-release CI run that resolved
//! `0.0.0~SNAPSHOT-<sha>` (a missing base `Version` rendered through the
//! snapshot template) and pushed six packages to a registry. The snapshot
//! *flag* is not the trigger — `--snapshot` already skips the publish stage
//! entirely via `skip_in_snapshot`. The trigger is the resolved *version
//! string*, evaluated here against [`anodizer_core::version::is_release_version`].
//!
//! The `--allow-snapshot-publish` flag (and the equivalent
//! [`anodizer_core::context::ContextOptions::allow_snapshot_publish`]) downgrades
//! the bail to a warning for the deliberate "publish a snapshot to a private
//! channel" case.

use anodizer_core::context::Context;
use anodizer_core::crate_scope::resolve_crate_tag;
use anodizer_core::log::StageLogger;
use anodizer_core::version::non_release_reason;
use anyhow::{Result, bail};

/// Refuse to publish when the resolved release version is non-release.
///
/// Evaluates the global resolved `Version` AND each in-scope crate's
/// per-crate resolved version (per-crate config mode renders its own version
/// from each crate's tag), so the guard is correct in all config modes —
/// single-crate, workspace-lockstep, and per-crate. The first non-release
/// version aborts with an actionable error naming the offending version and
/// the publishers that were about to run.
///
/// `--allow-snapshot-publish` downgrades the bail to a single warning and
/// proceeds. No-op when every resolved version is a genuine release version.
pub(crate) fn guard_release_version(
    ctx: &Context,
    log: &StageLogger,
    publisher_names: &[String],
) -> Result<()> {
    // The guard protects the EXTERNAL publish surface. Snapshot mode never
    // reaches here (`skip_in_snapshot` short-circuits the stage), and dry-run
    // publishers all no-op their network calls before any side effect — so a
    // non-release version in either mode produces no external leak and must not
    // abort the preview. Gate on real-release only, matching every other
    // external-effect gate in this stage.
    if ctx.is_dry_run() || ctx.is_snapshot() {
        return Ok(());
    }

    let Some((version, reason)) = first_non_release_version(ctx) else {
        return Ok(());
    };

    let publishers = if publisher_names.is_empty() {
        "(none configured)".to_string()
    } else {
        publisher_names.join(", ")
    };

    if ctx.options.allow_snapshot_publish {
        log.warn(&format!(
            "publishing non-release version '{version}' ({reason}) to: {publishers} \
             — proceeding because --allow-snapshot-publish was set. This version is \
             NOT a real release; only do this for a private/test channel.",
        ));
        return Ok(());
    }

    bail!(
        "publish: refusing to publish non-release version '{version}' ({reason}) to: \
         {publishers}. These publishers include one-way-door indexes; shipping a \
         snapshot / dev / 0.0.0 version is almost always a mistake (e.g. a missing \
         base Version rendered as '0.0.0~SNAPSHOT-<sha>'). Cut a real release with a \
         semver tag, or pass --allow-snapshot-publish to override (intended only for \
         a private/test channel).",
    );
}

/// The first non-release version found across the global resolved version and
/// every in-scope crate's per-crate resolved version, with the reason it is
/// non-release. `None` when every resolved version is a genuine release.
///
/// The global `Version` is checked first because it is what the snapshot
/// template stamps (`<base>-SNAPSHOT-<sha>`) and what the `0.0.0` sentinel
/// surfaces as — the exact accident class. Per-crate versions are then checked
/// so per-crate config mode (each crate rendering its own tag-derived version)
/// is covered, not just a single global.
fn first_non_release_version(ctx: &Context) -> Option<(String, &'static str)> {
    let global = ctx.version();
    if let Some(reason) = non_release_reason(&global) {
        return Some((global, reason));
    }

    // Per-crate: a crate may resolve its own version from its own tag in
    // per-crate config mode. A crate with no resolvable tag yields `None` here
    // (it would fail loud later at `with_crate_scope`); only an actually
    // resolved, non-release per-crate version trips the guard.
    let selected = &ctx.options.selected_crates;
    for crate_cfg in crate::util::all_crates(ctx) {
        if !selected.is_empty() && !selected.contains(&crate_cfg.name) {
            continue;
        }
        if let Some(version) = resolve_crate_tag(ctx, &crate_cfg)
            && let Some(reason) = non_release_reason(&version)
        {
            return Some((version, reason));
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::context::Context;

    fn ctx_with_version(version: &str) -> Context {
        let mut ctx = Context::test_fixture();
        ctx.template_vars_mut().set("Version", version);
        ctx
    }

    #[test]
    fn snapshot_zero_version_bails_naming_version_and_publishers() {
        let ctx = ctx_with_version("0.0.0~SNAPSHOT-d7813f0");
        let log = ctx.logger("publish-test");
        let err = guard_release_version(&ctx, &log, &["cloudsmith".to_string()])
            .expect_err("non-release version must bail before any publisher runs");
        let msg = err.to_string();
        assert!(
            msg.contains("0.0.0~SNAPSHOT-d7813f0"),
            "names the version: {msg}"
        );
        assert!(msg.contains("cloudsmith"), "names the publishers: {msg}");
        assert!(
            msg.contains("--allow-snapshot-publish"),
            "tells the user how to override: {msg}",
        );
    }

    #[test]
    fn allow_snapshot_publish_downgrades_to_warning() {
        let mut ctx = ctx_with_version("0.0.0~SNAPSHOT-d7813f0");
        ctx.options.allow_snapshot_publish = true;
        let log = ctx.logger("publish-test");
        guard_release_version(&ctx, &log, &["cloudsmith".to_string()])
            .expect("--allow-snapshot-publish must downgrade the bail to a warning");
    }

    #[test]
    fn real_semver_version_passes_silently() {
        let ctx = ctx_with_version("1.4.2");
        let log = ctx.logger("publish-test");
        guard_release_version(&ctx, &log, &["cloudsmith".to_string()])
            .expect("a real semver version must not trip the guard");
    }

    #[test]
    fn dry_run_does_not_trip_the_guard() {
        // Dry-run publishers no-op their external calls, so a non-release
        // version there is a preview, not a leak — the guard must step aside
        // (matching every other external-effect gate in the stage).
        let mut ctx = ctx_with_version("0.0.0~SNAPSHOT-d7813f0");
        ctx.options.dry_run = true;
        let log = ctx.logger("publish-test");
        guard_release_version(&ctx, &log, &["cloudsmith".to_string()])
            .expect("dry-run must not trip the non-release guard");
    }

    #[test]
    fn plain_snapshot_marker_version_bails() {
        let ctx = ctx_with_version("1.4.2-SNAPSHOT-abc1234");
        let log = ctx.logger("publish-test");
        let err = guard_release_version(&ctx, &log, &["cargo".to_string()])
            .expect_err("a snapshot-suffixed real base version must still bail");
        assert!(err.to_string().contains("snapshot marker"), "{err}");
    }
}
