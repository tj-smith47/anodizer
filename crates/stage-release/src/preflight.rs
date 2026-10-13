//! The release preflight: before anything is built or published, ask the
//! forge the release is created on whether this run can create it there.
//!
//! * GitHub: the token can push to the release repository, and the tag's
//!   release, when one already exists, is not immutable.
//! * GitLab: the token holds Developer access or higher on the project.
//! * Gitea: the token can push to the repository.
//!
//! A failed check is a blocker unless the crate's
//! `release.preflight.fail_on_error` is false, which reports it as a warning.

use std::collections::HashMap;
use std::time::Duration;

use anodizer_core::PreflightCheck;
use anodizer_core::config::{CrateConfig, ReleaseConfig, ScmRepoConfig, StringOrBool};
use anodizer_core::context::Context;
use anodizer_core::git::{GithubRepoProbe, RepoProbe, indeterminate_check};
use anodizer_core::retry::RetryPolicy;
use anodizer_core::scm::ScmTokenType;
use serde_json::Value;

use crate::publisher::active_release_configs;

const FAIL_ON_ERROR: &str = "release.preflight.fail_on_error";

/// The same budget the GitHub push probe gives one request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// One release the preflight asks about.
struct Target<'a> {
    crate_cfg: &'a CrateConfig,
    release_cfg: &'a ReleaseConfig,
    owner: String,
    repo: String,
    token: Option<String>,
    fail_on_error: Result<bool, String>,
}

/// Check every selected, non-skipped release against the forge the run
/// releases on (`ctx.token_type`). Pass, with no network call, when no
/// release is configured.
pub fn release_preflight(ctx: &Context) -> PreflightCheck {
    let targets = targets(ctx);
    if targets.is_empty() {
        return PreflightCheck::Pass;
    }
    match ctx.token_type {
        ScmTokenType::GitHub => github(ctx, targets),
        ScmTokenType::GitLab => gitlab(ctx, targets),
        ScmTokenType::Gitea => gitea(ctx, targets),
    }
}

/// One target per selected, non-skipped crate whose release block names a
/// repository on the run's forge, with the token the release stage would
/// use for it: the repository block's own `token`, else the run's token.
fn targets(ctx: &Context) -> Vec<Target<'_>> {
    let run_token = match ctx.token_type {
        ScmTokenType::GitHub => {
            anodizer_core::git::resolve_github_token_with_env(ctx.options.token.as_deref(), &|k| {
                ctx.env_var(k)
            })
        }
        _ => ctx.options.token.clone().filter(|t| !t.is_empty()),
    };
    let mut out = Vec::new();
    for crate_cfg in active_release_configs(ctx) {
        let release_cfg = crate_cfg
            .release
            .as_ref()
            .expect("filtered by active_release_configs");
        // A repository that cannot be resolved is the release stage's error
        // to report, with its own wording.
        if let Ok(Some(ScmRepoConfig { owner, name, .. })) =
            crate::resolve_release_repo(release_cfg, ctx.token_type, ctx)
        {
            out.push(Target {
                crate_cfg,
                release_cfg,
                owner,
                repo: name,
                token: crate::resolve_release_token(ctx, release_cfg).or_else(|| run_token.clone()),
                fail_on_error: fail_on_error(ctx, release_cfg),
            });
        }
    }
    out
}

/// `release.preflight.fail_on_error` for one crate: its own value, else the
/// top-level `release:` block's, else `true`. An empty or blank value is no
/// value, so it falls through to the next level. A value that does not
/// render to a boolean is the error text the run reports.
fn fail_on_error(ctx: &Context, release_cfg: &ReleaseConfig) -> Result<bool, String> {
    fn set(value: Option<&StringOrBool>) -> Option<&StringOrBool> {
        value.filter(|v| !matches!(v, StringOrBool::String(s) if s.trim().is_empty()))
    }
    let value = set(release_cfg
        .preflight
        .as_ref()
        .and_then(|p| p.fail_on_error.as_ref()))
    .or_else(|| {
        set(ctx
            .config
            .release
            .as_ref()?
            .preflight
            .as_ref()?
            .fail_on_error
            .as_ref())
    });
    match value {
        None => Ok(true),
        Some(v) => v
            .try_evaluate_strict(FAIL_ON_ERROR, |s| ctx.render_template(s))
            .map_err(|e| format!("{e:#}")),
    }
}

/// Apply a target's `fail_on_error` to its check. With `false` a blocker is
/// reported as a warning that says the run continues. A `fail_on_error` that
/// could not be read is a blocker, and it carries the check's own message so
/// neither problem hides the other.
fn apply_fail_on_error(
    check: PreflightCheck,
    fail_on_error: &Result<bool, String>,
) -> PreflightCheck {
    match (check, fail_on_error) {
        (PreflightCheck::Blocker(msg), Ok(false)) => PreflightCheck::Warning(format!(
            "{msg}; continuing because {FAIL_ON_ERROR} is false"
        )),
        (check, Ok(_)) => check,
        (PreflightCheck::Pass, Err(e)) => PreflightCheck::Blocker(e.clone()),
        (PreflightCheck::Warning(msg) | PreflightCheck::Blocker(msg), Err(e)) => {
            PreflightCheck::Blocker(format!("{e}; the check it decides on reported: {msg}"))
        }
    }
}

fn github(ctx: &Context, targets: Vec<Target<'_>>) -> PreflightCheck {
    let policy = RetryPolicy::PREFLIGHT;
    let log = ctx.logger("preflight");
    let base = anodizer_core::http::github_api_base_with_config(
        ctx.config.github_urls.as_ref(),
        ctx.env_source(),
    );
    // Crates sharing a release repository share one probe; each still applies
    // its own `fail_on_error` to the answer.
    let mut access: HashMap<(String, String, String), PreflightCheck> = HashMap::new();
    let mut existing: HashMap<(String, String, String, String), PreflightCheck> = HashMap::new();
    let mut acc = PreflightCheck::Pass;
    for t in targets {
        let (owner, repo) = (t.owner.as_str(), t.repo.as_str());
        let check = match t.token.as_deref() {
            None => PreflightCheck::Blocker(format!(
                "no GitHub token resolved ({}); cannot create the release in {owner}/{repo}",
                anodizer_core::git::github_token_hint()
            )),
            Some(token) => {
                let probe = GithubRepoProbe {
                    owner,
                    repo,
                    token: Some(token),
                    policy: &policy,
                    deadline: ctx.retry_deadline(),
                    strict: ctx.preflight_is_strict(),
                };
                let push = access
                    .entry((t.owner.clone(), t.repo.clone(), token.to_string()))
                    .or_insert_with(|| {
                        repo_push_check(
                            &probe,
                            ctx.config.github_urls.as_ref(),
                            ctx.env_source(),
                            &log,
                        )
                    })
                    .clone();
                match (&push, github_release_tag(ctx, t.crate_cfg, t.release_cfg)) {
                    (PreflightCheck::Blocker(_), _) | (_, None) => push,
                    (_, Some(tag)) => {
                        let found = existing
                            .entry((
                                t.owner.clone(),
                                t.repo.clone(),
                                tag.clone(),
                                token.to_string(),
                            ))
                            .or_insert_with(|| existing_release_check(&base, &probe, &tag, &log))
                            .clone();
                        push.merge(found)
                    }
                }
            }
        };
        acc = acc.merge(apply_fail_on_error(check, &t.fail_on_error));
    }
    acc
}

/// The tag the crate's release will be created on. A crate whose next
/// version the standalone preflight planned is asked about that version's
/// tag; one whose tag cannot be resolved is not asked about, since the
/// release stage reports that itself.
fn github_release_tag(
    ctx: &Context,
    crate_cfg: &CrateConfig,
    release_cfg: &ReleaseConfig,
) -> Option<String> {
    if release_cfg.tag.is_none()
        && !ctx.is_nightly()
        && ctx.planned_crate_versions.contains_key(&crate_cfg.name)
    {
        return anodizer_core::crate_scope::resolve_crate_tag(ctx, crate_cfg);
    }
    anodizer_core::release_tag::resolve_release_tag(ctx, crate_cfg, release_cfg.tag.as_deref()).ok()
}

/// `GET {base}/repos/{owner}/{repo}/releases/tags/{tag}`. No release (404)
/// passes; an immutable one blocks, since neither the release nor its
/// assets can be changed. A token the API refuses blocks too, because the
/// question then has no answer.
fn existing_release_check(
    base: &str,
    probe: &GithubRepoProbe<'_>,
    tag: &str,
    log: &anodizer_core::log::StageLogger,
) -> PreflightCheck {
    let (owner, repo, strict) = (probe.owner, probe.repo, probe.strict);
    let url = format!(
        "{base}/repos/{owner}/{repo}/releases/tags/{}",
        anodizer_core::url::percent_encode_path_segment(tag)
    );
    let client = match anodizer_core::http::blocking_client(PROBE_TIMEOUT) {
        Ok(c) => c,
        Err(e) => {
            return indeterminate_check(
                strict,
                format!("could not check {owner}/{repo} for an existing release {tag} ({e})"),
            );
        }
    };
    match anodizer_core::git::github_repo_probe(
        &client,
        &url,
        probe.token,
        probe.policy,
        probe.deadline,
        log,
    ) {
        RepoProbe::Body(body) => match serde_json::from_str::<Value>(&body) {
            Ok(v) if v.get("immutable").and_then(Value::as_bool) == Some(true) => {
                PreflightCheck::Blocker(format!(
                    "release {tag} in {owner}/{repo} already exists and is immutable, so it \
                     cannot be updated"
                ))
            }
            Ok(_) => PreflightCheck::Pass,
            Err(_) => indeterminate_check(
                strict,
                format!(
                    "could not parse the {owner}/{repo} release {tag} API response; check for an \
                     immutable release manually"
                ),
            ),
        },
        RepoProbe::Missing => PreflightCheck::Pass,
        RepoProbe::AuthDenied => PreflightCheck::Blocker(format!(
            "could not check {owner}/{repo} for an existing release {tag}: the GitHub token was \
             refused"
        )),
        RepoProbe::RateLimited => indeterminate_check(
            strict,
            format!(
                "GitHub API rate-limited while checking {owner}/{repo} for an existing release \
                 {tag}; check for an immutable release manually"
            ),
        ),
        RepoProbe::Inconclusive(reason) => indeterminate_check(
            strict,
            format!("could not check {owner}/{repo} for an existing release {tag} ({reason})"),
        ),
    }
}

fn gitlab(ctx: &Context, targets: Vec<Target<'_>>) -> PreflightCheck {
    let urls = ctx.config.gitlab_urls.clone().unwrap_or_default();
    let api = urls
        .api
        .clone()
        .unwrap_or_else(|| "https://gitlab.com/api/v4".to_string());
    let api = api.trim_end_matches('/');
    let skip_tls = urls.skip_tls_verify.unwrap_or(false);
    let mut probed: HashMap<(String, String), PreflightCheck> = HashMap::new();
    let mut acc = PreflightCheck::Pass;
    for t in targets {
        let project = crate::gitlab::gitlab_project_id(&t.owner, &t.repo);
        let check = match t.token.as_deref() {
            None => PreflightCheck::Blocker(format!(
                "no GitLab token available (set GITLAB_TOKEN, or pass --token); cannot create the \
                 release in {project}"
            )),
            Some(token)
                if crate::gitlab::resolve_use_job_token_with_env(
                    urls.use_job_token.unwrap_or(false),
                    token,
                    ctx.env_source(),
                ) =>
            {
                // A CI job token may create releases but may not read
                // `GET /projects/:id` (it is not among the endpoints GitLab
                // lets a job token call), so the refusal would say nothing
                // about whether the release can be created.
                ctx.logger("preflight").verbose(&format!(
                    "skipped the GitLab access check for {project}: a CI job token cannot read \
                     the project"
                ));
                PreflightCheck::Pass
            }
            Some(token) => probed
                .entry((project.clone(), token.to_string()))
                .or_insert_with(|| {
                    let url = format!(
                        "{api}/projects/{}",
                        crate::gitlab::encode_project_id(&project)
                    );
                    forge_probe(
                        ctx,
                        ForgeRequest {
                            url: &url,
                            auth: (crate::gitlab::auth_header(false), token.to_string()),
                            skip_tls,
                            subject: &format!("GitLab project {project}"),
                            forge: Forge::GitLab,
                        },
                        |v| gitlab_access(&project, v),
                    )
                })
                .clone(),
        };
        acc = acc.merge(apply_fail_on_error(check, &t.fail_on_error));
    }
    acc
}

/// GitLab reports the token's access on the project and on its group; the
/// higher one counts, and creating a release needs Developer (30). A project
/// without `permissions` (some token types never carry it) passes.
fn gitlab_access(project: &str, v: &Value) -> PreflightCheck {
    let Some(permissions) = v.get("permissions").filter(|p| !p.is_null()) else {
        return PreflightCheck::Pass;
    };
    let level = ["project_access", "group_access"]
        .iter()
        .filter_map(|k| permissions.get(k)?.get("access_level")?.as_u64())
        .max()
        .unwrap_or(0);
    if level < 30 {
        PreflightCheck::Blocker(format!(
            "GitLab token does not have developer or higher permissions for {project}; cannot \
             create the release"
        ))
    } else {
        PreflightCheck::Pass
    }
}

fn gitea(ctx: &Context, targets: Vec<Target<'_>>) -> PreflightCheck {
    let urls = ctx.config.gitea_urls.clone().unwrap_or_default();
    let api = anodizer_core::gitea_url::gitea_api_base(urls.api.as_deref());
    let skip_tls = urls.skip_tls_verify.unwrap_or(false);
    let mut probed: HashMap<(String, String, String), PreflightCheck> = HashMap::new();
    let mut acc = PreflightCheck::Pass;
    for t in targets {
        let (owner, repo) = (t.owner.as_str(), t.repo.as_str());
        let check = match t.token.as_deref() {
            None => PreflightCheck::Blocker(format!(
                "no Gitea token available (set GITEA_TOKEN, or pass --token); cannot create the \
                 release in {owner}/{repo}"
            )),
            Some(_) if !crate::gitea::has_scheme_and_host(&api) => PreflightCheck::Blocker(
                format!("invalid gitea_urls.api URL {api:?}; cannot create the release"),
            ),
            Some(token) => probed
                .entry((t.owner.clone(), t.repo.clone(), token.to_string()))
                .or_insert_with(|| {
                    let url = format!(
                        "{api}/repos/{}/{}",
                        anodizer_core::url::percent_encode_path_segment(owner),
                        anodizer_core::url::percent_encode_path_segment(repo)
                    );
                    forge_probe(
                        ctx,
                        ForgeRequest {
                            url: &url,
                            auth: ("Authorization", format!("token {token}")),
                            skip_tls,
                            subject: &format!("Gitea repository {owner}/{repo}"),
                            forge: Forge::Gitea,
                        },
                        |v| match v.get("permissions").filter(|p| !p.is_null()) {
                            Some(p) if p.get("push").and_then(Value::as_bool) != Some(true) => {
                                PreflightCheck::Blocker(format!(
                                    "Gitea token does not have push permission for \
                                     {owner}/{repo}; cannot create the release"
                                ))
                            }
                            _ => PreflightCheck::Pass,
                        },
                    )
                })
                .clone(),
        };
        acc = acc.merge(apply_fail_on_error(check, &t.fail_on_error));
    }
    acc
}

/// One GitLab or Gitea API request: the URL, the auth header the forge's
/// release client sends, its TLS setting, and the noun the messages name.
struct ForgeRequest<'a> {
    url: &'a str,
    auth: (&'static str, String),
    skip_tls: bool,
    subject: &'a str,
    forge: Forge,
}

#[derive(Clone, Copy)]
enum Forge {
    GitLab,
    Gitea,
}

impl Forge {
    /// The name a retry line gives the request.
    fn probe_label(self) -> &'static str {
        match self {
            Forge::GitLab => "gitlab project probe",
            Forge::Gitea => "gitea repo probe",
        }
    }

    /// Whether a 401 / 403 is the forge's rate limit rather than a refused
    /// token. GitLab sends `RateLimit-Remaining: 0` or `Retry-After` with a
    /// throttled request; Gitea sends `Retry-After`.
    fn rate_limited(self, headers: &reqwest::header::HeaderMap) -> bool {
        let exhausted = || {
            headers
                .get("ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim() == "0")
        };
        headers.contains_key(reqwest::header::RETRY_AFTER)
            || (matches!(self, Forge::GitLab) && exhausted())
    }
}

/// `GET` a GitLab project or Gitea repository and read the token's access
/// from the body with `read`. A 404 or a refused token blocks; a rate limit
/// (429, or a 401 / 403 the forge marks as one), a 5xx or a failed connection
/// warns (blocks under strict preflight).
fn forge_probe(
    ctx: &Context,
    req: ForgeRequest<'_>,
    read: impl FnOnce(&Value) -> PreflightCheck,
) -> PreflightCheck {
    let strict = ctx.preflight_is_strict();
    let subject = req.subject;
    let client = (|| -> anyhow::Result<reqwest::blocking::Client> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::HeaderName::from_bytes(req.auth.0.as_bytes())?,
            reqwest::header::HeaderValue::from_str(&req.auth.1)?,
        );
        Ok(reqwest::blocking::Client::builder()
            .user_agent(anodizer_core::http::USER_AGENT)
            .timeout(PROBE_TIMEOUT)
            .default_headers(headers)
            .danger_accept_invalid_certs(req.skip_tls)
            .build()?)
    })();
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            return indeterminate_check(
                strict,
                format!("could not probe {subject} ({e}); verify the token manually"),
            );
        }
    };
    let log = ctx.logger("preflight");
    let probe = anodizer_core::git::repo_probe(
        req.forge.probe_label(),
        || {
            client
                .get(req.url)
                .header(reqwest::header::ACCEPT, "application/json")
        },
        |headers| req.forge.rate_limited(headers),
        &RetryPolicy::PREFLIGHT,
        ctx.retry_deadline(),
        &log,
    );
    match probe {
        RepoProbe::Body(body) => match serde_json::from_str::<Value>(&body) {
            Ok(v) => read(&v),
            Err(_) => indeterminate_check(
                strict,
                format!("could not parse the {subject} API response; verify the token manually"),
            ),
        },
        RepoProbe::Missing | RepoProbe::AuthDenied => PreflightCheck::Blocker(format!(
            "{subject} not found or the token lacks access; cannot create the release"
        )),
        RepoProbe::RateLimited => indeterminate_check(
            strict,
            format!("API rate-limited while probing {subject}; verify the token manually"),
        ),
        RepoProbe::Inconclusive(reason) => indeterminate_check(
            strict,
            format!("could not probe {subject} ({reason}); verify the token manually"),
        ),
    }
}

/// Probe `GET {api_base}/repos/{owner}/{repo}` for push access, resolving
/// the base through [`anodizer_core::http::github_api_base_with_config`] so
/// the probe asks the host the release backend writes to (`github_urls.api`
/// on GitHub Enterprise Server).
/// See [`repo_push_check_at`] for the status/permission mapping.
pub(crate) fn repo_push_check<E: anodizer_core::EnvSource + ?Sized>(
    probe: &GithubRepoProbe<'_>,
    github_urls: Option<&anodizer_core::config::GitHubUrlsConfig>,
    env: &E,
    log: &anodizer_core::log::StageLogger,
) -> PreflightCheck {
    let base = anodizer_core::http::github_api_base_with_config(github_urls, env);
    let url = format!("{base}/repos/{}/{}", probe.owner, probe.repo);
    repo_push_check_at(&url, probe, log)
}

/// `url`-taking core of [`repo_push_check`] so a unit test can drive the
/// status/permission mapping against a local responder.
///
/// github-release is REQUIRED and creates the release + uploads assets, so a
/// definitively-unwritable target is a hard prerequisite the run cannot
/// satisfy, so those map to Blocker. A rate limit or a failed connection can
/// clear by the time the release runs, so it must not stop a release that
/// would otherwise succeed; those warn.
///
/// * 200 + `permissions.push == true` ⇒ Pass
/// * 200 + `permissions.push == false` ⇒ Blocker (token lacks `contents:write`)
/// * 200 + `permissions` absent ⇒ Warning (push scope undeterminable;
///   Blocker under strict preflight)
/// * 404 / 401 / 403 without a rate-limit signal ⇒ Blocker
/// * 429, or 401 / 403 carrying a rate-limit header ⇒ Warning (Blocker under
///   strict preflight)
/// * 5xx / transport failure / unexpected status ⇒ Warning (Blocker under
///   strict preflight)
pub(crate) fn repo_push_check_at(
    url: &str,
    probe: &GithubRepoProbe<'_>,
    log: &anodizer_core::log::StageLogger,
) -> PreflightCheck {
    let (owner, repo) = (probe.owner, probe.repo);
    anodizer_core::git::github_repo_push_check(
        url,
        probe,
        anodizer_core::git::RepoAccessOutcomes {
            push_denied: PreflightCheck::Blocker(format!(
                "GitHub token cannot push to {owner}/{repo} (needs contents:write); \
                 the release create / asset upload will fail"
            )),
            // A missing repo or a token that cannot access it is a hard
            // prerequisite the release-create path cannot satisfy — block.
            missing_or_denied: PreflightCheck::Blocker(format!(
                "release repo {owner}/{repo} not found or the GitHub token lacks access; \
                 cannot create the release"
            )),
        },
        log,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tlog() -> &'static anodizer_core::log::StageLogger {
        anodizer_core::test_helpers::test_logger()
    }

    fn one_attempt_policy() -> anodizer_core::retry::RetryPolicy {
        anodizer_core::retry::RetryPolicy {
            max_attempts: 1,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(1),
        }
    }

    /// A non-strict `acme/widget` probe carrying `token` and no wall-clock
    /// budget — the shape every status/permission-mapping test drives.
    fn probe<'a>(
        token: &'a str,
        policy: &'a anodizer_core::retry::RetryPolicy,
    ) -> anodizer_core::git::GithubRepoProbe<'a> {
        anodizer_core::git::GithubRepoProbe {
            owner: "acme",
            repo: "widget",
            token: Some(token),
            policy,
            deadline: None,
            strict: false,
        }
    }

    fn http(status_line: &str, body: &str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    #[test]
    fn repo_push_check_passes_when_push_true() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let (addr, _c) =
            spawn_oneshot_http_responder(vec![http("200 OK", r#"{"permissions":{"push":true}}"#)]);
        let url = format!("http://{addr}/repos/acme/widget");
        assert!(matches!(
            repo_push_check_at(&url, &probe("tok", &one_attempt_policy()), tlog()),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn repo_push_check_blocks_when_push_false() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        let (addr, _c) =
            spawn_oneshot_http_responder(vec![http("200 OK", r#"{"permissions":{"push":false}}"#)]);
        let url = format!("http://{addr}/repos/acme/widget");
        match repo_push_check_at(&url, &probe("tok", &one_attempt_policy()), tlog()) {
            PreflightCheck::Blocker(m) => assert!(m.contains("contents:write"), "{m}"),
            _ => panic!("expected Blocker when permissions.push is false"),
        }
    }

    #[test]
    fn repo_push_check_resolves_base_from_env_override() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // The probe must route through the shared github_api_base resolver:
        // the env override sends it to the local responder.
        let (addr, calls) =
            spawn_oneshot_http_responder(vec![http("200 OK", r#"{"permissions":{"push":true}}"#)]);
        let env = anodizer_core::MapEnvSource::new()
            .with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"));
        assert!(matches!(
            repo_push_check(&probe("tok", &one_attempt_policy()), None, &env, tlog()),
            PreflightCheck::Pass
        ));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "probe must hit the env-resolved base"
        );
    }

    #[test]
    fn repo_push_check_prefers_configured_ghes_api_base() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // A GHES `github_urls.api` config must win: the release backend
        // contacts that host, so the preflight verdict is only meaningful
        // against the same host.
        let (addr, calls) =
            spawn_oneshot_http_responder(vec![http("200 OK", r#"{"permissions":{"push":true}}"#)]);
        let urls = anodizer_core::config::GitHubUrlsConfig {
            api: Some(format!("http://{addr}/api/v3")),
            ..Default::default()
        };
        assert!(matches!(
            repo_push_check(
                &probe("tok", &one_attempt_policy()),
                Some(&urls),
                &anodizer_core::MapEnvSource::new(),
                tlog(),
            ),
            PreflightCheck::Pass
        ));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "probe must hit the configured GHES base"
        );
    }

    #[test]
    fn repo_push_check_blocks_on_auth_denied() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // A 401 with no rate-limit header ⇒ the token cannot access the repo.
        let (addr, _c) = spawn_oneshot_http_responder(vec![http("401 Unauthorized", "")]);
        let url = format!("http://{addr}/repos/acme/widget");
        match repo_push_check_at(&url, &probe("bad", &one_attempt_policy()), tlog()) {
            PreflightCheck::Blocker(m) => assert!(m.contains("acme/widget"), "{m}"),
            _ => panic!("expected Blocker on a 401"),
        }
    }

    fn http_with_header(status_line: &str, header: &str, body: &str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 {status_line}\r\n{header}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    #[test]
    fn repo_push_check_warns_on_rate_limited_403() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // A secondary-rate-limit 403 carries `X-RateLimit-Remaining: 0`; it is
        // transient (RepoProbe::RateLimited) and must NOT block a release whose
        // token is actually valid.
        let (addr, _c) = spawn_oneshot_http_responder(vec![http_with_header(
            "403 Forbidden",
            "X-RateLimit-Remaining: 0",
            "",
        )]);
        let url = format!("http://{addr}/repos/acme/widget");
        match repo_push_check_at(&url, &probe("tok", &one_attempt_policy()), tlog()) {
            PreflightCheck::Warning(m) => assert!(m.contains("rate-limited"), "{m}"),
            other => panic!("rate-limited 403 must degrade to Warning, not {other:?}"),
        }
    }

    #[test]
    fn repo_push_check_warns_on_inconclusive_5xx() {
        use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;
        // A 5xx is host-reachable-but-not-answering-cleanly
        // (RepoProbe::Inconclusive); it must warn rather than abort a release
        // whose token is fine.
        let (addr, _c) = spawn_oneshot_http_responder(vec![http("503 Service Unavailable", "")]);
        let url = format!("http://{addr}/repos/acme/widget");
        match repo_push_check_at(&url, &probe("tok", &one_attempt_policy()), tlog()) {
            PreflightCheck::Warning(m) => assert!(m.contains("acme/widget"), "{m}"),
            other => panic!("inconclusive 5xx must degrade to Warning, not {other:?}"),
        }
    }

    #[test]
    fn gitlab_access_takes_the_higher_level_and_passes_without_permissions() {
        let check = |body: &str| gitlab_access("o/r", &serde_json::from_str(body).unwrap());
        assert_eq!(check(r#"{"name":"r"}"#), PreflightCheck::Pass);
        assert_eq!(check(r#"{"permissions":null}"#), PreflightCheck::Pass);
        assert_eq!(
            check(r#"{"permissions":{"project_access":null,"group_access":{"access_level":40}}}"#),
            PreflightCheck::Pass
        );
        assert!(matches!(
            check(r#"{"permissions":{"project_access":{"access_level":20},"group_access":null}}"#),
            PreflightCheck::Blocker(_)
        ));
        assert!(matches!(
            check(r#"{"permissions":{"project_access":null,"group_access":null}}"#),
            PreflightCheck::Blocker(_)
        ));
    }

    #[test]
    fn an_unreadable_fail_on_error_keeps_the_check_message() {
        let err: Result<bool, String> = Err("bad value".into());
        assert_eq!(
            apply_fail_on_error(PreflightCheck::Pass, &err),
            PreflightCheck::Blocker("bad value".into())
        );
        assert_eq!(
            apply_fail_on_error(PreflightCheck::Warning("slow".into()), &err),
            PreflightCheck::Blocker("bad value; the check it decides on reported: slow".into())
        );
        assert_eq!(
            apply_fail_on_error(PreflightCheck::Blocker("denied".into()), &Ok(false)),
            PreflightCheck::Warning(
                "denied; continuing because release.preflight.fail_on_error is false".into()
            )
        );
        assert_eq!(
            apply_fail_on_error(PreflightCheck::Blocker("denied".into()), &Ok(true)),
            PreflightCheck::Blocker("denied".into())
        );
    }
}
