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
use crate::retry::{RetryPolicy, SuccessClass, retry_http_blocking};

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
pub fn resolve(source: &ContentSource, kind: &str, ctx: &Context) -> Result<String> {
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

            // Render header values (keys are literal per GoReleaser docs).
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
                &label,
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
