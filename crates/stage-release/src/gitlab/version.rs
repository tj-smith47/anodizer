use super::*;

/// The version probe's own retry budget. The release policy defaults to ten
/// attempts under a five-minute cap, which would leave a best-effort probe
/// looking wedged for most of half an hour; a transient failure is still
/// retried, since the answer picks the link field the release is created with.
pub(crate) const VERSION_PROBE_RETRY: RetryPolicy = RetryPolicy {
    max_attempts: 3,
    base_delay: std::time::Duration::from_millis(500),
    max_delay: std::time::Duration::from_secs(2),
};

/// Detect whether the GitLab server is pre-v17.
///
/// Strategy:
/// 1. Check `CI_SERVER_VERSION` environment variable (set in GitLab CI runners)
/// 2. Fall back to `GET /api/v4/version` API call, retried on
///    [`VERSION_PROBE_RETRY`] within the run's `deadline`
/// 3. If both fail, default to pre-v17 behavior (`filepath`) — conservative
///    approach: treat the API as pre-v17 on failure.
pub(crate) async fn detect_pre_v17_gitlab(
    client: &Client,
    api_url: &str,
    deadline: Option<std::time::Instant>,
    log: &anodizer_core::log::StageLogger,
) -> bool {
    detect_pre_v17_gitlab_with_env(client, api_url, &ProcessEnvSource, deadline, log).await
}

/// Env-injectable form of [`detect_pre_v17_gitlab`]. Production wires up
/// [`ProcessEnvSource`]; tests inject a
/// [`anodizer_core::MapEnvSource`] to pin the `CI_SERVER_VERSION` short
/// circuit without mutating the process env.
pub(crate) async fn detect_pre_v17_gitlab_with_env<E: EnvSource + ?Sized>(
    client: &Client,
    api_url: &str,
    env: &E,
    deadline: Option<std::time::Instant>,
    log: &anodizer_core::log::StageLogger,
) -> bool {
    // 1. Check environment variable first.
    if let Some(version_str) = env.var("CI_SERVER_VERSION") {
        return is_pre_v17(&version_str);
    }

    // 2. Fall back to API call.
    let api = api_url.trim_end_matches('/');
    let version_url = format!("{}/version", api);
    let probe = retry_http_async_deadline(
        RetryLog::new("gitlab: GET version", log),
        &VERSION_PROBE_RETRY,
        deadline,
        SuccessClass::Strict,
        |_| client.get(&version_url).send(),
        |status, body| {
            format!(
                "gitlab: version probe failed (HTTP {status}): {}",
                redact_bearer_tokens(body)
            )
        },
    )
    .await;
    match probe {
        Ok(resp) => {
            if let Ok(body) = resp.json::<serde_json::Value>().await
                && let Some(version_str) = body["version"].as_str()
            {
                return is_pre_v17(version_str);
            }
            // Could not parse version — default to pre-v17 (conservative).
            true
        }
        // API call failed — default to pre-v17 (conservative).
        Err(_) => true,
    }
}

/// Parse a GitLab version string and return true if the major version is < 17.
pub(crate) fn is_pre_v17(version_str: &str) -> bool {
    // CI_SERVER_VERSION is like "16.11.0" or "17.0.0"
    if let Some(major_str) = version_str.split('.').next()
        && let Ok(major) = major_str.parse::<u32>()
    {
        return major < 17;
    }
    false
}
