use std::collections::HashMap;
use std::ops::ControlFlow;

use anodizer_core::log::StageLogger;
use anodizer_core::retry::{HttpError, RetryLog, RetryPolicy, is_retriable, retry_sync};
use anyhow::{Context as _, Result};

/// Validate the format of a subreddit name against Reddit's documented rules:
/// 3–21 characters, ASCII letters / digits / underscore, no leading underscore.
/// Returning an error here avoids burning an OAuth round-trip just to discover
/// the post target is invalid.
fn validate_subreddit(name: &str) -> Result<()> {
    if name.len() < 3 || name.len() > 21 {
        anyhow::bail!(
            "reddit: subreddit '{name}' must be 3–21 characters (got {})",
            name.len()
        );
    }
    if name.starts_with('_') {
        anyhow::bail!("reddit: subreddit '{name}' cannot start with an underscore");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        anyhow::bail!(
            "reddit: subreddit '{name}' contains invalid characters \
             (only letters, digits, and underscore allowed)"
        );
    }
    Ok(())
}

/// Default Reddit OAuth2 token endpoint host.
const REDDIT_TOKEN_BASE: &str = "https://www.reddit.com";

/// Default Reddit authenticated API host.
const REDDIT_OAUTH_BASE: &str = "https://oauth.reddit.com";

/// Resolve the OAuth2 token endpoint base. Read at call time so tests can set
/// `ANODIZE_REDDIT_TOKEN_BASE` to redirect the `access_token` POST at a local
/// mock; production never sets the variable.
fn reddit_token_base() -> String {
    std::env::var("ANODIZE_REDDIT_TOKEN_BASE").unwrap_or_else(|_| REDDIT_TOKEN_BASE.to_string())
}

/// Resolve the authenticated-API base. Read at call time so tests can set
/// `ANODIZE_REDDIT_OAUTH_BASE` to redirect the `/api/submit` POST at a local
/// mock; production never sets the variable.
fn reddit_oauth_base() -> String {
    std::env::var("ANODIZE_REDDIT_OAUTH_BASE").unwrap_or_else(|_| REDDIT_OAUTH_BASE.to_string())
}

/// Bundled credentials + post payload for a Reddit submission. Grouped into a
/// single struct so `send_reddit` stays under clippy's argument-count limit
/// and the call-site reads as one record per submission.
pub struct RedditPost<'a> {
    pub application_id: &'a str,
    pub secret: &'a str,
    pub username: &'a str,
    pub password: &'a str,
    pub subreddit: &'a str,
    pub title: &'a str,
    pub url: &'a str,
}

/// Authenticate with Reddit's OAuth2 API and submit a link post to a subreddit.
///
/// 1. POST to `/api/v1/access_token` with Basic Auth (application_id:secret)
///    and `grant_type=password` to obtain a bearer token.
/// 2. POST to `/api/submit` on `oauth.reddit.com` with the bearer token to
///    create the link post.
pub fn send_reddit(post: &RedditPost<'_>, log: &StageLogger, policy: &RetryPolicy) -> Result<()> {
    let RedditPost {
        application_id,
        secret,
        username,
        password,
        subreddit,
        title,
        url,
    } = *post;
    validate_subreddit(subreddit)?;

    let client = crate::http::blocking_client()?;

    let token_body = retry_sync(
        RetryLog::new("reddit access token", log),
        policy,
        |_attempt| match client
            .post(format!("{}/api/v1/access_token", reddit_token_base()))
            .basic_auth(application_id, Some(secret))
            .form(&[
                ("grant_type", "password"),
                ("username", username),
                ("password", password),
            ])
            .send()
        {
            Err(e) => {
                let err = anyhow::Error::new(HttpError::from_response(e, None))
                    .context("reddit: OAuth token transport error");
                if is_retriable(err.as_ref()) {
                    Err(ControlFlow::Continue(err))
                } else {
                    Err(ControlFlow::Break(err))
                }
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp
                    .text()
                    .unwrap_or_else(|e| format!("<body read failed: {e}>"));
                if status.is_success() {
                    Ok(body)
                } else {
                    let inner =
                        anyhow::anyhow!("reddit: OAuth token request failed ({status}): {body}");
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
        },
    )?;

    let token_json: serde_json::Value = serde_json::from_str(&token_body)
        .context("reddit: OAuth token response was not valid JSON")?;
    let access_token = token_json["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("reddit: missing access_token in OAuth response"))?;

    // Rate-limit headers are logged from the final response on failure.
    let mut form = HashMap::new();
    form.insert("api_type", "json");
    form.insert("kind", "link");
    form.insert("sr", subreddit);
    form.insert("title", title);
    form.insert("url", url);

    let submit_body =
        retry_sync(
            RetryLog::new("reddit submit", log),
            policy,
            |_attempt| match client
                .post(format!("{}/api/submit", reddit_oauth_base()))
                .bearer_auth(access_token)
                .form(&form)
                .send()
            {
                Err(e) => {
                    let err = anyhow::Error::new(HttpError::from_response(e, None))
                        .context("reddit: submit transport error");
                    if is_retriable(err.as_ref()) {
                        Err(ControlFlow::Continue(err))
                    } else {
                        Err(ControlFlow::Break(err))
                    }
                }
                Ok(resp) => {
                    log_rate_limit(resp.headers(), log);
                    let status = resp.status();
                    let body = resp
                        .text()
                        .unwrap_or_else(|e| format!("<body read failed: {e}>"));
                    if status.is_success() {
                        Ok(body)
                    } else {
                        let inner = anyhow::anyhow!("reddit: submit failed ({status}): {body}");
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
            },
        )?;

    // Reddit returns 200 even on failure — check json.errors
    let submit_json: serde_json::Value =
        serde_json::from_str(&submit_body).context("reddit: submit response was not valid JSON")?;
    if let Some(errors) = submit_json
        .get("json")
        .and_then(|j| j.get("errors"))
        .and_then(|e| e.as_array())
        && !errors.is_empty()
    {
        anyhow::bail!("reddit: submit returned errors: {errors:?}");
    }

    Ok(())
}

/// Surface Reddit's `X-Ratelimit-*` headers so users see throttle pressure
/// before it turns into 429s on the next release.
fn log_rate_limit(headers: &reqwest::header::HeaderMap, log: &StageLogger) {
    let used = header_str(headers, "x-ratelimit-used");
    let remaining = header_str(headers, "x-ratelimit-remaining");
    let reset = header_str(headers, "x-ratelimit-reset");
    if used.is_none() && remaining.is_none() && reset.is_none() {
        return;
    }
    let remaining_num = remaining.as_deref().and_then(|s| s.parse::<f64>().ok());
    let line = format!(
        "reddit rate limit used={} remaining={} reset_in={}s",
        used.as_deref().unwrap_or("?"),
        remaining.as_deref().unwrap_or("?"),
        reset.as_deref().unwrap_or("?"),
    );
    if remaining_num.map(|n| n < 5.0).unwrap_or(false) {
        log.warn(&line);
    } else {
        log.status(&line);
    }
}

fn header_str(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::validate_subreddit;
    use super::{RedditPost, send_reddit};
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::retry::RetryPolicy;
    use anodizer_core::test_helpers::env::env_mutex;
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };

    const ONE_SHOT: RetryPolicy = RetryPolicy {
        max_attempts: 1,
        base_delay: std::time::Duration::from_millis(0),
        max_delay: std::time::Duration::from_millis(0),
    };

    /// Point BOTH reddit seams (token + oauth host) at one mock; the two POSTs
    /// distinguish themselves by path. Removes the vars on drop.
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                // env-ok: #[serial(announce_env)] + env_mutex; per-test API-base redirect
                std::env::remove_var("ANODIZE_REDDIT_TOKEN_BASE");
                // env-ok: #[serial(announce_env)] + env_mutex; per-test API-base redirect
                std::env::remove_var("ANODIZE_REDDIT_OAUTH_BASE");
            }
        }
    }
    fn set_bases(addr: std::net::SocketAddr) -> EnvGuard {
        let base = format!("http://{addr}");
        unsafe {
            // env-ok: #[serial(announce_env)] + env_mutex; per-test API-base redirect
            std::env::set_var("ANODIZE_REDDIT_TOKEN_BASE", &base);
            // env-ok: #[serial(announce_env)] + env_mutex; per-test API-base redirect
            std::env::set_var("ANODIZE_REDDIT_OAUTH_BASE", &base);
        }
        EnvGuard
    }
    fn http_response(status_line: &str, body: &str) -> &'static str {
        let resp = format!(
            "{status_line}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        Box::leak(resp.into_boxed_str())
    }
    fn post(subreddit: &str) -> RedditPost<'static> {
        // Owned-then-leaked so the borrow outlives the call without lifetime
        // gymnastics in each test.
        RedditPost {
            application_id: "appid",
            secret: "sekret",
            username: "user",
            password: "pw",
            subreddit: Box::leak(subreddit.to_string().into_boxed_str()),
            title: "MyApp v1.2.3 released",
            url: "https://example.com/releases/v1.2.3",
        }
    }

    #[test]
    #[serial_test::serial(announce_env)]
    fn send_reddit_two_step_flow_token_then_submit() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let (addr, log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/access_token",
                response: http_response("HTTP/1.1 200 OK", "{\"access_token\":\"BEARER-XYZ\"}"),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/submit",
                response: http_response("HTTP/1.1 200 OK", "{\"json\":{\"errors\":[]}}"),
                times: None,
            },
        ]);
        let _base = set_bases(addr);
        let log_s = StageLogger::new("reddit-test", Verbosity::Quiet);

        send_reddit(&post("rust"), &log_s, &ONE_SHOT).expect("two-step flow should succeed");

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2, "token POST then submit POST");

        // Leg 1: token request — Basic auth + grant_type=password form.
        let token = &entries[0];
        assert_eq!(token.method, "POST");
        assert_eq!(token.path, "/api/v1/access_token");
        // basic_auth("appid", "sekret") == base64("appid:sekret").
        let expect_basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("appid:sekret")
        );
        assert_eq!(token.header("authorization"), Some(expect_basic.as_str()));
        assert!(token.body.contains("grant_type=password"), "{}", token.body);
        assert!(token.body.contains("username=user"), "{}", token.body);

        // Leg 2: submit — Bearer of the token from leg 1 + link payload.
        let submit = &entries[1];
        assert_eq!(submit.path, "/api/submit");
        assert_eq!(submit.header("authorization"), Some("Bearer BEARER-XYZ"));
        assert!(submit.body.contains("kind=link"), "{}", submit.body);
        assert!(submit.body.contains("sr=rust"), "{}", submit.body);
        // The release title + URL are templated into the wire payload
        // (percent-encoded as form fields).
        assert!(
            submit.body.contains("title=MyApp+v1.2.3+released"),
            "title must be in body: {}",
            submit.body
        );
        assert!(
            submit.body.contains("example.com"),
            "url must be in body: {}",
            submit.body
        );
    }

    #[test]
    #[serial_test::serial(announce_env)]
    fn send_reddit_token_fetch_failure_aborts_before_submit() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // A 401 on the token leg must surface and NOT fire the submit POST.
        let (addr, log) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "POST",
            path_pattern: "/api/v1/access_token",
            response: http_response("HTTP/1.1 401 Unauthorized", "{\"error\":\"invalid_grant\"}"),
            times: None,
        }]);
        let _base = set_bases(addr);
        let log_s = StageLogger::new("reddit-test", Verbosity::Quiet);

        let err = format!(
            "{:#}",
            send_reddit(&post("rust"), &log_s, &ONE_SHOT).unwrap_err()
        );
        assert!(err.contains("401"), "status must surface: {err}");
        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 1, "submit must not fire after token 401");
    }

    #[test]
    #[serial_test::serial(announce_env)]
    fn send_reddit_submit_json_errors_surface() {
        let _g = env_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // Reddit returns HTTP 200 even on a logical submit rejection; a
        // non-empty `json.errors` array must be surfaced as an error.
        let (addr, _log) = spawn_scripted_responder(vec![
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/v1/access_token",
                response: http_response("HTTP/1.1 200 OK", "{\"access_token\":\"T\"}"),
                times: None,
            },
            ScriptedRoute {
                method: "POST",
                path_pattern: "/api/submit",
                response: http_response(
                    "HTTP/1.1 200 OK",
                    "{\"json\":{\"errors\":[[\"SUBREDDIT_NOEXIST\",\"that subreddit doesn't exist\"]]}}",
                ),
                times: None,
            },
        ]);
        let _base = set_bases(addr);
        let log_s = StageLogger::new("reddit-test", Verbosity::Quiet);

        let err = format!(
            "{:#}",
            send_reddit(&post("rust"), &log_s, &ONE_SHOT).unwrap_err()
        );
        assert!(
            err.contains("SUBREDDIT_NOEXIST"),
            "submit errors must surface: {err}"
        );
    }

    use base64::Engine as _;

    #[test]
    fn accepts_valid_names() {
        validate_subreddit("rust").unwrap();
        validate_subreddit("rust_lang").unwrap();
        validate_subreddit("AnodizerRel123").unwrap();
    }

    #[test]
    fn rejects_too_short() {
        let err = validate_subreddit("ab").unwrap_err().to_string();
        assert!(err.contains("3–21"), "{err}");
    }

    #[test]
    fn rejects_too_long() {
        let err = validate_subreddit(&"a".repeat(22)).unwrap_err().to_string();
        assert!(err.contains("3–21"), "{err}");
    }

    #[test]
    fn rejects_leading_underscore() {
        let err = validate_subreddit("_oops").unwrap_err().to_string();
        assert!(err.contains("underscore"), "{err}");
    }

    #[test]
    fn rejects_invalid_characters() {
        let err = validate_subreddit("has-hyphen").unwrap_err().to_string();
        assert!(err.contains("invalid characters"), "{err}");
        let err = validate_subreddit("has space").unwrap_err().to_string();
        assert!(err.contains("invalid characters"), "{err}");
    }

    #[test]
    fn accepts_min_length_three() {
        validate_subreddit("aaa").unwrap();
    }

    #[test]
    fn accepts_max_length_twenty_one() {
        validate_subreddit(&"a".repeat(21)).unwrap();
    }

    #[test]
    fn accepts_digits_only() {
        validate_subreddit("12345").unwrap();
    }

    #[test]
    fn rejects_unicode_chars() {
        // Unicode letters are not ASCII alphanumerics and must be rejected
        // before burning an OAuth round-trip.
        let err = validate_subreddit("café_x").unwrap_err().to_string();
        assert!(err.contains("invalid characters"), "{err}");
    }

    #[test]
    fn rejects_dot_or_slash() {
        // Reddit subreddit names disallow path-like chars; rejecting these
        // prevents an OAuth round-trip from being wasted on an invalid post.
        assert!(validate_subreddit("foo.bar").is_err());
        assert!(validate_subreddit("foo/bar").is_err());
    }

    /// `header_str` returns `None` for absent headers and `Some(value)` for
    /// present ones; used by `log_rate_limit` to short-circuit when no
    /// rate-limit headers exist.
    #[test]
    fn header_str_returns_value_when_present() {
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-used", HeaderValue::from_static("42"));
        assert_eq!(super::header_str(&h, "x-ratelimit-used"), Some("42".into()));
        assert_eq!(super::header_str(&h, "x-ratelimit-remaining"), None);
    }

    /// `log_rate_limit` must NOT panic when only some rate-limit
    /// headers are present. Reddit emits the trio together in
    /// production, but future API changes or partial responses must
    /// remain non-fatal.
    #[test]
    fn log_rate_limit_handles_partial_headers() {
        use anodizer_core::log::{StageLogger, Verbosity};
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining", HeaderValue::from_static("3"));
        let log = StageLogger::new("reddit-test", Verbosity::Quiet);
        super::log_rate_limit(&h, &log);
    }

    #[test]
    fn log_rate_limit_noop_when_no_headers_present() {
        use anodizer_core::log::{StageLogger, Verbosity};
        use reqwest::header::HeaderMap;
        let h = HeaderMap::new();
        let log = StageLogger::new("reddit-test", Verbosity::Quiet);
        super::log_rate_limit(&h, &log);
    }
}
