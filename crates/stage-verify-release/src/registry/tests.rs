use super::*;
use anodizer_core::test_helpers::responder::{
    canned_http_response, spawn_oneshot_http_responder, spawn_oneshot_http_responder_with,
};

/// A retry policy that spends no wall-clock time.
const NO_WAIT: RetryPolicy = RetryPolicy {
    max_attempts: 1,
    base_delay: std::time::Duration::ZERO,
    max_delay: std::time::Duration::ZERO,
};

/// A manifest document small enough to read in an assertion.
const MANIFEST_BODY: &str = "{\"schemaVersion\":2}";

fn probe_logger() -> StageLogger {
    StageLogger::new("verify-release", anodizer_core::log::Verbosity::Quiet)
}

/// Ask a loopback responder for `repository:reference`.
fn probe_local(
    addr: std::net::SocketAddr,
    repository: &str,
    reference: &str,
) -> Result<Option<String>> {
    let client = anodizer_core::http::blocking_client(PROBE_TIMEOUT).expect("client");
    let parsed = ImageRef {
        registry: addr.to_string(),
        repository: repository.to_string(),
        reference: reference.to_string(),
    };
    manifest_digest_with(&client, &parsed, &NO_WAIT, None, &probe_logger())
}

#[test]
fn a_host_qualified_reference_splits_into_registry_repository_and_tag() {
    assert_eq!(
        parse_image_ref("ghcr.io/owner/app:1.0.0").unwrap(),
        ImageRef {
            registry: "ghcr.io".into(),
            repository: "owner/app".into(),
            reference: "1.0.0".into(),
        }
    );
}

#[test]
fn a_bare_name_belongs_to_docker_hubs_library_namespace() {
    assert_eq!(
        parse_image_ref("alpine").unwrap(),
        ImageRef {
            registry: "docker.io".into(),
            repository: "library/alpine".into(),
            reference: "latest".into(),
        }
    );
    assert_eq!(
        parse_image_ref("owner/app").unwrap(),
        ImageRef {
            registry: "docker.io".into(),
            repository: "owner/app".into(),
            reference: "latest".into(),
        }
    );
}

/// A `:` inside a registry port is not a tag separator, and a `@sha256:`
/// suffix replaces the tag entirely.
#[test]
fn a_registry_port_and_a_digest_suffix_are_read_correctly() {
    assert_eq!(
        parse_image_ref("localhost:5000/app").unwrap(),
        ImageRef {
            registry: "localhost:5000".into(),
            repository: "app".into(),
            reference: "latest".into(),
        }
    );
    assert_eq!(
        parse_image_ref("ghcr.io/owner/app@sha256:abc").unwrap(),
        ImageRef {
            registry: "ghcr.io".into(),
            repository: "owner/app".into(),
            reference: "sha256:abc".into(),
        }
    );
}

#[test]
fn docker_hub_references_are_asked_of_the_distribution_endpoint() {
    let url = parse_image_ref("owner/app:1.0.0").unwrap().manifest_url();
    assert_eq!(
        url,
        "https://registry-1.docker.io/v2/owner/app/manifests/1.0.0"
    );
    let local = parse_image_ref("localhost:5000/app:1.0.0")
        .unwrap()
        .manifest_url();
    assert_eq!(local, "http://localhost:5000/v2/app/manifests/1.0.0");
}

/// The verdict is the SHA-256 over the served bytes, which is what a content
/// digest is defined to be.
#[test]
fn a_served_manifest_answers_with_its_own_content_digest() {
    let (addr, _calls) =
        spawn_oneshot_http_responder(vec![canned_http_response("200 OK", MANIFEST_BODY)]);
    let digest = probe_local(addr, "owner/app", "1.0.0").unwrap();
    assert_eq!(digest, Some(content_digest(MANIFEST_BODY.as_bytes())));
}

#[test]
fn an_unknown_reference_answers_absent_rather_than_failing() {
    let (addr, _calls) =
        spawn_oneshot_http_responder(vec![canned_http_response("404 Not Found", "")]);
    assert_eq!(probe_local(addr, "owner/app", "1.0.0").unwrap(), None);
}

/// A 401 the probe cannot answer must read as unverifiable — a pushed tag
/// hidden behind a credential the probe lacks is still live for everyone
/// holding one.
#[test]
fn an_unanswerable_challenge_is_an_error_not_an_absence() {
    let (addr, _calls) =
        spawn_oneshot_http_responder(vec![canned_http_response("401 Unauthorized", "")]);
    let err = probe_local(addr, "owner/app", "1.0.0").expect_err("401 is unverifiable");
    assert_eq!(http_status(&err), 401, "{err:#}");
}

/// The standard bearer dance: the registry names a realm, the probe fetches a
/// token there and re-asks the manifest with it.
#[test]
fn a_bearer_challenge_is_exchanged_for_a_token_and_the_ask_repeats() {
    let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
        let token = "{\"token\":\"tok\"}";
        vec![
            format!(
                "HTTP/1.1 401 Unauthorized\r\n\
                 WWW-Authenticate: Bearer realm=\"http://{addr}/token\",service=\"reg\",\
                 scope=\"repository:owner/app:pull\"\r\n\
                 Content-Length: 0\r\n\r\n"
            ),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{token}",
                token.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{MANIFEST_BODY}",
                MANIFEST_BODY.len()
            ),
        ]
    });
    let digest = probe_local(addr, "owner/app", "1.0.0").unwrap();
    assert_eq!(digest, Some(content_digest(MANIFEST_BODY.as_bytes())));
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "the 401, the token fetch and the authorized re-ask"
    );
}

#[test]
fn a_challenge_parameter_is_read_out_of_the_header() {
    let challenge = "Bearer realm=\"https://auth.example/token\",service=\"registry\"";
    assert_eq!(
        challenge_param(challenge, "realm").as_deref(),
        Some("https://auth.example/token")
    );
    assert_eq!(
        challenge_param(challenge, "service").as_deref(),
        Some("registry")
    );
    assert_eq!(challenge_param(challenge, "scope"), None);
}

#[test]
fn a_digest_matches_whatever_prefix_and_case_it_was_recorded_in() {
    assert!(digests_match("sha256:ABC", "sha256:abc"));
    assert!(digests_match("abc", "sha256:abc"));
    assert!(!digests_match("sha256:abc", "sha256:def"));
    assert!(!digests_match("", "sha256:abc"));
}

/// Every spelling `docker login` writes for one registry names the same
/// credential, Docker Hub's legacy index key included.
#[test]
fn a_stored_credential_is_found_under_every_spelling_docker_writes() {
    let auths = serde_json::json!({
        "https://index.docker.io/v1/": { "auth": "aHViCg==" },
        "ghcr.io": { "auth": "Z2hjcgo=" },
        "https://other.example": { "auth": "b3RoZXIK" },
    });
    let map = auths.as_object().expect("object");
    assert_eq!(
        stored_auth_for(map, "docker.io").as_deref(),
        Some("aHViCg==")
    );
    assert_eq!(stored_auth_for(map, "ghcr.io").as_deref(), Some("Z2hjcgo="));
    assert_eq!(
        stored_auth_for(map, "other.example").as_deref(),
        Some("b3RoZXIK")
    );
    assert_eq!(stored_auth_for(map, "absent.example"), None);
}
