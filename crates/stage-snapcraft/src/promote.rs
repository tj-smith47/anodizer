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
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::promote::{
    Promotable, PromoteOutcome, PromoteRequest, PromoteSelector, PromoteSkipReason,
    partial_promotion_error,
};
use anodizer_core::run::run_capture_timeout;
use anodizer_core::{PublishEvidenceExtra, PublishReport};
use anyhow::{Context as _, Result, bail};
use std::process::Command;

use crate::command::{
    SNAPCRAFT_PROBE_TIMEOUT, SNAPCRAFT_UPLOAD_TIMEOUT, is_snap_absent_from_store,
    snap_newest_revisions_in_channel_by_arch, snap_revisions_for_version_by_arch,
    snapcraft_list_revisions_command, snapcraft_release_command, snapcraft_whoami_command,
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
                Ok(revisions) if revisions.is_empty() => {
                    log.status(&format!(
                        "no snapcraft revision found for {} in {} — nothing to promote for {snap_name}",
                        req.selector.describe(),
                        req.from
                    ));
                }
                Ok(revisions) => {
                    released.push(snap_name.clone());
                    for revision in revisions {
                        promoted_revisions.push(format!("{snap_name} revision {revision}"));
                    }
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

/// Resolve and release one snap's selector-matched revisions — **one per
/// architecture**. A dual-arch snap mints one Snap Store revision per arch, so
/// releasing a single global maximum would leave the other arch stranded on
/// the source channel; this releases every arch's revision. Returns the list
/// of released revisions (empty = nothing to promote for this snap); `Err` =
/// the revision probe or a release itself failed.
fn release_one(req: &PromoteRequest, snap_name: &str, log: &StageLogger) -> Result<Vec<String>> {
    let revisions = resolve_revisions(req, snap_name, log)?;
    for revision in &revisions {
        let args = snapcraft_release_command(snap_name, revision, &req.to);
        log.verbose(&format!("running {}", args.join(" ")));
        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..]);
        let output =
            run_capture_timeout(&mut cmd, log, "snapcraft release", SNAPCRAFT_UPLOAD_TIMEOUT)
                .with_context(|| {
                    format!(
                        "failed to release snap '{snap_name}' rev {revision} to {}",
                        req.to
                    )
                })?;
        log.check_output(output, "snapcraft release")
            .with_context(|| {
                format!(
                    "failed to release snap '{snap_name}' rev {revision} to {}",
                    req.to
                )
            })?;

        log.status(&format!(
            "promoted snap {snap_name} revision {revision} {}→{}",
            req.from, req.to
        ));
    }
    Ok(revisions)
}

/// Resolve every distinct Snap Store name across the crate universe, mirroring
/// the publish stage's resolution chain per snap: explicit `snapcrafts[].name`,
/// else the project name, else the crate's primary binary. Iterates ALL
/// `snapcrafts[]` of every crate (a workspace can ship several snaps), dedups by
/// resolved name, and is order-stable so promotion targets the whole snap
/// family rather than only the first.
///
/// Honors the same gates the publish stage's `run_uploads` /
/// `collect_snapcraft_targets` enforce — `publish: true`, `skip:`, `if:` — so a
/// build-only snap (one that was never uploaded to the store) never enters the
/// promote set. Including it would make its store probe fail and, treated as
/// unregistered, needlessly skip; treated as a real error, hard-fail the run.
fn resolve_snap_names(ctx: &Context) -> Vec<String> {
    let project_name = &ctx.config.project_name;
    let mut names: Vec<String> = Vec::new();
    for krate in ctx.config.crate_universe() {
        let Some(snap_configs) = krate.snapcrafts.as_ref() else {
            continue;
        };
        for snap_cfg in snap_configs {
            if !snap_cfg.publish.unwrap_or(false) {
                continue;
            }
            if let Some(ref d) = snap_cfg.skip {
                let off = d
                    .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .unwrap_or(false);
                if off {
                    continue;
                }
            }
            // A render error on `if:` counts as proceed for name resolution —
            // the promote dispatch is a manual verb with no build stage to
            // re-diagnose it, and skipping a snap on a template hiccup would
            // silently drop it from promotion.
            let proceed = anodizer_core::config::evaluate_if_condition(
                snap_cfg.if_condition.as_deref(),
                "snapcraft promote target",
                |t| ctx.render_template(t),
            )
            .unwrap_or(true);
            if !proceed {
                continue;
            }
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

/// Resolve the revisions to release from the selector — **one per
/// architecture** so a dual-arch snap fully promotes. An empty Vec means
/// "nothing to promote" (no matching revision, or the snap is absent from the
/// store); `Err` means the resolution itself failed (a genuine probe/auth
/// error).
fn resolve_revisions(
    req: &PromoteRequest,
    snap_name: &str,
    log: &StageLogger,
) -> Result<Vec<String>> {
    match req.selector {
        PromoteSelector::FromRun { report, .. } => Ok(recorded_revisions(report, snap_name)),
        PromoteSelector::Version(version) => {
            let Some(output) = list_revisions(snap_name, log)? else {
                return Ok(Vec::new());
            };
            Ok(snap_revisions_for_version_by_arch(&output, version))
        }
        PromoteSelector::Newest => {
            let Some(output) = list_revisions(snap_name, log)? else {
                return Ok(Vec::new());
            };
            Ok(snap_newest_revisions_in_channel_by_arch(&output, &req.from))
        }
    }
}

/// Pull every recorded snapcraft revision for `snap_name` out of a prior run's
/// report, reading the snapcraft [`PublishEvidence`] the publish stage wrote.
/// A dual-arch snap records one entry per architecture, so this returns each
/// arch's revision (order-stable, de-duplicated) — releasing all of them is
/// what fully promotes a multi-arch snap via `--from-run`.
fn recorded_revisions(report: &PublishReport, snap_name: &str) -> Vec<String> {
    let mut revisions: Vec<String> = Vec::new();
    for revision in report
        .results
        .iter()
        .filter(|r| r.name == "snapcraft")
        .filter_map(|r| r.evidence.as_ref())
        .filter_map(|e| match &e.extra {
            PublishEvidenceExtra::Snapcraft(s) => Some(&s.snapcraft_targets),
            _ => None,
        })
        .flatten()
        .filter(|t| t.package_name == snap_name)
        .filter_map(|t| t.revision.clone())
    {
        if !revisions.contains(&revision) {
            revisions.push(revision);
        }
    }
    revisions
}

/// Run `snapcraft list-revisions <name>` (bounded) and return its stdout.
/// `Ok(None)` means the snap is simply absent from the store — not yet
/// registered or holding no revisions — which promotion treats as "nothing to
/// promote" (a skip). `Err` is reserved for a genuine probe/auth error (the
/// probe could not run, or the store answered with an authentication /
/// connectivity fault), which must surface honestly rather than masquerade as
/// an empty promote.
fn list_revisions(snap_name: &str, log: &StageLogger) -> Result<Option<String>> {
    let args = snapcraft_list_revisions_command(snap_name);
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    let output = run_capture_timeout(
        &mut cmd,
        log,
        "snapcraft list-revisions",
        SNAPCRAFT_PROBE_TIMEOUT,
    )
    .with_context(|| format!("failed to list revisions for snap '{snap_name}'"))?;
    if output.status.success() {
        return Ok(Some(log.redact(&String::from_utf8_lossy(&output.stdout))));
    }
    let combined = log.redact(&format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ));
    if is_snap_absent_from_store(&combined) {
        log.status(&format!(
            "snap '{snap_name}' is not registered / has no revisions in the Snap Store — \
             nothing to promote"
        ));
        return Ok(None);
    }
    bail!(
        "failed to list revisions for snap '{snap_name}': the Snap Store probe exited {} — \
         {}",
        output.status,
        combined.trim()
    );
}

/// Preflight for snapcraft promotion: the `snapcraft` CLI must be on `PATH`
/// AND a Snap Store session must be available. Called by the verb only when
/// snapcraft is among the selected publishers.
///
/// Credential presence is probed so the operator gets an actionable message
/// BEFORE dispatch rather than a mid-promote authentication failure. A
/// `SNAPCRAFT_STORE_CREDENTIALS` env var (the CI login path) short-circuits the
/// probe; otherwise `snapcraft whoami` is consulted. A clear not-logged-in
/// verdict bails with remediation; a probe that cannot run (spawn/timeout)
/// passes rather than block promotion on an unreliable check — the bounded
/// release itself surfaces a genuine auth fault.
pub fn preflight() -> Result<()> {
    if !anodizer_core::tool_detect::on_path("snapcraft") {
        bail!(
            "`snapcraft` not found on PATH — snap promotion runs \
             `snapcraft release`; install snapcraft or deselect it with --publishers"
        );
    }
    if std::env::var_os("SNAPCRAFT_STORE_CREDENTIALS").is_some() {
        return Ok(());
    }
    let log = StageLogger::new("snapcraft-promote", Verbosity::Normal);
    let args = snapcraft_whoami_command();
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    match run_capture_timeout(&mut cmd, &log, "snapcraft whoami", SNAPCRAFT_PROBE_TIMEOUT) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(_) => bail!(
            "snapcraft is installed but no Snap Store session is available — run \
             `snapcraft login` (or set SNAPCRAFT_STORE_CREDENTIALS) before promoting, \
             or deselect snapcraft with --publishers"
        ),
        // The credential probe itself could not run; do not block on an
        // unreliable check — the bounded `snapcraft release` surfaces a real
        // auth fault honestly.
        Err(e) => {
            log.debug(&format!(
                "snapcraft whoami credential probe could not run ({e}); proceeding"
            ));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{snap_newest_revision_in_channel, snap_revision_for_version};

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

    // A dual-arch snap: one revision per arch per version. The store-wide
    // promote must resolve BOTH arches' newest revisions, not a single global
    // max, or the lower-numbered arch is left stranded on the source channel.
    const DUAL_ARCH_REVISIONS: &str = "\
Rev    Uploaded              Arches  Version  Channels
5      2026-07-01T00:00:00Z  amd64   1.2.0    latest/candidate
4      2026-07-01T00:00:00Z  arm64   1.2.0    latest/candidate
3      2026-06-01T00:00:00Z  amd64   1.1.0    latest/stable
2      2026-06-01T00:00:00Z  arm64   1.1.0    latest/stable
";

    #[test]
    fn revisions_for_version_by_arch_returns_one_per_arch() {
        // Both arch revisions of 1.2.0 (amd64 rev 5, arm64 rev 4) must appear.
        // The pre-fix single global `.max()` would have returned only rev 5.
        assert_eq!(
            snap_revisions_for_version_by_arch(DUAL_ARCH_REVISIONS, "1.2.0"),
            vec!["4".to_string(), "5".to_string()]
        );
        assert!(snap_revisions_for_version_by_arch(DUAL_ARCH_REVISIONS, "9.9.9").is_empty());
    }

    #[test]
    fn newest_revisions_in_channel_by_arch_returns_one_per_arch() {
        // Both arches sit on `candidate` (rev 5 amd64, rev 4 arm64); both must
        // be resolved so `promote --newest` moves the whole dual-arch snap.
        assert_eq!(
            snap_newest_revisions_in_channel_by_arch(DUAL_ARCH_REVISIONS, "candidate"),
            vec!["4".to_string(), "5".to_string()]
        );
        assert_eq!(
            snap_newest_revisions_in_channel_by_arch(DUAL_ARCH_REVISIONS, "stable"),
            vec!["2".to_string(), "3".to_string()]
        );
        assert!(snap_newest_revisions_in_channel_by_arch(DUAL_ARCH_REVISIONS, "edge").is_empty());
    }

    #[test]
    fn revisions_by_arch_collapses_reupload_within_one_arch() {
        // A single arch re-uploaded twice at one version (rev 3 then rev 5)
        // contributes exactly ONE revision (the newest, 5) — releasing an
        // already-released revision again to the same channel is a store-side
        // no-op, so a re-promote of the same version stays idempotent.
        const REUPLOAD: &str = "\
Rev    Uploaded              Arches  Version  Channels
5      2026-07-02T00:00:00Z  amd64   1.2.0    latest/candidate
3      2026-07-01T00:00:00Z  amd64   1.2.0    -
";
        assert_eq!(
            snap_revisions_for_version_by_arch(REUPLOAD, "1.2.0"),
            vec!["5".to_string()],
            "one arch → one revision, even across re-uploads"
        );
    }

    #[test]
    fn recorded_revisions_reads_every_arch_from_snapcraft_evidence() {
        // A dual-arch snap records one evidence entry per architecture; the
        // FromRun promote must release BOTH recorded revisions, so
        // `recorded_revisions` returns each arch's revision (order-stable,
        // de-duplicated). Reading only the first (the pre-fix behavior) would
        // leave one architecture stranded on the source channel.
        use anodizer_core::publish_evidence::{SnapcraftExtra, SnapcraftTargetSnapshot};
        use anodizer_core::{
            PublishEvidence, PublishEvidenceExtra, PublisherGroup, PublisherOutcome,
            PublisherResult,
        };

        let mut evidence = PublishEvidence::new("snapcraft");
        evidence.extra = PublishEvidenceExtra::Snapcraft(SnapcraftExtra {
            snapcraft_targets: vec![
                SnapcraftTargetSnapshot {
                    crate_name: "app".into(),
                    package_name: "mysnap".into(),
                    channel: Some("candidate".into()),
                    arch: Some("amd64".into()),
                    revision: Some("7".into()),
                    version: Some("1.2.0".into()),
                    held_for_review: false,
                },
                SnapcraftTargetSnapshot {
                    crate_name: "app".into(),
                    package_name: "mysnap".into(),
                    channel: Some("candidate".into()),
                    arch: Some("arm64".into()),
                    revision: Some("8".into()),
                    version: Some("1.2.0".into()),
                    held_for_review: false,
                },
            ],
        });
        let mut report = PublishReport::default();
        report.results.push(PublisherResult {
            name: "snapcraft".into(),
            group: PublisherGroup::Submitter,
            required: false,
            outcome: PublisherOutcome::Succeeded,
            evidence: Some(evidence),
        });

        assert_eq!(
            recorded_revisions(&report, "mysnap"),
            vec!["7".to_string(), "8".to_string()],
            "both arch revisions must be recovered for a dual-arch --from-run promote"
        );
        assert!(recorded_revisions(&report, "othersnap").is_empty());
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
                            // Promotion targets only snaps the publish stage
                            // actually uploaded — `publish: true`.
                            publish: Some(true),
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
        // recorded revisions are empty, release_one returns an empty Vec with
        // no spawn, and the folded outcome is Skipped(NothingToPromote).
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
    fn resolve_snap_names_excludes_build_only_snap() {
        // A `publish: false` (build-only) snap was never uploaded, so it
        // must NOT enter the promote set — its store probe would otherwise
        // fail and, treated as an error, hard-fail the whole run.
        let cfg = Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                snapcrafts: Some(vec![
                    SnapcraftConfig {
                        name: Some("published".into()),
                        publish: Some(true),
                        ..Default::default()
                    },
                    SnapcraftConfig {
                        name: Some("buildonly".into()),
                        publish: Some(false),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let ctx = Context::new(cfg, ContextOptions::default());
        assert_eq!(
            resolve_snap_names(&ctx),
            vec!["published".to_string()],
            "a build-only snap must be excluded from the promote set"
        );
    }

    #[test]
    fn resolve_snap_names_excludes_skipped_and_if_false() {
        // `skip: true` and a falsy `if:` mirror the publish stage's
        // gates — a snap the publish stage would not upload must not promote.
        let cfg = Config {
            project_name: "demo".to_string(),
            crates: vec![CrateConfig {
                name: "app".to_string(),
                path: ".".to_string(),
                snapcrafts: Some(vec![
                    SnapcraftConfig {
                        name: Some("live".into()),
                        publish: Some(true),
                        ..Default::default()
                    },
                    SnapcraftConfig {
                        name: Some("skipped".into()),
                        publish: Some(true),
                        skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
                        ..Default::default()
                    },
                    SnapcraftConfig {
                        name: Some("gated-off".into()),
                        publish: Some(true),
                        if_condition: Some("false".to_string()),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let ctx = Context::new(cfg, ContextOptions::default());
        assert_eq!(
            resolve_snap_names(&ctx),
            vec!["live".to_string()],
            "skip:true and if:false snaps must be excluded from the promote set"
        );
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
                arch: Some("amd64".into()),
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
        // FromRun routes to recorded_revisions — no `list-revisions` spawn.
        let revs = resolve_revisions(&req, "mysnap", &log).expect("resolve");
        assert_eq!(revs, vec!["99".to_string()]);
        // An unknown snap yields no recorded revision.
        assert!(
            resolve_revisions(&req, "absent", &log)
                .expect("resolve")
                .is_empty()
        );
    }

    #[test]
    fn resolve_revisions_from_run_releases_every_arch() {
        // A populated dual-arch publish snapshot must yield BOTH arch
        // revisions (not NothingToPromote, not one arch).
        use anodizer_core::publish_evidence::{SnapcraftExtra, SnapcraftTargetSnapshot};
        use anodizer_core::{PublisherGroup, PublisherOutcome, PublisherResult};

        let ctx = ctx_with_snapcrafts("demo", vec![Some("mysnap")]);
        let log = ctx.logger("snapcraft-promote-test");
        let mut evidence = anodizer_core::PublishEvidence::new("snapcraft");
        evidence.extra = PublishEvidenceExtra::Snapcraft(SnapcraftExtra {
            snapcraft_targets: vec![
                SnapcraftTargetSnapshot {
                    crate_name: "app".into(),
                    package_name: "mysnap".into(),
                    channel: Some("candidate".into()),
                    arch: Some("amd64".into()),
                    revision: Some("41".into()),
                    version: Some("1.0.0".into()),
                    held_for_review: false,
                },
                SnapcraftTargetSnapshot {
                    crate_name: "app".into(),
                    package_name: "mysnap".into(),
                    channel: Some("candidate".into()),
                    arch: Some("arm64".into()),
                    revision: Some("42".into()),
                    version: Some("1.0.0".into()),
                    held_for_review: false,
                },
            ],
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
        assert_eq!(
            resolve_revisions(&req, "mysnap", &log).expect("resolve"),
            vec!["41".to_string(), "42".to_string()]
        );
    }

    // Shares the `path_env` serial token with the credential-clearing
    // preflight tests below — they mutate `SNAPCRAFT_STORE_CREDENTIALS`
    // process-wide, and this test reads it.
    #[test]
    #[serial_test::serial(path_env)]
    fn preflight_ok_when_snapcraft_and_credentials_present() {
        // With snapcraft on PATH AND `SNAPCRAFT_STORE_CREDENTIALS` set,
        // preflight short-circuits the whoami probe and passes. (The
        // no-tool and no-session bail branches are exercised below / on
        // hosts without snapcraft.)
        if anodizer_core::tool_detect::on_path("snapcraft")
            && std::env::var_os("SNAPCRAFT_STORE_CREDENTIALS").is_some()
        {
            preflight().expect("snapcraft + credentials present ⇒ preflight ok");
        }
    }

    /// RAII guard that clears `SNAPCRAFT_STORE_CREDENTIALS` for the duration of
    /// a preflight test that must exercise the `snapcraft whoami` probe (which
    /// only runs when the credential env var is absent), restoring the prior
    /// value on drop.
    struct CredsCleared(Option<std::ffi::OsString>);
    impl CredsCleared {
        fn new() -> Self {
            let prev = std::env::var_os("SNAPCRAFT_STORE_CREDENTIALS");
            // env-ok: serialised by #[serial(path_env)] on every caller test
            unsafe { std::env::remove_var("SNAPCRAFT_STORE_CREDENTIALS") };
            Self(prev)
        }
    }
    impl Drop for CredsCleared {
        fn drop(&mut self) {
            if let Some(v) = self.0.take() {
                // env-ok: serialised by #[serial(path_env)] on every caller test
                unsafe { std::env::set_var("SNAPCRAFT_STORE_CREDENTIALS", v) };
            }
        }
    }

    // A missing Snap Store session must surface an actionable message
    // BEFORE dispatch, not conflate with "snap unregistered" mid-promote.
    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn preflight_bails_when_no_snap_store_session() {
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(
                "if [ \"$1\" = \"whoami\" ]; then\n\
                 echo \"error: You are not logged in.\" 1>&2\n\
                 exit 1\nfi\nexit 1\n",
            )
            .install();
        let _path = tools.activate();
        let _creds = CredsCleared::new();
        let err = preflight().expect_err("no store session must bail actionably");
        assert!(
            format!("{err:#}").contains("no Snap Store session"),
            "preflight must name the missing session; got {err:#}"
        );
    }

    // A logged-in session (`snapcraft whoami` exits 0) passes even with
    // the credential env var absent.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn preflight_ok_when_whoami_reports_a_session() {
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(
                "if [ \"$1\" = \"whoami\" ]; then\n\
                 echo \"email: dev@example.com\"\n\
                 exit 0\nfi\nexit 1\n",
            )
            .install();
        let _path = tools.activate();
        let _creds = CredsCleared::new();
        preflight().expect("a live whoami session ⇒ preflight ok");
    }

    // An unregistered / not-yet-released snap is a SKIP (nothing to
    // promote), not a hard failure of the whole run.
    // The stubbed snapcraft uses FakeToolDir::script, which is unix-only.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn promote_skips_unregistered_snap_instead_of_hard_failing() {
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(
                "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"Snap 'mysnap' was not found in the Snap Store.\" 1>&2\n\
                 exit 1\nfi\nexit 1\n",
            )
            .install();
        let _path = tools.activate();

        let ctx = ctx_with_snapcrafts("", vec![Some("mysnap")]);
        let selector = PromoteSelector::Newest;
        let req = PromoteRequest {
            from: "candidate".to_string(),
            to: "stable".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };
        let out = SnapcraftPromoter
            .promote(&req)
            .expect("an unregistered snap must skip, never hard-fail the run");
        assert_eq!(
            out.status,
            PromoteStatus::Skipped(PromoteSkipReason::NothingToPromote)
        );
    }

    // A genuine probe/auth error (non-zero exit with NO absent marker)
    // must surface honestly as a failure, never masquerade as an empty skip.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial(path_env)]
    fn promote_surfaces_genuine_probe_error() {
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        let tools = FakeToolDir::new();
        tools
            .tool("snapcraft")
            .script(
                "if [ \"$1\" = \"list-revisions\" ]; then\n\
                 echo \"error: cannot reach the store: connection timed out\" 1>&2\n\
                 exit 1\nfi\nexit 1\n",
            )
            .install();
        let _path = tools.activate();

        let ctx = ctx_with_snapcrafts("", vec![Some("mysnap")]);
        let selector = PromoteSelector::Newest;
        let req = PromoteRequest {
            from: "candidate".to_string(),
            to: "stable".to_string(),
            selector: &selector,
            dry_run: false,
            ctx: &ctx,
        };
        let err = SnapcraftPromoter
            .promote(&req)
            .expect_err("a genuine store probe error must hard-fail, not skip");
        assert!(
            format!("{err:#}").contains("failed to list revisions"),
            "the honest probe error must surface; got {err:#}"
        );
    }
}
