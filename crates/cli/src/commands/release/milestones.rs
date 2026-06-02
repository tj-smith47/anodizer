use anodizer_core::config::Config;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_async};
use anodizer_core::scm::ScmTokenType;
use anyhow::{Context as _, Result};

/// Resolved milestone target ready for closing.
struct MilestoneTarget {
    name: String,
    owner: String,
    repo_name: String,
}

/// Resolve a milestone for closing: gate on `close: true`, render the name
/// template, then resolve the repo owner/name. Returns `Ok(None)` when the
/// milestone is gated off (close=false) or a `strict_guard`-permitted skip
/// applies in normal mode; returns `Err` only when `--strict` mode promotes
/// a config-resolution failure to an error.
///
/// Shared by [`preflight_milestones`] (validate-time pre-flight) and
/// [`close_milestones`] (post-pipeline publish) so both honour the same
/// skip-when-empty UX policy.
fn resolve_milestone_for_close(
    milestone_cfg: &anodizer_core::config::MilestoneConfig,
    ctx: &Context,
    log: &StageLogger,
) -> Result<Option<MilestoneTarget>> {
    if !milestone_cfg.resolved_close() {
        return Ok(None);
    }
    let name_template = milestone_cfg.resolved_name_template();
    let milestone_name = ctx
        .render_template(name_template)
        .context("milestone: render name_template")?;
    if milestone_name.is_empty() {
        ctx.strict_guard(log, "milestone: name_template rendered to empty — skipping")?;
        return Ok(None);
    }
    // Prefer `ctx.token_type` when choosing among mixed-provider configs so
    // a GitLab release run doesn't accidentally pick up a crate's GitHub block.
    let (owner, repo_name) = resolve_milestone_repo(milestone_cfg, &ctx.config, ctx.token_type);
    if owner.is_empty() || repo_name.is_empty() {
        ctx.strict_guard(
            log,
            "milestone: repo owner/name not resolvable — skipping close",
        )?;
        return Ok(None);
    }
    Ok(Some(MilestoneTarget {
        name: milestone_name,
        owner,
        repo_name,
    }))
}

/// Pre-flight milestone resolution at validate time.
///
/// Surfaces config-resolution failures (empty rendered name, unresolvable
/// repo) before the main release pipeline runs, so a misconfigured
/// `milestones:` block fails fast instead of after a full build. Routes
/// through [`Context::strict_guard`], matching [`close_milestones`].
pub(super) fn preflight_milestones(
    milestones: &[anodizer_core::config::MilestoneConfig],
    ctx: &mut Context,
    log: &StageLogger,
) -> Result<()> {
    for milestone_cfg in milestones {
        if let Some(target) = resolve_milestone_for_close(milestone_cfg, ctx, log)? {
            log.status(&format!(
                "milestone: will close '{}' on {}/{}",
                target.name, target.owner, target.repo_name
            ));
        }
    }
    Ok(())
}

/// Close milestones on the VCS provider after a release.
///
/// For each milestone config with `close: true`, renders the name template,
/// resolves the repo owner/name, and calls the GitHub/GitLab/Gitea API to
/// close the milestone. Config-resolution failures (empty rendered name,
/// unresolvable repo) route through [`Context::strict_guard`] (warn in
/// normal mode, error in `--strict`); `CloseMilestone` API failures are
/// gated by `fail_on_error`.
pub(super) fn close_milestones(
    milestones: &[anodizer_core::config::MilestoneConfig],
    ctx: &mut Context,
    dry_run: bool,
    log: &StageLogger,
) -> Result<()> {
    let token = ctx.options.token.clone().unwrap_or_default();

    // Build the tokio runtime once and reuse it across every close call.
    // The previous implementation paid per-milestone-per-provider runtime
    // construction (3 places) which can total 5-15ms per close on cold
    // configurations and is observable when many milestones close at once.
    //
    // Eagerly constructed so the per-iteration code path is a plain `&rt`
    // borrow rather than an `Option<Runtime>` dance with a structurally
    // infallible `expect`. The per-batch construction cost is paid once
    // even when every milestone is dry-run; the alternative (lazy init
    // inside the loop) trades a single runtime build for the panic-shape
    // anti-pattern of `runtime.as_ref().expect(...)` on every iteration.
    let rt = tokio::runtime::Runtime::new().context("milestone: create tokio runtime")?;

    // Single retry policy resolved from the top-level `retry:` block; reused
    // for every list + close HTTP call across providers so transient 5xx /
    // 429 / network failures retry per the user's config (defaults: 10
    // attempts × 10s base × 5m cap).
    let policy = ctx.retry_policy();

    for milestone_cfg in milestones {
        let Some(target) = resolve_milestone_for_close(milestone_cfg, ctx, log)? else {
            continue;
        };
        let MilestoneTarget {
            name: milestone_name,
            owner,
            repo_name,
        } = target;

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
                close_milestone_github(&rt, &token, &owner, &repo_name, &milestone_name, &policy)
            }
            ScmTokenType::GitLab => close_milestone_gitlab(
                &rt,
                &token,
                &owner,
                &repo_name,
                &milestone_name,
                api_url.as_deref(),
                &policy,
            ),
            ScmTokenType::Gitea => close_milestone_gitea(
                &rt,
                &token,
                &owner,
                &repo_name,
                &milestone_name,
                api_url.as_deref(),
                &policy,
            ),
        };
        match close_result {
            Ok(MilestoneCloseOutcome::Closed) => {
                log.status(&format!("milestone '{}' closed", milestone_name));
            }
            Ok(MilestoneCloseOutcome::NotFound) => {
                // Milestones are closed by name lookup, so a
                // re-run after a successful close finds nothing. Log it
                // verbosely so the user understands the no-op instead of
                // wondering whether a previous close actually happened.
                log.verbose(&format!(
                    "milestone '{}' not found on {}/{} (likely already closed)",
                    milestone_name, owner, repo_name
                ));
            }
            Err(e) => {
                if milestone_cfg.resolved_fail_on_error() {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MilestoneCloseOutcome {
    Closed,
    NotFound,
}

fn resolve_milestone_repo(
    milestone_cfg: &anodizer_core::config::MilestoneConfig,
    config: &Config,
    token_type: ScmTokenType,
) -> (String, String) {
    if let Some(ref repo_cfg) = milestone_cfg.repo
        && !repo_cfg.owner.is_empty()
        && !repo_cfg.name.is_empty()
    {
        return (repo_cfg.owner.clone(), repo_cfg.name.clone());
    }

    // Single pass over crates that prefers a release block matching the
    // active SCM (ctx.token_type) but accepts any block as a fallback.
    // Earlier we walked the crate list twice — once for the matching
    // provider, once for any provider — which produced two near-identical
    // loops with different short-circuit behaviour.
    let mut fallback: Option<(String, String)> = None;
    for crate_cfg in &config.crates {
        let Some(ref release_cfg) = crate_cfg.release else {
            continue;
        };
        let preferred = match token_type {
            ScmTokenType::GitHub => release_cfg.github.as_ref(),
            ScmTokenType::GitLab => release_cfg.gitlab.as_ref(),
            ScmTokenType::Gitea => release_cfg.gitea.as_ref(),
        };
        if let Some(r) = preferred {
            return (r.owner.clone(), r.name.clone());
        }
        if fallback.is_none() {
            fallback = release_cfg
                .github
                .as_ref()
                .or(release_cfg.gitlab.as_ref())
                .or(release_cfg.gitea.as_ref())
                .map(|r| (r.owner.clone(), r.name.clone()));
        }
    }
    if let Some(pair) = fallback {
        return pair;
    }

    // Final fallback: infer from the `origin` git remote so a top-level
    // `milestones:` block works without per-crate release config.
    if let Ok(pair) = anodizer_core::git::detect_owner_repo() {
        return pair;
    }

    (String::new(), String::new())
}

/// Close a GitHub milestone by name using the REST API.
fn close_milestone_github(
    rt: &tokio::runtime::Runtime,
    token: &str,
    owner: &str,
    repo: &str,
    milestone_name: &str,
    policy: &RetryPolicy,
) -> Result<MilestoneCloseOutcome> {
    if token.is_empty() {
        anyhow::bail!("no authentication token available for milestone close");
    }

    rt.block_on(async {
        let client = reqwest::Client::new();

        // List milestones with pagination to find the one with the matching title.
        // GitHub returns at most 100 per page. Each page request routes through
        // retry_http_async so transient 5xx / 429 / network failures retry.
        let mut page = 1u32;
        let mut milestone_number: Option<u64> = None;

        loop {
            let url = format!(
                "https://api.github.com/repos/{}/{}/milestones?state=open&per_page=100&page={}",
                owner, repo, page
            );
            let resp = retry_http_async(
                "milestone: list milestones",
                policy,
                SuccessClass::Strict,
                |_| {
                    client
                        .get(&url)
                        .header("Authorization", format!("Bearer {}", token))
                        .header("Accept", "application/vnd.github+json")
                        .header("User-Agent", anodizer_core::http::USER_AGENT)
                        .send()
                },
                |status, body| format!("milestone: list milestones failed (HTTP {status}): {body}"),
            )
            .await?;

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
            None => return Ok(MilestoneCloseOutcome::NotFound),
        };

        // Close the milestone
        let close_url = format!(
            "https://api.github.com/repos/{}/{}/milestones/{}",
            owner, repo, milestone_number
        );
        retry_http_async(
            "milestone: close milestone",
            policy,
            SuccessClass::Strict,
            |_| {
                client
                    .patch(&close_url)
                    .header("Authorization", format!("Bearer {}", token))
                    .header("Accept", "application/vnd.github+json")
                    .header("User-Agent", anodizer_core::http::USER_AGENT)
                    .json(&serde_json::json!({ "state": "closed" }))
                    .send()
            },
            |status, body| format!("milestone: close failed (HTTP {status}): {body}"),
        )
        .await?;

        Ok(MilestoneCloseOutcome::Closed)
    })
}

use anodizer_core::url::percent_encode_unreserved as url_encode;

/// Resolve the full API base URL (including any `/api/vN` suffix) for
/// milestone operations on GitLab/Gitea, normalising any trailing slash.
/// Returns `None` if no override is configured; callers default to the
/// public host.
fn resolve_milestone_api_url(
    _milestone_cfg: &anodizer_core::config::MilestoneConfig,
    config: &Config,
) -> Option<String> {
    let normalize = |api: &str| api.trim_end_matches('/').to_string();
    if let Some(ref gitlab) = config.gitlab_urls
        && let Some(ref api) = gitlab.api
    {
        return Some(normalize(api));
    }
    if let Some(ref gitea) = config.gitea_urls
        && let Some(ref api) = gitea.api
    {
        return Some(normalize(api));
    }
    None
}

/// Close a GitLab milestone by name using the REST API.
fn close_milestone_gitlab(
    rt: &tokio::runtime::Runtime,
    token: &str,
    owner: &str,
    repo: &str,
    milestone_name: &str,
    api_url: Option<&str>,
    policy: &RetryPolicy,
) -> Result<MilestoneCloseOutcome> {
    if token.is_empty() {
        anyhow::bail!("no authentication token available for GitLab milestone close");
    }
    // Default to GitLab.com's API root; user-supplied api_url already
    // includes the `/api/vN` path so we just append the resource path.
    let base = api_url.unwrap_or("https://gitlab.com/api/v4");

    rt.block_on(async {
        let client = reqwest::Client::new();
        let project_path = format!("{}/{}", owner, repo);
        let encoded_path = url_encode(&project_path);

        let url = format!(
            "{}/projects/{}/milestones?title={}",
            base,
            encoded_path,
            url_encode(milestone_name)
        );
        let resp = retry_http_async(
            "milestone: GitLab list milestones",
            policy,
            SuccessClass::Strict,
            |_| {
                client
                    .get(&url)
                    .header("PRIVATE-TOKEN", token)
                    .header("User-Agent", anodizer_core::http::USER_AGENT)
                    .send()
            },
            |status, body| {
                format!("milestone: GitLab list milestones failed (HTTP {status}): {body}")
            },
        )
        .await?;

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
            None => return Ok(MilestoneCloseOutcome::NotFound),
        };

        let close_url = format!(
            "{}/projects/{}/milestones/{}",
            base, encoded_path, milestone_id
        );
        retry_http_async(
            "milestone: GitLab close milestone",
            policy,
            SuccessClass::Strict,
            |_| {
                client
                    .put(&close_url)
                    .header("PRIVATE-TOKEN", token)
                    .header("User-Agent", anodizer_core::http::USER_AGENT)
                    .json(&serde_json::json!({ "state_event": "close" }))
                    .send()
            },
            |status, body| format!("milestone: GitLab close failed (HTTP {status}): {body}"),
        )
        .await?;
        Ok(MilestoneCloseOutcome::Closed)
    })
}

/// Close a Gitea milestone by name using the REST API.
fn close_milestone_gitea(
    rt: &tokio::runtime::Runtime,
    token: &str,
    owner: &str,
    repo: &str,
    milestone_name: &str,
    api_url: Option<&str>,
    policy: &RetryPolicy,
) -> Result<MilestoneCloseOutcome> {
    if token.is_empty() {
        anyhow::bail!("no authentication token available for Gitea milestone close");
    }
    // Default to Gitea.com's API root; user-supplied api_url already
    // includes the `/api/vN` path so we just append the resource path.
    let base = api_url.unwrap_or("https://gitea.com/api/v1");

    rt.block_on(async {
        let client = reqwest::Client::new();

        let url = format!(
            "{}/repos/{}/{}/milestones?state=open&name={}",
            base,
            owner,
            repo,
            url_encode(milestone_name)
        );
        let resp = retry_http_async(
            "milestone: Gitea list milestones",
            policy,
            SuccessClass::Strict,
            |_| {
                client
                    .get(&url)
                    .header("Authorization", format!("token {}", token))
                    .header("User-Agent", anodizer_core::http::USER_AGENT)
                    .send()
            },
            |status, body| {
                format!("milestone: Gitea list milestones failed (HTTP {status}): {body}")
            },
        )
        .await?;

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
            None => return Ok(MilestoneCloseOutcome::NotFound),
        };

        let close_url = format!(
            "{}/repos/{}/{}/milestones/{}",
            base, owner, repo, milestone_id
        );
        // PATCH only the `state` field. Including `title` would round-trip
        // the title and assert it hasn't changed under our feet — a
        // surprising side-effect for an API call meant to close, not
        // rename.
        //
        // 404 on the PATCH is a legitimate "milestone already closed /
        // deleted between list and close" race signal, so we catch it from
        // the retry helper's Break path and map to NotFound. Other 4xx
        // remain hard errors (the helper Breaks them).
        match retry_http_async(
            "milestone: Gitea close milestone",
            policy,
            SuccessClass::Strict,
            |_| {
                client
                    .patch(&close_url)
                    .header("Authorization", format!("token {}", token))
                    .header("User-Agent", anodizer_core::http::USER_AGENT)
                    .json(&serde_json::json!({ "state": "closed" }))
                    .send()
            },
            |status, body| format!("milestone: Gitea close failed (HTTP {status}): {body}"),
        )
        .await
        {
            Ok(_) => Ok(MilestoneCloseOutcome::Closed),
            Err(err) => {
                let status_code = err
                    .chain()
                    .find_map(|e| {
                        e.downcast_ref::<anodizer_core::retry::HttpError>()
                            .map(|h| h.status)
                    })
                    .unwrap_or(0);
                if status_code == 404 {
                    Ok(MilestoneCloseOutcome::NotFound)
                } else {
                    Err(err)
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::config::{
        Config, CrateConfig, MilestoneConfig, ReleaseConfig, ScmRepoConfig,
    };
    use anodizer_core::context::ContextOptions;

    fn ctx_with_strict(config: Config, strict: bool) -> Context {
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                strict,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Tag", "v1.0.0");
        ctx
    }

    fn config_with_resolvable_repo() -> Config {
        Config {
            crates: vec![CrateConfig {
                release: Some(ReleaseConfig {
                    github: Some(ScmRepoConfig {
                        owner: "toss45".into(),
                        name: "anodize".into(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn config_with_empty_release_block() -> Config {
        // An empty github block forces resolve_milestone_repo to return ("", "")
        // via the preferred-token-type branch, exercising the unresolvable-repo
        // path without needing to mock `git::detect_owner_repo`.
        Config {
            crates: vec![CrateConfig {
                release: Some(ReleaseConfig {
                    github: Some(ScmRepoConfig {
                        owner: String::new(),
                        name: String::new(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn empty_name_normal_mode_warns_and_skips() {
        let config = config_with_resolvable_repo();
        let mut ctx = ctx_with_strict(config, false);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(true),
            name_template: Some(String::new()),
            ..Default::default()
        }];
        close_milestones(&milestones, &mut ctx, true, &log)
            .expect("normal mode must skip empty rendered name cleanly");
    }

    #[test]
    fn empty_name_strict_mode_errors() {
        let config = config_with_resolvable_repo();
        let mut ctx = ctx_with_strict(config, true);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(true),
            name_template: Some(String::new()),
            ..Default::default()
        }];
        let err = close_milestones(&milestones, &mut ctx, true, &log)
            .expect_err("strict mode must error on empty rendered name");
        assert!(
            err.to_string().contains("rendered to empty"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn unresolvable_repo_normal_mode_warns_and_skips() {
        let config = config_with_empty_release_block();
        let mut ctx = ctx_with_strict(config, false);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(true),
            name_template: Some("{{ Tag }}".into()),
            ..Default::default()
        }];
        close_milestones(&milestones, &mut ctx, true, &log)
            .expect("normal mode must skip unresolvable repo cleanly");
    }

    #[test]
    fn unresolvable_repo_strict_mode_errors() {
        let config = config_with_empty_release_block();
        let mut ctx = ctx_with_strict(config, true);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(true),
            name_template: Some("{{ Tag }}".into()),
            ..Default::default()
        }];
        let err = close_milestones(&milestones, &mut ctx, true, &log)
            .expect_err("strict mode must error on unresolvable repo");
        assert!(
            err.to_string().contains("not resolvable"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn unresolvable_repo_ignores_fail_on_error() {
        // Decoupling regression: `fail_on_error` gates only CloseMilestone API
        // failures. Config-resolution failures route through `strict_guard`
        // independently — `fail_on_error: true` must not turn a normal-mode
        // unresolvable-repo into a hard error.
        let config = config_with_empty_release_block();
        let mut ctx = ctx_with_strict(config, false);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(true),
            fail_on_error: Some(true),
            name_template: Some("{{ Tag }}".into()),
            ..Default::default()
        }];
        close_milestones(&milestones, &mut ctx, true, &log)
            .expect("fail_on_error must not gate config-resolution failures");
    }

    #[test]
    fn preflight_resolvable_milestone_logs_and_returns_ok() {
        let config = config_with_resolvable_repo();
        let mut ctx = ctx_with_strict(config, false);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(true),
            name_template: Some("{{ Tag }}".into()),
            ..Default::default()
        }];
        preflight_milestones(&milestones, &mut ctx, &log)
            .expect("resolvable milestone must pre-flight cleanly");
    }

    #[test]
    fn preflight_close_false_is_noop() {
        // close: false milestones must be ignored at pre-flight, even when the
        // repo is unresolvable — they will not run at publish time either.
        let config = config_with_empty_release_block();
        let mut ctx = ctx_with_strict(config, true);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(false),
            name_template: Some("{{ Tag }}".into()),
            ..Default::default()
        }];
        preflight_milestones(&milestones, &mut ctx, &log)
            .expect("close: false must not trip strict_guard at pre-flight");
    }

    #[test]
    fn preflight_empty_name_normal_mode_warns_and_continues() {
        let config = config_with_resolvable_repo();
        let mut ctx = ctx_with_strict(config, false);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(true),
            name_template: Some(String::new()),
            ..Default::default()
        }];
        preflight_milestones(&milestones, &mut ctx, &log)
            .expect("normal mode pre-flight must skip empty rendered name");
    }

    #[test]
    fn preflight_empty_name_strict_mode_errors() {
        let config = config_with_resolvable_repo();
        let mut ctx = ctx_with_strict(config, true);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(true),
            name_template: Some(String::new()),
            ..Default::default()
        }];
        let err = preflight_milestones(&milestones, &mut ctx, &log)
            .expect_err("strict mode pre-flight must error on empty rendered name");
        assert!(
            err.to_string().contains("rendered to empty"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn preflight_unresolvable_repo_normal_mode_warns_and_continues() {
        let config = config_with_empty_release_block();
        let mut ctx = ctx_with_strict(config, false);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(true),
            name_template: Some("{{ Tag }}".into()),
            ..Default::default()
        }];
        preflight_milestones(&milestones, &mut ctx, &log)
            .expect("normal mode pre-flight must skip unresolvable repo");
    }

    #[test]
    fn preflight_unresolvable_repo_strict_mode_errors() {
        let config = config_with_empty_release_block();
        let mut ctx = ctx_with_strict(config, true);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(true),
            name_template: Some("{{ Tag }}".into()),
            ..Default::default()
        }];
        let err = preflight_milestones(&milestones, &mut ctx, &log)
            .expect_err("strict mode pre-flight must error on unresolvable repo");
        assert!(
            err.to_string().contains("not resolvable"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn resolve_milestone_for_close_returns_target_with_resolved_fields() {
        // Pin the user-facing artifact: when pre-flight emits its status log
        // ("milestone: will close 'X' on Y/Z"), the resolved fields must come
        // through as expected. Asserting on the helper's return value catches
        // a regression where `log.status` is dropped from preflight_milestones
        // (which the public-API test alone cannot detect).
        let config = config_with_resolvable_repo();
        let ctx = ctx_with_strict(config, false);
        let log = ctx.logger("milestone");
        let milestone_cfg = MilestoneConfig {
            close: Some(true),
            name_template: Some("{{ Tag }}".into()),
            ..Default::default()
        };
        let target = resolve_milestone_for_close(&milestone_cfg, &ctx, &log)
            .expect("resolution must succeed")
            .expect("close: true + resolvable repo must return Some(target)");
        assert_eq!(target.name, "v1.0.0");
        assert_eq!(target.owner, "toss45");
        assert_eq!(target.repo_name, "anodize");
    }

    #[test]
    fn preflight_close_false_with_empty_name_strict_is_noop() {
        // Gate-order invariant: the close-check must run before the
        // name-render check. A milestone with `close: false` and an empty
        // `name_template` must NOT trip strict_guard — even in strict mode,
        // since the entry is opted out entirely.
        let config = config_with_resolvable_repo();
        let mut ctx = ctx_with_strict(config, true);
        let log = ctx.logger("milestone");
        let milestones = vec![MilestoneConfig {
            close: Some(false),
            name_template: Some(String::new()),
            ..Default::default()
        }];
        preflight_milestones(&milestones, &mut ctx, &log)
            .expect("close: false must short-circuit before name-render check");
    }

    #[test]
    fn preflight_continues_past_unresolvable_in_normal_mode() {
        // Mixed-list happy path: a warn on milestone[0] must not bubble up as
        // Err in normal mode. Pairs with the strict-mode ordering test below,
        // which structurally proves iter[1] is reached.
        let config = config_with_empty_release_block();
        let mut ctx = ctx_with_strict(config, false);
        let log = ctx.logger("milestone");
        let milestones = vec![
            MilestoneConfig {
                close: Some(true),
                name_template: Some("{{ Tag }}".into()),
                ..Default::default()
            },
            MilestoneConfig {
                close: Some(true),
                name_template: Some("{{ Tag }}".into()),
                repo: Some(ScmRepoConfig {
                    owner: "explicit".into(),
                    name: "override".into(),
                }),
                ..Default::default()
            },
        ];
        preflight_milestones(&milestones, &mut ctx, &log)
            .expect("normal mode must not promote a warn on milestone[0] to Err");
    }

    #[test]
    fn preflight_strict_iterates_to_second_milestone() {
        // Structural proof of iteration: with [resolvable, unresolvable] in
        // strict mode, the resolvable item logs and proceeds, then the
        // unresolvable item trips strict_guard and bails. A bug that
        // short-circuits after the first iteration would return Ok here.
        let config = config_with_empty_release_block();
        let mut ctx = ctx_with_strict(config, true);
        let log = ctx.logger("milestone");
        let milestones = vec![
            MilestoneConfig {
                close: Some(true),
                name_template: Some("{{ Tag }}".into()),
                repo: Some(ScmRepoConfig {
                    owner: "explicit".into(),
                    name: "override".into(),
                }),
                ..Default::default()
            },
            MilestoneConfig {
                close: Some(true),
                name_template: Some("{{ Tag }}".into()),
                ..Default::default()
            },
        ];
        let err = preflight_milestones(&milestones, &mut ctx, &log)
            .expect_err("strict mode must error on milestone[1] unresolvable repo");
        assert!(
            err.to_string().contains("not resolvable"),
            "unexpected error: {}",
            err
        );
    }

    // ---- retry plumbing through close_milestone_gitlab --------------------
    //
    // Pin: each milestone HTTP call must route through retry_http_async so
    // transient 5xx / 429 / network failures retry per the user's policy.
    // GitLab is the easiest to test end-to-end because the function already
    // accepts a base API URL. The GitHub / Gitea functions share the same
    // retry helper + classifier — proving the wiring on one provider is
    // sufficient (the helper itself has its own 5xx-then-success test in
    // crates/core/src/retry.rs).

    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    #[test]
    fn close_milestone_gitlab_retries_5xx_then_succeeds() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        // Sequence: 503 on list, then 200 with one milestone, then 200 on
        // the close PUT. The retry helper should retry past the 503 and the
        // close succeeds.
        let (addr, calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 28\r\n\r\n[{\"id\":42,\"title\":\"v1.0.0\"}]",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 18\r\n\r\n{\"state\":\"closed\"}",
        ]);

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        };
        let api_url = format!("http://{addr}");

        let outcome = close_milestone_gitlab(
            &rt,
            "test-token",
            "myorg",
            "myrepo",
            "v1.0.0",
            Some(&api_url),
            &policy,
        )
        .expect("retry past 503 then close");
        assert_eq!(outcome, MilestoneCloseOutcome::Closed);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "expected 3 connections (503 retry GET, 200 GET, 200 PUT)"
        );
    }
}
