use anodizer_core::context::Context;
use anodizer_core::http::blocking_client;
use anodizer_core::log::StageLogger;
use anodizer_core::preflight::{PreflightEntry, PreflightReport, PublisherState};
use anodizer_core::retry::{RetryLog, RetryPolicy, SuccessClass, retry_http_blocking};
use anyhow::Result;
use std::time::Duration;

use crate::util;

use super::*;

/// Abstraction over a single publisher's state query so tests can inject
/// mock implementations without touching the network.
pub trait PreflightChecker: Send + Sync {
    /// Human-readable publisher name used in report entries.
    fn publisher_name(&self) -> &str;
    /// Query the remote registry for `package` at `version`. `log` surfaces
    /// per-attempt retry warns from the underlying HTTP probes.
    fn check(&self, package: &str, version: &str, log: &StageLogger) -> PublisherState;
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

    fn check(&self, package: &str, version: &str, log: &StageLogger) -> PublisherState {
        let url = crate::cargo::sparse_index_url(package);
        match query_crates_io(&url, package, version, &self.policy, log) {
            Ok(true) => PublisherState::Published,
            Ok(false) => PublisherState::Clean,
            Err(e) => PublisherState::Unknown {
                reason: format!("{e:#}"),
            },
        }
    }
}

/// Returns `Ok(true)` when the version is in the sparse index, `Ok(false)`
/// when it is absent (including 404 = crate never published).
pub(super) fn query_crates_io(
    url: &str,
    crate_name: &str,
    version: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<bool> {
    let client = blocking_client(Duration::from_secs(10))?;
    let label = format!("preflight: crates.io index for '{}'", crate_name);
    let result = retry_http_blocking(
        RetryLog::new(&label, log),
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

    fn check(&self, package: &str, version: &str, log: &StageLogger) -> PublisherState {
        use crate::chocolatey::package::{FeedHashResult, classify_moderation, package_feed_hash};

        match package_feed_hash(&self.source, package, version, &self.policy, log) {
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

    fn check(&self, package: &str, version: &str, log: &StageLogger) -> PublisherState {
        // Search for an open PR in microsoft/winget-pkgs whose title contains
        // `<PackageIdentifier> <version>`. anodizer's convention is to title
        // the PR `"New version: <PackageIdentifier> version <Version>"`, but
        // GitHub's `in:title` matches words independently so the query
        // works for any title that mentions both tokens.
        match query_winget_pr(package, version, self.token.as_deref(), &self.policy, log) {
            Ok(WingetPrLookup::Found(url)) => PublisherState::PRPending(url),
            Ok(WingetPrLookup::NotFound) => PublisherState::Clean,
            Ok(WingetPrLookup::ItemWithoutUrl) => PublisherState::Unknown {
                reason: "winget search response missing html_url".into(),
            },
            Err(e) => PublisherState::Unknown {
                reason: format!("{e:#}"),
            },
        }
    }
}

/// Three-way result for the winget PR lookup so the caller can distinguish
/// "no PR" from "PR row returned but `html_url` was missing" — the second
/// case used to fall back to the listing URL, which is not a PR.
#[derive(Debug)]
pub(super) enum WingetPrLookup {
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
    log: &StageLogger,
) -> Result<WingetPrLookup> {
    let query = format!(
        "repo:microsoft/winget-pkgs is:pr is:open {} {} in:title",
        package, version
    );
    let encoded = anodizer_core::url::percent_encode_unreserved(&query);
    // The [`PreflightChecker`] trait carries no env plumbing, so the base
    // resolves against the process env directly — the same source every
    // production caller of this override reads. Tests inject a responder
    // URL via [`query_winget_pr_at`] instead.
    let base = anodizer_core::http::github_api_base(&anodizer_core::ProcessEnvSource);
    let url = format!("{}/search/issues?q={}&per_page=1", base, encoded);
    query_winget_pr_at(&url, token, policy, log)
}

/// Variant of [`query_winget_pr`] that takes a pre-built URL. Sole call site
/// for the HTTP+parse plumbing — exposed so tests can substitute a local
/// mock-server URL while still exercising the retry / parse pipeline
/// end-to-end.
pub(super) fn query_winget_pr_at(
    url: &str,
    token: Option<&str>,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<WingetPrLookup> {
    let token_clone = token.map(str::to_string);
    let url_clone = url.to_string();
    let label = format!("preflight: winget PR search ({})", url);

    let client = blocking_client(Duration::from_secs(15))?;
    let result = retry_http_blocking(
        RetryLog::new(&label, log),
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

    fn check(&self, package: &str, version: &str, log: &StageLogger) -> PublisherState {
        match query_aur_rpc(package, version, &self.policy, log) {
            // AUR allows the same version to be re-pushed (it's a git push to
            // the AUR repo), so the row's existence is informational rather
            // than a blocker. Surface as Unknown with a reason so the report
            // is honest about it instead of pretending the version is sealed.
            Ok(true) => PublisherState::Unknown {
                reason: "AUR is informational — overwritable on republish".into(),
            },
            Ok(false) => PublisherState::Clean,
            Err(e) => PublisherState::Unknown {
                reason: format!("{e:#}"),
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
pub(super) fn query_aur_rpc(
    package: &str,
    version: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<bool> {
    let url = format!("https://aur.archlinux.org/rpc/v5/info?arg[]={}", package);
    query_aur_rpc_at(&url, version, policy, log)
}

/// Variant of [`query_aur_rpc`] that takes a pre-built URL. Sole call site
/// for the HTTP+parse plumbing — exposed so tests can substitute a local
/// mock-server URL while still exercising the retry / parse pipeline
/// end-to-end.
pub(super) fn query_aur_rpc_at(
    url: &str,
    version: &str,
    policy: &RetryPolicy,
    log: &StageLogger,
) -> Result<bool> {
    let client = blocking_client(Duration::from_secs(10))?;
    let label = format!("preflight: AUR RPC ({})", url);
    let url_clone = url.to_string();
    let result = retry_http_blocking(
        RetryLog::new(&label, log),
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
pub fn run_preflight(ctx: &mut Context, log: &StageLogger) -> Result<PreflightReport> {
    // Production entry point: wires the REAL `cargo publish --dry-run` spawn
    // for the publish-simulation preflight.
    let dry_run_runner = |krate: &str| run_cargo_dry_run(krate, log);
    run_preflight_inner(ctx, log, &RealCheckerFactory, &dry_run_runner, true)
}

/// [`run_preflight`] with the checker construction injected — exposed so
/// tests can drive the orchestration without spawning HTTP servers.
///
/// The publish-simulation dry-run runner is a NO-OP here: this seam exists for
/// publisher-state / rollback-scope tests, none of which configure real
/// workspace crates, so spawning `cargo publish --dry-run` would only produce
/// spurious "package ID did not match" noise. Tests that target the simulation
/// drive [`run_cargo_publish_simulation_with`] directly with an injected index
/// query + runner.
pub fn run_preflight_with_factory(
    ctx: &mut Context,
    log: &StageLogger,
    factory: &dyn CheckerFactory,
) -> Result<PreflightReport> {
    run_preflight_inner(ctx, log, factory, &noop_dry_run_runner, false)
}

/// A dry-run runner that never spawns and always reports the simulation
/// unavailable, so the caller degrades to the index-only partial-publish
/// check (which contributes no blocker on a clean/single-crate fixture).
/// Used by every preflight seam except the production [`run_preflight`].
pub(super) fn noop_dry_run_runner(_krate: &str) -> DryRunOutcome {
    DryRunOutcome::Unavailable("dry-run simulation disabled in this preflight path".into())
}

/// The single orchestrator behind every `run_preflight*` entry point. Threads
/// the checker factory AND the publish-simulation dry-run runner so each is
/// independently injectable. Production wires the real implementations; tests
/// wire stubs (no-op runner by default — see the `*_with_factory*` wrappers).
fn run_preflight_inner(
    ctx: &mut Context,
    log: &StageLogger,
    factory: &dyn CheckerFactory,
    dry_run_runner: &DryRunRunner<'_>,
    live_publisher_preflight: bool,
) -> Result<PreflightReport> {
    let mut report = PreflightReport::new();
    // Pre-publish state queries are an advisory gate, not a write that must
    // land; the shallow probe policy keeps a wedged endpoint from stalling the
    // gate across every configured publisher (the prod ladder is ~27min worst
    // case). Per-request HTTP timeouts still bound each attempt.
    let policy = RetryPolicy::PREFLIGHT;
    let version = ctx.version();

    // Walk every crate in the universe and collect per-publisher entries.
    let crates = ctx.config.crate_universe();
    let selected = &ctx.options.selected_crates;

    // A publisher deselected by `--skip` / `--publishers` will not run this
    // invocation, so its one-way-door state is irrelevant — probing it gates
    // the release on an upstream transition the run never makes. The canonical
    // case: the GH-hosted `publish-npm` job runs `--publish-only --publishers
    // npm`, and the winget PR that the SAME release's `Publish Release` job
    // opened minutes earlier is still pending; without this guard the npm job
    // blocks on `winget (pr-pending)` — a door it does not touch. Mirrors the
    // identical `publisher_deselected` filter the rollback-scope gate applies.
    let probe = |name: &str| !ctx.publisher_deselected(name);

    for krate in &crates {
        if !selected.is_empty() && !selected.contains(&krate.name) {
            continue;
        }
        let publish = match krate.publish.as_ref() {
            Some(p) => p,
            None => continue,
        };

        // ---- cargo -------------------------------------------------------
        if publish.cargo.is_some() && probe("cargo") {
            log.verbose(&format!("checking cargo for '{}@{}'", krate.name, version));
            let checker = factory.cargo(policy);
            let state = checker.check(&krate.name, &version, log);
            report.push(PreflightEntry {
                publisher: checker.publisher_name().to_string(),
                package: krate.name.clone(),
                version: version.clone(),
                state,
            });
        }

        // ---- chocolatey --------------------------------------------------
        if let Some(ref choco_cfg) = publish.chocolatey
            && probe("chocolatey")
        {
            let source = choco_cfg
                .source_repo
                .as_deref()
                .unwrap_or("https://push.chocolatey.org/")
                .to_string();
            let pkg_name = choco_cfg.name.as_deref().unwrap_or(&krate.name).to_string();
            log.verbose(&format!(
                "checking chocolatey for '{}@{}'",
                pkg_name, version
            ));
            let checker = factory.chocolatey(source, policy);
            let state = checker.check(&pkg_name, &version, log);
            report.push(PreflightEntry {
                publisher: checker.publisher_name().to_string(),
                package: pkg_name,
                version: version.clone(),
                state,
            });
        }

        // ---- winget ------------------------------------------------------
        if let Some(ref winget_cfg) = publish.winget
            && probe("winget")
        {
            let pkg_id = winget_cfg
                .package_identifier
                .as_deref()
                .or(winget_cfg.name.as_deref())
                .unwrap_or(&krate.name)
                .to_string();
            let token = util::resolve_repo_token(ctx, winget_cfg.repository.as_ref(), None);
            log.verbose(&format!("checking winget for '{}@{}'", pkg_id, version));
            let checker = factory.winget(token, policy);
            let state = checker.check(&pkg_id, &version, log);
            report.push(PreflightEntry {
                publisher: checker.publisher_name().to_string(),
                package: pkg_id,
                version: version.clone(),
                state,
            });
        }

        // ---- aur ---------------------------------------------------------
        if let Some(ref aur_cfg) = publish.aur
            && probe("aur")
        {
            let pkg_name = aur_cfg
                .name
                .as_deref()
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("{}-bin", krate.name));
            log.verbose(&format!("checking AUR for '{}@{}'", pkg_name, version));
            let checker = factory.aur(policy);
            let state = checker.check(&pkg_name, &version, log);
            report.push(PreflightEntry {
                publisher: checker.publisher_name().to_string(),
                package: pkg_name,
                version: version.clone(),
                state,
            });
        }
    }

    run_publisher_preflight_extension(ctx, &mut report, live_publisher_preflight)?;

    run_cargo_publish_simulation(ctx, log, &mut report, factory, dry_run_runner);

    Ok(report)
}
