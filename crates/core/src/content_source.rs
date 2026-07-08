//! Resolve a [`ContentSource`] to its string content.
//!
//! Hoisted to core so multiple stages (release, changelog, ...) can share one
//! implementation. Supports `Inline`, `FromFile` (template-render the path,
//! read the file), and `FromUrl` (template-render URL + headers, fetch via
//! HTTP GET with retries on transient errors / 5xx, fail fast on 4xx).
//!
//! `FromUrl` enforces a 256 KiB body cap and rejects CR/LF in rendered header
//! values to defend against header-injection via templated user data.

use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::config::ContentSource;
use crate::context::Context;
use crate::log::StageLogger;
use crate::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking};

const MAX_BODY_BYTES: usize = 256 * 1024;
/// Total per-request deadline. `reqwest::blocking::ClientBuilder` does
/// not expose a separate `read_timeout` (the API is async-only); the
/// total `timeout` bounds connect + transfer for the blocking surface,
/// so a stalled server cannot hold the connection open past 30 s.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Connect-only deadline. Allows the connect phase to fail fast on a
/// dead host without consuming the full request budget; the remaining
/// time is then available for the actual transfer.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLICY: RetryPolicy = RetryPolicy {
    max_attempts: 3,
    base_delay: Duration::from_millis(500),
    max_delay: Duration::from_secs(2),
};

/// Resolve a [`ContentSource`] to its string content.
///
/// `kind` is a short label (e.g. `"release header"`, `"changelog footer"`)
/// surfaced in error messages so misconfigured fields are easy to identify.
pub fn resolve(
    source: &ContentSource,
    kind: &str,
    ctx: &Context,
    log: &StageLogger,
) -> Result<String> {
    match source {
        ContentSource::Inline(s) => Ok(s.clone()),
        ContentSource::FromFile { from_file } => {
            let rendered_path = ctx
                .render_template(from_file)
                .with_context(|| format!("{kind}: render from_file path '{from_file}'"))?;
            std::fs::read_to_string(&rendered_path)
                .with_context(|| format!("{kind}: read from_file '{rendered_path}'"))
        }
        ContentSource::FromUrl { from_url, headers } => {
            let rendered_url = ctx
                .render_template(from_url)
                .with_context(|| format!("{kind}: render from_url '{from_url}'"))?;

            // Render header values (keys are literal).
            // Reject CR/LF anywhere in keys or rendered values — a template
            // interpolating user-tainted data could otherwise inject a new
            // header line.
            let mut rendered_headers: Vec<(String, String)> = Vec::new();
            if let Some(map) = headers {
                for (k, v) in map {
                    if k.contains('\r') || k.contains('\n') {
                        anyhow::bail!(
                            "{kind} from_url header key contains CR/LF (possible injection): {:?}",
                            k
                        );
                    }
                    let rendered_v = ctx.render_template(v).with_context(|| {
                        format!("{kind}: render header value for '{k}' at URL {rendered_url}")
                    })?;
                    if rendered_v.contains('\r') || rendered_v.contains('\n') {
                        anyhow::bail!(
                            "{kind} from_url header '{}' rendered to a value containing \
                             CR/LF (possible injection): {:?}",
                            k,
                            rendered_v
                        );
                    }
                    rendered_headers.push((k.clone(), rendered_v));
                }
            }

            let client = reqwest::blocking::Client::builder()
                .user_agent(crate::http::USER_AGENT)
                .timeout(HTTP_TIMEOUT)
                .connect_timeout(HTTP_CONNECT_TIMEOUT)
                .build()
                .context("build blocking HTTP client for ContentSource::FromUrl")?;

            // `retry_http_blocking` handles 5xx → retry, 4xx → fast-fail, and
            // transport errors via the shared `is_retriable` classifier.
            // Body-cap and label-formatting are applied on the returned
            // body string.
            let label = format!("{kind} from_url {rendered_url}");
            let rendered_url_for_err = rendered_url.clone();
            let (_status, body) = retry_http_blocking(
                RetryLog::new(&label, log),
                &POLICY,
                SuccessClass::Strict,
                |_attempt| {
                    let mut req = client.get(&rendered_url);
                    for (k, v) in &rendered_headers {
                        req = req.header(k.as_str(), v.as_str());
                    }
                    req.send()
                },
                |status, _body| format!("returned HTTP {status}"),
            )?;

            if body.len() > MAX_BODY_BYTES {
                anyhow::bail!(
                    "{kind} from_url {} body is {} bytes, exceeds {} KiB limit",
                    rendered_url_for_err,
                    body.len(),
                    MAX_BODY_BYTES / 1024,
                );
            }
            Ok(body)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::context::{Context, ContextOptions};
    use crate::test_helpers::responder::{
        spawn_oneshot_http_responder, spawn_request_capturing_responder,
    };
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;

    fn ctx() -> Context {
        let config = Config {
            project_name: "myapp".to_string(),
            ..Config::default()
        };
        Context::new(config, ContextOptions::default())
    }

    fn tlog() -> &'static StageLogger {
        static L: std::sync::OnceLock<StageLogger> = std::sync::OnceLock::new();
        L.get_or_init(|| StageLogger::new("test", crate::log::Verbosity::Quiet))
    }

    // ---- Inline ----

    #[test]
    fn inline_returns_string_verbatim() {
        let src = ContentSource::Inline("hello world".to_string());
        assert_eq!(resolve(&src, "k", &ctx(), tlog()).unwrap(), "hello world");
    }

    // ---- FromFile ----

    #[test]
    fn from_file_renders_path_template_and_reads_contents() {
        let dir = tempfile::tempdir().unwrap();
        let body = "release header from disk\n";
        // Render a path template that interpolates a template var so the
        // rendered path differs from the raw template string — proves the
        // template engine actually ran on the path.
        let file_path = dir.path().join("myapp-notes.md");
        std::fs::write(&file_path, body).unwrap();
        let template = format!("{}/{{{{ .ProjectName }}}}-notes.md", dir.path().display());
        let src = ContentSource::FromFile {
            from_file: template,
        };
        assert_eq!(
            resolve(&src, "release header", &ctx(), tlog()).unwrap(),
            body
        );
    }

    #[test]
    fn from_file_bails_when_path_template_invalid() {
        // Unknown filter is a recognized template parse error (see
        // `template/tests.rs::test_unknown_filter_error`). Routes through
        // the `render from_file path` `with_context` arm.
        let src = ContentSource::FromFile {
            from_file: "{{ ProjectName | nonexistent_filter }}".to_string(),
        };
        let err = resolve(&src, "release header", &ctx(), tlog()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("render from_file path"),
            "context missing: {chain}"
        );
    }

    #[test]
    fn from_file_bails_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.md");
        let src = ContentSource::FromFile {
            from_file: missing.display().to_string(),
        };
        let err = resolve(&src, "release header", &ctx(), tlog()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("read from_file"), "context missing: {chain}");
    }

    // ---- FromUrl ----

    #[test]
    fn from_url_success_returns_body() {
        let body = "remote header body";
        let body_len = body.len();
        let response: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
        );
        let (addr, calls) = spawn_oneshot_http_responder(vec![response]);
        let src = ContentSource::FromUrl {
            from_url: format!("http://{addr}/header.md"),
            headers: None,
        };
        let got = resolve(&src, "release header", &ctx(), tlog()).unwrap();
        assert_eq!(got, body);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "single attempt on 200");
    }

    #[test]
    fn from_url_renders_header_values_and_sends_them_verbatim() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        let (addr, captured) = spawn_request_capturing_responder(response);

        let mut headers = HashMap::new();
        // Key is literal; value is template-rendered.
        headers.insert(
            "X-App-Name".to_string(),
            "name={{ .ProjectName }}".to_string(),
        );
        let src = ContentSource::FromUrl {
            from_url: format!("http://{addr}/h.md"),
            headers: Some(headers),
        };
        let body = resolve(&src, "release header", &ctx(), tlog()).unwrap();
        assert_eq!(body, "ok");

        // Poll the capture briefly — the responder thread writes the
        // captured string asynchronously.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let captured_str = loop {
            let s = captured.lock().unwrap().clone();
            if !s.is_empty() || std::time::Instant::now() >= deadline {
                break s;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let lower = captured_str.to_ascii_lowercase();
        assert!(
            lower.contains("x-app-name: name=myapp"),
            "header missing or unrendered in request: {captured_str:?}"
        );
    }

    #[test]
    fn from_url_rejects_crlf_in_header_key() {
        let mut headers = HashMap::new();
        headers.insert("X-Bad\r\nInjected".to_string(), "v".to_string());
        let src = ContentSource::FromUrl {
            // Address doesn't matter; the key check fires before any
            // network IO.
            from_url: "http://127.0.0.1:1/".to_string(),
            headers: Some(headers),
        };
        let err = resolve(&src, "release header", &ctx(), tlog()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("header key contains CR/LF"),
            "expected CR/LF key guard, got: {chain}"
        );
    }

    #[test]
    fn from_url_rejects_crlf_in_rendered_header_value() {
        // The header value template renders a literal CR/LF sequence —
        // simulating an attacker-controlled template var that injected
        // a header-line terminator. `Env.X` falls back to literal-empty
        // when unset, so embed the CR/LF directly to make the test
        // deterministic.
        let mut headers = HashMap::new();
        headers.insert("X-Hdr".to_string(), "ok\r\nX-Injected: yes".to_string());
        let src = ContentSource::FromUrl {
            from_url: "http://127.0.0.1:1/".to_string(),
            headers: Some(headers),
        };
        let err = resolve(&src, "release header", &ctx(), tlog()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("rendered to a value containing CR/LF"),
            "expected CR/LF value guard, got: {chain}"
        );
    }

    #[test]
    fn from_url_bails_when_body_exceeds_cap() {
        // 256 KiB + 1 byte body — the cap is a strict `>` check.
        let oversize = "x".repeat(MAX_BODY_BYTES + 1);
        let body_len = oversize.len();
        let response: &'static str = Box::leak(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{oversize}")
                .into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![response]);
        let src = ContentSource::FromUrl {
            from_url: format!("http://{addr}/big.md"),
            headers: None,
        };
        let err = resolve(&src, "release header", &ctx(), tlog()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("exceeds 256 KiB limit"),
            "expected body-cap error, got: {chain}"
        );
    }

    #[test]
    fn from_url_4xx_fast_fails_no_retry() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);
        let src = ContentSource::FromUrl {
            from_url: format!("http://{addr}/missing.md"),
            headers: None,
        };
        let err = resolve(&src, "release header", &ctx(), tlog()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("404"), "status missing from chain: {chain}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "4xx must not retry (only one attempt observed)"
        );
    }

    #[test]
    fn from_url_5xx_exhausts_retries_then_fails() {
        // Drive exactly POLICY.max_attempts canned 500s so the responder
        // counter pins to the configured retry budget. Wiring through the
        // const means a future bump of POLICY.max_attempts updates the
        // test atomically without a stale literal silently passing.
        let max_attempts = POLICY.max_attempts as usize;
        let responses: Vec<&'static str> = std::iter::repeat_n(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
            max_attempts,
        )
        .collect();
        let (addr, calls) = spawn_oneshot_http_responder(responses);
        let src = ContentSource::FromUrl {
            from_url: format!("http://{addr}/flaky.md"),
            headers: None,
        };
        let err = resolve(&src, "release header", &ctx(), tlog()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("500"), "status missing from chain: {chain}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            max_attempts as u32,
            "all POLICY.max_attempts retries must run before bailing"
        );
    }
}
