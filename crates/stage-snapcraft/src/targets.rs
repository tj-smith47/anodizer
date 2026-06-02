//! Per-target snapshot recorded in `PublishEvidence::extra.snapcraft_targets`.
//!
//! Carved out of `publish_stage.rs` to keep the upload-flow file focused on
//! the `Stage` impl. The serde shape here is wire-stable: it is the value
//! carried in `PublishEvidence::extra.snapcraft_targets` and consumed by
//! `--rollback-only --from-run` to surface per-target channel-management
//! pointers. Byte-shape changes here are breaking for replay consumers.

use anodizer_core::context::Context;

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

/// Walk `ctx.config.crates[].snapcrafts[]` and build one
/// [`SnapcraftTarget`] per opted-in snap config. Mirrors the publish
/// stage's filters: `selected_crates` gate, `publish: true` opt-in.
/// Skipped configs (`skip: true`) are excluded here too so the recorded
/// evidence matches what actually shipped.
pub(crate) fn collect_snapcraft_targets(ctx: &Context) -> Vec<SnapcraftTarget> {
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
            let package_name = snap_cfg.name.clone().unwrap_or_else(|| krate.name.clone());
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
        // surface as an evidence target — the publish path also skips
        // it, and recording a target we never pushed would mislead
        // operators reading any replay consumer.
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
