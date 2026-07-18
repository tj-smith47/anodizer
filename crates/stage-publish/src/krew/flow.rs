use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
use serde::Serialize;

// ---------------------------------------------------------------------------
// krew-release-bot mode selection
// ---------------------------------------------------------------------------
//
// A plugin's first appearance in `kubernetes-sigs/krew-index` requires a
// human-reviewed PR; subsequent version bumps are mechanical. The krew
// maintainers run a hosted webhook (`krew-release-bot`) that performs the
// fork + version-bump PR server-side, under the bot's own GitHub account,
// for any plugin already in the index. anodizer drives that webhook
// directly so a release is self-contained — no separate GitHub-Actions
// workflow step is required.
//
// In `auto` mode the deciding signal is whether the plugin already
// exists in krew-index:
//   - Plugin NOT in index → `PrDirect`: anodizer clones a fork, writes
//     `plugins/<name>.yaml`, commits, and opens the initial PR against
//     `kubernetes-sigs/krew-index`. A human reviews + merges it.
//   - Plugin IS in index → `BotWebhook`: anodizer POSTs a `ReleaseRequest`
//     (the fully-rendered manifest plus the release tag) to the hosted
//     webhook, which opens the version-bump PR on the plugin's behalf.
//     No fork, no token, no workflow.
//
// The membership probe is a GET against the GitHub contents API:
//   `api.github.com/repos/kubernetes-sigs/krew-index/contents/plugins/<name>.yaml`
// → 200 means published; 404 means not yet. Any other status
// (rate-limit, 5xx) is indeterminate: `auto` mode then HARD-ERRORS
// rather than guessing, because a transient blip must never route a
// plugin already in the index into a fork PR (krew maintainers reject
// mechanical version bumps submitted as fork PRs). The probe is
// authenticated whenever a token is in context — the same token used
// for the GitHub release — which raises the rate limit from 60/hr (anon)
// to 5,000/hr and eliminates almost all indeterminate results. Set the
// krew `mode` config field to `bot` or `pr-direct` to skip the probe
// entirely.

/// The two flows the krew publisher dispatches between, after the
/// user-facing [`KrewMode`](anodizer_core::config::KrewMode) config knob
/// (and, in `auto`, the membership probe) have been resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum KrewFlow {
    /// Initial-submission flow — plugin isn't in krew-index yet.
    /// Behaviour: clone fork, write `plugins/<name>.yaml`, commit, PR
    /// against `kubernetes-sigs/krew-index`.
    PrDirect,
    /// Version-update flow — plugin IS in krew-index. Behaviour: POST a
    /// `ReleaseRequest` to the hosted krew-release-bot webhook, which
    /// opens the krew-index PR server-side. Self-contained: no fork, no
    /// token, no GitHub-Actions workflow step.
    BotWebhook,
}

/// Resolve the krew submission flow from the configured `mode` and (in
/// `auto`) a krew-index membership probe.
///
/// - `Bot` → [`KrewFlow::BotWebhook`] (probe skipped).
/// - `PrDirect` → [`KrewFlow::PrDirect`] (probe skipped).
/// - `Auto` → probe membership: definitively in-index →
///   `BotWebhook`; definitively absent → `PrDirect`; indeterminate
///   (rate-limit / network / unexpected status) → `Err`, so the caller
///   fails loudly instead of guessing the maintainer-hostile path.
///
/// `token` is the GitHub token resolved from the krew repository config
/// (else the release token); passing it authenticates the probe.
pub(super) fn detect_krew_flow(
    mode: anodizer_core::config::KrewMode,
    plugin_name: &str,
    token: Option<&str>,
) -> Result<KrewFlow> {
    use anodizer_core::config::KrewMode;
    match mode {
        KrewMode::Bot => Ok(KrewFlow::BotWebhook),
        KrewMode::PrDirect => Ok(KrewFlow::PrDirect),
        KrewMode::Auto => map_auto_probe(plugin_name, is_plugin_in_krew_index(plugin_name, token)),
    }
}

/// Pure dispatch for `auto` mode from a membership-probe result.
/// `Some(true)` → webhook flow; `Some(false)` → fork PR; `None`
/// (indeterminate) → loud error with an actionable hint, never a silent
/// fallback into the maintainer-hostile fork-PR path.
pub(super) fn map_auto_probe(plugin_name: &str, in_index: Option<bool>) -> Result<KrewFlow> {
    match in_index {
        Some(true) => Ok(KrewFlow::BotWebhook),
        Some(false) => Ok(KrewFlow::PrDirect),
        None => anyhow::bail!(
            "krew: could not determine krew-index membership for plugin '{}' \
             (the contents-API probe failed — likely a rate-limit or network \
             error). Refusing to guess: an existing plugin wrongly routed to a \
             fork PR is rejected by krew maintainers. Retry the release, ensure \
             a GitHub token is available ({}) \
             to raise the API rate limit, or set the krew `mode` field \
             explicitly to `bot` or `pr-direct`.",
            plugin_name,
            anodizer_core::git::GITHUB_TOKEN_ENV_LADDER.join(" / ")
        ),
    }
}

/// HTTP probe: does `kubernetes-sigs/krew-index/plugins/<name>.yaml` exist?
/// Returns:
///   - `Some(true)` → 200 OK, the plugin is published.
///   - `Some(false)` → 404 Not Found, the plugin is not yet published.
///   - `None` → network error, rate-limit, or unexpected status. Caller
///     treats this as indeterminate and (in `auto` mode) hard-errors.
///
/// `token` is optional — anodizer's GitHub PATs are scoped enough that
/// passing one raises the rate limit from 60/hr (anon) to 5,000/hr
/// (authenticated). The caller passes the release token so the probe is
/// authenticated in CI, which is what makes the `None`→hard-error path
/// rare in practice.
fn is_plugin_in_krew_index(plugin_name: &str, token: Option<&str>) -> Option<bool> {
    // Deliberately NOT routed through `core::http::github_api_base`: the
    // upstream kubernetes-sigs/krew-index lives on public github.com
    // regardless of the user's forge configuration or API-base override.
    let url = format!(
        "https://api.github.com/repos/kubernetes-sigs/krew-index/contents/plugins/{}.yaml",
        plugin_name
    );
    let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(10)).ok()?;
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    if let Some(tok) = token {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().ok()?;
    let status = resp.status();
    if status.is_success() {
        return Some(true);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Some(false);
    }
    // 403 (rate limited / token denied), 5xx (GitHub flaking) surface as
    // `None` (indeterminate). The probe runs only in `auto` mode, where an
    // indeterminate result is a hard error rather than a guess — an existing
    // plugin wrongly routed to a fork PR is rejected by krew maintainers.
    // Explicit `bot` / `pr-direct` modes never reach this probe.
    None
}

// ---------------------------------------------------------------------------
// krew-release-bot webhook submission
// ---------------------------------------------------------------------------

/// Default hosted krew-release-bot webhook endpoint. The bot forks
/// krew-index and opens the version-bump PR server-side under its own
/// GitHub account, so anodizer sends no token.
pub(super) const DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL: &str =
    "https://krew-release-bot.rajatjindal.com/github-action-webhook";

/// Resolve the effective webhook URL: the `KREW_RELEASE_BOT_WEBHOOK_URL`
/// env var (trimmed, empty treated as unset) else
/// [`DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL`]. Mirrors the bot client's own
/// `getWebhookURL()` precedence so a self-hosted deployment is reachable
/// the same way.
pub(super) fn resolve_webhook_url(env: &dyn anodizer_core::env_source::EnvSource) -> String {
    env.var("KREW_RELEASE_BOT_WEBHOOK_URL")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL.to_string())
}

/// The JSON body POSTed to the krew-release-bot webhook.
///
/// Field names and shapes mirror the bot's server-side `ReleaseRequest`
/// struct: `processed_template` is the fully-rendered manifest bytes
/// (the server's Go decoder expects a base64 string for its `[]byte`
/// field, which serde produces from a `Vec<u8>` only via an explicit
/// encoder — handled at the call site). The server validates the
/// manifest and commits these bytes to its krew-index fork verbatim
/// (it does not fetch release assets or recompute shas), so
/// `processed_template` already carries the final sha256 digests.
#[derive(Debug, Serialize)]
pub(super) struct KrewReleaseRequest {
    #[serde(rename = "tagName")]
    tag_name: String,
    #[serde(rename = "pluginName")]
    plugin_name: String,
    #[serde(rename = "pluginOwner")]
    plugin_owner: String,
    #[serde(rename = "pluginRepo")]
    plugin_repo: String,
    #[serde(rename = "pluginReleaseActor")]
    plugin_release_actor: String,
    #[serde(rename = "templateFile")]
    template_file: String,
    /// Base64 of the rendered manifest bytes. The bot's `[]byte` JSON
    /// field decodes from a base64 string (Go's `encoding/json`
    /// convention), so the bytes are pre-encoded here.
    #[serde(rename = "processedTemplate")]
    processed_template: String,
}

impl KrewReleaseRequest {
    /// Build a `ReleaseRequest` from the resolved release coordinates and
    /// the fully-rendered manifest. `tag_name` is normalized to the
    /// `v<semver>` shape the krew-index manifest's `spec.version` carries.
    pub(super) fn new(
        tag_name: &str,
        plugin_name: &str,
        plugin_owner: &str,
        plugin_repo: &str,
        plugin_release_actor: &str,
        rendered_manifest: &str,
    ) -> Self {
        use base64::Engine as _;
        Self {
            tag_name: tag_name.to_string(),
            plugin_name: plugin_name.to_string(),
            plugin_owner: plugin_owner.to_string(),
            plugin_repo: plugin_repo.to_string(),
            plugin_release_actor: plugin_release_actor.to_string(),
            template_file: ".krew.yaml".to_string(),
            processed_template: base64::engine::general_purpose::STANDARD
                .encode(rendered_manifest.as_bytes()),
        }
    }
}

/// Whether a non-200 webhook response body indicates the version/PR is
/// already submitted (an idempotent re-run), versus a genuine failure.
///
/// The bot server returns HTTP 500 for every failure path, wrapping the
/// underlying error message in the response body (`opening pr: <err>`).
/// Only two of those failure messages are benign re-runs of work the
/// previous submission already did. First, a PR for the same fork branch
/// already exists: GitHub's create-PR call fails with a 422 whose
/// message contains `pull request already exists`. Second, the manifest
/// is unchanged, so the commit step finds a clean tree and reports
/// `nothing to commit` / `clean working tree`.
///
/// The match is deliberately narrow — only these exact server phrases —
/// so a future genuine server error (a validation failure, an auth
/// error, an unexpected 5xx) is NOT silently swallowed as "already
/// submitted". Silently skipping a one-way publish is the worst failure
/// mode, so anything outside these phrases falls through to a loud
/// error.
pub(super) fn webhook_body_is_already_submitted(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("pull request already exists")
        || lower.contains("nothing to commit")
        || lower.contains("clean working tree")
}

/// POST a [`KrewReleaseRequest`] to the krew-release-bot webhook and map
/// the response to a publish result.
///
/// A single attempt with a 30s timeout mirrors the bot's own client
/// (the PR-submit action runner). The retry helper is deliberately
/// NOT used here: the server returns HTTP 500 for every failure path,
/// including the benign "PR already exists" case, so a generic 5xx-retry
/// classifier would both burn the budget on an idempotent re-run and
/// flood the bot with duplicate submissions.
///
/// Outcome mapping:
///   - HTTP 200 → success; the body (`PR "<url>" submitted successfully`)
///     is logged.
///   - non-200 whose body matches [`webhook_body_is_already_submitted`] →
///     idempotent no-op success (the version was already submitted).
///   - any other non-200 / transport error → loud error. The release
///     must not silently skip krew.
pub(super) fn submit_krew_release_webhook(
    webhook_url: &str,
    request: &KrewReleaseRequest,
    plugin_name: &str,
    version: &str,
    log: &StageLogger,
) -> Result<()> {
    let client = anodizer_core::http::blocking_client(std::time::Duration::from_secs(30))
        .context("krew: build webhook HTTP client")?;
    let body = serde_json::to_string(request).context("krew: serialize ReleaseRequest")?;

    let resp = client
        .post(webhook_url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .with_context(|| format!("krew: POST to krew-release-bot webhook {}", webhook_url))?;

    let status = resp.status();
    let resp_body = anodizer_core::http::body_of_blocking(resp);

    if status.is_success() {
        log.status(&format!(
            "submitted krew plugin {} v{} via bot-webhook to {} ({})",
            plugin_name,
            version,
            webhook_url,
            resp_body.trim()
        ));
        return Ok(());
    }

    if webhook_body_is_already_submitted(&resp_body) {
        log.status(&format!(
            "krew plugin {} v{} already submitted upstream — treating as \
             idempotent no-op (webhook HTTP {})",
            plugin_name, version, status
        ));
        return Ok(());
    }

    anyhow::bail!(
        "krew: krew-release-bot webhook {} returned HTTP {} for plugin '{}' v{}: {}",
        webhook_url,
        status,
        plugin_name,
        version,
        resp_body.trim()
    )
}
