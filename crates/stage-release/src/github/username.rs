use std::collections::HashMap;

use super::rate_limit::check_github_search_rate_limit;

/// Percent-encode a URL query value (for `?q=...` search parameters).
fn percent_encode_query(s: &str) -> String {
    // Encode everything except unreserved + sub-delims + ':'/'@'
    // which matches RFC 3986 pchar = unreserved / pct-encoded / sub-delims / ":" / "@"
    const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'`')
        .add(b'{')
        .add(b'}')
        .add(b'%')
        .add(b'/');
    percent_encoding::utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

// ---------------------------------------------------------------------------
// resolve_github_username — email → GitHub @mention resolution
// ---------------------------------------------------------------------------

/// Resolve a commit author's email to their GitHub username.
///
/// 1. Check for noreply emails: `ID+USERNAME@users.noreply.github.com` or
///    `USERNAME@users.noreply.github.com`.
/// 2. Check the in-memory cache for previously resolved emails.
/// 3. Fall back to the GitHub Search Users API: `GET /search/users?q={email}+in:email`.
///    If exactly 1 result, cache and return the login. Otherwise cache `None`.
///
/// The cache avoids repeated API calls (GitHub search limit is 30 req/min).
/// Before making API calls, checks if the search rate limit has sufficient
/// remaining quota (threshold of 5) and skips the search if not.
#[allow(dead_code)]
pub(crate) async fn resolve_github_username(
    octocrab: &octocrab::Octocrab,
    email: &str,
    cache: &mut HashMap<String, Option<String>>,
    rate_limit_client: Option<(&reqwest::Client, &str)>,
) -> Option<String> {
    // 1. Parse noreply email patterns.
    if let Some(domain_start) = email.find("@users.noreply.github.com") {
        let local = &email[..domain_start];
        // Format: ID+USERNAME@users.noreply.github.com
        if let Some(plus_pos) = local.find('+') {
            let username = &local[plus_pos + 1..];
            if !username.is_empty() {
                let result = username.to_string();
                cache.insert(email.to_string(), Some(result.clone()));
                return Some(result);
            }
        }
        // Format: USERNAME@users.noreply.github.com
        if !local.is_empty() {
            let result = local.to_string();
            cache.insert(email.to_string(), Some(result.clone()));
            return Some(result);
        }
    }

    // 2. Check cache.
    if let Some(cached) = cache.get(email) {
        return cached.clone();
    }

    // 3. Check search rate limit before calling the API (30 req/min limit).
    //    Skip the search if we're running low on quota.
    if let Some((client, token)) = rate_limit_client
        && !check_github_search_rate_limit(client, token, 5).await
    {
        // Search rate limit too low — skip and cache None.
        cache.insert(email.to_string(), None);
        return None;
    }

    let query = format!("{}+in:email", email);
    let route = format!(
        "/search/users?q={}&per_page=1",
        percent_encode_query(&query)
    );

    let result: Option<String> = match octocrab
        .get::<serde_json::Value, _, _>(route, None::<&()>)
        .await
    {
        Ok(json) => {
            let total = json
                .get("total_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if total == 1 {
                json.get("items")
                    .and_then(|items| items.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|user| user.get("login"))
                    .and_then(|login| login.as_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        }
        Err(_) => None,
    };

    cache.insert(email.to_string(), result.clone());
    result
}
