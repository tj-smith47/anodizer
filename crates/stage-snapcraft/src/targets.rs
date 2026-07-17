//! Per-target snapshot recorded in `PublishEvidence::extra.snapcraft_targets`.
//!
//! Carved out of `publish_stage.rs` to keep the upload-flow file focused on
//! the `Stage` impl. The serde shape here is wire-stable: it is the value
//! carried in `PublishEvidence::extra.snapcraft_targets` and consumed by
//! `--rollback-only --from-run` to surface per-target channel-management
//! pointers. Byte-shape changes here are breaking for replay consumers.

use anodizer_core::config::CrateConfig;
use anodizer_core::context::Context;

/// The crate's primary binary name — the first build's `binary`, falling back
/// to the crate name (BuildConfig's documented fallback). This is the last
/// resort of the snap-name resolution chain (`snapcrafts[].name` → project
/// name → primary binary), mirroring `generate_snap_yaml`, which names the
/// shipped snap after the first staged binary when nothing else is set.
pub(crate) fn crate_primary_binary(krate: &CrateConfig) -> String {
    krate
        .builds
        .as_ref()
        .and_then(|b| b.first())
        .and_then(|b| b.binary.clone())
        .unwrap_or_else(|| krate.name.clone())
}

/// Serialized shape of a recorded snapcraft publish. One entry per
/// `(crate, snapcraft config)` tuple whose `publish: true` opt-in
/// matched the [`SnapcraftPublishStage`](crate::SnapcraftPublishStage)
/// iteration order.
///
/// `package_name` is the resolved Snap Store package name (defaults to
/// the crate name when `snapcrafts[].name` is not overridden);
/// `channel` is the rendered channel template (or `None` when the
/// publish path falls back to the `grade`-derived default).
///
/// Aliased to the core-owned snapshot so the evidence schema lives in
/// [`anodizer_core::publish_evidence`] and credential-shaped fields
/// (`SNAPCRAFT_LOGIN`, token, auth) have no slot to land in.
pub(crate) type SnapcraftTarget = anodizer_core::publish_evidence::SnapcraftTargetSnapshot;

/// Walk the crate universe's `snapcrafts[]` (top-level `crates` plus every
/// `workspaces[].crates` entry) and build one
/// [`SnapcraftTarget`] per opted-in snap config. Mirrors the publish
/// stage's filters: `selected_crates` gate, `publish: true` opt-in.
/// Skipped configs (`skip: true`) are excluded here too so the recorded
/// evidence matches what actually shipped.
pub(crate) fn collect_snapcraft_targets(ctx: &Context) -> Vec<SnapcraftTarget> {
    let selected = &ctx.options.selected_crates;
    let mut out: Vec<SnapcraftTarget> = Vec::new();
    for krate in ctx.config.crate_universe() {
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
            // Treat a render-error on `if:` as proceed for target counting —
            // the canonical hard-error site is in build/publish, which sees
            // the same condition and re-renders it for real diagnostics.
            let proceed = anodizer_core::config::evaluate_if_condition(
                snap_cfg.if_condition.as_deref(),
                "snapcraft target",
                |t| ctx.render_template(t),
            )
            .unwrap_or(true);
            if !proceed {
                continue;
            }
            // Same fallback order as the upload path's `resolve_snap_name`
            // and the built snap.yaml (config name → project name → primary
            // binary) so the recorded package_name always matches the snap
            // actually uploaded.
            let package_name = snap_cfg.name.clone().unwrap_or_else(|| {
                if ctx.config.project_name.is_empty() {
                    crate_primary_binary(krate)
                } else {
                    ctx.config.project_name.clone()
                }
            });
            // `channel_templates` is a Vec rendered
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
                // Empty when the Version template var is unpopulated (e.g. a
                // bare test context) — an empty string is not a probeable
                // version, so record the honest absence instead.
                version: Some(ctx.version()).filter(|v| !v.is_empty()),
                held_for_review: false,
            });
        }
    }
    out
}

/// Decode the typed Snapcraft variant from `PublishEvidence::extra`.
/// Returns an empty Vec when the variant doesn't match.
#[cfg(test)]
pub(crate) fn decode_snapcraft_targets(
    extra: &anodizer_core::PublishEvidenceExtra,
) -> Vec<SnapcraftTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Snapcraft(s) => s.snapcraft_targets.clone(),
        _ => Vec::new(),
    }
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
            tag_template: Some("v{{ .Version }}".to_string()),
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
    fn snapcraft_target_extra_roundtrips() {
        let original = vec![
            SnapcraftTarget {
                crate_name: "demo".into(),
                package_name: "demo".into(),
                channel: Some("stable".into()),
                revision: None,
                ..Default::default()
            },
            SnapcraftTarget {
                crate_name: "widget".into(),
                package_name: "widget-snap".into(),
                channel: None,
                revision: None,
                ..Default::default()
            },
        ];
        let extra = anodizer_core::PublishEvidenceExtra::Snapcraft(
            anodizer_core::publish_evidence::SnapcraftExtra {
                snapcraft_targets: original.clone(),
            },
        );
        let decoded = decode_snapcraft_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn snapcraft_target_extra_carries_no_secret_material() {
        // Structural pin: build typed evidence and assert (a) no
        // credential-shaped keys appear AND (b) the operator-public
        // package coordinates serialize.
        let mut e = anodizer_core::PublishEvidence::new("snapcraft");
        e.extra = anodizer_core::PublishEvidenceExtra::Snapcraft(
            anodizer_core::publish_evidence::SnapcraftExtra {
                snapcraft_targets: vec![SnapcraftTarget {
                    crate_name: "demo".into(),
                    package_name: "demo".into(),
                    channel: Some("stable".into()),
                    revision: None,
                    ..Default::default()
                }],
            },
        );
        let s = serde_json::to_string(&e).expect("serialize");
        assert!(!s.contains("\"token\":"), "{s}");
        assert!(!s.contains("\"login\":"), "{s}");
        assert!(!s.contains("\"password\":"), "{s}");
        assert!(!s.contains("\"auth\":"), "{s}");
        assert!(!s.contains("\"api_key\":"), "{s}");
        assert!(!s.contains("\"snapcraft_login\":"), "{s}");
        assert!(!s.contains("\"private_key\":"), "{s}");
        assert!(!s.contains("\"secret\":"), "{s}");
        // Positive shape: package coordinates serialize.
        assert!(s.contains("\"package_name\":\"demo\""), "{s}");
        assert!(s.contains("\"channel\":\"stable\""), "{s}");
        assert!(s.contains("\"crate_name\":\"demo\""), "{s}");
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
    fn snapcraft_collect_targets_default_name_mirrors_resolve_snap_name() {
        // Same fallback order as the upload path's `resolve_snap_name`:
        // project name when set, crate name otherwise — so the recorded
        // package_name always names the snap actually uploaded.
        let mut ctx = TestContextBuilder::new()
            .crates(vec![snap_crate("demo", None, None)])
            .build();
        ctx.template_vars_mut().set("Version", "1.0.0");
        let targets = collect_snapcraft_targets(&ctx);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].package_name, "test-project");
        assert_eq!(targets[0].channel, None);
        assert_eq!(targets[0].version.as_deref(), Some("1.0.0"));

        let ctx = TestContextBuilder::new()
            .project_name("")
            .crates(vec![snap_crate("demo", None, None)])
            .build();
        let targets = collect_snapcraft_targets(&ctx);
        assert_eq!(targets[0].package_name, "demo");

        // With a build declaring an explicit binary name, the last-resort
        // fallback is that binary — the name generate_snap_yaml stamps into
        // the shipped snap — not the crate name.
        let mut krate = snap_crate("demo-cli", None, None);
        krate.builds = Some(vec![anodizer_core::config::BuildConfig {
            binary: Some("demo".to_string()),
            ..Default::default()
        }]);
        let ctx = TestContextBuilder::new()
            .project_name("")
            .crates(vec![krate])
            .build();
        let targets = collect_snapcraft_targets(&ctx);
        assert_eq!(targets[0].package_name, "demo");
    }

    #[test]
    fn snapcraft_collect_targets_sees_workspace_only_crate() {
        // A crate declared only under `workspaces[].crates` must surface
        // its opted-in snap configs: the walk resolves through the crate
        // universe, so a pure-workspace config records the same evidence a
        // top-level `crates:` entry would.
        let ctx = TestContextBuilder::new()
            .workspaces(vec![anodizer_core::config::WorkspaceConfig {
                name: "ws".to_string(),
                crates: vec![snap_crate("ws-only", Some("ws-snap"), Some("stable"))],
                ..Default::default()
            }])
            .build();
        assert!(
            ctx.config.crates.is_empty(),
            "fixture must be a pure-workspace config"
        );
        let targets = collect_snapcraft_targets(&ctx);
        assert_eq!(targets.len(), 1, "{targets:?}");
        assert_eq!(targets[0].crate_name, "ws-only");
        assert_eq!(targets[0].package_name, "ws-snap");
        assert_eq!(targets[0].channel.as_deref(), Some("stable"));
    }

    #[test]
    fn snapcraft_collect_targets_skips_non_publish_configs() {
        // A snapcrafts entry with `publish: false` (or unset) must NOT
        // surface as an evidence target — the publish path also skips
        // it, and recording a target we never pushed would mislead
        // operators reading any replay consumer.
        let krate = CrateConfig {
            name: "demo".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
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
