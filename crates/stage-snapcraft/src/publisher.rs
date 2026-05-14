//! `SnapcraftPublisher` — Submitter-group `Publisher` impl wrapping the
//! existing [`SnapcraftPublishStage::run`](crate::publish_stage::SnapcraftPublishStage)
//! entrypoint.
//!
//! Snapcraft is structurally a Submitter publisher: the Snap Store has
//! no public unpublish API, and already-installed snaps on user systems
//! keep the published revision regardless of any post-hoc channel
//! changes. Rollback for snapcraft is therefore warn-only — we record
//! `(crate_name, package_name, channel)` tuples in
//! [`PublishEvidence::extra`] so a `--rollback-only` invocation can
//! surface the exact snap-store listing the operator needs to address
//! manually (release that channel back to an older revision via
//! `snapcraft release` or the snapcraft.io UI).
//!
//! [`SnapcraftPublishStage`] stays as a separate stage running AFTER
//! `PublishStage` (existing pipeline order unchanged); this `Publisher`
//! impl participates in the trait-based registry so the Submitter gate
//! can see snapcraft alongside chocolatey, winget, upstream-AUR, and
//! cargo.
//!
//! CREDENTIAL HANDLING: [`SnapcraftTarget`] stores no auth material.
//! The `SNAPCRAFT_LOGIN` env var (resolved at publish time inside the
//! `snapcraft upload` subprocess) is irrelevant to a warn-only rollback
//! — channel management runs through the snapcraft.io web UI under the
//! package owner's account, not via the upload login token.

use anodizer_core::context::Context;
use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

/// Snapcraft publisher: Submitter group, no rollback path (snaps already
/// installed by users keep the published revision; the Snap Store does not
/// expose an unpublish API).
///
/// Stays in its own stage (`SnapcraftPublishStage`) running AFTER
/// `PublishStage`; this `Publisher` impl participates in the trait-based
/// registry so the Submitter gate can see it.
pub struct SnapcraftPublisher;

impl SnapcraftPublisher {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SnapcraftPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl Publisher for SnapcraftPublisher {
    fn name(&self) -> &str {
        "snapcraft"
    }

    fn group(&self) -> PublisherGroup {
        PublisherGroup::Submitter
    }

    fn required(&self) -> bool {
        false
    }

    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Some("SNAPCRAFT_LOGIN")
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<PublishEvidence> {
        // Snapshot targets BEFORE the stage runs so a mid-publish
        // failure still leaves the operator a manual channel-management
        // pointer for each snap we attempted to push.
        let targets = collect_snapcraft_targets(ctx);
        let stage = crate::publish_stage::SnapcraftPublishStage;
        <crate::publish_stage::SnapcraftPublishStage as anodizer_core::stage::Stage>::run(
            &stage, ctx,
        )?;
        let mut evidence = PublishEvidence::new("snapcraft");
        if let Some(first) = targets.first() {
            evidence.primary_ref = Some(format!("https://snapcraft.io/{}", first.package_name));
        }
        evidence.extra = serde_json::json!({ "snapcraft_targets": targets });
        Ok(evidence)
    }

    fn rollback(&self, ctx: &mut Context, evidence: &PublishEvidence) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_snapcraft_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&anodizer_core::rollback_empty_warning_msg(
                "snapcraft",
                "published snaps",
            ));
            return Ok(());
        }
        // Snap Store has no programmatic unpublish endpoint and
        // already-installed snaps on user systems keep the published
        // revision regardless of channel changes. Surface a warn per
        // recorded target naming the (package, channel) tuple plus the
        // snapcraft.io listing URL for manual channel management. This
        // is intentionally NOT an error: a failed automated rollback
        // should not gate the rest of the pipeline.
        for target in &targets {
            log.warn(&format!(
                "snapcraft: published '{}' revision in channel '{}' is irreversible \
                 (already-installed snaps keep the revision); manage future revisions \
                 at https://snapcraft.io/{}/listing",
                target.package_name,
                target.channel.as_deref().unwrap_or("(default)"),
                target.package_name,
            ));
        }
        log.status(&format!(
            "snapcraft: {} revision(s) require manual channel-management at snapcraft.io",
            targets.len()
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<PreflightCheck> {
        Ok(PreflightCheck::Pass)
    }
}

/// Serialized shape of a recorded snapcraft publish. One entry per
/// `(crate, snapcraft config)` tuple whose `publish: true` opt-in
/// matched the [`SnapcraftPublishStage`] iteration order.
///
/// `package_name` is the resolved Snap Store package name (defaults to
/// the crate name when `snapcrafts[].name` is not overridden);
/// `channel` is the rendered channel template (or `None` when the
/// publish path falls back to the `grade`-derived default).
///
/// NB: no `token`, `login`, or `password` fields — see module rustdoc
/// for the credential-handling rationale.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SnapcraftTarget {
    /// The crate this publish covered.
    crate_name: String,
    /// Snap Store package name — defaults to the crate name when
    /// `snapcrafts[].name` is not set.
    package_name: String,
    /// First rendered channel template, or `None` when the publish
    /// path falls back to the `grade`-derived default.
    channel: Option<String>,
    /// Reserved for future use — snapcraft prints the revision number
    /// on upload but the existing publish stage does not capture
    /// stdout, so this stays `None` until we wire that capture.
    revision: Option<String>,
}

/// Walk `ctx.config.crates[].snapcrafts[]` and build one
/// [`SnapcraftTarget`] per opted-in snap config. Mirrors the publish
/// stage's filters: `selected_crates` gate, `publish: true` opt-in.
/// Skipped configs (`skip: true`) are excluded here too so the rollback
/// warn surface matches what actually shipped.
fn collect_snapcraft_targets(ctx: &Context) -> Vec<SnapcraftTarget> {
    let selected = &ctx.options.selected_crates;
    let mut out: Vec<SnapcraftTarget> = Vec::new();
    for krate in &ctx.config.crates {
        if !selected.is_empty() && !selected.contains(&krate.name) {
            continue;
        }
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
            let package_name = snap_cfg.name.clone().unwrap_or_else(|| krate.name.clone());
            // GoReleaser parity: `channel_templates` is a Vec rendered
            // through the template engine. Capture the first non-empty
            // rendering — operators reading the warn line only need one
            // channel pointer to find the listing page.
            let channel = snap_cfg.channel_templates.as_ref().and_then(|tmpls| {
                tmpls
                    .iter()
                    .filter_map(|t| ctx.render_template(t).ok().filter(|s| !s.is_empty()))
                    .next()
            });
            out.push(SnapcraftTarget {
                crate_name: krate.name.clone(),
                package_name,
                channel,
                revision: None,
            });
        }
    }
    out
}

/// Decode the `snapcraft_targets` array from [`PublishEvidence::extra`].
///
/// Returns an empty Vec on any of: missing key, wrong shape, empty
/// array. Rollback treats empty-decode the same as no-evidence and
/// emits the canonical empty-evidence warn.
fn decode_snapcraft_targets(extra: &serde_json::Value) -> Vec<SnapcraftTarget> {
    extra
        .get("snapcraft_targets")
        .and_then(|v| serde_json::from_value::<Vec<SnapcraftTarget>>(v.clone()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{CrateConfig, SnapcraftConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    fn snap_crate(name: &str, package_name: Option<&str>, channel: Option<&str>) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            snapcrafts: Some(vec![SnapcraftConfig {
                name: package_name.map(|s| s.to_string()),
                publish: Some(true),
                channel_templates: channel.map(|c| vec![c.to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn snapcraft_publisher_classification() {
        let p = SnapcraftPublisher::new();
        assert_eq!(p.name(), "snapcraft");
        assert_eq!(p.group(), PublisherGroup::Submitter);
        assert!(!p.required());
        assert_eq!(p.rollback_scope_needed(), Some("SNAPCRAFT_LOGIN"));
    }

    #[test]
    fn snapcraft_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = SnapcraftPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn snapcraft_rollback_warns_when_no_targets_recorded() {
        let mut ctx = TestContextBuilder::new().build();
        let evidence = PublishEvidence::new("snapcraft");
        let p = SnapcraftPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let msg = anodizer_core::rollback_empty_warning_msg("snapcraft", "published snaps");
        assert!(msg.starts_with("snapcraft:"), "{msg}");
        assert!(msg.contains("published snaps"), "{msg}");
        assert!(msg.contains("verify"), "{msg}");
        assert!(msg.contains("manually"), "{msg}");
    }

    #[test]
    fn snapcraft_rollback_warns_per_target_when_evidence_present() {
        // Warn-only when targets are recorded; assert it does NOT
        // return Err so the dispatch chain continues.
        let mut ctx = TestContextBuilder::new().build();
        let mut evidence = PublishEvidence::new("snapcraft");
        evidence.extra = serde_json::json!({
            "snapcraft_targets": [
                {
                    "crate_name": "demo",
                    "package_name": "demo",
                    "channel": "stable",
                    "revision": null,
                },
                {
                    "crate_name": "widget",
                    "package_name": "widget",
                    "channel": null,
                    "revision": null,
                },
            ],
        });
        let p = SnapcraftPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());
        // Sanity-check the decode shape — both targets round-trip back
        // through the same JSON layout the warn loop consumes.
        let decoded = decode_snapcraft_targets(&evidence.extra);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].package_name, "demo");
        assert_eq!(decoded[0].channel.as_deref(), Some("stable"));
        assert_eq!(decoded[1].channel, None);
    }

    #[test]
    fn snapcraft_target_extra_roundtrips() {
        let original = vec![
            SnapcraftTarget {
                crate_name: "demo".into(),
                package_name: "demo".into(),
                channel: Some("stable".into()),
                revision: None,
            },
            SnapcraftTarget {
                crate_name: "widget".into(),
                package_name: "widget-snap".into(),
                channel: None,
                revision: None,
            },
        ];
        let extra = serde_json::json!({ "snapcraft_targets": original.clone() });
        let decoded = decode_snapcraft_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn snapcraft_target_extra_carries_no_secret_material() {
        // Defense-in-depth: serialize a target and assert no field
        // names that could leak SNAPCRAFT_LOGIN / token / auth material
        // are present.
        let t = SnapcraftTarget {
            crate_name: "demo".into(),
            package_name: "demo".into(),
            channel: Some("stable".into()),
            revision: None,
        };
        let s = serde_json::to_string(&t).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"login\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        assert!(!s.contains("\"snapcraft_login\":"), "{s}");
    }

    #[test]
    fn snapcraft_collect_targets_resolves_package_name_override() {
        let ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", Some("demo-snap"), Some("stable"))])
            .build();
        let targets = collect_snapcraft_targets(&ctx);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].crate_name, "demo");
        assert_eq!(targets[0].package_name, "demo-snap");
        assert_eq!(targets[0].channel.as_deref(), Some("stable"));
    }

    #[test]
    fn snapcraft_collect_targets_defaults_to_crate_name() {
        let ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, None)])
            .build();
        let targets = collect_snapcraft_targets(&ctx);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].package_name, "demo");
        assert_eq!(targets[0].channel, None);
    }

    #[test]
    fn snapcraft_collect_targets_skips_non_publish_configs() {
        // A snapcrafts entry with `publish: false` (or unset) must NOT
        // surface as a rollback target — the publish path also skips
        // it, and recording a target we never pushed would mislead
        // operators reading the warn line.
        let krate = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            snapcrafts: Some(vec![SnapcraftConfig {
                name: Some("demo".to_string()),
                publish: Some(false),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let ctx = TestContextBuilder::new().crates(vec![krate]).build();
        let targets = collect_snapcraft_targets(&ctx);
        assert!(targets.is_empty(), "publish:false should be filtered out");
    }
}
