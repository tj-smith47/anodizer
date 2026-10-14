//! The short and full Docker Hub repository descriptions an entry sets.
use anodizer_core::config::DockerHubFullDescription;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::redact::redact_bearer_tokens;
use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking_deadline};
use anyhow::{Context as _, Result, bail};

/// Resolve the full description content from either a local file or a URL.
/// `from_file` takes precedence over `from_url` when both are set.
///
/// The `from_url` branch routes through [`retry_http_blocking_deadline`] so
/// transient 5xx / 429 / network failures retry per the user's top-level
/// `retry:` policy and stop once the invocation's wall-clock budget is spent;
/// 4xx fails at once so a typo'd URL is reported immediately.
///
/// Both `from_file.path` and `from_url.url` are template-rendered through
/// `ctx` before use so configs like
/// `from_url: { url: "https://raw.githubusercontent.com/{{ .Env.OWNER }}/.../README.md" }`
/// work as documented.
pub fn resolve_full_description(
    desc: &DockerHubFullDescription,
    ctx: &Context,
    client: &reqwest::blocking::Client,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<String> {
    if let Some(ref from_file) = desc.from_file {
        let path = ctx.render_template(&from_file.path).with_context(|| {
            format!(
                "dockerhub: render full_description from_file path '{}'",
                from_file.path
            )
        })?;
        return std::fs::read_to_string(&path)
            .with_context(|| format!("dockerhub: failed to read file '{}'", path));
    }

    if let Some(ref from_url) = desc.from_url {
        let url = ctx.render_template(&from_url.url).with_context(|| {
            format!(
                "dockerhub: render full_description from_url '{}'",
                from_url.url
            )
        })?;
        let headers = from_url
            .headers
            .iter()
            .flatten()
            .map(|(k, v)| {
                let value = crate::util::render_secret(
                    ctx,
                    &format!("dockerhub.full_description.from_url.headers[{k}]"),
                    v,
                )?;
                Ok((k.clone(), value))
            })
            .collect::<Result<Vec<(String, String)>>>()?;
        let label = format!("dockerhub: fetch full_description from {}", url);
        let (_, body) = retry_http_blocking_deadline(
            RetryLog::new(&label, log),
            policy,
            ctx.retry_deadline(),
            SuccessClass::Strict,
            |_| {
                let mut req = client.get(&url);
                for (key, value) in &headers {
                    req = req.header(key, value);
                }
                req.send()
            },
            |status, body| {
                format!(
                    "dockerhub: GET {} returned HTTP {}: {}",
                    url,
                    status,
                    redact_bearer_tokens(body)
                )
            },
        )?;
        return Ok(body);
    }

    bail!("dockerhub: full_description has neither from_file nor from_url set")
}

/// Resolve the short description for a DockerHub entry. Returns
/// `entry.description` when set non-empty; otherwise falls back to the
/// top-level `metadata.description` via
/// [`Config::meta_description`](anodizer_core::config::Config::meta_description).
/// Returns `None` when both sources are unset/empty so the caller can
/// short-circuit the PATCH and avoid clobbering an existing remote
/// description with the empty string.
pub(super) fn effective_description(
    entry: &anodizer_core::config::DockerHubConfig,
    ctx: &Context,
) -> Option<String> {
    entry
        .description
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            ctx.config
                .meta_description_project()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}
