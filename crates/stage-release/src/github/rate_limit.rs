use crate::release_log;

// ---------------------------------------------------------------------------
// check_github_rate_limit — proactive rate limit checking
// ---------------------------------------------------------------------------

/// proactively check GitHub API
/// rate limits before making requests. If remaining calls are below the
/// threshold, sleep until the reset time.
pub(crate) async fn check_github_rate_limit(client: &reqwest::Client, token: &str, threshold: u64) {
    let url = "https://api.github.com/rate_limit";
    let resp = match client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", anodizer_core::http::USER_AGENT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return, // Can't check — continue and hope for the best
    };

    if !resp.status().is_success() {
        return;
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return,
    };

    let remaining = body
        .pointer("/resources/core/remaining")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);
    let reset_epoch = body
        .pointer("/resources/core/reset")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if remaining > threshold {
        return;
    }

    // Sleep until reset + small buffer
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let sleep_secs = if reset_epoch > now {
        reset_epoch - now + 1
    } else {
        5 // Minimum 5 seconds if reset is in the past
    };
    release_log().status(&format!(
        "rate limit almost reached ({remaining} remaining), sleeping for {sleep_secs}s..."
    ));
    tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
}

// ---------------------------------------------------------------------------
// check_github_search_rate_limit — proactive search rate limit checking
// ---------------------------------------------------------------------------

/// Check GitHub Search API rate limits (separate from core rate limit).
/// Returns `true` if enough quota remains to make a search request.
#[allow(dead_code)]
pub(crate) async fn check_github_search_rate_limit(
    client: &reqwest::Client,
    token: &str,
    threshold: u64,
) -> bool {
    let url = "https://api.github.com/rate_limit";
    let resp = match client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", anodizer_core::http::USER_AGENT)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return true, // Can't check — assume ok
    };

    if !resp.status().is_success() {
        return true;
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return true,
    };

    let remaining = body
        .pointer("/resources/search/remaining")
        .and_then(|v| v.as_u64())
        .unwrap_or(u64::MAX);

    remaining > threshold
}
