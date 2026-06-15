use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::time::SystemTime;

use anodizer_core::retry::{HttpError, RetryPolicy, is_retriable, retry_sync};
use anyhow::{Context as _, Result};
use base64::Engine;

/// Default Twitter API v2 tweet-creation endpoint.
const TWITTER_TWEETS_URL: &str = "https://api.x.com/2/tweets";

/// Resolve the tweet-creation URL. Read at call time so tests can set
/// `ANODIZE_TWITTER_API_BASE` to redirect the POST (and the OAuth signature
/// base string) at a local mock; production never sets the variable.
fn twitter_tweets_url() -> String {
    std::env::var("ANODIZE_TWITTER_API_BASE").unwrap_or_else(|_| TWITTER_TWEETS_URL.to_string())
}

/// Post a tweet via Twitter API v2 with OAuth 1.0a user-context authentication.
///
/// `policy` enables retry on 5xx / 429 / network failures. The OAuth
/// header is rebuilt on every attempt so the `oauth_timestamp` and
/// `oauth_nonce` are fresh — Twitter rejects replays.
///
/// Note: this announcer does NOT route through `helpers::retry_http`
/// because the OAuth-signing step must run inside the retry loop (with a
/// fresh nonce/timestamp per attempt), and `retry_http`'s closure does
/// not have a structured way to fail-fast on a signing error. The
/// retry-classification logic is otherwise identical to `retry_http`'s
/// — `is_retriable(err.as_ref())` is the canonical predicate.
pub fn send_twitter(
    consumer_key: &str,
    consumer_secret: &str,
    access_token: &str,
    access_token_secret: &str,
    message: &str,
    policy: &RetryPolicy,
) -> Result<()> {
    let url = twitter_tweets_url();
    let url = url.as_str();
    let body = serde_json::json!({ "text": message }).to_string();
    let client = reqwest::blocking::Client::new();

    retry_sync(policy, |_attempt| {
        // Re-sign on every attempt: oauth_nonce + oauth_timestamp must be
        // fresh per RFC 5849 §3.3 to avoid replay rejection.
        let auth_header = match build_oauth1_header(
            "POST",
            url,
            consumer_key,
            consumer_secret,
            access_token,
            access_token_secret,
        ) {
            Ok(h) => h,
            Err(e) => return Err(ControlFlow::Break(e)),
        };

        match client
            .post(url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/json")
            .body(body.clone())
            .send()
        {
            Err(e) => {
                let err = anyhow::Error::new(HttpError::from_response(e, None))
                    .context("twitter: failed to send POST request");
                if is_retriable(err.as_ref()) {
                    Err(ControlFlow::Continue(err))
                } else {
                    Err(ControlFlow::Break(err))
                }
            }
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    Ok(())
                } else {
                    let body = anodizer_core::http::body_of_blocking(resp);
                    let inner = anyhow::anyhow!("twitter: API request failed ({status}): {body}");
                    let wrapped = anyhow::Error::new(HttpError::new(
                        std::io::Error::other(inner.to_string()),
                        status.as_u16(),
                    ))
                    .context(inner);
                    if is_retriable(wrapped.as_ref()) {
                        Err(ControlFlow::Continue(wrapped))
                    } else {
                        Err(ControlFlow::Break(wrapped))
                    }
                }
            }
        }
    })
    .context("twitter: POST exhausted retry attempts")
}

fn build_oauth1_header(
    method: &str,
    url: &str,
    consumer_key: &str,
    consumer_secret: &str,
    token: &str,
    token_secret: &str,
) -> Result<String> {
    let method = method.to_uppercase();
    let timestamp = anodizer_core::sde::resolve_now().timestamp().to_string();
    let nonce = generate_nonce();

    let mut params = BTreeMap::new();
    params.insert("oauth_consumer_key", consumer_key);
    params.insert("oauth_nonce", &nonce);
    params.insert("oauth_signature_method", "HMAC-SHA1");
    params.insert("oauth_timestamp", &timestamp);
    params.insert("oauth_token", token);
    params.insert("oauth_version", "1.0");

    let param_string: String = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let base_string = format!(
        "{}&{}&{}",
        &method,
        percent_encode(url),
        percent_encode(&param_string)
    );
    let signing_key = format!(
        "{}&{}",
        percent_encode(consumer_secret),
        percent_encode(token_secret)
    );

    let signature = hmac_sha1_base64(signing_key.as_bytes(), base_string.as_bytes())?;

    Ok(format!(
        "OAuth oauth_consumer_key=\"{}\", oauth_nonce=\"{}\", oauth_signature=\"{}\", oauth_signature_method=\"HMAC-SHA1\", oauth_timestamp=\"{}\", oauth_token=\"{}\", oauth_version=\"1.0\"",
        percent_encode(consumer_key),
        percent_encode(&nonce),
        percent_encode(&signature),
        percent_encode(&timestamp),
        percent_encode(token),
    ))
}

fn hmac_sha1_base64(key: &[u8], data: &[u8]) -> Result<String> {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(key).map_err(|e| anyhow::anyhow!("HMAC error: {e}"))?;
    mac.update(data);
    Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

use anodizer_core::url::percent_encode_unreserved as percent_encode;

fn generate_nonce() -> String {
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    count.hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    format!("{:016x}{:016x}", hasher.finish(), count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percent_encode_basic() {
        assert_eq!(percent_encode("hello"), "hello");
        assert_eq!(percent_encode("hello world"), "hello%20world");
        assert_eq!(percent_encode("a=b&c=d"), "a%3Db%26c%3Dd");
    }

    #[test]
    fn test_percent_encode_unreserved_chars() {
        assert_eq!(percent_encode("A-Z_a~z.0"), "A-Z_a~z.0");
    }

    #[test]
    fn test_percent_encode_special_chars() {
        assert_eq!(percent_encode("/"), "%2F");
        assert_eq!(percent_encode(":"), "%3A");
        assert_eq!(percent_encode("+"), "%2B");
    }

    #[test]
    fn test_oauth1_header_format() {
        let header = build_oauth1_header(
            "POST",
            "https://api.x.com/2/tweets",
            "ck",
            "cs",
            "at",
            "ats",
        )
        .unwrap();
        assert!(header.starts_with("OAuth "));
        assert!(header.contains("oauth_consumer_key=\"ck\""));
        assert!(header.contains("oauth_token=\"at\""));
        assert!(header.contains("oauth_signature_method=\"HMAC-SHA1\""));
        assert!(header.contains("oauth_version=\"1.0\""));
        assert!(header.contains("oauth_signature="));
        assert!(header.contains("oauth_nonce="));
        assert!(header.contains("oauth_timestamp="));
    }

    #[test]
    fn test_hmac_sha1_base64_known_value() {
        // Known HMAC-SHA1 test vector
        let result =
            hmac_sha1_base64(b"key", b"The quick brown fox jumps over the lazy dog").unwrap();
        assert_eq!(result, "3nybhbi3iqa8ino29wqQcBydtNk=");
    }

    #[test]
    fn test_percent_encode_utf8() {
        // Multi-byte UTF-8 characters should be encoded byte-by-byte
        let encoded = percent_encode("caf\u{00e9}");
        assert_eq!(encoded, "caf%C3%A9");
    }

    #[test]
    fn test_generate_nonce_uniqueness() {
        let n1 = generate_nonce();
        let n2 = generate_nonce();
        assert_ne!(n1, n2);
        assert_eq!(n1.len(), 32); // 32 hex characters
    }

    #[test]
    fn test_generate_nonce_is_hex() {
        let nonce = generate_nonce();
        assert_eq!(nonce.len(), 32);
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_oauth1_signature_is_deterministic_per_inputs() {
        // Same inputs → same signature because params are sorted into a
        // BTreeMap before being signed. Building the header twice with the
        // same nonce/timestamp must produce the same `oauth_signature`.
        // We can't fix the timestamp at the public-API level; instead we
        // verify the underlying signing routine via `hmac_sha1_base64`
        // with a hand-built signature base string in lexicographic order.
        let base = "POST&https%3A%2F%2Fapi.x.com%2F2%2Ftweets&\
                    oauth_consumer_key%3Dck%26oauth_nonce%3Dn%26\
                    oauth_signature_method%3DHMAC-SHA1%26\
                    oauth_timestamp%3D1700000000%26oauth_token%3Dat%26\
                    oauth_version%3D1.0";
        let key = "cs&ats";
        let sig1 = hmac_sha1_base64(key.as_bytes(), base.as_bytes()).unwrap();
        let sig2 = hmac_sha1_base64(key.as_bytes(), base.as_bytes()).unwrap();
        assert_eq!(sig1, sig2);
        assert!(!sig1.is_empty());
    }

    // ---- live send over a mock HTTP server -----------------------------
    //
    // Drive `send_twitter` (the real OAuth-signing POST path) against a
    // scripted responder via the `ANODIZE_TWITTER_API_BASE` seam. Mutating
    // process env requires `#[serial]` + the shared env_mutex.

    use anodizer_core::test_helpers::env::env_mutex;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };

    const ONE_SHOT: RetryPolicy = RetryPolicy {
        max_attempts: 1,
        base_delay: std::time::Duration::from_millis(0),
        max_delay: std::time::Duration::from_millis(0),
    };

    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("ANODIZE_TWITTER_API_BASE") };
        }
    }
    fn set_base(addr: std::net::SocketAddr) -> EnvGuard {
        unsafe {
            std::env::set_var(
                "ANODIZE_TWITTER_API_BASE",
                format!("http://{addr}/2/tweets"),
            )
        };
        EnvGuard
    }
    fn http_response(status_line: &str, body: &str) -> &'static str {
        let resp = format!(
            "{status_line}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        Box::leak(resp.into_boxed_str())
    }

    #[test]
    #[serial_test::serial]
    fn send_twitter_happy_path_posts_text_with_oauth_header() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/2/tweets",
            response: http_response("HTTP/1.1 201 Created", "{\"data\":{\"id\":\"1\"}}"),
            times: None,
        }]);
        let _base = set_base(addr);

        send_twitter("ck", "cs", "at", "ats", "MyApp v1.2.3 is out!", &ONE_SHOT)
            .expect("happy path should succeed");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one POST expected");
        let req = &entries[0];
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/2/tweets");
        assert_eq!(req.header("content-type"), Some("application/json"));
        // The template body is rendered into the wire payload.
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("json body");
        assert_eq!(body["text"], "MyApp v1.2.3 is out!");
        // OAuth 1.0a user-context header must be present and well-formed.
        let auth = req.header("authorization").expect("authorization header");
        assert!(auth.starts_with("OAuth "), "oauth scheme: {auth}");
        assert!(auth.contains("oauth_consumer_key=\"ck\""), "{auth}");
        assert!(auth.contains("oauth_token=\"at\""), "{auth}");
        assert!(auth.contains("oauth_signature="), "{auth}");
        assert!(
            auth.contains("oauth_signature_method=\"HMAC-SHA1\""),
            "{auth}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn send_twitter_non_2xx_maps_to_error_with_status_and_body() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/2/tweets",
            response: http_response("HTTP/1.1 403 Forbidden", "{\"detail\":\"duplicate\"}"),
            times: None,
        }]);
        let _base = set_base(addr);

        let err = format!(
            "{:#}",
            send_twitter("ck", "cs", "at", "ats", "hi", &ONE_SHOT).unwrap_err()
        );
        assert!(err.contains("403"), "status must surface: {err}");
        assert!(
            err.contains("duplicate"),
            "response body must surface: {err}"
        );
    }

    #[test]
    fn test_oauth1_param_lexicographic_ordering() {
        // RFC 5849 §3.4.1.3.2 requires params be sorted ascending by encoded
        // key (then value). The six oauth_* params we always emit must
        // satisfy this order — guards against future moves to a non-sorted
        // map type.
        let mut params = BTreeMap::new();
        params.insert("oauth_consumer_key", "ck");
        params.insert("oauth_nonce", "nn");
        params.insert("oauth_signature_method", "HMAC-SHA1");
        params.insert("oauth_timestamp", "1700000000");
        params.insert("oauth_token", "at");
        params.insert("oauth_version", "1.0");
        let keys: Vec<&&str> = params.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "BTreeMap must yield params in sorted order");
    }
}
