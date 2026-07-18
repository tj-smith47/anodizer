use super::*;
use anodizer_core::config::{CloudSmithConfig, Config, StringOrBool};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{EnvRequirement, PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

/// Build a plain (non-dry-run) Context carrying `cloudsmiths` config so the
/// filter/requirements helpers see live entries.
fn ctx_with_cloudsmiths(entries: Vec<CloudSmithConfig>) -> Context {
    let config = Config {
        cloudsmiths: Some(entries),
        ..Default::default()
    };
    Context::new(config, ContextOptions::default())
}

fn active_entry(org: &str) -> CloudSmithConfig {
    CloudSmithConfig {
        organization: Some(org.to_string()),
        repository: Some("tools".to_string()),
        ..Default::default()
    }
}

#[test]
fn cloudsmith_publisher_classification() {
    let p = CloudsmithPublisher::new();
    assert_eq!(p.name(), "cloudsmith");
    assert_eq!(p.group(), PublisherGroup::Assets);
    assert!(!p.required());
    assert_eq!(
        p.rollback_scope_needed(),
        Some("CLOUDSMITH_API_KEY package_delete")
    );
}

#[test]
fn cloudsmith_preflight_defaults_to_pass() {
    let ctx = TestContextBuilder::new().build();
    let p = CloudsmithPublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight ok"),
        PreflightCheck::Pass
    ));
}

#[test]
fn cloudsmith_rollback_warns_when_no_targets_recorded() {
    // Empty evidence drives rollback into the no-targets branch.
    // The capture pins that production actually invoked `log.warn`
    // with the helper-formatted message — a hand-constructed expected
    // string compared against the helper output would pass even if
    // the rollback body forgot the warn entirely.
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    ctx.with_log_capture(capture.clone());
    let evidence = PublishEvidence::new("cloudsmith");
    let p = CloudsmithPublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());

    let warns = capture.warn_messages();
    assert!(
        warns.iter().any(|m| m.contains("cloudsmith")
            && m.contains("upload targets")
            && m.contains("verify")),
        "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
    );
}

/// Important #4 — per-target warn message renders a real cleanup
/// instruction (org/repo/filename), not a fake URL.
#[test]
fn cloudsmith_manual_cleanup_msg_is_actionable() {
    let target = CloudsmithTarget {
        org: "acme".to_string(),
        repo: "widget".to_string(),
        filename: "widget_1.0.0_amd64.deb".to_string(),
        slug: None,
    };
    let msg = cloudsmith_manual_cleanup_msg(&target);
    assert!(msg.contains("widget_1.0.0_amd64.deb"), "{msg}");
    assert!(msg.contains("acme/widget"), "{msg}");
    // The prior implementation rendered a `?filename=` URL — make
    // sure that shape can't sneak back in.
    assert!(!msg.contains("?filename="), "{msg}");
    assert!(!msg.contains("api.cloudsmith.io"), "{msg}");
}

/// Structured (org, repo, filename) tuples round-trip through
/// PublishEvidence.extra so a future schema change cannot silently
/// regress the rollback warn shape.
#[test]
fn cloudsmith_target_extra_roundtrips() {
    let targets = vec![
        CloudsmithTarget {
            org: "acme".to_string(),
            repo: "widget".to_string(),
            filename: "widget_1.0.0_amd64.deb".to_string(),
            slug: None,
        },
        CloudsmithTarget {
            org: "acme".to_string(),
            repo: "widget".to_string(),
            filename: "widget-1.0.0-1.x86_64.rpm".to_string(),
            slug: None,
        },
    ];
    let encoded = encode_cloudsmith_targets(&targets);
    let decoded = decode_cloudsmith_targets(&encoded);
    assert_eq!(decoded, targets);
}

// Slug captured at upload time round-trips through evidence so
// rollback can issue real DELETEs. Also pins the wire-format key
// for older anodize binaries decoding this evidence.
#[test]
fn cloudsmith_target_serde_roundtrip_with_slug() {
    let targets = vec![
        CloudsmithTarget {
            org: "acme".to_string(),
            repo: "widget".to_string(),
            filename: "widget_1.0.0_amd64.deb".to_string(),
            slug: Some("aBcD1234".to_string()),
        },
        CloudsmithTarget {
            org: "acme".to_string(),
            repo: "widget".to_string(),
            filename: "widget-1.0.0-1.x86_64.rpm".to_string(),
            slug: Some("xY9Z".to_string()),
        },
    ];
    let encoded = encode_cloudsmith_targets(&targets);
    let decoded = decode_cloudsmith_targets(&encoded);
    assert_eq!(decoded, targets);
    // Wire-format pin: serialize through evidence and inspect the
    // JSON to confirm the slug rides under the `cloudsmith_targets`
    // key (matches the pre-typed shape).
    let mut e = PublishEvidence::new("cloudsmith");
    e.extra = encoded;
    let s = serde_json::to_string(&e).expect("serialize");
    let v: serde_json::Value = serde_json::from_str(&s).expect("parse");
    let arr = v["extra"]["cloudsmith_targets"]
        .as_array()
        .expect("cloudsmith_targets array");
    let first = arr.first().expect("at least one entry");
    assert_eq!(first.get("slug").and_then(|s| s.as_str()), Some("aBcD1234"));
}

// Evidence written by versions before slug capture decodes with
// `slug = None`, so rollback degrades cleanly to the warn-only
// path. The snapshot's `#[serde(default)]` on `slug` powers this
// wire-compat path.
#[test]
fn cloudsmith_target_decode_tolerates_missing_slug_field() {
    // Hand-rolled JSON matching the pre-slug-capture evidence shape
    // — wrapped in the `PublishEvidence` envelope so deserialization
    // exercises the same path live evidence files take.
    let raw = r#"{
            "schema_version": 1,
            "publisher": "cloudsmith",
            "artifact_paths": [],
            "extra": {
                "cloudsmith_targets": [
                    {
                        "org": "acme",
                        "repo": "widget",
                        "filename": "widget_1.0.0_amd64.deb"
                    },
                    {
                        "org": "acme",
                        "repo": "widget",
                        "filename": "widget-1.0.0-1.x86_64.rpm"
                    }
                ]
            }
        }"#;
    let e: PublishEvidence = serde_json::from_str(raw).expect("deserialize");
    let decoded = decode_cloudsmith_targets(&e.extra);
    assert_eq!(decoded.len(), 2);
    assert!(
        decoded.iter().all(|t| t.slug.is_none()),
        "expected all slugs to decode as None for older evidence"
    );
    assert_eq!(decoded[0].filename, "widget_1.0.0_amd64.deb");
    assert_eq!(decoded[1].filename, "widget-1.0.0-1.x86_64.rpm");
}

// `null` slug values (the explicit serde shape when
// `Option<String>` is None) also decode to `slug = None`.
#[test]
fn cloudsmith_target_decode_tolerates_null_slug() {
    let raw = r#"{
            "schema_version": 1,
            "publisher": "cloudsmith",
            "artifact_paths": [],
            "extra": {
                "cloudsmith_targets": [
                    {
                        "org": "acme",
                        "repo": "widget",
                        "filename": "widget_1.0.0_amd64.deb",
                        "slug": null
                    }
                ]
            }
        }"#;
    let e: PublishEvidence = serde_json::from_str(raw).expect("deserialize");
    let decoded = decode_cloudsmith_targets(&e.extra);
    assert_eq!(decoded.len(), 1);
    assert!(decoded[0].slug.is_none());
}

#[test]
fn cloudsmith_target_extra_carries_no_secret_material() {
    // Structural pin: build typed evidence and assert (a) no
    // credential-shaped keys appear AND (b) the operator-public
    // upload coordinates serialize.
    let mut e = PublishEvidence::new("cloudsmith");
    e.extra = encode_cloudsmith_targets(&[CloudsmithTarget {
        org: "acme".into(),
        repo: "widget".into(),
        filename: "widget_1.0.0_amd64.deb".into(),
        slug: Some("aBcD1234".into()),
    }]);
    let s = serde_json::to_string(&e).expect("serialize");
    assert!(!s.contains("\"token\":"), "{s}");
    assert!(!s.contains("\"password\":"), "{s}");
    assert!(!s.contains("\"pat\":"), "{s}");
    assert!(!s.contains("\"auth\":"), "{s}");
    assert!(!s.contains("\"private_key\":"), "{s}");
    assert!(!s.contains("\"secret\":"), "{s}");
    assert!(!s.contains("\"api_key\":"), "{s}");
    // Positive shape: org/repo/filename + slug present.
    assert!(s.contains("\"org\":\"acme\""), "{s}");
    assert!(s.contains("\"repo\":\"widget\""), "{s}");
    assert!(s.contains("\"filename\":\"widget_1.0.0_amd64.deb\""), "{s}");
    assert!(s.contains("\"slug\":\"aBcD1234\""), "{s}");
}

// B13 — rollback against evidence whose targets all lack a slug
// (older `--rollback-only --from-run` replays, or step-3 responses
// that omitted the slug field) returns Ok and never tries to issue
// a DELETE against the Cloudsmith API. The `CLOUDSMITH_API_KEY` is
// also absent here to make doubly sure no network call fires.
#[test]
fn cloudsmith_rollback_falls_back_to_warn_when_slug_missing() {
    // Inject an empty env source so `CLOUDSMITH_API_KEY` resolves
    // unset regardless of the ambient process env; the warn-only
    // path is forced for both the no-slug AND no-token reasons.
    let mut ctx = TestContextBuilder::new().build();
    ctx.set_env_source(anodizer_core::MapEnvSource::new());
    let targets = vec![
        CloudsmithTarget {
            org: "acme".to_string(),
            repo: "widget".to_string(),
            filename: "widget_1.0.0_amd64.deb".to_string(),
            slug: None,
        },
        CloudsmithTarget {
            org: "acme".to_string(),
            repo: "widget".to_string(),
            filename: "widget-1.0.0-1.x86_64.rpm".to_string(),
            slug: None,
        },
    ];
    let mut evidence = PublishEvidence::new("cloudsmith");
    evidence.extra = encode_cloudsmith_targets(&targets);
    evidence.artifact_paths = targets
        .iter()
        .map(|t| std::path::PathBuf::from(format!("{}/{}/{}", t.org, t.repo, t.filename)))
        .collect();

    let p = CloudsmithPublisher::new();
    assert!(
        p.rollback(&mut ctx, &evidence).is_ok(),
        "rollback must return Ok in warn-only fallback"
    );

    // Pin the exact warn-line shape so a refactor of
    // `cloudsmith_manual_cleanup_msg` can't silently regress the
    // operator instructions.
    let msg = cloudsmith_manual_cleanup_msg(&targets[0]);
    assert!(msg.contains("widget_1.0.0_amd64.deb"), "{msg}");
    assert!(msg.contains("acme/widget"), "{msg}");
    assert!(msg.contains("per-package slug not surfaced"), "{msg}");
    assert!(msg.contains("Cloudsmith dashboard"), "{msg}");
}

// `active_cloudsmith_configs` keeps only the entries whose `skip:` is inactive
// — the shared filter behind `requirements` and `config_fully_inactive`.
#[test]
fn active_cloudsmith_configs_drops_skipped_entries() {
    let ctx = ctx_with_cloudsmiths(vec![
        active_entry("keep-me"),
        CloudSmithConfig {
            organization: Some("skip-me".to_string()),
            repository: Some("tools".to_string()),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        },
    ]);
    let active = active_cloudsmith_configs(&ctx);
    assert_eq!(active.len(), 1, "the skipped entry must be filtered out");
    assert_eq!(active[0].organization.as_deref(), Some("keep-me"));
}

#[test]
fn active_cloudsmith_configs_empty_without_config() {
    let ctx = TestContextBuilder::new().build();
    assert!(active_cloudsmith_configs(&ctx).is_empty());
}

// `config_fully_inactive` is `true` exactly when no active entry remains,
// so the publisher self-deactivates when every entry is skipped.
#[test]
fn config_fully_inactive_tracks_active_entries() {
    let p = CloudsmithPublisher::new();

    let all_skipped = ctx_with_cloudsmiths(vec![CloudSmithConfig {
        organization: Some("skip-me".to_string()),
        repository: Some("tools".to_string()),
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    }]);
    assert!(p.config_fully_inactive(&all_skipped));

    let has_active = ctx_with_cloudsmiths(vec![active_entry("keep-me")]);
    assert!(!p.config_fully_inactive(&has_active));

    let no_config = TestContextBuilder::new().build();
    assert!(p.config_fully_inactive(&no_config));
}

// `requirements` yields one `EnvAllOf` per active entry, naming that entry's
// resolved secret env var (explicit `secret_name`, else `CLOUDSMITH_TOKEN`).
#[test]
fn requirements_names_resolved_secret_env_per_active_entry() {
    let ctx = ctx_with_cloudsmiths(vec![
        active_entry("acme"),
        CloudSmithConfig {
            organization: Some("beta".to_string()),
            repository: Some("tools".to_string()),
            secret_name: Some("BETA_CLOUDSMITH_KEY".to_string()),
            ..Default::default()
        },
        // Skipped entries contribute no requirement.
        CloudSmithConfig {
            organization: Some("gamma".to_string()),
            repository: Some("tools".to_string()),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        },
    ]);
    let p = CloudsmithPublisher::new();
    let reqs = p.requirements(&ctx);
    assert_eq!(
        reqs,
        vec![
            EnvRequirement::EnvAllOf {
                vars: vec!["CLOUDSMITH_TOKEN".to_string()],
            },
            EnvRequirement::EnvAllOf {
                vars: vec!["BETA_CLOUDSMITH_KEY".to_string()],
            },
        ]
    );
}

#[test]
fn requirements_empty_without_config() {
    let ctx = TestContextBuilder::new().build();
    assert!(CloudsmithPublisher::new().requirements(&ctx).is_empty());
}

// Cloudsmith supports versioned packages, so nightly uploads are allowed
// (they don't clobber stable content).
#[test]
fn skips_on_nightly_is_false() {
    assert!(!CloudsmithPublisher::new().skips_on_nightly());
}

// `retain_on_rollback` defaults to `false` (participate in rollback) and
// honours the config-supplied override.
#[test]
fn retain_on_rollback_defaults_false_and_honours_override() {
    assert!(!CloudsmithPublisher::new().retain_on_rollback());
    let opted_out = CloudsmithPublisher::with_overrides(None, Some(true));
    assert!(opted_out.retain_on_rollback());
}
