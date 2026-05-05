//! Gitea Compare API commit fetcher (`use: gitea`).

use std::collections::BTreeSet;

use anyhow::Result;

use anodizer_core::context::Context;
use anodizer_core::git::detect_owner_repo;
use anodizer_core::log::StageLogger;

use crate::group::{CommitInfo, extract_co_authors, parse_commit_message};

// ---------------------------------------------------------------------------
// Helper: fetch commits from Gitea API (use: gitea)
// ---------------------------------------------------------------------------

/// Fetch commits via the Gitea Compare API.
///
/// Uses `GET {api}/repos/{owner}/{repo}/compare/{prev}...{current}` to retrieve
/// commits between tags. Authentication is via `Authorization: token {value}`.
///
/// Falls back to an error (caller falls back to git) when no token is available
/// or the API call fails.
pub(crate) fn fetch_gitea_commits(
    ctx: &Context,
    prev_tag: &Option<String>,
    log: &StageLogger,
) -> Result<(Vec<CommitInfo>, String)> {
    let token = ctx
        .options
        .token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("gitea changelog: no token available"))?;

    let gitea_urls = ctx.config.gitea_urls.clone().unwrap_or_default();
    let api_url = gitea_urls
        .api
        .unwrap_or_else(|| "https://gitea.com/api/v1".to_string());
    let api = api_url.trim_end_matches('/');
    let skip_tls = gitea_urls.skip_tls_verify.unwrap_or(false);

    let (owner, repo) = detect_owner_repo()?;

    let url = if let Some(prev) = prev_tag {
        // Compare endpoint: GET /api/v1/repos/:owner/:repo/compare/:base...:head
        format!("{}/repos/{}/{}/compare/{}...HEAD", api, owner, repo, prev)
    } else {
        // No previous tag — list recent commits via the Commits API (not
        // /git/commits which returns a different JSON shape without the
        // top-level author object). This endpoint returns the same
        // GitHub-style commit objects as the compare endpoint.
        format!(
            "{}/repos/{}/{}/commits?sha=HEAD&limit=100",
            api, owner, repo
        )
    };

    log.status(&format!("fetching commits from Gitea API: {}", url));

    let client = reqwest::blocking::Client::builder()
        .user_agent(anodizer_core::http::USER_AGENT)
        .danger_accept_invalid_certs(skip_tls)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let response = client
        .get(&url)
        .header("Authorization", format!("token {}", token))
        .send()
        .map_err(|e| anyhow::anyhow!("gitea changelog: API request failed: {}", e))?;

    if !response.status().is_success() {
        anyhow::bail!(
            "gitea changelog: API returned status {} for {}",
            response.status(),
            url
        );
    }

    let body: serde_json::Value = response
        .json()
        .map_err(|e| anyhow::anyhow!("gitea changelog: failed to parse response: {}", e))?;

    // The compare endpoint returns { "commits": [...] }.
    // The commits listing endpoint returns [...] directly.
    let commits_arr = if let Some(arr) = body.get("commits").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = body.as_array() {
        arr.clone()
    } else {
        anyhow::bail!("gitea changelog: unexpected response format");
    };

    let mut logins = BTreeSet::new();
    let mut all_commit_infos = Vec::new();

    for item in &commits_arr {
        // Gitea compare response: commits have "sha", "commit.message",
        // "author.full_name", "author.email", "author.login".
        let sha = item.get("sha").and_then(|v| v.as_str()).unwrap_or_default();
        let short_sha = if sha.len() >= 7 { &sha[..7] } else { sha };
        let message = item
            .pointer("/commit/message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let subject = message.lines().next().unwrap_or(message);

        // Author info: try top-level "author" object first (Gitea API user),
        // then fall back to commit-level author fields.
        let author_name = item
            .pointer("/author/full_name")
            .and_then(|v| v.as_str())
            .or_else(|| item.pointer("/commit/author/name").and_then(|v| v.as_str()))
            .unwrap_or_default();
        let author_email = item
            .pointer("/author/email")
            .and_then(|v| v.as_str())
            .or_else(|| {
                item.pointer("/commit/author/email")
                    .and_then(|v| v.as_str())
            })
            .unwrap_or_default();
        let login = item
            .pointer("/author/login")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if !login.is_empty() {
            logins.insert(login.to_string());
        }

        let mut info = parse_commit_message(subject);
        info.hash = short_sha.to_string();
        info.full_hash = sha.to_string();
        info.author_name = author_name.to_string();
        info.author_email = author_email.to_string();
        info.login = login.to_string();

        // Extract co-authors from the full commit message body.
        let co_authors = extract_co_authors(message);
        for co_author in &co_authors {
            logins.insert(co_author.clone());
        }
        info.co_authors = co_authors;

        all_commit_infos.push(info);
    }

    log.status(&format!(
        "fetched {} commits from Gitea API",
        all_commit_infos.len()
    ));

    let logins_str = logins.into_iter().collect::<Vec<_>>().join(",");
    Ok((all_commit_infos, logins_str))
}
