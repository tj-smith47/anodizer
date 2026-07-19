//! Snapcraft channel promotion — the reference [`Promotable`] implementation.
//!
//! Moves an already-uploaded snap revision from one channel to another via
//! `snapcraft release <name> <revision> <channel>`, without rebuilding or
//! re-uploading. This is the first of four promotion-capable publishers; npm
//! dist-tags, OCI floating tags, and GitHub prerelease flips implement the same
//! [`Promotable`] trait in their own crates.
//!
//! Every distinct snap across the crate universe is promoted best-effort: each
//! snap's revision is resolved and released independently; a snap with no
//! matching revision is "nothing to promote" for that snap (skipped, not a
//! failure), while a snap whose release fails is collected and the remaining
//! snaps are still attempted, then the run fails naming both what was already
//! released and what failed.
//!
//! The revision to release is resolved from the [`PromoteSelector`]:
//! * [`PromoteSelector::Version`] → the revision the store lists at that
//!   version (`snapcraft list-revisions`).
//! * [`PromoteSelector::FromRun`] → the revision the prior run recorded in its
//!   snapcraft [`PublishEvidence`] — no store round-trip.
//! * [`PromoteSelector::Newest`] → the highest revision currently in the
//!   `from` channel (`snapcraft list-revisions`).

use anodizer_core::config::SnapcraftConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::promote::{
    Promotable, PromoteOutcome, PromoteRequest, PromoteSelector, PromoteSkipReason,
    partial_promotion_error,
};
use anodizer_core::run::run_checked;
use anodizer_core::{PublishEvidenceExtra, PublishReport};
use anyhow::{Context as _, Result, bail};
use std::process::Command;

use crate::command::{
    snap_newest_revision_in_channel, snap_revision_for_version, snapcraft_list_revisions_command,
    snapcraft_release_command,
};

/// The snapcraft promotion capability. Zero-sized; all state comes from the
/// [`PromoteRequest`]'s [`Context`], mirroring [`crate::SnapcraftPublishStage`].
pub struct SnapcraftPromoter;

impl Promotable for SnapcraftPromoter {
    fn name(&self) -> &str {
        "snapcraft"
    }

    /// Snapcraft's native channels are `edge`/`beta`/`candidate`/`stable`, so
    /// those map to themselves. The publisher-neutral `prerelease` alias maps
    /// to `candidate` — snap's conventional last-stop before `stable`. Anything
    /// else (a raw `latest/edge` track path, an operator's custom track) passes
    /// through verbatim.
    fn resolve_track(&self, canonical: &str) -> String {
        match canonical {
            "prerelease" => "candidate".to_string(),
            other => other.to_string(),
        }
    }

    fn promote(&self, req: &PromoteRequest) -> Result<PromoteOutcome> {
        let log = req.ctx.logger("snapcraft-promote");
        let snap_names = resolve_snap_names(req.ctx);
        if snap_names.is_empty() {
            bail!(
                "no snapcraft config with a resolvable snap name; \
                 `anodizer promote --publishers snapcraft` needs a `snapcrafts:` block"
            );
        }

        // The `from` shown in the folded outcome names the source the selector
        // actually targets (`--version`/`--from-run`), not the canonical track.
        let from_label = req.selector.source_label(&req.from);

        // Dry-run: describe the plan and spawn nothing. Revision resolution
        // would require a `snapcraft list-revisions` round-trip, so dry-run
        // names the selector rather than the concrete revision.
        if req.dry_run {
            for snap_name in &snap_names {
                log.status(&format!(
                    "(dry-run) would promote snapcraft {snap_name} {} {}→{}",
                    req.selector.describe(),
                    req.from,
                    req.to
                ));
            }
            return Ok(PromoteOutcome::dry_run(
                self.name(),
                from_label,
                &req.to,
                None,
            ));
        }

        let mut released: Vec<String> = Vec::new();
        let mut failed: Vec<(String, String)> = Vec::new();
        let mut promoted_revisions: Vec<String> = Vec::new();
        for snap_name in &snap_names {
            match release_one(req, snap_name, &log) {
                Ok(Some(revision)) => {
                    released.push(snap_name.clone());
                    promoted_revisions.push(format!("{snap_name} revision {revision}"));
                }
                Ok(None) => {
                    log.status(&format!(
                        "no snapcraft revision found for {} in {} — nothing to promote for {snap_name}",
                        req.selector.describe(),
                        req.from
                    ));
                }
                Err(err) => failed.push((snap_name.clone(), format!("{err:#}"))),
            }
        }

        if !failed.is_empty() {
            bail!("{}", partial_promotion_error(&released, &failed));
        }

        if released.is_empty() {
            return Ok(PromoteOutcome::skipped(
                self.name(),
                from_label,
                &req.to,
                PromoteSkipReason::NothingToPromote,
            ));
        }

        Ok(PromoteOutcome::promoted(
            self.name(),
            from_label,
            &req.to,
            promoted_revisions.join(", "),
        ))
    }
}

/// Resolve and release one snap's selector-matched revision. `Ok(Some(rev))` =
/// released; `Ok(None)` = no matching revision (nothing to promote for this
/// snap); `Err` = the revision probe or the release itself failed.
fn release_one(req: &PromoteRequest, snap_name: &str, log: &StageLogger) -> Result<Option<String>> {
    let Some(revision) = resolve_revision(req, snap_name, log)? else {
        return Ok(None);
    };

    let args = snapcraft_release_command(snap_name, &revision, &req.to);
    log.verbose(&format!("running {}", args.join(" ")));
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    run_checked(&mut cmd, log, "snapcraft release").with_context(|| {
        format!(
            "failed to release snap '{snap_name}' rev {revision} to {}",
            req.to
        )
    })?;

    log.status(&format!(
        "promoted snap {snap_name} revision {revision} {}→{}",
        req.from, req.to
    ));
    Ok(Some(revision))
}

/// Resolve every distinct Snap Store name across the crate universe, mirroring
/// the publish stage's resolution chain per snap: explicit `snapcrafts[].name`,
/// else the project name, else the crate's primary binary. Iterates ALL
/// `snapcrafts[]` of every crate (a workspace can ship several snaps), dedups by
/// resolved name, and is order-stable so promotion targets the whole snap
/// family rather than only the first.
fn resolve_snap_names(ctx: &Context) -> Vec<String> {
    let project_name = &ctx.config.project_name;
    let mut names: Vec<String> = Vec::new();
    for krate in ctx.config.crate_universe() {
        let Some(snap_configs) = krate.snapcrafts.as_ref() else {
            continue;
        };
        for snap_cfg in snap_configs {
            let name = snap_name_for(snap_cfg, project_name, &primary_binary(krate));
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// The crate's primary binary name — the first build's `binary`, falling back
/// to the crate name. Last resort of the snap-name resolution chain.
fn primary_binary(krate: &anodizer_core::config::CrateConfig) -> String {
    krate
        .builds
        .as_ref()
        .and_then(|b| b.first())
        .and_then(|b| b.binary.clone())
        .unwrap_or_else(|| krate.name.clone())
}

/// `snapcrafts[].name` → project name → primary binary, mirroring
/// `generate_snap_yaml` and the publish stage's `resolve_snap_name`.
fn snap_name_for(snap_cfg: &SnapcraftConfig, project_name: &str, primary_binary: &str) -> String {
    snap_cfg.name.clone().unwrap_or_else(|| {
        if project_name.is_empty() {
            primary_binary.to_string()
        } else {
            project_name.to_string()
        }
    })
}

/// Resolve the revision to release from the selector. `Ok(None)` means "nothing
/// to promote" (no matching revision); `Err` means the resolution itself failed
/// (e.g. the store probe could not run).
fn resolve_revision(
    req: &PromoteRequest,
    snap_name: &str,
    log: &StageLogger,
) -> Result<Option<String>> {
    match req.selector {
        PromoteSelector::FromRun { report, .. } => Ok(recorded_revision(report, snap_name)),
        PromoteSelector::Version(version) => {
            let output = list_revisions(snap_name, log)?;
            // `promote` operates store-wide with no per-artifact arch context,
            // so it intentionally searches every architecture and returns
            // whichever revision of `version` is numerically highest.
            Ok(snap_revision_for_version(&output, version, None))
        }
        PromoteSelector::Newest => {
            let output = list_revisions(snap_name, log)?;
            Ok(snap_newest_revision_in_channel(&output, &req.from))
        }
    }
}

/// Pull the recorded snapcraft revision for `snap_name` out of a prior run's
/// report, reading the snapcraft [`PublishEvidence`] the publish stage wrote.
fn recorded_revision(report: &PublishReport, snap_name: &str) -> Option<String> {
    report
        .results
        .iter()
        .filter(|r| r.name == "snapcraft")
        .filter_map(|r| r.evidence.as_ref())
        .filter_map(|e| match &e.extra {
            PublishEvidenceExtra::Snapcraft(s) => Some(&s.snapcraft_targets),
            _ => None,
        })
        .flatten()
        .find(|t| t.package_name == snap_name)
        .and_then(|t| t.revision.clone())
}

/// Run `snapcraft list-revisions <name>` and return its stdout. Unlike the
/// publish stage's idempotency probe (which tolerates a failing probe by
/// proceeding to upload), promotion cannot proceed without knowing the
/// revision, so a probe failure is a hard error here.
fn list_revisions(snap_name: &str, log: &StageLogger) -> Result<String> {
    let args = snapcraft_list_revisions_command(snap_name);
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    let output = run_checked(&mut cmd, log, "snapcraft list-revisions")
        .with_context(|| format!("failed to list revisions for snap '{snap_name}'"))?;
    Ok(log.redact(&String::from_utf8_lossy(&output.stdout)))
}

/// Preflight for snapcraft promotion: the `snapcraft` CLI must be on `PATH`.
/// Called by the verb only when snapcraft is among the selected publishers.
pub fn preflight() -> Result<()> {
    if !anodizer_core::tool_detect::on_path("snapcraft") {
        bail!(
            "`snapcraft` not found on PATH — snap promotion runs \
             `snapcraft release`; install snapcraft or deselect it with --publishers"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_track_maps_prerelease_to_candidate_else_identity() {
        let p = SnapcraftPromoter;
        assert_eq!(p.resolve_track("prerelease"), "candidate");
        assert_eq!(p.resolve_track("stable"), "stable");
        assert_eq!(p.resolve_track("candidate"), "candidate");
        assert_eq!(p.resolve_track("beta"), "beta");
        assert_eq!(p.resolve_track("edge"), "edge");
        // Unknown / raw native track passes through verbatim.
        assert_eq!(p.resolve_track("latest/edge"), "latest/edge");
    }

    #[test]
    fn release_command_is_positional_and_non_interactive() {
        let args = snapcraft_release_command("mysnap", "42", "stable");
        assert_eq!(args, vec!["snapcraft", "release", "mysnap", "42", "stable"]);
    }

    const LIST_REVISIONS: &str = "\
Rev    Uploaded              Arches  Version  Channels
5      2026-07-01T00:00:00Z  amd64   1.2.0    latest/candidate
4      2026-06-01T00:00:00Z  amd64   1.1.0    latest/stable
3      2026-05-01T00:00:00Z  amd64   1.2.0    -
2      2026-04-01T00:00:00Z  amd64   1.0.0    -
";

    #[test]
    fn revision_for_version_picks_highest_matching_revision() {
        // Version 1.2.0 was uploaded twice (rev 3 then re-uploaded as rev 5);
        // the highest revision wins so a re-promotion targets the latest upload.
        assert_eq!(
            snap_revision_for_version(LIST_REVISIONS, "1.2.0", None),
            Some("5".to_string())
        );
        assert_eq!(
            snap_revision_for_version(LIST_REVISIONS, "1.0.0", None),
            Some("2".to_string())
        );
        assert_eq!(
            snap_revision_for_version(LIST_REVISIONS, "9.9.9", None),
            None
        );
    }

    #[test]
    fn newest_revision_in_channel_matches_risk_component() {
        // `latest/candidate` counts as being in the `candidate` channel.
        assert_eq!(
            snap_newest_revision_in_channel(LIST_REVISIONS, "candidate"),
            Some("5".to_string())
        );
        assert_eq!(
            snap_newest_revision_in_channel(LIST_REVISIONS, "stable"),
            Some("4".to_string())
        );
        assert_eq!(
            snap_newest_revision_in_channel(LIST_REVISIONS, "edge"),
            None
        );
    }

    #[test]
    fn newest_revision_in_channel_tolerates_progressive_marker() {
        // A progressive/follower channel token carries a trailing `*` marker;
        // the risk component must still match `candidate`.
        const PROGRESSIVE: &str = "\
Rev    Uploaded              Arches  Version  Channels
7      2026-08-01T00:00:00Z  amd64   1.3.0    latest/candidate*
";
        assert_eq!(
            snap_newest_revision_in_channel(PROGRESSIVE, "candidate"),
            Some("7".to_string())
        );
    }

    #[test]
    fn recorded_revision_reads_snapcraft_evidence() {
        use anodizer_core::publish_evidence::{SnapcraftExtra, SnapcraftTargetSnapshot};
        use anodizer_core::{
            PublishEvidence, PublishEvidenceExtra, PublisherGroup, PublisherOutcome,
            PublisherResult,
        };

        let mut evidence = PublishEvidence::new("snapcraft");
        evidence.extra = PublishEvidenceExtra::Snapcraft(SnapcraftExtra {
            snapcraft_targets: vec![SnapcraftTargetSnapshot {
                crate_name: "app".into(),
                package_name: "mysnap".into(),
                channel: Some("candidate".into()),
                revision: Some("7".into()),
                version: Some("1.2.0".into()),
                held_for_review: false,
            }],
        });
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "snapcraft".into(),
            group: PublisherGroup::Submitter,
            required: false,
            outcome: PublisherOutcome::Succeeded,
            evidence: Some(evidence),
        });

        assert_eq!(recorded_revision(&report, "mysnap"), Some("7".to_string()));
        assert_eq!(recorded_revision(&report, "othersnap"), None);
    }

    use anodizer_core::config::{Config, CrateConfig, SnapcraftConfig};
    use anodizer_core::context::ContextOptions;
    use anodizer_core::promote::PromoteStatus;

    fn ctx_with_snapcrafts(project: &str, snaps: Vec<Option<&str>>) -> Context {
        let cfg = Config {
            project_name: project.to_string(),
            crates: vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                snapcrafts: Some(
                    snaps
                        .into_iter()
                        .map(|n| SnapcraftConfig {
                            name: n.map(String::from),
                            ..Default::default()
                        })
                        .collect(),
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        Context::new(cfg, ContextOptions::default())
    }

    #[test]
    fn promote_bails_without_any_snapcraft_config() {
        // No `snapcrafts:` anywhere ⇒ resolve_snap_names is empty and promote
        // must bail with an actionable message naming the missing block before
        // any revision resolution or subprocess.
        let ctx = Context::new(Config::default(), ContextOptions::default());
        let selector = PromoteSelector::Newest;
        let req = PromoteRequest {
            from: "candidate".to_string(),
            to: "stable".to_string(),
            selector: &selector,
            dry_run: true,
            ctx: &ctx,
        };
        let err = SnapcraftPromoter
            .promote(&req)
            .expect_err("no snapcrafts block must bail");
        assert!(
            format!("{err:#}").contains("snapcrafts:"),
            "error should name the missing snapcrafts block; got {err:#}"
        );
    }

    #[test]
    fn promote_dry_run_names_plan_and_spawns_nothing() {
        // A Version selector's dry-run resolves the plan without a
        // `list-revisions` round-trip and returns a DryRun outcome whose `from`
        // label is the version itself.
        let ctx = ctx_with_snapcrafts("demo", vec![Some("mysnap")]);
        let selector = PromoteSelector::Version("1.4.0".to_string());
        let req = PromoteRequest {
            from: "candidate".to_string(),
            to: "stable".to_string(),
            selector: &selector,
            dry_run: true,
            ctx: &ctx,
        };
        let out = SnapcraftPromoter.promote(&req).expect("dry-run ok");
        assert_eq!(out.status, PromoteStatus::DryRun);
        assert_eq!(out.publisher, "snapcraft");
        assert_eq!(out.from, "1.4.0");
        assert_eq!(out.to, "stable");
    }

    #[test]
    fn promote_from_run_empty_report_is_skipped_nothing_to_promote() {
        // FromRun with a report holding no snapcraft targets ⇒ every snap's
        // recorded revision is None, release_one returns Ok(None) with no
        // spawn, and the folded outcome is Skipped(NothingToPromote).
        let ctx = ctx_with_snapcrafts("demo", vec![Some("mysnap")]);
        let selector = PromoteSelector::FromRun {
            run_id: "run42".to_string(),
            report: PublishReport::default(),
        };
        let req = PromoteRequest {
            from: "candidate".to_string(),
            to: "stable".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };
        let out = SnapcraftPromoter
            .promote(&req)
            .expect("empty recorded family ok");
        assert_eq!(
            out.status,
            PromoteStatus::Skipped(PromoteSkipReason::NothingToPromote)
        );
        assert_eq!(out.from, "run run42");
    }

    #[test]
    fn resolve_snap_names_dedups_and_is_order_stable() {
        // Two snaps under one crate, the second a duplicate of the first's
        // resolved name: the result keeps first-seen order and dedups.
        let ctx = ctx_with_snapcrafts("demo", vec![Some("alpha"), Some("beta"), Some("alpha")]);
        assert_eq!(resolve_snap_names(&ctx), vec!["alpha", "beta"]);
    }

    #[test]
    fn resolve_snap_names_skips_crate_without_snapcrafts() {
        // A crate with no `snapcrafts:` block is skipped by the `continue`
        // guard, contributing no names.
        let cfg = Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                snapcrafts: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        let ctx = Context::new(cfg, ContextOptions::default());
        assert!(resolve_snap_names(&ctx).is_empty());
    }

    #[test]
    fn snap_name_for_resolution_chain() {
        // Explicit name wins.
        let named = SnapcraftConfig {
            name: Some("explicit".into()),
            ..Default::default()
        };
        assert_eq!(snap_name_for(&named, "proj", "bin"), "explicit");
        // No explicit name, non-empty project ⇒ project name.
        let bare = SnapcraftConfig::default();
        assert_eq!(snap_name_for(&bare, "proj", "bin"), "proj");
        // No explicit name, empty project ⇒ primary binary fallback.
        assert_eq!(snap_name_for(&bare, "", "bin"), "bin");
    }

    #[test]
    fn primary_binary_prefers_first_build_binary_else_crate_name() {
        use anodizer_core::config::BuildConfig;
        let with_build = CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            builds: Some(vec![BuildConfig {
                binary: Some("mybin".into()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert_eq!(primary_binary(&with_build), "mybin");
        // No builds ⇒ the crate name is the last resort.
        let no_build = CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            builds: None,
            ..Default::default()
        };
        assert_eq!(primary_binary(&no_build), "app");
    }

    #[test]
    fn resolve_revision_from_run_reads_recorded_without_spawn() {
        use anodizer_core::publish_evidence::{SnapcraftExtra, SnapcraftTargetSnapshot};
        use anodizer_core::{PublisherGroup, PublisherOutcome, PublisherResult};

        let ctx = ctx_with_snapcrafts("demo", vec![Some("mysnap")]);
        let log = ctx.logger("snapcraft-promote-test");
        let mut evidence = anodizer_core::PublishEvidence::new("snapcraft");
        evidence.extra = PublishEvidenceExtra::Snapcraft(SnapcraftExtra {
            snapcraft_targets: vec![SnapcraftTargetSnapshot {
                crate_name: "app".into(),
                package_name: "mysnap".into(),
                channel: Some("candidate".into()),
                revision: Some("99".into()),
                version: Some("1.0.0".into()),
                held_for_review: false,
            }],
        });
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "snapcraft".into(),
            group: PublisherGroup::Submitter,
            required: false,
            outcome: PublisherOutcome::Succeeded,
            evidence: Some(evidence),
        });
        let selector = PromoteSelector::FromRun {
            run_id: "r".into(),
            report,
        };
        let req = PromoteRequest {
            from: "candidate".to_string(),
            to: "stable".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };
        // FromRun routes to recorded_revision — no `list-revisions` spawn.
        let rev = resolve_revision(&req, "mysnap", &log).expect("resolve");
        assert_eq!(rev, Some("99".to_string()));
        // An unknown snap yields no recorded revision.
        assert_eq!(
            resolve_revision(&req, "absent", &log).expect("resolve"),
            None
        );
    }

    #[test]
    fn preflight_ok_when_snapcraft_on_path() {
        // In CI/this env snapcraft is present on PATH, so preflight passes.
        // (The bail branch is exercised on hosts without snapcraft.)
        if anodizer_core::tool_detect::on_path("snapcraft") {
            preflight().expect("snapcraft present ⇒ preflight ok");
        }
    }
}
