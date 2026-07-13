//! Tests for the [`CargoPublisher`] adapter surface.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, Publisher, PublisherGroup};

#[test]
fn cargo_publisher_classification() {
    let p = CargoPublisher::new();
    assert_eq!(p.name(), "cargo");
    assert_eq!(p.group(), PublisherGroup::Submitter);
    assert!(p.required());
    assert_eq!(p.rollback_scope_needed(), Some("CARGO_REGISTRY_TOKEN yank"));
}

/// The yank invocation injects the supplied token as a `CARGO_REGISTRY_TOKEN`
/// ENV pair — never on the argv (which would leak it into the process list).
#[test]
fn build_yank_invocation_injects_token_via_env_not_argv() {
    let t = CargoYankTarget {
        name: "mycrate".into(),
        version: "1.2.3".into(),
        registry: None,
        index: None,
    };
    let (args, env) = build_yank_invocation(&t, Some("cio-minted-abc"));
    assert_eq!(
        args,
        vec!["yank", "--version", "1.2.3", "mycrate"],
        "argv must carry no credential"
    );
    assert!(
        !args.iter().any(|a| a.contains("cio-minted-abc")),
        "token must never appear on argv: {args:?}"
    );
    assert_eq!(
        env,
        Some((
            "CARGO_REGISTRY_TOKEN".to_string(),
            "cio-minted-abc".to_string()
        )),
        "token must be injected via the CARGO_REGISTRY_TOKEN env pair"
    );
}

/// No token (ambient inherit) and an empty token both yield no env pair, so the
/// yank inherits the process env unchanged.
#[test]
fn build_yank_invocation_no_env_pair_without_token() {
    let t = CargoYankTarget {
        name: "c".into(),
        version: "0.1.0".into(),
        registry: Some("custom".into()),
        index: None,
    };
    let (args, env) = build_yank_invocation(&t, None);
    assert!(env.is_none(), "None token must yield no env pair");
    assert!(
        args.contains(&"--registry".to_string()) && args.contains(&"custom".to_string()),
        "registry flag must be threaded: {args:?}"
    );
    let (_, empty_env) = build_yank_invocation(&t, Some(""));
    assert!(empty_env.is_none(), "empty token must yield no env pair");
}

#[test]
fn run_start_message_names_selected_total() {
    let msg = run_start_message(3);
    assert!(msg.starts_with("starting cargo publish"), "{msg}");
    assert!(msg.contains("3 selected"), "{msg}");
}

#[test]
fn run_per_crate_start_message_names_crate() {
    let msg = run_per_crate_start_message("demo");
    assert!(msg.starts_with("starting per-crate cargo publish"), "{msg}");
    assert!(msg.contains("'demo'"), "{msg}");
}

#[test]
fn run_done_message_reports_processed_count() {
    let msg = run_done_message(2);
    assert!(msg.starts_with("finished cargo publish"), "{msg}");
    assert!(msg.contains("2 selected crate(s) processed"), "{msg}");
}

#[test]
fn run_no_eligible_crates_warning_names_remediation() {
    let msg = run_no_eligible_crates_warning(5);
    assert!(msg.starts_with("cargo publisher registered"), "{msg}");
    assert!(msg.contains("0 of 5 effective"), "{msg}");
    assert!(msg.contains("nothing pushed"), "{msg}");
    assert!(msg.contains("--crate"), "{msg}");
    assert!(msg.contains("--all"), "{msg}");
}

#[test]
fn cargo_preflight_passes_when_unconfigured() {
    // No `publish.cargo` block ⇒ the token-validity probe is skipped
    // (nothing to publish), so no network round-trip occurs. The live
    // 401⇒Blocker / 2xx⇒Pass mapping is covered by
    // `publisher_preflight::tests::token_auth_*`.
    let ctx = TestContextBuilder::new().build();
    let p = CargoPublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight ok"),
        PreflightCheck::Pass
    ));
}

#[test]
fn cargo_preflight_skips_crates_io_probe_for_alternate_registry() {
    use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};

    // A non-default registry publishes with `CARGO_REGISTRIES_<NAME>_TOKEN`,
    // NOT the crates.io `CARGO_REGISTRY_TOKEN` this probe presents. Even
    // with a token present, the crates.io `/me` probe must be skipped
    // (returns Pass without a network hit) so a private-registry release is
    // never false-Blockered.
    let crate_cfg = CrateConfig {
        name: "mytool".to_string(),
        publish: Some(PublishConfig {
            cargo: Some(CargoPublishConfig {
                registry: Some("my-corp".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let ctx = TestContextBuilder::new()
        .project_name("mytool")
        .crates(vec![crate_cfg])
        .env("CARGO_REGISTRY_TOKEN", "present-but-for-another-registry")
        .build();
    let p = CargoPublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight ok"),
        PreflightCheck::Pass
    ));
}

#[test]
fn cargo_preflight_accepts_cookie_only_and_scope_denials_from_crates_io() {
    use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};
    use anodizer_core::test_helpers::env::{EnvGuard, env_mutex};
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    // crates.io authenticates the token before endpoint policy, so a 403
    // whose body is the cookie-only or endpoint-scope denial proves the
    // token is real and MUST NOT block the release as a Blocker. `authentication
    // failed` (unknown/expired token) stays a Blocker.
    let bodies: &[(&str, bool)] = &[
        (
            r#"{"errors":[{"detail":"this action can only be performed on the crates.io website"}]}"#,
            true,
        ),
        (
            r#"{"errors":[{"detail":"this token does not have the required permissions to perform this action"}]}"#,
            true,
        ),
        (r#"{"errors":[{"detail":"authentication failed"}]}"#, false),
    ];
    let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let _harness = EnvGuard::set("ANODIZE_TEST_HARNESS", "1");
    for (body, expect_pass) in bodies {
        let resp: &'static str = Box::leak(
                format!(
                    "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .into_boxed_str(),
            );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
        let _base = EnvGuard::set(
            "ANODIZER_TEST_CRATES_IO_API_BASE",
            &format!("http://{addr}"),
        );
        let ctx = TestContextBuilder::new()
            .project_name("mytool")
            .crates(vec![CrateConfig {
                name: "mytool".to_string(),
                publish: Some(PublishConfig {
                    cargo: Some(CargoPublishConfig::default()),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .env("CARGO_REGISTRY_TOKEN", "cio-test-token")
            .build();
        let check = CargoPublisher::new().preflight(&ctx).expect("preflight ok");
        if *expect_pass {
            assert!(
                matches!(check, PreflightCheck::Pass),
                "body {body}: {check:?}"
            );
        } else {
            assert!(
                matches!(&check, PreflightCheck::Blocker(m) if m.contains("token invalid")),
                "body {body}: {check:?}"
            );
        }
    }
}

#[test]
fn first_published_crate_prefers_project_name_match() {
    use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};

    let with_cargo = |name: &str| CrateConfig {
        name: name.to_string(),
        publish: Some(PublishConfig {
            cargo: Some(CargoPublishConfig::default()),
            ..Default::default()
        }),
        ..Default::default()
    };
    // Iteration order: util crate is first, but project_name matches
    // the marquee crate later in the list — the helper MUST prefer
    // the project_name match instead of first-iterated.
    let ctx = TestContextBuilder::new()
        .project_name("anodizer")
        .crates(vec![with_cargo("anodizer-util"), with_cargo("anodizer")])
        .build();

    let r = first_published_crate(&ctx).expect("eligible crate");
    assert_eq!(r.name, "anodizer");
}

#[test]
fn first_published_crate_falls_back_to_first_when_no_project_match() {
    use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};

    let with_cargo = |name: &str| CrateConfig {
        name: name.to_string(),
        publish: Some(PublishConfig {
            cargo: Some(CargoPublishConfig::default()),
            ..Default::default()
        }),
        ..Default::default()
    };
    // project_name doesn't match ANY eligible crate; fall back to
    // first-iterated to preserve historical behaviour.
    let ctx = TestContextBuilder::new()
        .project_name("ghost")
        .crates(vec![with_cargo("anodizer-util"), with_cargo("anodizer")])
        .build();

    let r = first_published_crate(&ctx).expect("eligible crate");
    assert_eq!(r.name, "anodizer-util");
}

#[test]
fn cargo_publisher_emits_visible_work_when_configured() {
    use crate::testing::assert_publisher_visible_work_contract;
    use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};

    let cargo_crate = CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        publish: Some(PublishConfig {
            cargo: Some(CargoPublishConfig::default()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = TestContextBuilder::new()
        .crates(vec![cargo_crate])
        .selected_crates(vec!["demo".to_string()])
        .dry_run(true)
        .build();
    let p = CargoPublisher::new();
    assert_publisher_visible_work_contract(&p, &mut ctx);
}
