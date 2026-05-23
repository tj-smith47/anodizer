//! Pre-flight publisher-state queries for one-way-door publishers.
//!
//! Runs before the release pipeline to detect versions already submitted /
//! approved / in moderation, preventing a wasted release cycle.
//!
//! ## Checked publishers
//!
//! | Publisher    | One-way door? | Check mechanism                             |
//! |--------------|---------------|---------------------------------------------|
//! | crates.io    | yes           | Sparse index HTTPS GET                      |
//! | Chocolatey   | yes           | NuGet V2 OData feed                         |
//! | WinGet       | yes           | GitHub API — open PRs + fork branch          |
//! | AUR          | informational | AUR RPC v5 info endpoint                    |
//!
//! Cloudsmith is intentionally excluded: versions can be re-uploaded.

use anodizer_core::context::Context;
use anodizer_core::http::blocking_client;
use anodizer_core::log::StageLogger;
use anodizer_core::preflight::{PreflightEntry, PreflightReport, PublisherState};
use anodizer_core::retry::{RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::Result;
use std::time::Duration;

use crate::util;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Abstraction over a single publisher's state query so tests can inject
/// mock implementations without touching the network.
pub trait PreflightChecker: Send + Sync {
    /// Human-readable publisher name used in report entries.
    fn publisher_name(&self) -> &str;
    /// Query the remote registry for `package` at `version`.
    fn check(&self, package: &str, version: &str) -> PublisherState;
}

// ---------------------------------------------------------------------------
// crates.io checker
// ---------------------------------------------------------------------------

pub struct CargoCratesIo {
    policy: RetryPolicy,
}

impl CargoCratesIo {
    pub fn new(policy: RetryPolicy) -> Self {
        Self { policy }
    }
}

impl PreflightChecker for CargoCratesIo {
    fn publisher_name(&self) -> &str {
        "cargo"
    }

    fn check(&self, package: &str, version: &str) -> PublisherState {
        let url = sparse_index_url(package);
        match query_crates_io(&url, package, version, &self.policy) {
            Ok(true) => PublisherState::Published,
            Ok(false) => PublisherState::Clean,
            Err(e) => PublisherState::Unknown {
                reason: e.to_string(),
            },
        }
    }
}

/// Build the sparse-index URL for a crate name (mirrors `cargo.rs`).
fn sparse_index_url(crate_name: &str) -> String {
    let lower = crate_name.to_ascii_lowercase();
    let prefix = match lower.len() {
        1 => format!("1/{}", lower),
        2 => format!("2/{}", lower),
        3 => format!("3/{}/{}", &lower[..1], lower),
        _ => format!("{}/{}/{}", &lower[..2], &lower[2..4], lower),
    };
    format!("https://index.crates.io/{}", prefix)
}

/// Returns `Ok(true)` when the version is in the sparse index, `Ok(false)`
/// when it is absent (including 404 = crate never published).
fn query_crates_io(
    url: &str,
    crate_name: &str,
    version: &str,
    policy: &RetryPolicy,
) -> Result<bool> {
    let client = blocking_client(Duration::from_secs(10))?;
    let label = format!("preflight: crates.io index for '{}'", crate_name);
    let result = retry_http_blocking(
        &label,
        policy,
        SuccessClass::Strict,
        |_| client.get(url).send(),
        |status, body| {
            format!(
                "preflight: crates.io index returned {} for '{}': {}",
                status,
                crate_name,
                anodizer_core::redact::redact_bearer_tokens(body)
            )
        },
    );

    let (_status, body) = match result {
        Ok(pair) => pair,
        Err(err) => {
            // 404 → crate has never been published.
            let status_code = err
                .chain()
                .find_map(|e| {
                    e.downcast_ref::<anodizer_core::retry::HttpError>()
                        .map(|h| h.status)
                })
                .unwrap_or(0);
            if status_code == 404 {
                return Ok(false);
            }
            return Err(err);
        }
    };

    // Sparse index body is JSON-lines: look for a line with `"vers":"<version>"`.
    let present = body.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("vers").and_then(|v| v.as_str()).map(str::to_string))
            .is_some_and(|v| v == version)
    });
    Ok(present)
}

// ---------------------------------------------------------------------------
// Chocolatey checker
// ---------------------------------------------------------------------------

pub struct Chocolatey {
    source: String,
    policy: RetryPolicy,
}

impl Chocolatey {
    pub fn new(source: String, policy: RetryPolicy) -> Self {
        Self { source, policy }
    }
}

impl PreflightChecker for Chocolatey {
    fn publisher_name(&self) -> &str {
        "chocolatey"
    }

    fn check(&self, package: &str, version: &str) -> PublisherState {
        use crate::chocolatey::package::{FeedHashResult, classify_moderation, package_feed_hash};

        match package_feed_hash(&self.source, package, version, &self.policy) {
            FeedHashResult::Present {
                status,
                is_approved,
                ..
            } => {
                // Moderation discriminator is `<d:PackageStatus>` (with
                // `<d:IsApproved>` as fallback). The community feed does
                // NOT emit `<d:Listed>`, so any state machine keyed on it
                // is dead code.
                let (reason, in_moderation) = classify_moderation(status.as_deref(), is_approved);
                if in_moderation {
                    PublisherState::InModeration {
                        reason: reason.to_string(),
                    }
                } else {
                    PublisherState::Published
                }
            }
            FeedHashResult::PresentNoHash => {
                // Version exists but hash unreadable — treat as published.
                PublisherState::Published
            }
            FeedHashResult::Absent => PublisherState::Clean,
        }
    }
}

// ---------------------------------------------------------------------------
// WinGet checker
// ---------------------------------------------------------------------------

pub struct Winget {
    /// GitHub personal-access token (or `ANODIZER_GITHUB_TOKEN`).
    token: Option<String>,
    policy: RetryPolicy,
}

impl Winget {
    pub fn new(token: Option<String>, policy: RetryPolicy) -> Self {
        Self { token, policy }
    }
}

impl PreflightChecker for Winget {
    fn publisher_name(&self) -> &str {
        "winget"
    }

    fn check(&self, package: &str, version: &str) -> PublisherState {
        // Search for an open PR in microsoft/winget-pkgs whose title contains
        // `<PackageIdentifier> <version>`. anodizer's convention is to title
        // the PR `"New version: <PackageIdentifier> version <Version>"`, but
        // GitHub's `in:title` matches words independently so the query
        // works for any title that mentions both tokens.
        match query_winget_pr(package, version, self.token.as_deref(), &self.policy) {
            Ok(WingetPrLookup::Found(url)) => PublisherState::PRPending(url),
            Ok(WingetPrLookup::NotFound) => PublisherState::Clean,
            Ok(WingetPrLookup::ItemWithoutUrl) => PublisherState::Unknown {
                reason: "winget search response missing html_url".into(),
            },
            Err(e) => PublisherState::Unknown {
                reason: e.to_string(),
            },
        }
    }
}

/// Three-way result for the winget PR lookup so the caller can distinguish
/// "no PR" from "PR row returned but `html_url` was missing" — the second
/// case used to fall back to the listing URL, which is not a PR.
#[derive(Debug)]
enum WingetPrLookup {
    Found(String),
    NotFound,
    ItemWithoutUrl,
}

/// Query the GitHub search API for open PRs in microsoft/winget-pkgs that
/// mention `<package> <version>` in the title.
///
/// Returns `Ok(Some(url))` when a matching open PR is found, `Ok(None)`
/// when no PR exists.
///
/// Verified API shape (2026-05-13 against live PR #373590,
/// `TJSmith.Anodizer 0.2.0`): the JSON has `total_count: u64`,
/// `items: [{ html_url, title, state, ... }]`. The conventional anodizer
/// PR title format is `"New version: <PackageIdentifier> version <Version>"`.
/// GitHub's `in:title` operator matches words independently, so a query
/// containing `<id>` + `<version>` finds the PR even though the title also
/// contains the literal word "version".
fn query_winget_pr(
    package: &str,
    version: &str,
    token: Option<&str>,
    policy: &RetryPolicy,
) -> Result<WingetPrLookup> {
    let query = format!(
        "repo:microsoft/winget-pkgs is:pr is:open {} {} in:title",
        package, version
    );
    let encoded = percent_encode(&query);
    let url = format!(
        "https://api.github.com/search/issues?q={}&per_page=1",
        encoded
    );
    query_winget_pr_at(&url, token, policy)
}

/// Variant of [`query_winget_pr`] that takes a pre-built URL. Sole call site
/// for the HTTP+parse plumbing — exposed so tests can substitute a local
/// mock-server URL while still exercising the retry / parse pipeline
/// end-to-end.
fn query_winget_pr_at(
    url: &str,
    token: Option<&str>,
    policy: &RetryPolicy,
) -> Result<WingetPrLookup> {
    let token_clone = token.map(str::to_string);
    let url_clone = url.to_string();
    let label = format!("preflight: winget PR search ({})", url);

    let client = blocking_client(Duration::from_secs(15))?;
    let result = retry_http_blocking(
        &label,
        policy,
        SuccessClass::Strict,
        move |_| {
            let mut b = client
                .get(&url_clone)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            if let Some(ref tok) = token_clone
                && !tok.is_empty()
            {
                b = b.header("Authorization", format!("Bearer {}", tok));
            }
            b.send()
        },
        |status, body| {
            format!(
                "preflight: GitHub search API returned {} for winget PR check: {}",
                status,
                anodizer_core::redact::redact_bearer_tokens(body)
            )
        },
    );

    let body = match result {
        Ok((_status, body)) => body,
        Err(err) => {
            let status_code = err
                .chain()
                .find_map(|e| {
                    e.downcast_ref::<anodizer_core::retry::HttpError>()
                        .map(|h| h.status)
                })
                .unwrap_or(0);
            // 422 = query validation error — treat as no-PR rather than
            // bubbling as Unknown (a malformed query is not a network blip).
            if status_code == 422 {
                return Ok(WingetPrLookup::NotFound);
            }
            return Err(err);
        }
    };

    // Surface malformed JSON as a typed error so the caller maps it to
    // Unknown — silently coalescing to `Null` makes a corrupted response
    // indistinguishable from "no PR" (Clean).
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("malformed winget search response: {}", e))?;
    let total = v.get("total_count").and_then(|n| n.as_u64()).unwrap_or(0);

    if total == 0 {
        return Ok(WingetPrLookup::NotFound);
    }

    let pr_url = v
        .get("items")
        .and_then(|items| items.get(0))
        .and_then(|item| item.get("html_url"))
        .and_then(|u| u.as_str())
        .map(str::to_string);

    // Surface "row returned but no html_url" as a distinct outcome so the
    // caller can flag it as Unknown rather than synthesizing a misleading
    // listing-page URL.
    match pr_url {
        Some(u) => Ok(WingetPrLookup::Found(u)),
        None => Ok(WingetPrLookup::ItemWithoutUrl),
    }
}

/// Minimal percent-encoder for GitHub search query strings.
///
/// Encodes space as `+` and leaves alphanumerics, `-`, `.`, `_`, `~`, `/`,
/// `:` unencoded (safe in query-string values for this use case).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for ch in s.chars() {
        match ch {
            ' ' => out.push('+'),
            c if c.is_ascii_alphanumeric() || "-._~/:".contains(c) => out.push(c),
            c => {
                for byte in c.to_string().as_bytes() {
                    out.push('%');
                    out.push_str(&format!("{:02X}", byte));
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// AUR checker
// ---------------------------------------------------------------------------

pub struct Aur {
    policy: RetryPolicy,
}

impl Aur {
    pub fn new(policy: RetryPolicy) -> Self {
        Self { policy }
    }
}

impl PreflightChecker for Aur {
    fn publisher_name(&self) -> &str {
        "aur"
    }

    fn check(&self, package: &str, version: &str) -> PublisherState {
        match query_aur_rpc(package, version, &self.policy) {
            // AUR allows the same version to be re-pushed (it's a git push to
            // the AUR repo), so the row's existence is informational rather
            // than a blocker. Surface as Unknown with a reason so the report
            // is honest about it instead of pretending the version is sealed.
            Ok(true) => PublisherState::Unknown {
                reason: "AUR is informational — overwritable on republish".into(),
            },
            Ok(false) => PublisherState::Clean,
            Err(e) => PublisherState::Unknown {
                reason: e.to_string(),
            },
        }
    }
}

/// Returns `Ok(true)` when the AUR RPC v5 reports the package at `version`.
///
/// Verified API shape (2026-05-13 against live `yay` package): the JSON has
/// `resultcount: u64`, `type: "multiinfo"`, `version: 5`,
/// `results: [{ Name, Version, Maintainer, ... }]`. The `Version` field
/// uses the `<pkgver>-<pkgrel>` format (e.g. `"12.5.7-1"`), so a parser
/// looking for our semver alone must accept both an exact match and a
/// `<version>-` prefix.
fn query_aur_rpc(package: &str, version: &str, policy: &RetryPolicy) -> Result<bool> {
    let url = format!("https://aur.archlinux.org/rpc/v5/info?arg[]={}", package);
    query_aur_rpc_at(&url, version, policy)
}

/// Variant of [`query_aur_rpc`] that takes a pre-built URL. Sole call site
/// for the HTTP+parse plumbing — exposed so tests can substitute a local
/// mock-server URL while still exercising the retry / parse pipeline
/// end-to-end.
fn query_aur_rpc_at(url: &str, version: &str, policy: &RetryPolicy) -> Result<bool> {
    let client = blocking_client(Duration::from_secs(10))?;
    let label = format!("preflight: AUR RPC ({})", url);
    let url_clone = url.to_string();
    let result = retry_http_blocking(
        &label,
        policy,
        SuccessClass::Strict,
        move |_| client.get(&url_clone).send(),
        |status, body| format!("preflight: AUR RPC returned {}: {}", status, body),
    );

    let body = match result {
        Ok((_status, body)) => body,
        Err(err) => {
            let status_code = err
                .chain()
                .find_map(|e| {
                    e.downcast_ref::<anodizer_core::retry::HttpError>()
                        .map(|h| h.status)
                })
                .unwrap_or(0);
            if status_code == 404 {
                return Ok(false);
            }
            return Err(err);
        }
    };

    // Surface malformed JSON as a typed error so the caller maps it to
    // Unknown — silently coalescing to `Null` makes a corrupted response
    // indistinguishable from "no results" (Clean).
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("malformed AUR RPC response: {}", e))?;
    let found_version = v
        .get("results")
        .and_then(|r| r.as_array())
        .and_then(|arr| arr.first())
        .and_then(|pkg| pkg.get("Version"))
        .and_then(|v| v.as_str())
        .is_some_and(|v| v == version || v.starts_with(&format!("{}-", version)));

    Ok(found_version)
}

// ---------------------------------------------------------------------------
// run_preflight — orchestrates all enabled checkers
// ---------------------------------------------------------------------------

/// Per-publisher checker construction. Production code uses
/// [`RealCheckerFactory`] (which builds the real network-hitting checkers);
/// tests inject a mock factory that returns canned `PublisherState`s
/// without touching the network.
pub trait CheckerFactory {
    fn cargo(&self, policy: RetryPolicy) -> Box<dyn PreflightChecker>;
    fn chocolatey(&self, source: String, policy: RetryPolicy) -> Box<dyn PreflightChecker>;
    fn winget(&self, token: Option<String>, policy: RetryPolicy) -> Box<dyn PreflightChecker>;
    fn aur(&self, policy: RetryPolicy) -> Box<dyn PreflightChecker>;
}

/// Production factory — wires up the real HTTP-driven checkers.
pub struct RealCheckerFactory;

impl CheckerFactory for RealCheckerFactory {
    fn cargo(&self, policy: RetryPolicy) -> Box<dyn PreflightChecker> {
        Box::new(CargoCratesIo::new(policy))
    }
    fn chocolatey(&self, source: String, policy: RetryPolicy) -> Box<dyn PreflightChecker> {
        Box::new(Chocolatey::new(source, policy))
    }
    fn winget(&self, token: Option<String>, policy: RetryPolicy) -> Box<dyn PreflightChecker> {
        Box::new(Winget::new(token, policy))
    }
    fn aur(&self, policy: RetryPolicy) -> Box<dyn PreflightChecker> {
        Box::new(Aur::new(policy))
    }
}

/// Run all enabled one-way-door publisher checks and return an aggregated
/// [`PreflightReport`].
///
/// Checkers run sequentially. Each checker is only constructed when the
/// corresponding publisher is configured for at least one selected crate.
///
/// Takes `&mut Context` so the gpg capability probe can append to
/// `ctx.determinism.compile_time_allowlist` when the local gpg binary
/// lacks `--faked-system-time` support.
pub fn run_preflight(ctx: &mut Context, log: &StageLogger) -> Result<PreflightReport> {
    run_preflight_with_factory_and_gpg_probe(
        ctx,
        log,
        &RealCheckerFactory,
        anodizer_core::signing::gpg_supports_faked_system_time,
    )
}

/// [`run_preflight`] with the checker construction injected — exposed so
/// tests can drive the orchestration without spawning HTTP servers. Uses
/// the real gpg probe; tests that need to drive the probe use
/// [`run_preflight_with_factory_and_gpg_probe`] instead.
pub fn run_preflight_with_factory(
    ctx: &mut Context,
    log: &StageLogger,
    factory: &dyn CheckerFactory,
) -> Result<PreflightReport> {
    run_preflight_with_factory_and_gpg_probe(
        ctx,
        log,
        factory,
        anodizer_core::signing::gpg_supports_faked_system_time,
    )
}

/// Like [`run_preflight_with_factory`] but with the gpg `--faked-system-time`
/// capability probe also injected. Tests pass a closure returning the
/// canned support state without spawning a real `gpg` subprocess.
pub fn run_preflight_with_factory_and_gpg_probe(
    ctx: &mut Context,
    log: &StageLogger,
    factory: &dyn CheckerFactory,
    gpg_probe: fn() -> bool,
) -> Result<PreflightReport> {
    let mut report = PreflightReport::new();
    let policy = ctx.retry_policy();
    let version = ctx.version();

    // Walk every crate in the universe and collect per-publisher entries.
    let crates = util::all_crates(ctx);
    let selected = &ctx.options.selected_crates;

    for krate in &crates {
        if !selected.is_empty() && !selected.contains(&krate.name) {
            continue;
        }
        let publish = match krate.publish.as_ref() {
            Some(p) => p,
            None => continue,
        };

        // ---- cargo -------------------------------------------------------
        if publish.cargo.is_some() {
            log.verbose(&format!(
                "preflight: checking cargo for '{}@{}'",
                krate.name, version
            ));
            let checker = factory.cargo(policy);
            let state = checker.check(&krate.name, &version);
            report.push(PreflightEntry {
                publisher: checker.publisher_name().to_string(),
                package: krate.name.clone(),
                version: version.clone(),
                state,
            });
        }

        // ---- chocolatey --------------------------------------------------
        if let Some(ref choco_cfg) = publish.chocolatey {
            let source = choco_cfg
                .source_repo
                .as_deref()
                .unwrap_or("https://push.chocolatey.org/")
                .to_string();
            let pkg_name = choco_cfg.name.as_deref().unwrap_or(&krate.name).to_string();
            log.verbose(&format!(
                "preflight: checking chocolatey for '{}@{}'",
                pkg_name, version
            ));
            let checker = factory.chocolatey(source, policy);
            let state = checker.check(&pkg_name, &version);
            report.push(PreflightEntry {
                publisher: checker.publisher_name().to_string(),
                package: pkg_name,
                version: version.clone(),
                state,
            });
        }

        // ---- winget ------------------------------------------------------
        if let Some(ref winget_cfg) = publish.winget {
            let pkg_id = winget_cfg
                .package_identifier
                .as_deref()
                .or(winget_cfg.name.as_deref())
                .unwrap_or(&krate.name)
                .to_string();
            let token = util::resolve_repo_token(ctx, winget_cfg.repository.as_ref(), None);
            log.verbose(&format!(
                "preflight: checking winget for '{}@{}'",
                pkg_id, version
            ));
            let checker = factory.winget(token, policy);
            let state = checker.check(&pkg_id, &version);
            report.push(PreflightEntry {
                publisher: checker.publisher_name().to_string(),
                package: pkg_id,
                version: version.clone(),
                state,
            });
        }

        // ---- aur ---------------------------------------------------------
        if let Some(ref aur_cfg) = publish.aur {
            let pkg_name = aur_cfg
                .name
                .as_deref()
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("{}-bin", krate.name));
            log.verbose(&format!(
                "preflight: checking AUR for '{}@{}'",
                pkg_name, version
            ));
            let checker = factory.aur(policy);
            let state = checker.check(&pkg_name, &version);
            report.push(PreflightEntry {
                publisher: checker.publisher_name().to_string(),
                package: pkg_name,
                version: version.clone(),
                state,
            });
        }
    }

    run_publisher_preflight_extension(ctx, &mut report)?;

    run_gpg_capability_probe(ctx, &mut report, gpg_probe);

    Ok(report)
}

// ---------------------------------------------------------------------------
// gpg --faked-system-time capability probe
// ---------------------------------------------------------------------------

/// If gpg is configured for signing somewhere in the config AND the
/// local gpg binary doesn't support `--faked-system-time`, register the
/// `gpg-signature.asc` artifact in the compile-time allow-list so the
/// determinism harness excludes it from drift detection. Also emit a
/// preflight warning so the operator sees the fallback at pipeline
/// start.
///
/// `gpg --faked-system-time` is how anodize asks gpg to embed a
/// deterministic timestamp; without it, gpg embeds the real wall-clock
/// time and the signature bytes drift between runs.
fn run_gpg_capability_probe(
    ctx: &mut anodizer_core::context::Context,
    report: &mut PreflightReport,
    gpg_probe: fn() -> bool,
) {
    if !ctx.config.has_gpg_sign_configured() {
        return;
    }
    if gpg_probe() {
        return;
    }
    report.warnings.push(
        "gpg binary does not support --faked-system-time; gpg signatures will be excluded from determinism harness drift detection".into(),
    );
    if let Some(state) = ctx.determinism.as_mut() {
        state.compile_time_allowlist.push((
            "gpg-signature.asc".into(),
            "gpg binary does not support --faked-system-time".into(),
        ));
    }
}

// ---------------------------------------------------------------------------
// Rollback-scope + publisher-preflight extension
// ---------------------------------------------------------------------------

/// Walk the trait-based publisher registry and surface two classes of
/// resilience concerns into the report:
///
/// 1. Rollback scope availability — every publisher whose
///    [`Publisher::rollback_scope_needed`] returns `Some(label)` is checked
///    against the env var named in `label`. Missing scope becomes a
///    warning by default and a blocker under `--strict`. If
///    `--rollback=best-effort` was explicitly requested and any
///    `required` publisher lacks rollback scope, this function returns
///    `Err` so the CLI bails before any publish work runs.
/// 2. Publisher self-check — each publisher's [`Publisher::preflight`]
///    return value is folded into the report (`Warning` -> warnings,
///    `Blocker` -> blockers, `Err` -> blockers tagged as preflight error).
///    All publishers currently return `Pass`; the wiring is here so
///    future per-publisher preflight logic flows through the same channel.
fn run_publisher_preflight_extension(
    ctx: &anodizer_core::context::Context,
    report: &mut PreflightReport,
) -> Result<()> {
    let publishers = crate::registry::configured_publishers(ctx);
    let mut required_missing_scope: Vec<String> = Vec::new();

    for p in &publishers {
        // ---- rollback scope check ------------------------------------
        if let Some(label) = p.rollback_scope_needed()
            && !crate::scope::scope_available(label)
        {
            let msg = crate::scope::warn_scope_unavailable_msg("preflight", p.name(), label);
            if ctx.options.strict {
                report.blockers.push(msg);
            } else {
                report.warnings.push(msg);
            }
            if p.required() {
                required_missing_scope.push(p.name().to_string());
            }
        }

        // ---- publisher self-check ------------------------------------
        match p.preflight(ctx) {
            Ok(anodizer_core::PreflightCheck::Pass) => {}
            Ok(anodizer_core::PreflightCheck::Warning(msg)) => {
                report.warnings.push(format!("{}: {}", p.name(), msg));
            }
            Ok(anodizer_core::PreflightCheck::Blocker(msg)) => {
                report.blockers.push(format!("{}: {}", p.name(), msg));
            }
            Err(err) => {
                report
                    .blockers
                    .push(format!("{}: preflight error: {}", p.name(), err));
            }
        }
    }

    // Hard error: `--rollback=best-effort` was explicitly requested but a
    // required publisher lacks rollback scope. Bail before any side-effect
    // stage runs so the operator can elevate the token (or accept losing
    // rollback) before starting a release that cannot recover from failure.
    if matches!(
        ctx.options.rollback_mode,
        Some(anodizer_core::context::RollbackMode::BestEffort)
    ) && !required_missing_scope.is_empty()
    {
        anyhow::bail!(
            "preflight: --rollback=best-effort was requested but the following required publishers lack rollback scope: {}",
            required_missing_scope.join(", "),
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodizer_core::preflight::PublisherState;

    // Minimal mock checker for report-aggregation tests.
    struct MockChecker {
        name: &'static str,
        state: PublisherState,
    }

    impl PreflightChecker for MockChecker {
        fn publisher_name(&self) -> &str {
            self.name
        }
        fn check(&self, _package: &str, _version: &str) -> PublisherState {
            self.state.clone()
        }
    }

    fn run_mocks(checkers: Vec<(&'static str, PublisherState)>) -> PreflightReport {
        let mut report = PreflightReport::new();
        for (name, state) in checkers {
            let checker = MockChecker { name, state };
            let s = checker.check("testpkg", "1.0.0");
            report.push(PreflightEntry {
                publisher: checker.publisher_name().to_string(),
                package: "testpkg".to_string(),
                version: "1.0.0".to_string(),
                state: s,
            });
        }
        report
    }

    #[test]
    fn mock_all_clean_no_blockers() {
        let report = run_mocks(vec![
            ("cargo", PublisherState::Clean),
            ("chocolatey", PublisherState::Clean),
            ("winget", PublisherState::Clean),
            ("aur", PublisherState::Clean),
        ]);
        assert!(!report.has_blockers(false));
        assert_eq!(report.clean_count(), 4);
    }

    #[test]
    fn mock_in_moderation_is_blocker() {
        let report = run_mocks(vec![
            ("cargo", PublisherState::Clean),
            (
                "chocolatey",
                PublisherState::InModeration {
                    reason: "package in moderation queue".into(),
                },
            ),
            ("winget", PublisherState::Clean),
            ("aur", PublisherState::Published),
        ]);
        assert!(report.has_blockers(false));
        let blockers = report.blockers(false);
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].publisher, "chocolatey");
    }

    #[test]
    fn mock_pr_pending_is_blocker() {
        let report = run_mocks(vec![(
            "winget",
            PublisherState::PRPending("https://github.com/microsoft/winget-pkgs/pull/9999".into()),
        )]);
        assert!(report.has_blockers(false));
    }

    #[test]
    fn mock_published_is_not_blocker() {
        let report = run_mocks(vec![
            ("cargo", PublisherState::Published),
            ("aur", PublisherState::Published),
        ]);
        assert!(!report.has_blockers(false));
        assert!(!report.has_blockers(true));
    }

    #[test]
    fn mock_unknown_non_strict_not_blocker() {
        let report = run_mocks(vec![(
            "aur",
            PublisherState::Unknown {
                reason: "timeout connecting to AUR".into(),
            },
        )]);
        assert!(!report.has_blockers(false));
        assert!(report.has_blockers(true));
    }

    // ---- HTTP-mock tests for crates.io index check ------------------------

    use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

    fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
        }
    }

    #[test]
    fn crates_io_checker_absent_on_404() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);
        let url = format!("http://{}/", addr);
        let result = query_crates_io(&url, "foo", "1.0.0", &fast_retry());
        assert!(result.is_ok());
        assert!(!result.unwrap(), "absent on 404");
    }

    #[test]
    fn crates_io_checker_present_when_version_in_body() {
        let body = r#"{"name":"foo","vers":"1.0.0","cksum":"abc123"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (addr, _calls) =
            spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
        let url = format!("http://{}/", addr);
        let result = query_crates_io(&url, "foo", "1.0.0", &fast_retry());
        assert!(result.is_ok());
        assert!(result.unwrap(), "present when version matches");
    }

    #[test]
    fn crates_io_checker_absent_when_version_not_in_body() {
        let body = r#"{"name":"foo","vers":"0.9.0","cksum":"abc123"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (addr, _calls) =
            spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
        let url = format!("http://{}/", addr);
        let result = query_crates_io(&url, "foo", "1.0.0", &fast_retry());
        assert!(result.is_ok());
        assert!(!result.unwrap(), "absent when version does not match");
    }

    #[test]
    fn aur_rpc_absent_on_empty_results() {
        let body = r#"{"version":5,"type":"multiinfo","resultcount":0,"results":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (addr, _calls) =
            spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
        let url = format!("http://{}/rpc/v5/info?arg[]=mypkg", addr);
        // query_aur_rpc does GET to the URL directly; reuse it with overridden URL
        // by calling the lower-level function with the mock address.
        let result = query_aur_rpc_at(&url, "1.0.0", &fast_retry());
        assert!(result.is_ok());
        assert!(!result.unwrap(), "absent on empty results");
    }

    #[test]
    fn aur_rpc_present_when_version_matches() {
        let body = r#"{"version":5,"type":"multiinfo","resultcount":1,"results":[{"Name":"mypkg","Version":"1.0.0-1"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (addr, _calls) =
            spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
        let url = format!("http://{}/rpc/v5/info?arg[]=mypkg", addr);
        let result = query_aur_rpc_at(&url, "1.0.0", &fast_retry());
        assert!(result.is_ok());
        assert!(
            result.unwrap(),
            "present when AUR version starts with 1.0.0-"
        );
    }

    #[test]
    fn winget_pr_absent_on_empty_results() {
        let body = r#"{"total_count":0,"incomplete_results":false,"items":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (addr, _calls) =
            spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
        let url = format!(
            "http://{}/search/issues?q=mypkg+1.0.0+in%3Atitle&per_page=1",
            addr
        );
        let result = query_winget_pr_at(&url, None, &fast_retry()).expect("ok");
        assert!(
            matches!(result, WingetPrLookup::NotFound),
            "no PR when total_count=0"
        );
    }

    #[test]
    fn winget_pr_present_on_result() {
        let body = r#"{"total_count":1,"incomplete_results":false,"items":[{"html_url":"https://github.com/microsoft/winget-pkgs/pull/9999","title":"New version: mypkg 1.0.0"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (addr, _calls) =
            spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
        let url = format!(
            "http://{}/search/issues?q=mypkg+1.0.0+in%3Atitle&per_page=1",
            addr
        );
        let result = query_winget_pr_at(&url, None, &fast_retry()).expect("ok");
        match result {
            WingetPrLookup::Found(u) => assert!(u.contains("pull/9999"), "correct PR URL: {u}"),
            other => panic!("expected Found, got: {:?}", std::mem::discriminant(&other)),
        }
    }

    // ---- Winget: html_url missing → ItemWithoutUrl ------------------------

    #[test]
    fn winget_pr_item_without_url_is_unknown_signal() {
        let body = r#"{"total_count":1,"incomplete_results":false,"items":[{"title":"a PR row"}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (addr, _calls) =
            spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
        let url = format!("http://{}/search/issues", addr);
        let result = query_winget_pr_at(&url, None, &fast_retry()).expect("ok");
        assert!(
            matches!(result, WingetPrLookup::ItemWithoutUrl),
            "items[0] without html_url must surface as a distinct outcome"
        );
    }

    // ---- Winget: malformed JSON → Err (mapped to Unknown by caller) ------

    #[test]
    fn winget_pr_malformed_json_is_error() {
        let body = "not json at all";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (addr, _calls) =
            spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
        let url = format!("http://{}/search/issues", addr);
        let err = query_winget_pr_at(&url, None, &fast_retry()).expect_err("must be Err");
        assert!(
            err.to_string().contains("malformed winget search response"),
            "{err}"
        );
    }

    // ---- AUR: malformed JSON → Err (mapped to Unknown by caller) ---------

    #[test]
    fn aur_rpc_malformed_json_is_error() {
        let body = "garbage";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let (addr, _calls) =
            spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
        let url = format!("http://{}/rpc/v5/info?arg[]=mypkg", addr);
        let err = query_aur_rpc_at(&url, "1.0.0", &fast_retry()).expect_err("must be Err");
        assert!(
            err.to_string().contains("malformed AUR RPC response"),
            "{err}"
        );
    }

    // ---- AUR: 404 → Ok(false) (Clean) ------------------------------------

    #[test]
    fn aur_rpc_absent_on_404() {
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);
        let url = format!("http://{}/rpc/v5/info?arg[]=mypkg", addr);
        let result = query_aur_rpc_at(&url, "1.0.0", &fast_retry()).expect("ok");
        assert!(
            !result,
            "404 must map to Ok(false) so the caller emits Clean"
        );
    }

    // ---- crates.io: network error (connect-refused) → Unknown via Err ----

    #[test]
    fn crates_io_checker_unknown_on_network_error() {
        // Bind a port to learn a free one, then drop the listener so the
        // following GET attempt fails with connection refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);

        let url = format!("http://{}/", addr);
        let result = query_crates_io(&url, "foo", "1.0.0", &fast_retry());
        let err = result.expect_err("must be Err on connect-refused");

        // The trait-level wrapper would surface this as Unknown { reason } —
        // exercise the path explicitly to confirm.
        let checker_state = match query_crates_io(&url, "foo", "1.0.0", &fast_retry()) {
            Ok(true) => PublisherState::Published,
            Ok(false) => PublisherState::Clean,
            Err(e) => PublisherState::Unknown {
                reason: e.to_string(),
            },
        };
        assert!(
            matches!(checker_state, PublisherState::Unknown { .. }),
            "network error must surface as Unknown, got: {:?}",
            checker_state
        );
        // Sanity: the underlying error mentioned the host/port we used.
        let msg = err.to_string();
        assert!(!msg.is_empty(), "error message must be non-empty");
    }

    // ---- Winget: Authorization header is sent when token is set --------

    use anodizer_core::test_helpers::responder::spawn_request_capturing_responder;

    #[test]
    fn winget_pr_sends_authorization_header_when_token_set() {
        let body = r#"{"total_count":0,"incomplete_results":false,"items":[]}"#;
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        );
        let (addr, captured) = spawn_request_capturing_responder(response);
        let url = format!("http://{}/search/issues", addr);
        // `.expect()` propagates Result; discard the WingetPrLookup payload
        // — this test asserts on the captured Authorization header side
        // effect, not the response body.
        query_winget_pr_at(&url, Some("secret-token"), &fast_retry()).expect("ok");

        // reqwest lowercases header names on the wire (HTTP/2 style); match
        // case-insensitively so the assertion isn't brittle to that detail.
        let req = captured.lock().unwrap().clone();
        let lower = req.to_ascii_lowercase();
        assert!(
            lower.contains("authorization: bearer secret-token"),
            "Authorization header missing or malformed; request was:\n{req}"
        );
    }

    // ---- Chocolatey checker fixtures (PackageStatus / IsApproved) -------

    fn choco_odata_entry(version: &str, status: Option<&str>, is_approved: Option<bool>) -> String {
        let mut props = String::new();
        props.push_str("<d:PackageHash>deadbeef</d:PackageHash>");
        props.push_str("<d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>");
        if let Some(s) = status {
            props.push_str(&format!("<d:PackageStatus>{}</d:PackageStatus>", s));
        }
        if let Some(a) = is_approved {
            props.push_str(&format!("<d:IsApproved>{}</d:IsApproved>", a));
        }
        format!(
            r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<entry>
  <id>http://example.com/api/v2/Packages(Id='foo',Version='{}')</id>
  <m:properties>{}</m:properties>
</entry>"#,
            version, props
        )
    }

    fn choco_http_resp(body: String) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            )
            .into_boxed_str(),
        )
    }

    #[test]
    fn chocolatey_checker_submitted_is_in_moderation() {
        // Mirrors the live `anodizer 0.2.0` response: PackageStatus=Submitted,
        // IsApproved=false, no <d:Listed>.
        let body = choco_odata_entry("1.0.0", Some("Submitted"), Some(false));
        let (addr, _calls) = spawn_oneshot_http_responder(vec![choco_http_resp(body)]);
        let source = format!("http://{}/", addr);

        let checker = Chocolatey::new(source, fast_retry());
        let state = checker.check("foo", "1.0.0");
        match state {
            PublisherState::InModeration { reason } => assert!(
                reason.contains("moderation"),
                "reason should mention moderation: {reason}"
            ),
            other => panic!("expected InModeration, got: {:?}", other),
        }
    }

    #[test]
    fn chocolatey_checker_approved_is_published() {
        // Mirrors the live `git 2.50.1` response: PackageStatus=Approved,
        // IsApproved=true, no <d:Listed>.
        let body = choco_odata_entry("1.0.0", Some("Approved"), Some(true));
        let (addr, _calls) = spawn_oneshot_http_responder(vec![choco_http_resp(body)]);
        let source = format!("http://{}/", addr);

        let checker = Chocolatey::new(source, fast_retry());
        let state = checker.check("foo", "1.0.0");
        assert!(
            matches!(state, PublisherState::Published),
            "approved row must be Published, got: {:?}",
            state
        );
    }

    #[test]
    fn chocolatey_checker_404_is_clean() {
        // The OData entry endpoint returns 404 when the row is absent.
        let (addr, _calls) = spawn_oneshot_http_responder(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        ]);
        let source = format!("http://{}/", addr);

        let checker = Chocolatey::new(source, fast_retry());
        let state = checker.check("foo", "1.0.0");
        assert!(
            matches!(state, PublisherState::Clean),
            "absent row must be Clean, got: {:?}",
            state
        );
    }

    // ---- run_preflight orchestration with injected mock factory -------

    /// Mock checker that ignores inputs and returns a canned state. The
    /// `name` field is the publisher label written into the report entry.
    struct StaticChecker {
        name: &'static str,
        state: PublisherState,
    }

    impl PreflightChecker for StaticChecker {
        fn publisher_name(&self) -> &str {
            self.name
        }
        fn check(&self, _package: &str, _version: &str) -> PublisherState {
            self.state.clone()
        }
    }

    /// Factory wired up to return the four canned states the orchestration
    /// test asserts against.
    struct CannedFactory {
        cargo_state: PublisherState,
        choco_state: PublisherState,
        winget_state: PublisherState,
        aur_state: PublisherState,
    }

    impl CheckerFactory for CannedFactory {
        fn cargo(&self, _policy: RetryPolicy) -> Box<dyn PreflightChecker> {
            Box::new(StaticChecker {
                name: "cargo",
                state: self.cargo_state.clone(),
            })
        }
        fn chocolatey(&self, _source: String, _policy: RetryPolicy) -> Box<dyn PreflightChecker> {
            Box::new(StaticChecker {
                name: "chocolatey",
                state: self.choco_state.clone(),
            })
        }
        fn winget(
            &self,
            _token: Option<String>,
            _policy: RetryPolicy,
        ) -> Box<dyn PreflightChecker> {
            Box::new(StaticChecker {
                name: "winget",
                state: self.winget_state.clone(),
            })
        }
        fn aur(&self, _policy: RetryPolicy) -> Box<dyn PreflightChecker> {
            Box::new(StaticChecker {
                name: "aur",
                state: self.aur_state.clone(),
            })
        }
    }

    #[test]
    fn run_preflight_aggregates_per_publisher_in_config_order() {
        use anodizer_core::config::{
            AurConfig, CargoPublishConfig, ChocolateyConfig, Config, CrateConfig, PublishConfig,
            WingetConfig,
        };
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let publish = PublishConfig {
            cargo: Some(CargoPublishConfig::default()),
            chocolatey: Some(ChocolateyConfig::default()),
            winget: Some(WingetConfig::default()),
            aur: Some(AurConfig::default()),
            ..Default::default()
        };
        let crate_cfg = CrateConfig {
            name: "mytool".to_string(),
            publish: Some(publish),
            ..Default::default()
        };

        let config = Config {
            project_name: "mytool".to_string(),
            crates: vec![crate_cfg],
            ..Default::default()
        };

        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        let log = StageLogger::new("preflight", Verbosity::Normal);

        let factory = CannedFactory {
            cargo_state: PublisherState::Clean,
            choco_state: PublisherState::InModeration {
                reason: "package in moderation queue".into(),
            },
            winget_state: PublisherState::PRPending(
                "https://github.com/microsoft/winget-pkgs/pull/1".into(),
            ),
            aur_state: PublisherState::Unknown {
                reason: "AUR is informational — overwritable on republish".into(),
            },
        };

        let report = run_preflight_with_factory(&mut ctx, &log, &factory).expect("ok");

        // One entry per configured publisher, in the dispatcher's traversal
        // order (cargo → chocolatey → winget → aur).
        let order: Vec<&str> = report
            .entries
            .iter()
            .map(|e| e.publisher.as_str())
            .collect();
        assert_eq!(order, vec!["cargo", "chocolatey", "winget", "aur"]);

        // Per-publisher state is preserved unchanged.
        assert!(matches!(report.entries[0].state, PublisherState::Clean));
        assert!(matches!(
            report.entries[1].state,
            PublisherState::InModeration { .. }
        ));
        assert!(matches!(
            report.entries[2].state,
            PublisherState::PRPending(_)
        ));
        assert!(matches!(
            report.entries[3].state,
            PublisherState::Unknown { .. }
        ));

        // Each entry carries the resolved version.
        for entry in &report.entries {
            assert_eq!(entry.version, "1.0.0");
        }

        // Blocker tally: 2 hard blockers (InModeration + PRPending), AUR
        // Unknown only blocks in strict.
        assert_eq!(report.blockers(false).len(), 2);
        assert_eq!(report.blockers(true).len(), 3);
    }

    // ---- rollback-scope + Publisher::preflight() extension ----
    //
    // These tests mutate process-wide env vars (CARGO_REGISTRY_TOKEN,
    // GITHUB_TOKEN, ANODIZER_GITHUB_TOKEN). The `serial_test::serial`
    // attribute serializes them under a shared lock to prevent races
    // with other tests in the suite that read the same vars.

    /// Build a Context where a single crate has `publish.cargo`
    /// configured. Used by the rollback-scope tests below; the
    /// CargoPublisher is the canonical `required=true` publisher with a
    /// scope label (`"CARGO_REGISTRY_TOKEN yank"`).
    fn fixture_cargo_publisher(
        strict: bool,
        rollback_mode: Option<anodizer_core::context::RollbackMode>,
    ) -> anodizer_core::context::Context {
        use anodizer_core::config::{CargoPublishConfig, Config, CrateConfig, PublishConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let publish = PublishConfig {
            cargo: Some(CargoPublishConfig::default()),
            ..Default::default()
        };
        let crate_cfg = CrateConfig {
            name: "mytool".to_string(),
            publish: Some(publish),
            ..Default::default()
        };
        let config = Config {
            project_name: "mytool".to_string(),
            crates: vec![crate_cfg],
            ..Default::default()
        };
        let options = ContextOptions {
            strict,
            rollback_mode,
            ..Default::default()
        };
        let mut ctx = Context::new(config, options);
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx
    }

    fn empty_factory() -> CannedFactory {
        CannedFactory {
            cargo_state: PublisherState::Clean,
            choco_state: PublisherState::Clean,
            winget_state: PublisherState::Clean,
            aur_state: PublisherState::Clean,
        }
    }

    #[test]
    #[serial_test::serial(preflight_env)]
    fn preflight_warns_on_missing_rollback_scope() {
        use anodizer_core::log::{StageLogger, Verbosity};
        // SAFETY: serialized via serial_test::serial across the env-mutating
        // tests in this set; no other thread reads CARGO_REGISTRY_TOKEN
        // during the remove_var window.
        unsafe {
            std::env::remove_var("CARGO_REGISTRY_TOKEN");
        }

        let mut ctx = fixture_cargo_publisher(false, None);
        let log = StageLogger::new("preflight", Verbosity::Normal);
        let factory = empty_factory();
        let report = run_preflight_with_factory(&mut ctx, &log, &factory).expect("ok");

        assert_eq!(
            report.warnings.len(),
            1,
            "expected 1 scope warning, got: {:?}",
            report.warnings
        );
        assert!(
            report.warnings[0].contains("cargo")
                && report.warnings[0].contains("CARGO_REGISTRY_TOKEN"),
            "warning text: {}",
            report.warnings[0]
        );
        assert!(
            report.blockers.is_empty(),
            "blockers should be empty in default mode, got: {:?}",
            report.blockers
        );
    }

    #[test]
    #[serial_test::serial(preflight_env)]
    fn preflight_blocks_on_missing_rollback_scope_when_strict() {
        use anodizer_core::log::{StageLogger, Verbosity};
        // SAFETY: serialized via serial_test::serial.
        unsafe {
            std::env::remove_var("CARGO_REGISTRY_TOKEN");
        }

        let mut ctx = fixture_cargo_publisher(true, None);
        let log = StageLogger::new("preflight", Verbosity::Normal);
        let factory = empty_factory();
        let report = run_preflight_with_factory(&mut ctx, &log, &factory).expect("ok");

        assert!(
            report.warnings.is_empty(),
            "warnings should be empty in strict mode, got: {:?}",
            report.warnings
        );
        assert_eq!(
            report.blockers.len(),
            1,
            "expected 1 scope blocker under --strict, got: {:?}",
            report.blockers
        );
        assert!(
            report.blockers[0].contains("cargo"),
            "blocker text: {}",
            report.blockers[0]
        );
    }

    #[test]
    #[serial_test::serial(preflight_env)]
    fn preflight_bails_when_required_publisher_missing_scope_and_rollback_best_effort() {
        use anodizer_core::context::RollbackMode;
        use anodizer_core::log::{StageLogger, Verbosity};
        // SAFETY: serialized via serial_test::serial.
        unsafe {
            std::env::remove_var("CARGO_REGISTRY_TOKEN");
        }

        let mut ctx = fixture_cargo_publisher(false, Some(RollbackMode::BestEffort));
        let log = StageLogger::new("preflight", Verbosity::Normal);
        let factory = empty_factory();
        let err = run_preflight_with_factory(&mut ctx, &log, &factory).expect_err(
            "must bail when required publisher lacks rollback scope under --rollback=best-effort",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("--rollback=best-effort"),
            "error message must name the requested rollback mode: {}",
            msg
        );
        assert!(
            msg.contains("cargo"),
            "error message must name the offending publisher: {}",
            msg
        );
    }

    /// Test Publisher that returns a fixed `PreflightCheck` so we can drive
    /// the per-publisher self-check path without configuring a real
    /// publisher. Routed through the `configured_publishers` trait registry
    /// is not possible without registry surgery, so this test exercises the
    /// helper that the extension dispatches against directly.
    struct StubPublisher {
        outcome: anodizer_core::PreflightCheck,
    }

    impl anodizer_core::Publisher for StubPublisher {
        fn name(&self) -> &str {
            "stub"
        }
        fn run(
            &self,
            _ctx: &mut anodizer_core::context::Context,
        ) -> anyhow::Result<anodizer_core::PublishEvidence> {
            Ok(anodizer_core::PublishEvidence::new("stub"))
        }
        fn group(&self) -> anodizer_core::PublisherGroup {
            anodizer_core::PublisherGroup::Manager
        }
        fn required(&self) -> bool {
            false
        }
        fn preflight(
            &self,
            _ctx: &anodizer_core::context::Context,
        ) -> anyhow::Result<anodizer_core::PreflightCheck> {
            Ok(self.outcome.clone())
        }
    }

    #[test]
    fn preflight_invokes_publisher_preflight_warning() {
        // Direct unit test of the Publisher::preflight() return-value
        // routing: invoking the stub through the same match the extension
        // uses must land the message in `report.warnings` prefixed by the
        // publisher name.
        let stub = StubPublisher {
            outcome: anodizer_core::PreflightCheck::Warning("foo".into()),
        };
        let mut report = PreflightReport::new();
        let p: &dyn anodizer_core::Publisher = &stub;
        match p.preflight(&anodizer_core::context::Context::test_fixture()) {
            Ok(anodizer_core::PreflightCheck::Pass) => {}
            Ok(anodizer_core::PreflightCheck::Warning(m)) => {
                report.warnings.push(format!("{}: {}", p.name(), m))
            }
            Ok(anodizer_core::PreflightCheck::Blocker(m)) => {
                report.blockers.push(format!("{}: {}", p.name(), m))
            }
            Err(e) => report
                .blockers
                .push(format!("{}: preflight error: {}", p.name(), e)),
        }
        assert_eq!(report.warnings, vec!["stub: foo".to_string()]);
        assert!(report.blockers.is_empty());

        // Blocker variant: must land in blockers, not warnings.
        let stub_b = StubPublisher {
            outcome: anodizer_core::PreflightCheck::Blocker("bar".into()),
        };
        let mut report2 = PreflightReport::new();
        let p2: &dyn anodizer_core::Publisher = &stub_b;
        match p2.preflight(&anodizer_core::context::Context::test_fixture()) {
            Ok(anodizer_core::PreflightCheck::Pass) => {}
            Ok(anodizer_core::PreflightCheck::Warning(m)) => {
                report2.warnings.push(format!("{}: {}", p2.name(), m))
            }
            Ok(anodizer_core::PreflightCheck::Blocker(m)) => {
                report2.blockers.push(format!("{}: {}", p2.name(), m))
            }
            Err(e) => report2
                .blockers
                .push(format!("{}: preflight error: {}", p2.name(), e)),
        }
        assert!(report2.warnings.is_empty());
        assert_eq!(report2.blockers, vec!["stub: bar".to_string()]);
    }

    #[test]
    #[serial_test::serial(preflight_env)]
    fn preflight_honors_anodizer_github_token_fallback() {
        use anodizer_core::config::{
            Config, CrateConfig, HomebrewConfig, PublishConfig, RepositoryConfig,
        };
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        // SAFETY: serialized via serial_test::serial.
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
            std::env::set_var("ANODIZER_GITHUB_TOKEN", "fallback-token");
        }

        let publish = PublishConfig {
            homebrew: Some(HomebrewConfig {
                repository: Some(RepositoryConfig {
                    owner: Some("acme".to_string()),
                    name: Some("homebrew-tap".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let crate_cfg = CrateConfig {
            name: "mytool".to_string(),
            publish: Some(publish),
            ..Default::default()
        };
        let config = Config {
            project_name: "mytool".to_string(),
            crates: vec![crate_cfg],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        let log = StageLogger::new("preflight", Verbosity::Normal);
        let factory = empty_factory();

        let report = run_preflight_with_factory(&mut ctx, &log, &factory).expect("ok");

        let homebrew_scope_warnings: Vec<&String> = report
            .warnings
            .iter()
            .filter(|w| w.contains("homebrew") && w.contains("GITHUB_TOKEN"))
            .collect();
        assert!(
            homebrew_scope_warnings.is_empty(),
            "ANODIZER_GITHUB_TOKEN fallback must satisfy GITHUB_TOKEN scope; warnings: {:?}",
            report.warnings
        );

        // SAFETY: serialized via serial_test::serial.
        unsafe {
            std::env::remove_var("ANODIZER_GITHUB_TOKEN");
        }
    }

    // ---- gpg --faked-system-time capability probe ----

    /// Build a Context whose top-level `signs:` declares a gpg signature
    /// covering all artifacts (the canonical user-facing way to enable
    /// gpg signing). The probe path only fires when
    /// `Config::has_gpg_sign_configured()` is true.
    fn fixture_gpg_signing() -> anodizer_core::context::Context {
        use anodizer_core::config::{Config, SignConfig};
        use anodizer_core::context::{Context, ContextOptions};
        let config = Config {
            project_name: "mytool".to_string(),
            signs: vec![SignConfig {
                artifacts: Some("all".to_string()),
                // cmd: None — defaults to gpg
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.determinism =
            Some(anodizer_core::DeterminismState::seed_from_commit(0).expect("0 is non-negative"));
        ctx
    }

    fn fixture_cosign_only() -> anodizer_core::context::Context {
        use anodizer_core::config::{Config, SignConfig};
        use anodizer_core::context::{Context, ContextOptions};
        let config = Config {
            project_name: "mytool".to_string(),
            signs: vec![SignConfig {
                artifacts: Some("all".to_string()),
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.determinism =
            Some(anodizer_core::DeterminismState::seed_from_commit(0).expect("0 is non-negative"));
        ctx
    }

    fn gpg_probe_returns_false() -> bool {
        false
    }

    fn gpg_probe_returns_true() -> bool {
        true
    }

    #[test]
    fn preflight_warns_when_gpg_lacks_faked_system_time() {
        use anodizer_core::log::{StageLogger, Verbosity};
        let mut ctx = fixture_gpg_signing();
        let log = StageLogger::new("preflight", Verbosity::Normal);
        let factory = empty_factory();
        let report = run_preflight_with_factory_and_gpg_probe(
            &mut ctx,
            &log,
            &factory,
            gpg_probe_returns_false,
        )
        .expect("ok");

        let gpg_warnings: Vec<&String> = report
            .warnings
            .iter()
            .filter(|w| w.contains("--faked-system-time"))
            .collect();
        assert_eq!(
            gpg_warnings.len(),
            1,
            "expected exactly one gpg-fallback warning, got: {:?}",
            report.warnings
        );
    }

    #[test]
    fn preflight_adds_compile_time_allowlist_entry_when_gpg_unsupported() {
        use anodizer_core::log::{StageLogger, Verbosity};
        let mut ctx = fixture_gpg_signing();
        let log = StageLogger::new("preflight", Verbosity::Normal);
        let factory = empty_factory();
        let _report = run_preflight_with_factory_and_gpg_probe(
            &mut ctx,
            &log,
            &factory,
            gpg_probe_returns_false,
        )
        .expect("ok");

        let state = ctx.determinism.expect("determinism state seeded");
        let entry = state
            .compile_time_allowlist
            .iter()
            .find(|(name, _)| name == "gpg-signature.asc")
            .expect("gpg-signature.asc allowlist entry must be present");
        assert!(
            entry.1.contains("--faked-system-time"),
            "reason text must reference --faked-system-time: {}",
            entry.1
        );
    }

    #[test]
    fn preflight_no_gpg_warning_when_probe_succeeds() {
        use anodizer_core::log::{StageLogger, Verbosity};
        let mut ctx = fixture_gpg_signing();
        let log = StageLogger::new("preflight", Verbosity::Normal);
        let factory = empty_factory();
        let report = run_preflight_with_factory_and_gpg_probe(
            &mut ctx,
            &log,
            &factory,
            gpg_probe_returns_true,
        )
        .expect("ok");

        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("--faked-system-time")),
            "no gpg-fallback warning expected when probe succeeds: {:?}",
            report.warnings
        );
        let state = ctx.determinism.expect("determinism state seeded");
        assert!(
            !state
                .compile_time_allowlist
                .iter()
                .any(|(n, _)| n == "gpg-signature.asc"),
            "no gpg-signature.asc allowlist entry expected when probe succeeds"
        );
    }

    #[test]
    fn preflight_skips_gpg_probe_when_no_gpg_config() {
        use anodizer_core::log::{StageLogger, Verbosity};
        let mut ctx = fixture_cosign_only();
        let log = StageLogger::new("preflight", Verbosity::Normal);
        let factory = empty_factory();
        // Pass the always-false probe — the probe path must not run because
        // no gpg-using sign config is present.
        let report = run_preflight_with_factory_and_gpg_probe(
            &mut ctx,
            &log,
            &factory,
            gpg_probe_returns_false,
        )
        .expect("ok");

        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("--faked-system-time")),
            "no gpg-fallback warning when only cosign is configured: {:?}",
            report.warnings
        );
        let state = ctx.determinism.expect("determinism state seeded");
        assert!(
            !state
                .compile_time_allowlist
                .iter()
                .any(|(n, _)| n == "gpg-signature.asc"),
            "no gpg-signature.asc allowlist entry when only cosign is configured"
        );
    }
}
