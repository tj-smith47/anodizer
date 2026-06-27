//! GitLab Repository Compare API commit fetcher (`use: gitlab`).

use anyhow::Result;

use anodizer_core::context::Context;
use anodizer_core::git::resolve_repo_slug;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{SuccessClass, retry_http_blocking};

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
    // Only send JOB-TOKEN when
    // CI_JOB_TOKEN is set, the flag is on, and the provided token equals
    // CI_JOB_TOKEN. Otherwise fall back to PRIVATE-TOKEN.
    let use_job_token = {
        let ci_token = ctx.env_var("CI_JOB_TOKEN").unwrap_or_default();
        !ci_token.is_empty() && gitlab_urls.use_job_token.unwrap_or(false) && token == ci_token
    };
    let skip_tls = gitlab_urls.skip_tls_verify.unwrap_or(false);

    // Project ID = owner/repo: config override (`release.gitlab`) wins over
    // the origin remote. URL-encode slashes below.
    let cfg = ctx.config.release.as_ref().and_then(|r| r.gitlab.as_ref());
    let slug = resolve_repo_slug(cfg.map(|c| c.owner.as_str()), cfg.map(|c| c.name.as_str()))?;
    let (owner, repo) = (slug.owner().to_string(), slug.name().to_string());
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
    let to_ref = ctx.options.changelog_to.as_deref().unwrap_or("HEAD");

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
        "gitlab changelog: compare API",
        &policy,
        SuccessClass::Strict,
        |_| client.get(&url).header(auth_header, token).send(),
        |status, body| {
            format!(
                "gitlab changelog: API returned status {} for {}: {}",
                status, url, body
            )
        },
    )?;

    let body: serde_json::Value = serde_json::from_str(&body_text)
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
        // `id` is untrusted JSON; a non-char-boundary at byte 7 (a multibyte
        // char in a malformed id) would panic a byte slice, so truncate safely.
        let short_sha = sha.get(..7).unwrap_or(sha);
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

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{Config, GitLabUrlsConfig};
    use anodizer_core::context::ContextOptions;
    use anodizer_core::test_helpers::CwdGuard;
    use std::process::Command;

    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    /// Create a temp git repo with a remote pointing at the given URL so
    /// `resolve_repo_slug()` (which calls `git remote get-url origin`)
    /// returns ("myorg", "myrepo"). Returns the tempdir handle so the
    /// caller can keep it alive.
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
    fn fetch_gitlab_commits_retries_5xx_then_succeeds() {
        use std::sync::atomic::Ordering;

        // GitLab compare endpoint returns {"commits": [...]}; we return one
        // commit so the parser has something to chew on.
        let body = r#"{"commits":[{"id":"abcdef1234567890abcdef1234567890abcdef12","message":"feat: add x","author_name":"Ada","author_email":"ada@example.com"}]}"#;
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

        // Tight retry policy so the 503 retry waits a couple of ms total.
        let retry_yaml = "attempts: 3\ndelay: 1ms\nmax_delay: 2ms\n";
        let retry_cfg: anodizer_core::config::RetryConfig =
            serde_yaml_ng::from_str(retry_yaml).expect("parse retry");

        let config = Config {
            retry: Some(retry_cfg),
            gitlab_urls: Some(GitLabUrlsConfig {
                api: Some(format!("http://{addr}/api/v4")),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Fake git repo so resolve_repo_slug() returns ("myorg", "myrepo").
        // CwdGuard restores cwd on drop so test parallelism isn't affected
        // beyond the brief git-init window.
        let repo_dir = temp_git_repo_with_remote("git@gitlab.example.com:myorg/myrepo.git");
        let _cwd_guard = CwdGuard::new(repo_dir.path()).expect("cwd guard");

        let ctx = Context::new(
            config,
            ContextOptions {
                token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        let log = ctx.logger("changelog");

        let (commits, _logins) = fetch_gitlab_commits(&ctx, &Some("v1.0.0".to_string()), &log)
            .expect("retries 5xx then parses");
        assert_eq!(commits.len(), 1);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "one 503 retry then success"
        );
    }

    // A short, multibyte `id` (a malformed GitLab payload) must NOT panic the
    // `&sha[..7]` byte slice the parser once used: `sha.get(..7)` truncates on
    // a char boundary or falls back to the whole string.
    #[test]
    #[serial_test::serial]
    fn fetch_gitlab_commits_short_multibyte_id_does_not_panic() {
        // `id` is "café" — 4 chars / 5 bytes, shorter than 7 and with a
        // multibyte char straddling byte 4: a naive `&sha[..7]` would panic.
        let body = r#"{"commits":[{"id":"café","message":"feat: add x","author_name":"Ada","author_email":"ada@example.com"}]}"#;
        let body_len = body.len();
        let ok_resp: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {body_len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        );
        let (addr, _calls) = spawn_oneshot_http_responder(vec![ok_resp]);

        let config = Config {
            gitlab_urls: Some(GitLabUrlsConfig {
                api: Some(format!("http://{addr}/api/v4")),
                ..Default::default()
            }),
            ..Default::default()
        };

        let repo_dir = temp_git_repo_with_remote("git@gitlab.example.com:myorg/myrepo.git");
        let _cwd_guard = CwdGuard::new(repo_dir.path()).expect("cwd guard");

        let ctx = Context::new(
            config,
            ContextOptions {
                token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        let log = ctx.logger("changelog");

        let (commits, _logins) = fetch_gitlab_commits(&ctx, &Some("v1.0.0".to_string()), &log)
            .expect("short multibyte id parses without panic");
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].hash, "café", "short id kept whole");
    }
}
