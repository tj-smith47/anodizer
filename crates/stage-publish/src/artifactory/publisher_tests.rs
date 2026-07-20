#![allow(clippy::field_reassign_with_default)]

use super::*;
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

#[test]
fn artifactory_publisher_classification() {
    let p = ArtifactoryPublisher::new();
    assert_eq!(p.name(), "artifactory");
    assert_eq!(p.group(), PublisherGroup::Assets);
    assert!(!p.required());
    assert_eq!(p.rollback_scope_needed(), Some("ARTIFACTORY_TOKEN delete"));
}

#[test]
fn artifactory_preflight_defaults_to_pass() {
    let ctx = TestContextBuilder::new().build();
    let p = ArtifactoryPublisher::new();
    assert!(matches!(
        p.preflight(&ctx).expect("preflight ok"),
        PreflightCheck::Pass
    ));
}

#[test]
fn artifactory_rollback_warns_when_no_targets_recorded() {
    // Empty evidence drives rollback into the no-targets branch.
    // The capture pins that production actually invoked `log.warn`
    // with the helper-formatted message — a hand-constructed expected
    // string compared against the helper output would pass even if
    // the rollback body forgot the warn entirely.
    let capture = anodizer_core::log::LogCapture::new();
    let mut ctx = TestContextBuilder::new().build();
    ctx.with_log_capture(capture.clone());
    let evidence = PublishEvidence::new("artifactory");
    let p = ArtifactoryPublisher::new();
    assert!(p.rollback(&mut ctx, &evidence).is_ok());

    let warns = capture.warn_messages();
    assert!(
        warns.iter().any(|m| m.contains("artifactory")
            && m.contains("upload URLs")
            && m.contains("verify")),
        "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
    );
}

/// The empty-evidence warn text comes from the shared helper. Tests
/// across the Assets-group publishers reuse this helper so the
/// message wording can be pinned in one place.
#[test]
fn artifactory_rollback_empty_warning_msg_shape() {
    let msg = crate::publisher_helpers::rollback_empty_warning_msg("artifactory", "upload URLs");
    assert!(
        msg.starts_with("no upload URLs recorded in artifactory evidence"),
        "{msg}"
    );
    assert!(msg.contains("upload URLs"), "{msg}");
    assert!(msg.contains("verify"), "{msg}");
    assert!(msg.contains("manually"), "{msg}");
}

/// Critical #1 — rollback must reuse the publish path's basic-auth
/// credentials, not narrowly read `ARTIFACTORY_TOKEN`. Verified at
/// the seam: the helper that resolves a given entry's credentials
/// returns the configured (username, password) for an entry whose
/// config carries them.
#[test]
fn artifactory_rollback_uses_publish_credentials() {
    use anodizer_core::config::{ArtifactoryConfig, Config};
    use anodizer_core::context::ContextOptions;
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        username: Some("deployer".to_string()),
        password: Some("hunter2".to_string()),
        ..Default::default()
    }]);
    let ctx = Context::new(config, ContextOptions::default());
    let resolved = resolve_rollback_credentials(&ctx, "prod")
        .expect("entry credentials must resolve via publish-path helper");
    assert_eq!(resolved.0, "deployer");
    assert_eq!(resolved.1, "hunter2");
}

/// Critical #3 — 404 / 410 on DELETE classify as already-absent so a
/// re-run after a partial rollback does not print false failures.
#[test]
fn artifactory_rollback_treats_404_as_already_absent() {
    let outcome = classify_delete_status(reqwest::StatusCode::NOT_FOUND);
    assert!(matches!(outcome, DeleteOutcome::AlreadyAbsent));
    let outcome = classify_delete_status(reqwest::StatusCode::GONE);
    assert!(matches!(outcome, DeleteOutcome::AlreadyAbsent));
}

/// 2xx → Deleted; everything else → Failed (so 5xx still surfaces as
/// a failure for the operator).
#[test]
fn artifactory_rollback_classifies_status_buckets() {
    assert!(matches!(
        classify_delete_status(reqwest::StatusCode::OK),
        DeleteOutcome::Deleted
    ));
    assert!(matches!(
        classify_delete_status(reqwest::StatusCode::NO_CONTENT),
        DeleteOutcome::Deleted
    ));
    assert!(matches!(
        classify_delete_status(reqwest::StatusCode::UNAUTHORIZED),
        DeleteOutcome::Failed(_)
    ));
    assert!(matches!(
        classify_delete_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
        DeleteOutcome::Failed(_)
    ));
}

/// Round-trip the structured (entry, url) JSON shape so a future
/// schema change cannot silently break rollback's entry lookup.
#[test]
fn artifactory_rollback_target_extra_roundtrips() {
    let targets = vec![
        ArtifactoryTarget {
            entry: "prod".to_string(),
            url: "https://art.example.com/repo/foo.tar.gz".to_string(),
        },
        ArtifactoryTarget {
            entry: "staging".to_string(),
            url: "https://art.example.com/staging/bar.zip".to_string(),
        },
    ];
    let encoded = encode_artifactory_targets(&targets);
    let decoded = decode_artifactory_targets(&encoded);
    assert_eq!(decoded, targets);
}

#[test]
fn artifactory_target_extra_carries_no_secret_material() {
    // Structural pin: build typed evidence with a populated
    // variant and assert (a) no credential-shaped keys appear AND
    // (b) the operator-public upload coordinates are preserved.
    let mut e = anodizer_core::PublishEvidence::new("artifactory");
    e.extra = encode_artifactory_targets(&[ArtifactoryTarget {
        entry: "prod".into(),
        url: "https://art.example.com/repo/foo.tar.gz".into(),
    }]);
    let s = serde_json::to_string(&e).expect("serialize");
    assert!(!s.contains("\"token\":"), "{s}");
    assert!(!s.contains("\"password\":"), "{s}");
    assert!(!s.contains("\"pat\":"), "{s}");
    assert!(!s.contains("\"username\":"), "{s}");
    assert!(!s.contains("\"private_key\":"), "{s}");
    assert!(!s.contains("\"secret\":"), "{s}");
    assert!(!s.contains("\"api_key\":"), "{s}");
    // Positive shape: operator-public coordinates present.
    assert!(s.contains("\"entry\":\"prod\""), "{s}");
    assert!(
        s.contains("\"url\":\"https://art.example.com/repo/foo.tar.gz\""),
        "{s}"
    );
}

/// A non-Artifactory variant decodes to an empty vec so rollback
/// falls back to URL-only deletion without panicking.
#[test]
fn artifactory_rollback_target_extra_tolerates_missing_field() {
    assert!(decode_artifactory_targets(&anodizer_core::PublishEvidenceExtra::Empty).is_empty());
    // Wrong variant: a homebrew evidence is not an artifactory
    // evidence — defensive isolation between publishers.
    let homebrew = anodizer_core::PublishEvidenceExtra::Homebrew(
        anodizer_core::publish_evidence::HomebrewExtra {
            homebrew_targets: Vec::new(),
        },
    );
    assert!(decode_artifactory_targets(&homebrew).is_empty());
}

// -----------------------------------------------------------------
// parallel_delete — live DELETE fan-out classification
// -----------------------------------------------------------------

use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder};

fn delete_client() -> reqwest::blocking::Client {
    anodizer_core::http::blocking_client(std::time::Duration::from_secs(5)).expect("client")
}

/// A 2xx DELETE is counted as deleted and the request reaches the
/// wire as an actual HTTP DELETE.
#[test]
fn parallel_delete_2xx_counts_as_deleted() {
    let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "DELETE",
        path_pattern: "/repo/gone.tar.gz",
        response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let jobs = vec![RollbackJob {
        url: format!("http://{addr}/repo/gone.tar.gz"),
        basic_auth: Some(("u".to_string(), "p".to_string())),
        bearer: None,
    }];
    let (deleted, absent, failed) = parallel_delete(&delete_client(), &jobs, &log);
    assert_eq!((deleted, absent, failed), (1, 0, 0));

    let entries = log_recorder.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].method, "DELETE");
    assert_eq!(entries[0].path, "/repo/gone.tar.gz");
}

/// A 404 DELETE classifies as already-absent (not failed), so a
/// re-run after a partial rollback doesn't print phantom failures.
#[test]
fn parallel_delete_404_counts_as_already_absent() {
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "DELETE",
        path_pattern: "/repo/missing.tar.gz",
        response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let jobs = vec![RollbackJob {
        url: format!("http://{addr}/repo/missing.tar.gz"),
        basic_auth: None,
        bearer: Some("tok".to_string()),
    }];
    let (deleted, absent, failed) = parallel_delete(&delete_client(), &jobs, &log);
    assert_eq!((deleted, absent, failed), (0, 1, 0));
}

/// A 5xx DELETE classifies as failed and emits an operator-facing
/// warn naming the URL.
#[test]
fn parallel_delete_5xx_counts_as_failed_and_warns() {
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "DELETE",
        path_pattern: "/repo/boom.tar.gz",
        response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let capture = anodizer_core::log::LogCapture::new();
    let log =
        StageLogger::new("artifactory", Verbosity::Quiet).with_capture_handle(capture.clone());
    let url = format!("http://{addr}/repo/boom.tar.gz");
    let jobs = vec![RollbackJob {
        url: url.clone(),
        basic_auth: Some(("u".to_string(), "p".to_string())),
        bearer: None,
    }];
    let (deleted, absent, failed) = parallel_delete(&delete_client(), &jobs, &log);
    assert_eq!((deleted, absent, failed), (0, 0, 1));
    assert!(
        capture
            .warn_messages()
            .iter()
            .any(|m| m.contains("boom.tar.gz") && m.contains("manual cleanup")),
        "expected failed-DELETE warn naming the URL; got: {:?}",
        capture.warn_messages()
    );
}

/// A transport error (connection refused — no responder listening)
/// counts as failed and emits a transport-error warn.
#[test]
fn parallel_delete_transport_error_counts_as_failed() {
    let capture = anodizer_core::log::LogCapture::new();
    let log =
        StageLogger::new("artifactory", Verbosity::Quiet).with_capture_handle(capture.clone());
    // Port 1 on loopback refuses connections.
    let jobs = vec![RollbackJob {
        url: "http://127.0.0.1:1/repo/unreachable.tar.gz".to_string(),
        basic_auth: None,
        bearer: Some("tok".to_string()),
    }];
    let (deleted, absent, failed) = parallel_delete(&delete_client(), &jobs, &log);
    assert_eq!((deleted, absent, failed), (0, 0, 1));
    assert!(
        capture
            .warn_messages()
            .iter()
            .any(|m| m.contains("transport error")),
        "expected transport-error warn; got: {:?}",
        capture.warn_messages()
    );
}

/// A mixed batch larger than ROLLBACK_PARALLELISM exercises the
/// chunked fan-out and aggregates every bucket correctly.
#[test]
fn parallel_delete_mixed_batch_aggregates_all_buckets() {
    let (addr, _log) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/ok1",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/ok2",
            response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/gone",
            response: "HTTP/1.1 410 Gone\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/bad",
            response: "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
    ]);
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let mk = |p: &str| RollbackJob {
        url: format!("http://{addr}{p}"),
        basic_auth: Some(("u".to_string(), "p".to_string())),
        bearer: None,
    };
    // Five jobs > ROLLBACK_PARALLELISM (4) so the chunking loop runs
    // more than once. `/ok1` repeated lands two deletes.
    let jobs = vec![mk("/ok1"), mk("/ok2"), mk("/gone"), mk("/bad"), mk("/ok1")];
    let (deleted, absent, failed) = parallel_delete(&delete_client(), &jobs, &log);
    assert_eq!(deleted, 3, "ok1 + ok2 + ok1");
    assert_eq!(absent, 1, "410 Gone");
    assert_eq!(failed, 1, "403 Forbidden");
}

/// Full rollback through the Publisher trait: structured evidence
/// resolves per-entry basic auth and issues a live DELETE that the
/// responder records, then logs the summary line.
#[test]
fn rollback_issues_delete_for_recorded_url() {
    use anodizer_core::config::{ArtifactoryConfig, Config};
    use anodizer_core::context::ContextOptions;
    let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "DELETE",
        path_pattern: "/repo/foo.tar.gz",
        response: "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let url = format!("http://{addr}/repo/foo.tar.gz");

    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some(format!("http://{addr}/repo/")),
        username: Some("deployer".to_string()),
        password: Some("hunter2".to_string()),
        ..Default::default()
    }]);
    let mut ctx = Context::new(config, ContextOptions::default());

    let mut evidence = PublishEvidence::new("artifactory");
    evidence.artifact_paths = vec![std::path::PathBuf::from(&url)];
    evidence.extra = encode_artifactory_targets(&[ArtifactoryTarget {
        entry: "prod".to_string(),
        url: url.clone(),
    }]);

    let p = ArtifactoryPublisher::new();
    p.rollback(&mut ctx, &evidence).expect("rollback ok");

    let entries = log_recorder.lock().unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].method, "DELETE");
    assert_eq!(entries[0].path, "/repo/foo.tar.gz");
}
