//! Live pre-publish probes shared by the publishers whose
//! [`Publisher::preflight`](anodizer_core::Publisher::preflight) gate needs a
//! real network check rather than the presence-only
//! [`requirements()`](anodizer_core::Publisher::requirements) declaration.
//!
//! Two probe families live here:
//!
//! * [`probe_token_auth`] — a `whoami`-style authenticated GET that proves a
//!   registry token is accepted (not merely present). Consumed by the
//!   irreversible cargo / npm publishers, whose token slot is a one-way door.
//! * [`github_repo_check`] / [`github_repo_config_check`] — a
//!   `GET /repos/{owner}/{repo}` probe that proves the target index/fork repo
//!   exists and the resolved token can push to it. Shared by every
//!   GitHub-repo-backed publisher (homebrew, scoop, nix, krew, winget).
//!
//! [`probe_version_published`] backs the npm duplicate-version warning — npm
//! has no companion state-query checker, so this is its only duplicate guard.
//!
//! All probes degrade to [`PreflightCheck::Warning`] (never a hard block) on a
//! transport failure or an indeterminate status: a transient network blip must
//! surface but must not abort a release that would otherwise succeed.

use std::time::Duration;

use anodizer_core::PreflightCheck;
use anodizer_core::context::Context;
use anodizer_core::http::blocking_client;
use anodizer_core::redact::redact_bearer_tokens;

use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, http_status, retry_http_blocking};

/// Per-probe HTTP timeout. Generous enough to tolerate a cold TLS handshake to
/// crates.io / npm / the GitHub API, short enough that a wedged endpoint cannot
/// stall the pre-publish gate indefinitely.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Combine two outcomes keeping the most severe: `Blocker` > `Warning` >
/// `Pass`. The first-seen message wins within a severity so the operator sees
/// a stable, deterministic line rather than whichever target iterated last.
pub(crate) fn merge(acc: PreflightCheck, next: PreflightCheck) -> PreflightCheck {
    acc.merge(next)
}

/// Outcome of an authenticated token probe against a registry `whoami`.
pub(crate) enum TokenAuth {
    /// The registry accepted the credential (2xx).
    Valid,
    /// The registry rejected the credential (401/403) — a hard prerequisite
    /// the publisher cannot satisfy at publish time.
    Invalid,
    /// The probe could not reach a verdict (transport failure, 5xx, or an
    /// unexpected status). Carries a redacted reason for the warn line.
    Indeterminate(String),
}

/// Probe an authenticated `whoami`-style endpoint to prove `authorization` is
/// accepted by the registry.
///
/// * 2xx ⇒ [`TokenAuth::Valid`]
/// * 401 / 403 ⇒ [`TokenAuth::Invalid`]
/// * anything else (transport error, 5xx, unexpected status) ⇒
///   [`TokenAuth::Indeterminate`]
///
/// `authorization` is the full `Authorization` header value (callers supply
/// `Bearer <token>` for npm, the raw token for crates.io) so the probe stays
/// agnostic to each registry's auth scheme. `url` is passed in full so a unit
/// test can point the probe at a local responder without a network round-trip.
pub(crate) fn probe_token_auth(
    url: &str,
    authorization: &str,
    label: &str,
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> TokenAuth {
    let client = match blocking_client(PROBE_TIMEOUT) {
        Ok(c) => c,
        Err(e) => return TokenAuth::Indeterminate(format!("could not build HTTP client: {e}")),
    };
    let auth = authorization.to_string();
    let result = retry_http_blocking(
        RetryLog::new(label, log),
        policy,
        SuccessClass::Strict,
        |_| {
            client
                .get(url)
                .header("Authorization", &auth)
                .header("Accept", "application/json")
                .send()
        },
        |status, body| format!("{status}: {}", redact_bearer_tokens(body)),
    );
    match result {
        Ok(_) => TokenAuth::Valid,
        Err(err) => match http_status(&err) {
            401 | 403 => TokenAuth::Invalid,
            0 => TokenAuth::Indeterminate(format!("network failure: {err}")),
            other => TokenAuth::Indeterminate(format!("unexpected HTTP {other}")),
        },
    }
}

/// Whether a registry resource exists (HTTP 200) at `url`.
///
/// Used for the npm duplicate-version warning: an existing `<registry>/<pkg>/
/// <version>` means the publish will be rejected. Any non-2xx (404 = absent,
/// transport error, 5xx) returns `false` — the duplicate warning is
/// best-effort and must never fabricate a false positive from a network blip.
pub(crate) fn probe_version_published(
    url: &str,
    label: &str,
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> bool {
    let client = match blocking_client(PROBE_TIMEOUT) {
        Ok(c) => c,
        Err(_) => return false,
    };
    retry_http_blocking(
        RetryLog::new(label, log),
        policy,
        SuccessClass::Strict,
        |_| client.get(url).send(),
        |status, body| format!("{status}: {}", redact_bearer_tokens(body)),
    )
    .is_ok()
}

/// HTTP verb for a [`classify_http_endpoint`] reachability check. `Get` for a
/// health/whoami/repo GET; `PostJson` for an endpoint (Docker Hub `users/login`)
/// whose credentials travel in a JSON body rather than a header.
pub(crate) enum ProbeMethod {
    Get,
    PostJson(serde_json::Value),
}

/// Credential to attach to a [`classify_http_endpoint`] request. Each publisher
/// supplies the scheme its own publish path uses so the probe authenticates
/// identically to the real upload.
pub(crate) enum ProbeAuth {
    /// No credential — anonymous reachability only (Docker Hub login carries its
    /// credentials in the request body instead).
    None,
    /// `Authorization: Token <token>` (Cloudsmith API scheme).
    Token(String),
    /// HTTP Basic — username + password (generic uploads / Artifactory) or
    /// Gemfury's token-as-username, empty-password scheme.
    Basic { username: String, password: String },
}

/// Severity a *definitive* probe failure maps to. A REQUIRED publisher passes
/// [`FailSeverity::Blocker`] (a proven-unpublishable endpoint must abort before
/// the tag/one-way doors); an OPTIONAL publisher passes
/// [`FailSeverity::Warning`] (surface loudly but don't fail the whole release
/// for an optional surface).
///
/// Only the *definitive* failures (credentials rejected, endpoint unreachable,
/// resource missing) honour this severity; an *indeterminate* result (5xx or an
/// unexpected status — endpoint reachable but not answering cleanly) always
/// degrades to [`PreflightCheck::Warning`] regardless, so a transient upstream
/// hiccup never aborts a release whose credentials are actually valid.
#[derive(Clone, Copy)]
pub(crate) enum FailSeverity {
    Blocker,
    Warning,
}

impl FailSeverity {
    /// A REQUIRED publisher's probe failure must abort before the one-way doors
    /// ([`FailSeverity::Blocker`]); an OPTIONAL one's must surface but not abort
    /// ([`FailSeverity::Warning`]). Derived from the publisher's own
    /// [`required()`](anodizer_core::Publisher::required) so preflight severity
    /// can never be stricter than the publish gate it precedes.
    pub(crate) fn for_required(required: bool) -> Self {
        if required {
            FailSeverity::Blocker
        } else {
            FailSeverity::Warning
        }
    }

    /// Wrap `msg` in the [`PreflightCheck`] severity this maps to. Lets a
    /// publisher with a bespoke probe (chocolatey's `ChocoKeyProbe`) route its
    /// definitive failures through the same required→Blocker / optional→Warning
    /// policy the shared HTTP probes use.
    pub(crate) fn apply(self, msg: String) -> PreflightCheck {
        match self {
            FailSeverity::Blocker => PreflightCheck::Blocker(msg),
            FailSeverity::Warning => PreflightCheck::Warning(msg),
        }
    }
}

/// Build the blocking HTTP client used by the credential-less probes
/// (cloudsmith / gemfury / dockerhub). The mTLS-capable publishers
/// (uploads / artifactory) build their own client via `build_reqwest_client`
/// and pass it to [`probe_http_endpoint`] directly.
pub(crate) fn default_probe_client() -> anyhow::Result<reqwest::blocking::Client> {
    blocking_client(PROBE_TIMEOUT)
}

/// Terminal classification of a single [`classify_http_endpoint`] probe.
pub(crate) enum EndpointStatus {
    /// 2xx / 3xx — host reachable and (if a credential was sent) accepted.
    Reachable,
    /// 401 / 403 — the credential was rejected.
    AuthRejected,
    /// 404 — the probed resource does not exist. For a *resource* probe this is
    /// a failure; for a bare base-URL *reachability* probe it still proves the
    /// host is up (the caller decides — see [`probe_http_endpoint`] vs the
    /// uploads publisher).
    NotFound,
    /// Transport failure (connection refused / DNS / TLS) after the retry policy
    /// is exhausted — the endpoint is unreachable.
    Unreachable(String),
    /// 5xx or an unexpected status — host reachable but not answering cleanly;
    /// verdict indeterminate.
    Indeterminate(String),
}

/// Issue ONE authenticated request against `url` under `policy` and classify the
/// outcome into an [`EndpointStatus`]. `client` is supplied by the caller so an
/// mTLS / custom-CA publisher probes through the same client its publish path
/// uses; `url` is passed in full so a unit test can point at a local responder.
pub(crate) fn classify_http_endpoint(
    client: &reqwest::blocking::Client,
    method: ProbeMethod,
    url: &str,
    auth: &ProbeAuth,
    label: &str,
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> EndpointStatus {
    let result = retry_http_blocking(
        RetryLog::new(label, log),
        policy,
        // A base-URL HEAD/GET may legitimately 301/302 to a canonical path;
        // a redirect proves reachability, not an auth failure.
        SuccessClass::AllowRedirects,
        |_| {
            let mut req = match &method {
                ProbeMethod::Get => client.get(url),
                ProbeMethod::PostJson(body) => client.post(url).json(body),
            };
            req = req.header("Accept", "application/json");
            req = match auth {
                ProbeAuth::None => req,
                ProbeAuth::Token(t) => req.header("Authorization", format!("Token {t}")),
                ProbeAuth::Basic { username, password } => req.basic_auth(username, Some(password)),
            };
            req.send()
        },
        |status, body| format!("{status}: {}", redact_bearer_tokens(body)),
    );
    match result {
        Ok(_) => EndpointStatus::Reachable,
        Err(err) => match http_status(&err) {
            401 | 403 => EndpointStatus::AuthRejected,
            404 => EndpointStatus::NotFound,
            0 => EndpointStatus::Unreachable(format!("{err}")),
            other => EndpointStatus::Indeterminate(format!("HTTP {other}")),
        },
    }
}

/// Probe a *resource* endpoint (a health/whoami/repo URL the publish path
/// expects to exist) and map the outcome to a [`PreflightCheck`]:
///
/// * reachable ⇒ [`PreflightCheck::Pass`].
/// * credential rejected (401/403) ⇒ `fail` severity — the publish would fail
///   with the same auth error.
/// * resource missing (404) ⇒ `fail` severity — the target repo/endpoint does
///   not exist.
/// * unreachable (connection refused / DNS / TLS) ⇒ `fail` severity — the exact
///   failure mode a no-op preflight let slip past the one-way doors.
/// * indeterminate (5xx / unexpected) ⇒ [`PreflightCheck::Warning`] — likely
///   transient, must not abort a release whose credentials are actually valid.
///
/// A bare base-URL reachability probe (whose root path legitimately 404s on a
/// healthy host) must NOT use this — it should call [`classify_http_endpoint`]
/// directly and treat [`EndpointStatus::NotFound`] as reachable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn probe_http_endpoint(
    client: &reqwest::blocking::Client,
    method: ProbeMethod,
    url: &str,
    auth: &ProbeAuth,
    label: &str,
    fail: FailSeverity,
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> PreflightCheck {
    match classify_http_endpoint(client, method, url, auth, label, policy, log) {
        EndpointStatus::Reachable => PreflightCheck::Pass,
        EndpointStatus::AuthRejected => fail.apply(format!(
            "{label}: endpoint {url} rejected the configured credentials (HTTP 401/403); \
             the publish would fail with the same auth error"
        )),
        EndpointStatus::NotFound => {
            fail.apply(format!("{label}: endpoint {url} not found (HTTP 404)"))
        }
        EndpointStatus::Unreachable(e) => {
            fail.apply(format!("{label}: endpoint {url} unreachable ({e})"))
        }
        EndpointStatus::Indeterminate(e) => PreflightCheck::Warning(format!(
            "{label}: could not verify {url} ({e}); verify the endpoint manually"
        )),
    }
}

/// Map a *reachability* probe against a bare base URL (or an endpoint that
/// legitimately 404s on a healthy host, e.g. a not-yet-published Gemfury
/// version) to a [`PreflightCheck`]. Only two outcomes are actionable:
///
/// * credential rejected (401/403) ⇒ `fail` severity.
/// * host unreachable (connection refused / DNS / TLS) ⇒ `fail` severity — the
///   failure mode a no-op preflight let slip past the one-way doors.
///
/// A 2xx/3xx, a 404 (host up, resource simply absent), and any 5xx (degraded to
/// a Warning) all prove the host is reachable, so the probe must not abort on
/// them. Use this — not [`probe_http_endpoint`] — whenever a 404 does NOT mean a
/// misconfigured target.
pub(crate) fn reachability_outcome(
    status: EndpointStatus,
    url: &str,
    label: &str,
    fail: FailSeverity,
) -> PreflightCheck {
    match status {
        EndpointStatus::Reachable | EndpointStatus::NotFound => PreflightCheck::Pass,
        EndpointStatus::AuthRejected => fail.apply(format!(
            "{label}: endpoint {url} rejected the configured credentials (HTTP 401/403); \
             the publish would fail with the same auth error"
        )),
        EndpointStatus::Unreachable(e) => fail.apply(format!(
            "{label}: endpoint {url} unreachable ({e}); the publish would fail to connect"
        )),
        EndpointStatus::Indeterminate(e) => PreflightCheck::Warning(format!(
            "{label}: could not verify {url} ({e}); verify the endpoint manually"
        )),
    }
}

/// Probe `GET {api_base}/repos/{owner}/{repo}` to prove the target
/// index/fork repo exists and `token` can push to it, resolving the base
/// through [`anodizer_core::http::github_api_base`] — the same resolver the
/// publish path's PR/branch lookups use, so one override redirects the whole
/// run. See [`github_repo_check_at`] for the outcome mapping.
pub(crate) fn github_repo_check<E: anodizer_core::EnvSource + ?Sized>(
    owner: &str,
    repo: &str,
    token: Option<&str>,
    policy: &RetryPolicy,
    env: &E,
    log: &anodizer_core::log::StageLogger,
) -> PreflightCheck {
    let base = anodizer_core::http::github_api_base(env);
    let url = format!("{base}/repos/{owner}/{repo}");
    github_repo_check_at(&url, owner, repo, token, policy, log)
}

/// `url`-taking core of [`github_repo_check`] so a unit test can drive the
/// status/permission mapping against a local responder.
///
/// * 404 ⇒ [`PreflightCheck::Blocker`] (repo missing under a good token)
/// * 401 / 403 without a rate-limit signal ⇒ [`PreflightCheck::Blocker`]
///   (the token cannot read the repo)
/// * 429, or 401 / 403 carrying a `Retry-After` / `X-RateLimit-Remaining: 0`
///   header ⇒ [`PreflightCheck::Warning`] (a transient GitHub rate limit must
///   not abort a release that would otherwise succeed)
/// * 200 with `permissions.push == false` ⇒ [`PreflightCheck::Warning`]
/// * 200 with `permissions` absent (unauthenticated read) ⇒
///   [`PreflightCheck::Warning`] (push scope undeterminable)
/// * 200 with `permissions.push == true` ⇒ [`PreflightCheck::Pass`]
/// * transport failure / other status ⇒ [`PreflightCheck::Warning`]
pub(crate) fn github_repo_check_at(
    url: &str,
    owner: &str,
    repo: &str,
    token: Option<&str>,
    policy: &RetryPolicy,
    log: &anodizer_core::log::StageLogger,
) -> PreflightCheck {
    anodizer_core::git::github_repo_push_check(
        url,
        owner,
        repo,
        token,
        policy,
        anodizer_core::git::RepoAccessOutcomes {
            // A tap/index the token cannot push to only degrades this one
            // publisher, so warn rather than block the whole release.
            push_denied: PreflightCheck::Warning(format!(
                "token cannot push to {owner}/{repo}; the publish PR/commit will fail"
            )),
            missing_or_denied: PreflightCheck::Blocker(format!(
                "index/fork repo {owner}/{repo} not found or token lacks read access"
            )),
        },
        log,
    )
}

/// Resolve a publisher's repository config to owner/name + token and run
/// [`github_repo_check`].
///
/// Returns [`PreflightCheck::Pass`] (silent) when the repo's owner/name are not
/// both set: an absent target is config-validation territory, and the run path
/// already fails loud on it — the preflight must not manufacture a duplicate
/// blocker for a config error caught elsewhere. owner/name are rendered through
/// the same template engine the publish path uses so `{{ .Env.X }}`-templated
/// coordinates probe their resolved value.
pub(crate) fn github_repo_config_check(
    ctx: &Context,
    repo: Option<&anodizer_core::config::RepositoryConfig>,
    preferred_env: &str,
    policy: &RetryPolicy,
) -> PreflightCheck {
    // A `git.url` override routes the push over SSH / to a self-hosted GHE
    // host, not api.github.com. Probing github.com for those coordinates would
    // false-404 a repo that lives elsewhere, and an SSH-key push is not what a
    // REST-token read probe measures. Defer to the publish path's own checks.
    if repo
        .and_then(|r| r.git.as_ref())
        .and_then(|g| g.url.as_deref())
        .is_some_and(|u| !u.trim().is_empty())
    {
        return PreflightCheck::Pass;
    }
    let Some((owner_raw, name_raw)) = crate::util::resolve_repo_owner_name(repo) else {
        return PreflightCheck::Pass;
    };
    let owner = ctx.render_template(&owner_raw).unwrap_or(owner_raw);
    let name = ctx.render_template(&name_raw).unwrap_or(name_raw);
    if owner.trim().is_empty() || name.trim().is_empty() {
        return PreflightCheck::Pass;
    }
    let token = crate::util::resolve_repo_token(ctx, repo, Some(preferred_env));
    github_repo_check(
        &owner,
        &name,
        token.as_deref(),
        policy,
        ctx.env_source(),
        &ctx.logger("preflight"),
    )
}

/// Run [`github_repo_config_check`] over every active entry of a
/// GitHub-repo-backed publisher and merge the per-entry outcomes into a
/// single [`PreflightCheck`] (worst severity wins).
///
/// `entries` is the publisher's pre-selected entry stream (each publisher
/// supplies its own crate-universe / flat-list projection so the iterated
/// universe — crate-keyed configs vs. top-level casks — stays at the call
/// site). `is_active` is the per-entry gate; it is caller-supplied because
/// the publisher schemas disagree on whether a `skip` field exists, so no
/// single predicate fits every caller. `repo` extracts the entry's
/// repository coordinates. Inactive entries are skipped without probing.
pub(crate) fn for_each_active_github_repo<'e, E, G, R>(
    ctx: &Context,
    policy: &RetryPolicy,
    token_env: &str,
    entries: impl Iterator<Item = E>,
    is_active: G,
    repo: R,
) -> PreflightCheck
where
    E: Copy,
    G: Fn(E) -> bool,
    R: Fn(E) -> Option<&'e anodizer_core::config::RepositoryConfig>,
{
    fold_active_checks(entries, is_active, |entry| {
        github_repo_config_check(ctx, repo(entry), token_env, policy)
    })
}

/// Iterate `entries`, run `check` on each active entry, and merge the
/// outcomes (worst severity wins). The IO-bound check is injected so the
/// gate/merge orchestration can be unit-tested without a live probe.
fn fold_active_checks<E, G, C>(
    entries: impl Iterator<Item = E>,
    is_active: G,
    check: C,
) -> PreflightCheck
where
    E: Copy,
    G: Fn(E) -> bool,
    C: Fn(E) -> PreflightCheck,
{
    let mut acc = PreflightCheck::Pass;
    for entry in entries {
        if is_active(entry) {
            acc = acc.merge(check(entry));
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        }
    }

    fn http(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn fold_active_checks_merges_active_entries_to_worst_severity() {
        // Two active entries; the second's check fails (Blocker). The merge
        // must surface the worst severity across the active universe.
        let entries = [
            (true, PreflightCheck::Warning("w".into())),
            (true, PreflightCheck::Blocker("missing repo".into())),
        ];
        let out = fold_active_checks(
            entries.iter(),
            |e: &(bool, PreflightCheck)| e.0,
            |e: &(bool, PreflightCheck)| e.1.clone(),
        );
        assert!(matches!(out, PreflightCheck::Blocker(_)));
    }

    #[test]
    fn fold_active_checks_skips_inactive_entries() {
        // The only Blocker lives on an inactive entry; the gate must skip it so
        // it never reaches the merge — result stays Pass.
        let entries = [
            (true, PreflightCheck::Pass),
            (false, PreflightCheck::Blocker("must-not-count".into())),
        ];
        let out = fold_active_checks(
            entries.iter(),
            |e: &(bool, PreflightCheck)| e.0,
            |e: &(bool, PreflightCheck)| e.1.clone(),
        );
        assert!(matches!(out, PreflightCheck::Pass));
    }

    #[test]
    fn merge_keeps_most_severe() {
        let b = PreflightCheck::Blocker("b".into());
        let w = PreflightCheck::Warning("w".into());
        assert!(matches!(
            merge(PreflightCheck::Pass, w.clone()),
            PreflightCheck::Warning(_)
        ));
        assert!(matches!(
            merge(w.clone(), b.clone()),
            PreflightCheck::Blocker(_)
        ));
        assert!(matches!(merge(b, w), PreflightCheck::Blocker(_)));
        assert!(matches!(
            merge(PreflightCheck::Pass, PreflightCheck::Pass),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn token_auth_valid_on_200() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"username":"me"}"#).into_boxed_str(),
        )]);
        let url = format!("http://{addr}/-/whoami");
        assert!(matches!(
            probe_token_auth(
                &url,
                "Bearer t",
                "test",
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            TokenAuth::Valid
        ));
    }

    #[test]
    fn token_auth_invalid_on_401() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("401 Unauthorized", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/-/whoami");
        assert!(matches!(
            probe_token_auth(
                &url,
                "Bearer bad",
                "test",
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            TokenAuth::Invalid
        ));
    }

    #[test]
    fn token_auth_invalid_on_403() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("403 Forbidden", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/me");
        assert!(matches!(
            probe_token_auth(
                &url,
                "raw-token",
                "test",
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            TokenAuth::Invalid
        ));
    }

    #[test]
    fn token_auth_indeterminate_on_network_error() {
        // Bind then drop to obtain a closed port → connection refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let url = format!("http://{addr}/-/whoami");
        assert!(matches!(
            probe_token_auth(
                &url,
                "Bearer t",
                "test",
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            TokenAuth::Indeterminate(_)
        ));
    }

    #[test]
    fn version_published_true_on_200() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"version":"1.0.0"}"#).into_boxed_str(),
        )]);
        let url = format!("http://{addr}/pkg/1.0.0");
        assert!(probe_version_published(
            &url,
            "test",
            &fast_retry(),
            anodizer_core::test_helpers::test_logger()
        ));
    }

    #[test]
    fn version_published_false_on_404() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("404 Not Found", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/pkg/9.9.9");
        assert!(!probe_version_published(
            &url,
            "test",
            &fast_retry(),
            anodizer_core::test_helpers::test_logger()
        ));
    }

    #[test]
    fn github_repo_pass_when_push_true() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"permissions":{"push":true}}"#).into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(
                &url,
                "o",
                "r",
                Some("tok"),
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn github_repo_check_resolves_base_from_env_override() {
        // The probe must route through the shared github_api_base resolver:
        // the env override redirects it to the local responder instead of
        // the hardcoded public host.
        let (addr, calls) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"permissions":{"push":true}}"#).into_boxed_str(),
        )]);
        let env = anodizer_core::MapEnvSource::new()
            .with("ANODIZER_GITHUB_API_BASE", format!("http://{addr}"));
        assert!(matches!(
            github_repo_check(
                "o",
                "r",
                Some("tok"),
                &fast_retry(),
                &env,
                anodizer_core::test_helpers::test_logger()
            ),
            PreflightCheck::Pass
        ));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "probe must hit the env-resolved base"
        );
    }

    #[test]
    fn github_repo_warns_when_push_false() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"permissions":{"push":false}}"#).into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        match github_repo_check_at(
            &url,
            "o",
            "r",
            Some("tok"),
            &fast_retry(),
            anodizer_core::test_helpers::test_logger(),
        ) {
            PreflightCheck::Warning(m) => assert!(m.contains("cannot push"), "{m}"),
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn github_repo_warns_when_permissions_absent() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"full_name":"o/r"}"#).into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(
                &url,
                "o",
                "r",
                None,
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            PreflightCheck::Warning(_)
        ));
    }

    #[test]
    fn github_repo_blocks_on_404() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("404 Not Found", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/missing");
        match github_repo_check_at(
            &url,
            "o",
            "missing",
            Some("tok"),
            &fast_retry(),
            anodizer_core::test_helpers::test_logger(),
        ) {
            PreflightCheck::Blocker(m) => assert!(m.contains("not found"), "{m}"),
            other => panic!("expected Blocker, got {other:?}"),
        }
    }

    #[test]
    fn github_repo_blocks_on_403() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("403 Forbidden", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(
                &url,
                "o",
                "r",
                Some("tok"),
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            PreflightCheck::Blocker(_)
        ));
    }

    /// A raw HTTP response with one extra header line beyond `Content-Length`,
    /// for exercising the rate-limit header inspection.
    fn http_with_header(status_line: &str, header: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\n{header}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn github_repo_warns_on_rate_limited_403() {
        // A secondary-rate-limit 403 carries `X-RateLimit-Remaining: 0`; it is
        // transient and must NOT block a release whose token is actually valid.
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http_with_header("403 Forbidden", "X-RateLimit-Remaining: 0", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(
            matches!(
                github_repo_check_at(
                    &url,
                    "o",
                    "r",
                    Some("tok"),
                    &fast_retry(),
                    anodizer_core::test_helpers::test_logger()
                ),
                PreflightCheck::Warning(_)
            ),
            "rate-limited 403 must degrade to Warning, not Blocker"
        );
    }

    #[test]
    fn github_repo_warns_on_retry_after_403() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http_with_header("403 Forbidden", "Retry-After: 60", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(
                &url,
                "o",
                "r",
                Some("tok"),
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            PreflightCheck::Warning(_)
        ));
    }

    #[test]
    fn github_repo_warns_on_429() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("429 Too Many Requests", "").into_boxed_str(),
        )]);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(
                &url,
                "o",
                "r",
                Some("tok"),
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            PreflightCheck::Warning(_)
        ));
    }

    #[test]
    fn github_repo_config_check_skips_probe_for_ssh_git_url() {
        // A `git.url` SSH/GHE override pushes elsewhere than api.github.com;
        // the probe must short-circuit to Pass WITHOUT a network round-trip
        // (a bound-then-dropped port would surface as a Warning, not Pass, if
        // the probe ran).
        use anodizer_core::config::{GitRepoConfig, RepositoryConfig};
        let repo = RepositoryConfig {
            owner: Some("o".into()),
            name: Some("r".into()),
            git: Some(GitRepoConfig {
                url: Some("ssh://git@ghe.corp.example/o/r.git".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let ctx = anodizer_core::context::Context::test_fixture();
        assert!(matches!(
            github_repo_config_check(&ctx, Some(&repo), "GITHUB_TOKEN", &fast_retry()),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn github_repo_warns_on_network_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let url = format!("http://{addr}/repos/o/r");
        assert!(matches!(
            github_repo_check_at(
                &url,
                "o",
                "r",
                Some("tok"),
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            PreflightCheck::Warning(_)
        ));
    }

    #[test]
    fn classify_reachable_on_200() {
        let (addr, _c) =
            spawn_oneshot_http_responder(vec![Box::leak(http("200 OK", "ok").into_boxed_str())]);
        let client = default_probe_client().expect("client");
        let url = format!("http://{addr}/health");
        assert!(matches!(
            classify_http_endpoint(
                &client,
                ProbeMethod::Get,
                &url,
                &ProbeAuth::Token("t".into()),
                "test",
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            EndpointStatus::Reachable
        ));
    }

    #[test]
    fn classify_auth_rejected_on_401() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("401 Unauthorized", "").into_boxed_str(),
        )]);
        let client = default_probe_client().expect("client");
        let url = format!("http://{addr}/health");
        assert!(matches!(
            classify_http_endpoint(
                &client,
                ProbeMethod::Get,
                &url,
                &ProbeAuth::Basic {
                    username: "bad".into(),
                    password: String::new()
                },
                "test",
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            EndpointStatus::AuthRejected
        ));
    }

    #[test]
    fn classify_not_found_on_404() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("404 Not Found", "").into_boxed_str(),
        )]);
        let client = default_probe_client().expect("client");
        let url = format!("http://{addr}/missing");
        assert!(matches!(
            classify_http_endpoint(
                &client,
                ProbeMethod::Get,
                &url,
                &ProbeAuth::None,
                "test",
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            EndpointStatus::NotFound
        ));
    }

    #[test]
    fn classify_unreachable_on_closed_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let client = default_probe_client().expect("client");
        let url = format!("http://{addr}/health");
        assert!(matches!(
            classify_http_endpoint(
                &client,
                ProbeMethod::Get,
                &url,
                &ProbeAuth::None,
                "test",
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            EndpointStatus::Unreachable(_)
        ));
    }

    #[test]
    fn probe_http_endpoint_blocks_on_404_when_required() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("404 Not Found", "").into_boxed_str(),
        )]);
        let client = default_probe_client().expect("client");
        let url = format!("http://{addr}/repo");
        match probe_http_endpoint(
            &client,
            ProbeMethod::Get,
            &url,
            &ProbeAuth::Token("t".into()),
            "test",
            FailSeverity::Blocker,
            &fast_retry(),
            anodizer_core::test_helpers::test_logger(),
        ) {
            PreflightCheck::Blocker(m) => assert!(m.contains("not found"), "{m}"),
            other => panic!("expected Blocker, got {other:?}"),
        }
    }

    #[test]
    fn probe_http_endpoint_warns_on_404_when_optional() {
        let (addr, _c) = spawn_oneshot_http_responder(vec![Box::leak(
            http("404 Not Found", "").into_boxed_str(),
        )]);
        let client = default_probe_client().expect("client");
        let url = format!("http://{addr}/repo");
        assert!(matches!(
            probe_http_endpoint(
                &client,
                ProbeMethod::Get,
                &url,
                &ProbeAuth::Token("t".into()),
                "test",
                FailSeverity::Warning,
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            PreflightCheck::Warning(_)
        ));
    }

    #[test]
    fn reachability_outcome_passes_on_not_found() {
        // A bare base-URL / not-yet-published-version 404 still proves the host
        // is reachable — never a failure for a reachability probe.
        assert!(matches!(
            reachability_outcome(
                EndpointStatus::NotFound,
                "http://x/y",
                "test",
                FailSeverity::Blocker
            ),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn reachability_outcome_blocks_on_auth_when_required() {
        match reachability_outcome(
            EndpointStatus::AuthRejected,
            "http://x/y",
            "test",
            FailSeverity::Blocker,
        ) {
            PreflightCheck::Blocker(m) => assert!(m.contains("rejected"), "{m}"),
            other => panic!("expected Blocker, got {other:?}"),
        }
    }

    #[test]
    fn probe_post_json_reaches_login_endpoint() {
        let (addr, calls) = spawn_oneshot_http_responder(vec![Box::leak(
            http("200 OK", r#"{"token":"jwt"}"#).into_boxed_str(),
        )]);
        let client = default_probe_client().expect("client");
        let url = format!("http://{addr}/v2/users/login/");
        let body = serde_json::json!({ "username": "u", "password": "p" });
        assert!(matches!(
            probe_http_endpoint(
                &client,
                ProbeMethod::PostJson(body),
                &url,
                &ProbeAuth::None,
                "test",
                FailSeverity::Warning,
                &fast_retry(),
                anodizer_core::test_helpers::test_logger()
            ),
            PreflightCheck::Pass
        ));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
