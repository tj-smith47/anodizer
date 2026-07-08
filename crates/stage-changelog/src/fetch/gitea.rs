//! Gitea Compare API commit fetcher (`use: gitea`).

use std::collections::BTreeSet;

use anyhow::Result;

use anodizer_core::context::Context;
use anodizer_core::git::resolve_repo_slug;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{RetryLog, SuccessClass, retry_http_blocking};

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

    let cfg = ctx.config.release.as_ref().and_then(|r| r.gitea.as_ref());
    let slug = resolve_repo_slug(cfg.map(|c| c.owner.as_str()), cfg.map(|c| c.name.as_str()))?;
    let (owner, repo) = (slug.owner(), slug.name());

    let upper = ctx.options.changelog_to.as_deref().unwrap_or("HEAD");
    let url = if let Some(prev) = prev_tag {
        // Compare endpoint: GET /api/v1/repos/:owner/:repo/compare/:base...:head
        format!(
            "{}/repos/{}/{}/compare/{}...{}",
            api, owner, repo, prev, upper
        )
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

    log.status(&format!("fetching commits from {}", url));

    let client = reqwest::blocking::Client::builder()
        .user_agent(anodizer_core::http::USER_AGENT)
        .danger_accept_invalid_certs(skip_tls)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // Single retry policy resolved from the top-level `retry:` block so
    // transient 5xx / 429 / network failures retry per the user's config
    // (defaults: 10 attempts × 10s base × 5m cap).
    let policy = ctx.retry_policy();
    let (_, body_text) = retry_http_blocking(
        RetryLog::new("gitea changelog: compare API", log),
        &policy,
        SuccessClass::Strict,
        |_| {
            client
                .get(&url)
                .header("Authorization", format!("token {}", token))
                .send()
        },
        |status, body| {
            format!(
                "gitea changelog: API returned status {} for {}: {}",
                status, url, body
            )
        },
    )?;

    let body: serde_json::Value = serde_json::from_str(&body_text)
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

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{Config, GiteaUrlsConfig};
    use anodizer_core::context::ContextOptions;
    use anodizer_core::test_helpers::CwdGuard;
    use std::process::Command;

    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    /// Create a temp git repo with a remote pointing at the given URL so
    /// `resolve_repo_slug()` returns the parsed owner/repo. Returns the
    /// tempdir handle so the caller can keep it alive.
    fn temp_git_repo_with_remote(remote_url: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["init", "-q"]).current_dir(path);
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["remote", "add", "origin", remote_url])
                        .current_dir(path);
                    cmd
                },
                "git",
            )
            .status
            .success()
        );
        dir
    }

    // `serial` because the test mutates process-wide cwd via CwdGuard, which
    // races with other tests that shell out to `git log HEAD` from the
    // workspace root (e.g. test_changelog_stage_*_falls_back_to_git_no_token).
    #[test]
    #[serial_test::serial]
    fn fetch_gitea_commits_retries_5xx_then_succeeds() {
        use std::sync::atomic::Ordering;

        // Gitea compare endpoint returns {"commits":[...]} with
        // {sha, commit.message, author.full_name/email/login}.
        let body = r#"{"commits":[{"sha":"abcdef1234567890abcdef1234567890abcdef12","commit":{"message":"feat: add x","author":{"name":"Ada","email":"ada@example.com"}},"author":{"full_name":"Ada","email":"ada@example.com","login":"ada"}}]}"#;
        let body_len = body.len();
        let ok_resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        );
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            ok_resp,
        ]);

        let retry_yaml = "attempts: 3\ndelay: 1ms\nmax_delay: 2ms\n";
        let retry_cfg: anodizer_core::config::RetryConfig =
            serde_yaml_ng::from_str(retry_yaml).expect("parse retry");

        let config = Config {
            retry: Some(retry_cfg),
            gitea_urls: Some(GiteaUrlsConfig {
                api: Some(format!("http://{addr}/api/v1")),
                ..Default::default()
            }),
            ..Default::default()
        };

        let repo_dir = temp_git_repo_with_remote("git@gitea.example.com:myorg/myrepo.git");
        let _cwd_guard = CwdGuard::new(repo_dir.path()).expect("cwd guard");

        let ctx = Context::new(
            config,
            ContextOptions {
                token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        let log = ctx.logger("changelog");

        let (commits, logins) = fetch_gitea_commits(&ctx, &Some("v1.0.0".to_string()), &log)
            .expect("retries 5xx then parses");
        assert_eq!(commits.len(), 1);
        assert_eq!(logins, "ada");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one 503 retry then success"
        );
    }
}
