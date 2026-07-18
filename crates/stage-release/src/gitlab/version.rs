use super::*;

/// Detect whether the GitLab server is pre-v17.
///
/// Strategy:
/// 1. Check `CI_SERVER_VERSION` environment variable (set in GitLab CI runners)
/// 2. Fall back to `GET /api/v4/version` API call
/// 3. If both fail, default to pre-v17 behavior (`filepath`) — conservative
///    approach: treat the API as pre-v17 on failure.
pub(crate) async fn detect_pre_v17_gitlab(client: &Client, api_url: &str) -> bool {
    detect_pre_v17_gitlab_with_env(client, api_url, &ProcessEnvSource).await
}

/// Env-injectable form of [`detect_pre_v17_gitlab`]. Production wires up
/// [`ProcessEnvSource`]; tests inject a
/// [`anodizer_core::MapEnvSource`] to pin the `CI_SERVER_VERSION` short
/// circuit without mutating the process env.
pub(crate) async fn detect_pre_v17_gitlab_with_env<E: EnvSource + ?Sized>(
    client: &Client,
    api_url: &str,
    env: &E,
) -> bool {
    // 1. Check environment variable first.
    if let Some(version_str) = env.var("CI_SERVER_VERSION") {
        return is_pre_v17(&version_str);
    }

    // 2. Fall back to API call.
    let api = api_url.trim_end_matches('/');
    let version_url = format!("{}/version", api);
    match client.get(&version_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(body) = resp.json::<serde_json::Value>().await
                && let Some(version_str) = body["version"].as_str()
            {
                return is_pre_v17(version_str);
            }
            // Could not parse version — default to pre-v17 (conservative).
            true
        }
        // API call failed — default to pre-v17 (conservative).
        _ => true,
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
