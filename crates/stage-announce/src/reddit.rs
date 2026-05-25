use std::collections::HashMap;
use std::ops::ControlFlow;

use anodizer_core::log::StageLogger;
use anodizer_core::retry::{HttpError, RetryPolicy, is_retriable, retry_sync};
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

    let client = reqwest::blocking::Client::builder()
        .user_agent(anodizer_core::http::USER_AGENT)
        .build()
        .context("reddit: build HTTP client")?;

    let token_body = retry_sync(policy, |_attempt| {
        match client
            .post("https://www.reddit.com/api/v1/access_token")
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
        }
    })?;

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

    let submit_body = retry_sync(policy, |_attempt| {
        match client
            .post("https://oauth.reddit.com/api/submit")
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
        }
    })?;

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
        "reddit rate limit: used={} remaining={} reset_in={}s",
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
