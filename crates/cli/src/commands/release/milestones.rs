use anodize_core::config::Config;
use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anodize_core::scm::ScmTokenType;
use anyhow::{Context as _, Result};

/// Close milestones on the VCS provider after a release.
///
/// For each milestone config with `close: true`, renders the name template,
/// resolves the repo owner/name, and calls the GitHub/GitLab/Gitea API to
/// close the milestone. Errors are logged as warnings unless `fail_on_error` is set.
pub(super) fn close_milestones(
    milestones: &[anodize_core::config::MilestoneConfig],
    ctx: &mut Context,
    dry_run: bool,
    log: &StageLogger,
) -> Result<()> {
    let token = ctx.options.token.clone().unwrap_or_default();

    for milestone_cfg in milestones {
        if !milestone_cfg.close.unwrap_or(false) {
            continue;
        }

        let name_template = milestone_cfg
            .name_template
            .as_deref()
            .unwrap_or("{{ Tag }}");
        let milestone_name = ctx
            .render_template(name_template)
            .context("milestone: render name_template")?;

        if milestone_name.is_empty() {
            log.verbose("milestone: skipping empty name");
            continue;
        }

        // Determine repo owner/name from milestone config or release config.
        // Prefer `ctx.token_type` when choosing among mixed-provider configs so
        // a GitLab release run doesn't accidentally pick up a crate's GitHub block.
        let (owner, repo_name) = resolve_milestone_repo(milestone_cfg, &ctx.config, ctx.token_type);

        if owner.is_empty() || repo_name.is_empty() {
            if milestone_cfg.fail_on_error.unwrap_or(false) {
                anyhow::bail!("milestone: repo owner/name not configured");
            }
            log.warn("milestone: skipping — repo owner/name not configured");
            continue;
        }

        if dry_run {
            log.status(&format!(
                "(dry-run) would close milestone '{}' on {}/{}",
                milestone_name, owner, repo_name
            ));
            continue;
        }

        log.status(&format!(
            "closing milestone '{}' on {}/{}",
            milestone_name, owner, repo_name
        ));

        // Prefer the effective SCM provider for this run (ctx.token_type) over
        // a best-guess scan of crate configs. A mixed-provider config where the
        // first crate's release block is GitHub but the user is running a
        // GitLab release would otherwise misroute the milestone close.
        let api_url = resolve_milestone_api_url(milestone_cfg, &ctx.config);
        let close_result = match ctx.token_type {
            ScmTokenType::GitHub => {
                close_milestone_github(&token, &owner, &repo_name, &milestone_name)
            }
            ScmTokenType::GitLab => close_milestone_gitlab(
                &token,
                &owner,
                &repo_name,
                &milestone_name,
                api_url.as_deref(),
            ),
            ScmTokenType::Gitea => close_milestone_gitea(
                &token,
                &owner,
                &repo_name,
                &milestone_name,
                api_url.as_deref(),
            ),
        };
        match close_result {
            Ok(()) => {
                log.status(&format!("milestone '{}' closed", milestone_name));
            }
            Err(e) => {
                if milestone_cfg.fail_on_error.unwrap_or(false) {
                    return Err(
                        e.context(format!("milestone: failed to close '{}'", milestone_name))
                    );
                }
                log.warn(&format!(
                    "milestone: could not close '{}': {}",
                    milestone_name, e
                ));
            }
        }
    }
    Ok(())
}

fn resolve_milestone_repo(
    milestone_cfg: &anodize_core::config::MilestoneConfig,
    config: &Config,
    token_type: ScmTokenType,
) -> (String, String) {
    if let Some(ref repo_cfg) = milestone_cfg.repo
        && !repo_cfg.owner.is_empty()
        && !repo_cfg.name.is_empty()
    {
        return (repo_cfg.owner.clone(), repo_cfg.name.clone());
    }

    // Fall back to the first crate's release config for the matching provider.
    // Scans in preferred-provider order — first a release block on the active
    // SCM (ctx.token_type), then anything else — so a mixed-provider repo
    // doesn't pick up an irrelevant block.
    for crate_cfg in &config.crates {
        if let Some(ref release_cfg) = crate_cfg.release {
            let matched = match token_type {
                ScmTokenType::GitHub => release_cfg
                    .github
                    .as_ref()
                    .map(|r| (r.owner.clone(), r.name.clone())),
                ScmTokenType::GitLab => release_cfg
                    .gitlab
                    .as_ref()
                    .map(|r| (r.owner.clone(), r.name.clone())),
                ScmTokenType::Gitea => release_cfg
                    .gitea
                    .as_ref()
                    .map(|r| (r.owner.clone(), r.name.clone())),
            };
            if let Some(pair) = matched {
                return pair;
            }
        }
    }

    // Last resort: any release block regardless of provider. Keeps behaviour
    // for older single-provider configs that never set a release block on the
    // token's matching provider (e.g. GitLab API via a Gitea-style block).
    for crate_cfg in &config.crates {
        if let Some(ref release_cfg) = crate_cfg.release {
            if let Some(ref gh) = release_cfg.github {
                return (gh.owner.clone(), gh.name.clone());
            }
            if let Some(ref gl) = release_cfg.gitlab {
                return (gl.owner.clone(), gl.name.clone());
            }
            if let Some(ref gt) = release_cfg.gitea {
                return (gt.owner.clone(), gt.name.clone());
            }
        }
    }

    // Final fallback: infer from the `origin` git remote (matches GoReleaser
    // milestone.go:30-41 `ExtractRepoFromConfig`). Lets top-level `milestones:`
    // blocks work without any per-crate release config when the `origin`
    // remote already points at the right owner/name.
    if let Ok(pair) = anodize_core::git::detect_owner_repo() {
        return pair;
    }

    (String::new(), String::new())
}

/// Close a GitHub milestone by name using the REST API.
fn close_milestone_github(
    token: &str,
    owner: &str,
    repo: &str,
    milestone_name: &str,
) -> Result<()> {
    if token.is_empty() {
        anyhow::bail!("no authentication token available for milestone close");
    }

    let rt = tokio::runtime::Runtime::new().context("milestone: create tokio runtime")?;
    rt.block_on(async {
        let client = reqwest::Client::new();

        // List milestones with pagination to find the one with the matching title.
        // GitHub returns at most 100 per page.
        let mut page = 1u32;
        let mut milestone_number: Option<u64> = None;

        loop {
            let url = format!(
                "https://api.github.com/repos/{}/{}/milestones?state=open&per_page=100&page={}",
                owner, repo, page
            );
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {}", token))
                .header("Accept", "application/vnd.github+json")
                .header("User-Agent", anodize_core::http::USER_AGENT)
                .send()
                .await
                .context("milestone: list milestones request failed")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "milestone: list milestones failed (HTTP {}): {}",
                    status,
                    body
                );
            }

            let milestones: Vec<serde_json::Value> = resp
                .json()
                .await
                .context("milestone: parse milestones response")?;

            if milestones.is_empty() {
                break;
            }

            if let Some(m) = milestones.iter().find(|m| {
                m.get("title")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == milestone_name)
            }) {
                milestone_number = m.get("number").and_then(|n| n.as_u64());
                break;
            }

            // If we got fewer than 100 results, there are no more pages.
            if milestones.len() < 100 {
                break;
            }
            page += 1;
        }

        let milestone_number = match milestone_number {
            Some(n) => n,
            None => {
                // Milestone not found -- treat as success (may have been closed already)
                return Ok(());
            }
        };

        // Close the milestone
        let close_url = format!(
            "https://api.github.com/repos/{}/{}/milestones/{}",
            owner, repo, milestone_number
        );
        let resp = client
            .patch(&close_url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", anodize_core::http::USER_AGENT)
            .json(&serde_json::json!({ "state": "closed" }))
            .send()
            .await
            .context("milestone: close milestone request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("milestone: close failed (HTTP {}): {}", status, body);
        }

        Ok(())
    })
}

use anodize_core::url::percent_encode_unreserved as url_encode;

/// Resolve the API base URL for milestone operations on GitLab/Gitea.
fn resolve_milestone_api_url(
    _milestone_cfg: &anodize_core::config::MilestoneConfig,
    config: &Config,
) -> Option<String> {
    // Check top-level gitlab_urls / gitea_urls config
    if let Some(ref gitlab) = config.gitlab_urls
        && let Some(ref api) = gitlab.api
    {
        // Strip trailing /api/v4/ to get base URL
        let base = api.trim_end_matches('/').trim_end_matches("/api/v4");
        return Some(base.to_string());
    }
    if let Some(ref gitea) = config.gitea_urls
        && let Some(ref api) = gitea.api
    {
        let base = api.trim_end_matches('/').trim_end_matches("/api/v1");
        return Some(base.to_string());
    }
    None
}

/// Close a GitLab milestone by name using the REST API.
fn close_milestone_gitlab(
    token: &str,
    owner: &str,
    repo: &str,
    milestone_name: &str,
    api_url: Option<&str>,
) -> Result<()> {
    if token.is_empty() {
        anyhow::bail!("no authentication token available for GitLab milestone close");
    }
    let base = api_url.unwrap_or("https://gitlab.com");

    let rt = tokio::runtime::Runtime::new().context("milestone: create tokio runtime")?;
    rt.block_on(async {
        let client = reqwest::Client::new();
        let project_path = format!("{}/{}", owner, repo);
        let encoded_path = url_encode(&project_path);

        // List milestones to find matching title
        let url = format!(
            "{}/api/v4/projects/{}/milestones?title={}",
            base,
            encoded_path,
            url_encode(milestone_name)
        );
        let resp = client
            .get(&url)
            .header("PRIVATE-TOKEN", token)
            .header("User-Agent", anodize_core::http::USER_AGENT)
            .send()
            .await
            .context("milestone: GitLab list milestones failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "milestone: GitLab list milestones failed (HTTP {}): {}",
                status,
                body
            );
        }

        let milestones: Vec<serde_json::Value> = resp
            .json()
            .await
            .context("milestone: parse GitLab milestones")?;

        let milestone_id = milestones
            .iter()
            .find(|m| {
                m.get("title")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == milestone_name)
            })
            .and_then(|m| m.get("id").and_then(|i| i.as_u64()));

        let milestone_id = match milestone_id {
            Some(id) => id,
            None => return Ok(()), // Not found — may be already closed
        };

        // Close the milestone (GoReleaser: StateEvent = "close")
        let close_url = format!(
            "{}/api/v4/projects/{}/milestones/{}",
            base, encoded_path, milestone_id
        );
        let resp = client
            .put(&close_url)
            .header("PRIVATE-TOKEN", token)
            .header("User-Agent", anodize_core::http::USER_AGENT)
            .json(&serde_json::json!({ "state_event": "close" }))
            .send()
            .await
            .context("milestone: GitLab close milestone failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("milestone: GitLab close failed (HTTP {}): {}", status, body);
        }
        Ok(())
    })
}

/// Close a Gitea milestone by name using the REST API.
fn close_milestone_gitea(
    token: &str,
    owner: &str,
    repo: &str,
    milestone_name: &str,
    api_url: Option<&str>,
) -> Result<()> {
    if token.is_empty() {
        anyhow::bail!("no authentication token available for Gitea milestone close");
    }
    let base = api_url.unwrap_or("https://gitea.com");

    let rt = tokio::runtime::Runtime::new().context("milestone: create tokio runtime")?;
    rt.block_on(async {
        let client = reqwest::Client::new();

        // List milestones to find matching title
        let url = format!(
            "{}/api/v1/repos/{}/{}/milestones?state=open&name={}",
            base,
            owner,
            repo,
            url_encode(milestone_name)
        );
        let resp = client
            .get(&url)
            .header("Authorization", format!("token {}", token))
            .header("User-Agent", anodize_core::http::USER_AGENT)
            .send()
            .await
            .context("milestone: Gitea list milestones failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "milestone: Gitea list milestones failed (HTTP {}): {}",
                status,
                body
            );
        }

        let milestones: Vec<serde_json::Value> = resp
            .json()
            .await
            .context("milestone: parse Gitea milestones")?;

        let milestone_id = milestones
            .iter()
            .find(|m| {
                m.get("title")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t == milestone_name)
            })
            .and_then(|m| m.get("id").and_then(|i| i.as_u64()));

        let milestone_id = match milestone_id {
            Some(id) => id,
            None => return Ok(()), // Not found — may be already closed
        };

        // Close the milestone (GoReleaser: state = "closed")
        let close_url = format!(
            "{}/api/v1/repos/{}/{}/milestones/{}",
            base, owner, repo, milestone_id
        );
        let resp = client
            .patch(&close_url)
            .header("Authorization", format!("token {}", token))
            .header("User-Agent", anodize_core::http::USER_AGENT)
            .json(&serde_json::json!({ "state": "closed", "title": milestone_name }))
            .send()
            .await
            .context("milestone: Gitea close milestone failed")?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            // 404 means milestone not found
            return Ok(());
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("milestone: Gitea close failed (HTTP {}): {}", status, body);
        }
        Ok(())
    })
}
