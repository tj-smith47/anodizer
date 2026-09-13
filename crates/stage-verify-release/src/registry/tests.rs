use super::*;
use anodizer_core::test_helpers::responder::{
    canned_http_response, spawn_capturing_http_responder_with, spawn_oneshot_http_responder,
    spawn_oneshot_http_responder_bytes, spawn_oneshot_http_responder_with,
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

/// A token endpoint that answers a token under `access_token` — the OAuth2
/// spelling, which is what Google Artifact Registry and Azure answer with —
/// authorizes the re-ask the same way.
#[test]
fn an_access_token_is_read_the_same_way_as_a_token() {
    let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
        let token = "{\"access_token\":\"tok\"}";
        vec![
            bearer_challenge(addr),
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

/// A token endpoint that refuses hands the registry's own 401 back, so the
/// image reads as unverifiable rather than absent. The re-ask is never made:
/// there is no token to make it with.
#[test]
fn a_token_endpoint_that_refuses_leaves_the_registrys_own_401() {
    let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
        vec![
            bearer_challenge(addr),
            canned_http_response("403 Forbidden", "").to_string(),
        ]
    });
    let err = probe_local(addr, "owner/app", "1.0.0").expect_err("401 is unverifiable");
    assert_eq!(http_status(&err), 401, "{err:#}");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the 401 and the refused token fetch, and no re-ask"
    );
}

/// A token endpoint answering something that is not JSON (an HTML error page
/// behind a proxy, say) is the same non-answer as a refusal.
#[test]
fn a_token_endpoint_answering_something_other_than_json_is_a_non_answer() {
    let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
        vec![
            bearer_challenge(addr),
            canned_http_response("200 OK", "<html>proxy error</html>").to_string(),
        ]
    });
    let err = probe_local(addr, "owner/app", "1.0.0").expect_err("401 is unverifiable");
    assert_eq!(http_status(&err), 401, "{err:#}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A well-formed JSON answer naming neither key carries no token, so the
/// probe reports the 401 rather than re-asking with an empty credential.
#[test]
fn a_json_answer_naming_no_token_key_is_a_non_answer() {
    let (addr, calls) = spawn_oneshot_http_responder_with(|addr| {
        vec![
            bearer_challenge(addr),
            canned_http_response("200 OK", "{\"expires_in\":300}").to_string(),
        ]
    });
    let err = probe_local(addr, "owner/app", "1.0.0").expect_err("401 is unverifiable");
    assert_eq!(http_status(&err), 401, "{err:#}");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

/// A `Bearer` challenge naming this responder's own `/token` endpoint.
fn bearer_challenge(addr: std::net::SocketAddr) -> String {
    format!(
        "HTTP/1.1 401 Unauthorized\r\n\
         WWW-Authenticate: Bearer realm=\"http://{addr}/token\",service=\"reg\",\
         scope=\"repository:owner/app:pull\"\r\n\
         Content-Length: 0\r\n\r\n"
    )
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

/// docker treats the whole `127.0.0.0/8` range and the IPv6 loopback as
/// local, bracketed or not, with or without a port. Everything else is asked
/// over TLS.
#[test]
fn every_loopback_spelling_is_asked_over_plain_http() {
    for host in [
        "localhost",
        "localhost:5000",
        "127.0.0.1",
        "127.0.0.1:5000",
        "127.0.0.53",
        "::1",
        "[::1]",
        "[::1]:5000",
    ] {
        assert!(is_loopback(host), "{host} names the local machine");
    }
    for host in [
        "ghcr.io",
        "registry.example:5000",
        "127.example.com",
        "[2001:db8::1]",
        "[2001:db8::1]:5000",
    ] {
        assert!(!is_loopback(host), "{host} is a remote registry");
    }
    assert_eq!(
        parse_image_ref("[::1]:5000/app:1.0.0")
            .unwrap()
            .manifest_url(),
        "http://[::1]:5000/v2/app/manifests/1.0.0"
    );
}

/// The registry names the digest a pull resolves the reference to, so that
/// answer is preferred over re-deriving one from the served document.
#[test]
fn the_digest_the_registry_names_is_preferred_over_the_body_hash() {
    let named = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let (addr, _calls) = spawn_oneshot_http_responder_with(|_| {
        vec![format!(
            "HTTP/1.1 200 OK\r\nDocker-Content-Digest: {named}\r\nContent-Length: {}\r\n\r\n{MANIFEST_BODY}",
            MANIFEST_BODY.len()
        )]
    });
    let digest = probe_local(addr, "owner/app", "1.0.0").unwrap();
    assert_eq!(digest.as_deref(), Some(named));
    assert_ne!(
        digest.as_deref(),
        Some(content_digest(MANIFEST_BODY.as_bytes()).as_str()),
        "the fixture must distinguish the two answers"
    );
}

/// A content digest is the SHA-256 over the bytes the registry served, and a
/// manifest is not required to be valid UTF-8. Decoding the body as text
/// would substitute U+FFFD for the byte below and answer a digest no registry
/// serves.
#[test]
fn a_manifest_that_is_not_text_still_answers_with_its_own_content_digest() {
    let raw = b"{\"schemaVersion\":2,\"x\":\"\xff\"}".to_vec();
    let mut response =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", raw.len()).into_bytes();
    response.extend_from_slice(&raw);
    let (addr, _calls) = spawn_oneshot_http_responder_bytes(vec![response]);
    let digest = probe_local(addr, "owner/app", "1.0.0").unwrap();
    assert_eq!(digest, Some(content_digest(&raw)));
    assert_ne!(
        digest,
        Some(content_digest(String::from_utf8_lossy(&raw).as_bytes())),
        "the fixture must distinguish the raw bytes from a lossy decode"
    );
}

/// A stock `registry:2` behind htpasswd answers `Basic`, whose realm is a
/// human-readable name rather than a token endpoint. The credential itself is
/// what such a registry wants.
#[test]
#[serial_test::serial(env)]
fn a_basic_challenge_is_answered_with_the_stored_credential() {
    let (addr, requests) = spawn_capturing_http_responder_with(|_| {
        vec![
            "HTTP/1.1 401 Unauthorized\r\n\
             WWW-Authenticate: Basic realm=\"Registry Realm\"\r\n\
             Content-Length: 0\r\n\r\n"
                .to_string(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{MANIFEST_BODY}",
                MANIFEST_BODY.len()
            ),
        ]
    });
    let _config = docker_config_with_auth(&addr.to_string(), "dXNlcjpwYXNz");
    let digest = probe_local(addr, "owner/app", "1.0.0").unwrap();
    assert_eq!(digest, Some(content_digest(MANIFEST_BODY.as_bytes())));
    let asked = requests.lock().unwrap().clone();
    assert_eq!(asked.len(), 2, "the challenge and the authorized re-ask");
    assert!(
        asked[1].contains("Basic dXNlcjpwYXNz"),
        "the re-ask carries the stored credential: {:?}",
        asked[1]
    );
}

/// A private repository is probed as its publisher: the credential docker
/// stored for the registry is what the token endpoint is asked with.
#[test]
#[serial_test::serial(env)]
fn a_stored_credential_is_sent_to_the_token_endpoint() {
    let (addr, requests) = spawn_capturing_http_responder_with(|addr| {
        let token = "{\"token\":\"tok\"}";
        vec![
            format!(
                "HTTP/1.1 401 Unauthorized\r\n\
                 WWW-Authenticate: Bearer realm=\"http://{addr}/token\",service=\"reg\"\r\n\
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
    let _config = docker_config_with_auth(&addr.to_string(), "dXNlcjpwYXNz");
    let digest = probe_local(addr, "owner/app", "1.0.0").unwrap();
    assert_eq!(digest, Some(content_digest(MANIFEST_BODY.as_bytes())));
    let asked = requests.lock().unwrap().clone();
    assert_eq!(asked.len(), 3, "the 401, the token fetch, the re-ask");
    assert!(
        asked[1].contains("Basic dXNlcjpwYXNz"),
        "the token request carries the stored credential: {:?}",
        asked[1]
    );
    assert!(
        asked[2].contains("Bearer tok"),
        "the re-ask carries the issued token: {:?}",
        asked[2]
    );
}

/// Write a docker `config.json` holding `auth` for `registry` and point
/// `DOCKER_CONFIG` at it. The guard restores the environment on drop.
fn docker_config_with_auth(
    registry: &str,
    auth: &str,
) -> (
    tempfile::TempDir,
    anodizer_core::test_helpers::env::EnvGuard,
) {
    let dir = tempfile::tempdir().expect("temp dir");
    let auths: serde_json::Value = serde_json::json!({
        "auths": { registry: { "auth": auth } },
    });
    std::fs::write(
        dir.path().join("config.json"),
        serde_json::to_string(&auths).expect("json"),
    )
    .expect("write config.json");
    let guard = anodizer_core::test_helpers::env::EnvGuard::set(
        "DOCKER_CONFIG",
        dir.path().to_string_lossy().as_ref(),
    );
    (dir, guard)
}

/// The credential file is read from `DOCKER_CONFIG` when it is set, and a
/// missing or malformed file leaves the probe anonymous rather than failing
/// it.
#[test]
#[serial_test::serial(env)]
fn the_stored_credential_is_read_from_docker_config_and_degrades_to_none() {
    let dir = tempfile::tempdir().expect("temp dir");
    let _guard = anodizer_core::test_helpers::env::EnvGuard::set(
        "DOCKER_CONFIG",
        dir.path().to_string_lossy().as_ref(),
    );
    assert_eq!(
        stored_registry_auth("ghcr.io"),
        None,
        "no config.json at all"
    );
    std::fs::write(dir.path().join("config.json"), "{ not json").expect("write");
    assert_eq!(stored_registry_auth("ghcr.io"), None, "malformed config");
    std::fs::write(
        dir.path().join("config.json"),
        "{\"auths\": {\"ghcr.io\": {\"auth\": \"Z2hjcgo=\"}}}",
    )
    .expect("write");
    assert_eq!(
        stored_registry_auth("ghcr.io").as_deref(),
        Some("Z2hjcgo="),
        "the entry docker wrote"
    );
    assert_eq!(
        stored_registry_auth("other.example"),
        None,
        "a registry with no entry stays anonymous"
    );
}

/// Challenge parameters are comma-separated with quoted or bare values, a
/// registry may name one scope per resource, and a key matches whole.
#[test]
fn challenge_parameters_are_read_whole_quoted_or_bare_and_repeated() {
    let challenge = "Bearer realm=\"https://auth.example/token\",service=registry,\
                     scope=\"repository:owner/app:pull\",scope=\"repository:owner/other:pull\"";
    assert_eq!(
        challenge_param(challenge, "realm").as_deref(),
        Some("https://auth.example/token")
    );
    assert_eq!(
        challenge_param(challenge, "service").as_deref(),
        Some("registry"),
        "an unquoted token value is a value"
    );
    assert_eq!(
        challenge_params(challenge, "scope"),
        vec![
            "repository:owner/app:pull".to_string(),
            "repository:owner/other:pull".to_string()
        ],
        "every scope the registry named"
    );
    assert_eq!(
        challenge_param("Bearer xrealm=\"https://elsewhere.example/\"", "realm"),
        None,
        "a longer key is a different key"
    );
    assert_eq!(
        challenge_param("Bearer realm=\"https://auth.example/a,b\"", "realm").as_deref(),
        Some("https://auth.example/a,b"),
        "a comma inside a quoted value is not a separator"
    );
}
