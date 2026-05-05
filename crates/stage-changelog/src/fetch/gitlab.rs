//! GitLab Repository Compare API commit fetcher (`use: gitlab`).

use anyhow::Result;

use anodizer_core::context::Context;
use anodizer_core::git::detect_owner_repo;
use anodizer_core::log::StageLogger;

use crate::group::{CommitInfo, extract_co_authors, parse_commit_message};

// ---------------------------------------------------------------------------
// Helper: fetch commits from GitLab API (use: gitlab)
// ---------------------------------------------------------------------------

/// Fetch commits via the GitLab Repository Compare API.
///
/// Uses `GET {api}/projects/{project_id}/repository/compare?from={prev}&to={current}`
/// to retrieve commits between tags. Authentication is via `PRIVATE-TOKEN` header
/// (or `JOB-TOKEN` when `use_job_token` is configured in `gitlab_urls`).
///
/// Falls back to an error (caller falls back to git) when no token is available
/// or the API call fails.
pub(crate) fn fetch_gitlab_commits(
    ctx: &Context,
    prev_tag: &Option<String>,
    log: &StageLogger,
) -> Result<(Vec<CommitInfo>, String)> {
    let token = ctx
        .options
        .token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("gitlab changelog: no token available"))?;

    let gitlab_urls = ctx.config.gitlab_urls.clone().unwrap_or_default();
    let api_url = gitlab_urls
        .api
        .unwrap_or_else(|| "https://gitlab.com/api/v4".to_string());
    let api = api_url.trim_end_matches('/');
    // Match GoReleaser's `checkUseJobToken`: only send JOB-TOKEN when
    // CI_JOB_TOKEN is set, the flag is on, and the provided token equals
    // CI_JOB_TOKEN. Otherwise fall back to PRIVATE-TOKEN.
    let use_job_token = {
        let ci_token = std::env::var("CI_JOB_TOKEN").unwrap_or_default();
        !ci_token.is_empty() && gitlab_urls.use_job_token.unwrap_or(false) && token == ci_token
    };
    let skip_tls = gitlab_urls.skip_tls_verify.unwrap_or(false);

    // Derive project ID from git remote (owner/repo), URL-encode slashes.
    let (owner, repo) = detect_owner_repo()?;
    let project_path = if owner.is_empty() {
        repo.clone()
    } else {
        format!("{}/{}", owner, repo)
    };
    // URL-encode the project path (slashes become %2F).
    let encoded_project = project_path.replace('/', "%2F");

    let auth_header = if use_job_token {
        "JOB-TOKEN"
    } else {
        "PRIVATE-TOKEN"
    };

    let from_ref = prev_tag.as_deref().unwrap_or("");
    let to_ref = "HEAD";

    let url = if from_ref.is_empty() {
        // No previous tag — list recent commits.
        format!(
            "{}/projects/{}/repository/commits?per_page=100&ref_name={}",
            api, encoded_project, to_ref
        )
    } else {
        format!(
            "{}/projects/{}/repository/compare?from={}&to={}",
            api, encoded_project, from_ref, to_ref
        )
    };

    log.status(&format!("fetching commits from GitLab API: {}", url));

    let client = reqwest::blocking::Client::builder()
        .user_agent(anodizer_core::http::USER_AGENT)
        .danger_accept_invalid_certs(skip_tls)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let response = client
        .get(&url)
        .header(auth_header, token)
        .send()
        .map_err(|e| anyhow::anyhow!("gitlab changelog: API request failed: {}", e))?;

    if !response.status().is_success() {
        anyhow::bail!(
            "gitlab changelog: API returned status {} for {}",
            response.status(),
            url
        );
    }

    let body: serde_json::Value = response
        .json()
        .map_err(|e| anyhow::anyhow!("gitlab changelog: failed to parse response: {}", e))?;

    // The compare endpoint returns { "commits": [...] }.
    // The commits listing endpoint returns [...] directly.
    let commits_arr = if let Some(arr) = body.get("commits").and_then(|v| v.as_array()) {
        arr.clone()
    } else if let Some(arr) = body.as_array() {
        arr.clone()
    } else {
        anyhow::bail!("gitlab changelog: unexpected response format");
    };

    let mut all_commit_infos = Vec::new();

    for item in &commits_arr {
        let sha = item.get("id").and_then(|v| v.as_str()).unwrap_or_default();
        let short_sha = if sha.len() >= 7 { &sha[..7] } else { sha };
        let message = item
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Use first line of the commit message as the subject.
        let subject = message.lines().next().unwrap_or(message);
        let author_name = item
            .get("author_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let author_email = item
            .get("author_email")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let mut info = parse_commit_message(subject);
        info.hash = short_sha.to_string();
        info.full_hash = sha.to_string();
        info.author_name = author_name.to_string();
        info.author_email = author_email.to_string();
        // GitLab's compare API does not include login information,
        // but we can extract co-authors from commit message trailers.
        info.co_authors = extract_co_authors(message);
        all_commit_infos.push(info);
    }

    log.status(&format!(
        "fetched {} commits from GitLab API",
        all_commit_infos.len()
    ));

    // Aggregate co-author names into logins (GitLab has no username API).
    let mut logins = std::collections::BTreeSet::new();
    for info in &all_commit_infos {
        for co_author in &info.co_authors {
            logins.insert(co_author.clone());
        }
    }
    let logins_str = logins.into_iter().collect::<Vec<_>>().join(",");
    Ok((all_commit_infos, logins_str))
}
