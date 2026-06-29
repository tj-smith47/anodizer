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
        let url = crate::cargo::sparse_index_url(package);
        match query_crates_io(&url, package, version, &self.policy) {
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
                reason: format!("{e:#}"),
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
    // Production entry point: wires the REAL `cargo publish --dry-run` spawn
    // for the publish-simulation preflight.
    let dry_run_runner = |krate: &str| run_cargo_dry_run(krate, log);
    run_preflight_inner(
        ctx,
        log,
        &RealCheckerFactory,
        anodizer_core::signing::gpg_supports_faked_system_time,
        &dry_run_runner,
        true,
    )
}

/// [`run_preflight`] with the checker construction injected — exposed so
/// tests can drive the orchestration without spawning HTTP servers. Uses
/// the real gpg probe; tests that need to drive the probe use
/// [`run_preflight_with_factory_and_gpg_probe`] instead.
///
/// The publish-simulation dry-run runner is a NO-OP here: this seam exists for
/// publisher-state / rollback-scope / gpg-probe tests, none of which configure
/// real workspace crates, so spawning `cargo publish --dry-run` would only
/// produce spurious "package ID did not match" noise. Tests that target the
/// simulation drive [`run_cargo_publish_simulation_with`] directly with an
/// injected index query + runner.
pub fn run_preflight_with_factory(
    ctx: &mut Context,
    log: &StageLogger,
    factory: &dyn CheckerFactory,
) -> Result<PreflightReport> {
    run_preflight_inner(
        ctx,
        log,
        factory,
        anodizer_core::signing::gpg_supports_faked_system_time,
        &noop_dry_run_runner,
        false,
    )
}

/// Like [`run_preflight_with_factory`] but with the gpg `--faked-system-time`
/// capability probe also injected. Tests pass a closure returning the
/// canned support state without spawning a real `gpg` subprocess.
///
/// Uses the NO-OP dry-run runner for the same reason as
/// [`run_preflight_with_factory`] — these are publisher/probe tests, not
/// publish-simulation tests.
pub fn run_preflight_with_factory_and_gpg_probe(
    ctx: &mut Context,
    log: &StageLogger,
    factory: &dyn CheckerFactory,
    gpg_probe: fn() -> bool,
) -> Result<PreflightReport> {
    run_preflight_inner(ctx, log, factory, gpg_probe, &noop_dry_run_runner, false)
}

/// A dry-run runner that never spawns and always reports the simulation
/// unavailable, so the caller degrades to the index-only partial-publish
/// check (which contributes no blocker on a clean/single-crate fixture).
/// Used by every preflight seam except the production [`run_preflight`].
fn noop_dry_run_runner(_krate: &str) -> DryRunOutcome {
    DryRunOutcome::Unavailable("dry-run simulation disabled in this preflight path".into())
}

/// The single orchestrator behind every `run_preflight*` entry point. Threads
/// the checker factory, gpg probe, AND the publish-simulation dry-run runner so
/// each is independently injectable. Production wires the real implementations;
/// tests wire stubs (no-op runner by default — see the `*_with_factory*`
/// wrappers).
fn run_preflight_inner(
    ctx: &mut Context,
    log: &StageLogger,
    factory: &dyn CheckerFactory,
    gpg_probe: fn() -> bool,
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
            log.verbose(&format!("checking cargo for '{}@{}'", krate.name, version));
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
                "checking chocolatey for '{}@{}'",
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
            log.verbose(&format!("checking winget for '{}@{}'", pkg_id, version));
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
            log.verbose(&format!("checking AUR for '{}@{}'", pkg_name, version));
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

    run_publisher_preflight_extension(ctx, &mut report, live_publisher_preflight)?;

    run_gpg_capability_probe(ctx, &mut report, gpg_probe);

    run_cargo_publish_simulation(ctx, log, &mut report, factory, dry_run_runner);

    Ok(report)
}

// ---------------------------------------------------------------------------
// crates.io publish-simulation preflight
// ---------------------------------------------------------------------------

/// Outcome of simulating one crate's `cargo publish --dry-run`.
#[derive(Debug, PartialEq, Eq)]
enum DryRunOutcome {
    /// The dry-run compiled and packaged cleanly.
    Ok,
    /// A verify/compile error — a dependent cannot build against the version
    /// of its dependency that the registry would resolve. Carries the matched
    /// stderr line so the abort message points the operator at the cause.
    CompileError(String),
    /// `cargo` resolved a sibling crate that is itself in the to-publish set
    /// but not yet on the registry. Benign during a real publish (cargo
    /// publishes siblings first), so it must NOT abort.
    BenignSiblingMissing(String),
    /// The dry-run could not run for an environmental reason (cargo absent,
    /// spawn failure). The caller degrades to the partial-publish check rather
    /// than failing the release on infrastructure.
    Unavailable(String),
}

/// Pluggable index-state query for the partial-publish check. Production wires
/// [`CargoCratesIo`]; tests inject a closure returning canned states without a
/// network round-trip.
type IndexQuery<'a> = dyn Fn(&str, &str) -> PublisherState + 'a;

/// Pluggable `cargo publish --dry-run` runner. Production wires
/// [`run_cargo_dry_run`] (a real spawn); tests inject a closure or drive the
/// real spawn against a PATH-injected `cargo` stub.
type DryRunRunner<'a> = dyn Fn(&str) -> DryRunOutcome + 'a;

/// Simulate the crates.io publish BEFORE the irreversible cargo publisher
/// fires, aborting (via report blockers) when the workspace cannot publish
/// consistently at the target version.
///
/// Gated to a real release: snapshot / nightly / dry-run runs skip entirely
/// (they never reach crates.io, and the determinism harness rebuilds under
/// `--snapshot` must not spawn cargo or hit the network here).
///
/// Applies the real-release gate, then delegates to
/// [`run_cargo_publish_simulation_with`].
///
/// Both side-effecting seams are injected by the caller (the `run_preflight*`
/// entry points), so no test path ever hits the network or spawns cargo here:
/// - the partial-publish index query routes through the supplied
///   [`CheckerFactory`] — production uses [`RealCheckerFactory`] (a real
///   sparse-index GET); tests inject a mock factory returning canned states;
/// - the dry-run runner is the real `cargo publish --dry-run` spawn in
///   production and [`noop_dry_run_runner`] in every test seam.
fn run_cargo_publish_simulation(
    ctx: &mut Context,
    log: &StageLogger,
    report: &mut PreflightReport,
    factory: &dyn CheckerFactory,
    dry_run_runner: &DryRunRunner<'_>,
) {
    // Gated to a REAL release that actually reaches crates.io. Snapshot /
    // nightly / dry-run never publish (and the determinism harness rebuilds
    // under `--snapshot --skip=...,publish`), and a skipped publish stage has
    // no irreversible door to guard — mirror the prepublish-guard gating.
    if ctx.is_snapshot() || ctx.is_nightly() || ctx.is_dry_run() || ctx.should_skip("publish") {
        return;
    }

    let policy = RetryPolicy::PREFLIGHT;
    let checker = factory.cargo(policy);
    let index_query = |krate: &str, version: &str| checker.check(krate, version);

    run_cargo_publish_simulation_with(ctx, log, report, &index_query, dry_run_runner);
}

/// [`run_cargo_publish_simulation`] with the index query and dry-run runner
/// injected so tests can drive both checks without a network round-trip or a
/// real `cargo` (beyond a PATH-injected stub). Does NOT re-apply the
/// snapshot/nightly/dry-run gate — the production wrapper owns that so tests
/// can exercise the logic directly.
fn run_cargo_publish_simulation_with(
    ctx: &mut Context,
    log: &StageLogger,
    report: &mut PreflightReport,
    index_query: &IndexQuery<'_>,
    dry_run_runner: &DryRunRunner<'_>,
) {
    // Resolve the to-publish set via the same publish-graph derivation the
    // real cargo publisher uses, so the preflight can never disagree about
    // which crates publish or in what order.
    let selected = ctx.options.selected_crates.clone();
    let plan = match crate::cargo::cargo_publish_plan(ctx, &selected, log) {
        Ok(p) => p,
        Err(e) => {
            // A render failure in `skip:`/`if:` would also break the real
            // publish; surface it as a blocker rather than silently skipping.
            report
                .blockers
                .push(format!("cargo publish-simulation: {e:#}"));
            return;
        }
    };

    if plan.order.is_empty() {
        return;
    }

    let to_publish: Vec<(String, String)> = plan
        .order
        .iter()
        .map(|name| {
            let version = plan
                .versions
                .get(name)
                .cloned()
                .unwrap_or_else(|| ctx.version());
            (name.clone(), version)
        })
        .collect();

    // ---- (1) partial-publish abort (cheap, HTTP-only) --------------------
    if check_partial_publish(&to_publish, index_query, log, report) {
        // A mixed index state (or an Unknown transport error) already pushed a
        // blocker; the dependent build can't be simulated meaningfully against
        // an inconsistent registry, so stop here.
        return;
    }

    // ---- (1b) dep-completeness guard -------------------------------------
    // Refuse a release where a publishing crate depends on a workspace crate
    // that is neither in the publish set nor on crates.io. `cargo publish
    // --dry-run` (step 2) does NOT catch this — dry-run resolves the dep via
    // the local workspace PATH — so this guard is the only preflight check
    // for the missing-from-set failure that burned the 0.6.0/0.7.0 CLI
    // publish. Reuses the injected `index_query` so tests drive it without a
    // network round-trip; an inconclusive probe never blocks.
    if check_publish_set_completeness(&plan, index_query, log, report) {
        return;
    }

    // ---- (2) cargo publish --dry-run, dependency order -------------------
    simulate_dry_run_publishes(&to_publish, index_query, dry_run_runner, log, report);
}

/// Preflight wrapper over [`crate::cargo::check_publish_set_completeness`].
///
/// Maps the injected [`IndexQuery`] (which returns [`PublisherState`]) onto
/// the guard's tri-state probe, runs the guard, and converts a guard error
/// into a report blocker so the preflight aborts loudly (same channel as the
/// partial-publish and dry-run checks) instead of bubbling an `Err`. Returns
/// `true` when a blocker was pushed and the caller should stop.
fn check_publish_set_completeness(
    plan: &crate::cargo::CargoPublishPlan,
    index_query: &IndexQuery<'_>,
    log: &StageLogger,
    report: &mut PreflightReport,
) -> bool {
    use crate::cargo::DepIndexState;

    let probe = |name: &str, version: &str| match index_query(name, version) {
        PublisherState::Published => DepIndexState::Present,
        PublisherState::Clean => DepIndexState::Absent,
        // A transport error (Unknown) or any non-crates.io state is treated
        // conservatively — the guard does not fail on an inconclusive probe.
        _ => DepIndexState::Unknown,
    };

    match crate::cargo::check_publish_set_completeness(
        &plan.order,
        &plan.all_crates,
        &plan.versions,
        &probe,
        log,
    ) {
        Ok(()) => false,
        Err(e) => {
            report.blockers.push(format!("{e:#}"));
            true
        }
    }
}

/// (1) PARTIAL-PUBLISH ABORT.
///
/// Query each to-be-published crate's already-published state at its target
/// version and classify the set:
/// - MIXED (≥1 Published AND ≥1 Clean at the same V) → push a blocker naming
///   an example of each, and return `true` (caller stops). crates.io versions
///   are immutable, so a half-published version can never complete.
/// - any Unknown (transport error) → push a blocker (don't silently pass) and
///   return `true`.
/// - all-Clean → fresh release; return `false` (proceed to dry-run).
/// - all-Published → idempotent (the real publish would skip cargo); return
///   `false` (nothing to simulate, no abort).
///
/// Returns `true` when a blocker was pushed and the caller should stop.
fn check_partial_publish(
    to_publish: &[(String, String)],
    index_query: &IndexQuery<'_>,
    log: &StageLogger,
    report: &mut PreflightReport,
) -> bool {
    let mut first_published: Option<(String, String)> = None;
    let mut first_clean: Option<(String, String)> = None;

    for (name, version) in to_publish {
        log.verbose(&format!("checking crates.io state for '{name}@{version}'"));
        match index_query(name, version) {
            PublisherState::Published => {
                if first_published.is_none() {
                    first_published = Some((name.clone(), version.clone()));
                }
            }
            PublisherState::Clean => {
                if first_clean.is_none() {
                    first_clean = Some((name.clone(), version.clone()));
                }
            }
            PublisherState::Unknown { reason } => {
                report.blockers.push(format!(
                    "cargo publish-simulation: could not determine crates.io state for \
                     '{name}@{version}' ({reason}); refusing to start a release that may \
                     partially publish — retry once the registry is reachable"
                ));
                return true;
            }
            // crates.io never yields InModeration / PRPending; treat any other
            // state conservatively as "present" so a mixed set still aborts.
            other => {
                log.verbose(&format!(
                    "'{name}@{version}' reported {other}; treating as published"
                ));
                if first_published.is_none() {
                    first_published = Some((name.clone(), version.clone()));
                }
            }
        }
    }

    match (first_published, first_clean) {
        (Some((pub_name, pub_ver)), Some((clean_name, clean_ver))) => {
            report.blockers.push(format!(
                "crates.io version {pub_ver} is partially published ({pub_name}@{pub_ver} \
                 exists, {clean_name}@{clean_ver} does not); crates.io versions are \
                 immutable, so this release cannot complete consistently — bump to a new \
                 version"
            ));
            true
        }
        // all-Published → idempotent skip; all-Clean → fresh; either proceeds.
        _ => false,
    }
}

/// (2) `cargo publish --dry-run` simulation in dependency order.
///
/// For each still-Clean crate (skipping any already-Published — the real
/// publish would skip those), run the dry-run and classify:
/// - [`DryRunOutcome::Ok`] → continue.
/// - [`DryRunOutcome::CompileError`] → push a blocker and stop: a dependent
///   cannot build against the dependency version the registry resolves (the
///   `probe_dir` failure mode).
/// - [`DryRunOutcome::BenignSiblingMissing`] → continue: cargo couldn't find a
///   sibling that is itself in the to-publish set and will be published first.
/// - [`DryRunOutcome::Unavailable`] → warn and fall back to check (1) (already
///   run) rather than hard-failing the release on infrastructure.
fn simulate_dry_run_publishes(
    to_publish: &[(String, String)],
    index_query: &IndexQuery<'_>,
    dry_run_runner: &DryRunRunner<'_>,
    log: &StageLogger,
    report: &mut PreflightReport,
) {
    let in_set: std::collections::HashSet<&str> =
        to_publish.iter().map(|(n, _)| n.as_str()).collect();

    for (name, version) in to_publish {
        // Skip crates already on the registry — the real publish skips them,
        // and `cargo publish --dry-run` would refuse an existing version.
        if matches!(index_query(name, version), PublisherState::Published) {
            continue;
        }

        log.verbose(&format!(
            "running cargo publish --dry-run -p {name} (publish simulation)"
        ));
        match dry_run_runner(name) {
            DryRunOutcome::Ok => {}
            DryRunOutcome::BenignSiblingMissing(detail) => {
                // Benign ONLY when the unresolved crate is itself in the
                // to-publish set (a sibling the real publish lands first). A
                // missing crate that is NOT in the set is a genuine resolution
                // failure that would also break the real publish — abort.
                if in_set.iter().any(|sib| detail.contains(sib)) {
                    log.verbose(&format!(
                        "'{name}' dry-run resolved a not-yet-published sibling \
                         ({detail}); benign — the real publish orders siblings first"
                    ));
                } else {
                    report.blockers.push(format!(
                        "cargo publish-simulation: `cargo publish --dry-run -p {name}` could \
                         not resolve a dependency ({detail}); it is not a workspace crate this \
                         release publishes, so the real publish would fail the same way — fix \
                         the dependency before releasing"
                    ));
                    return;
                }
            }
            DryRunOutcome::CompileError(detail) => {
                report.blockers.push(format!(
                    "cargo publish-simulation: `cargo publish --dry-run -p {name}` failed to \
                     build ({detail}); a published dependency is missing API this crate needs, \
                     so the real publish would fire the irreversible cargo publisher and then \
                     fail mid-release — bump to a new version or fix the dependency"
                ));
                return;
            }
            DryRunOutcome::Unavailable(detail) => {
                log.warn(&format!(
                    "skipped `cargo publish --dry-run -p {name}` — {detail} \
                     (relying on the partial-publish index check alone)"
                ));
            }
        }
    }
}

/// Spawn `cargo publish --dry-run -p <crate>` and classify the result.
///
/// Best-effort: a spawn failure (cargo absent / not executable) yields
/// [`DryRunOutcome::Unavailable`] so the caller degrades gracefully rather
/// than failing the release on a missing toolchain.
fn run_cargo_dry_run(crate_name: &str, log: &StageLogger) -> DryRunOutcome {
    run_cargo_dry_run_with_binary(std::path::Path::new("cargo"), crate_name, log)
}

/// Path-taking sibling of [`run_cargo_dry_run`]: `cargo_binary` is the
/// binary to spawn. Production passes `Path::new("cargo")` (PATH
/// lookup); tests point at a nonexistent path to exercise the
/// spawn-failure branch without clobbering the process-wide `PATH`
/// (which would make every concurrent PATH-resolved spawn in the test
/// binary flaky). Same seam convention as
/// `core::git::gh_api_get_with_binary`.
fn run_cargo_dry_run_with_binary(
    cargo_binary: &std::path::Path,
    crate_name: &str,
    log: &StageLogger,
) -> DryRunOutcome {
    use std::process::Command;

    let output = Command::new(cargo_binary)
        .args(["publish", "--dry-run", "-p", crate_name])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => return DryRunOutcome::Unavailable(format!("spawn cargo: {e}")),
    };

    if output.status.success() {
        return DryRunOutcome::Ok;
    }

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    log.verbose(&format!(
        "`cargo publish --dry-run -p {crate_name}` exited non-zero:\n{}",
        anodizer_core::redact::redact_bearer_tokens(stderr.trim_end())
    ));
    classify_dry_run_stderr(&stderr)
}

/// Classify a failed `cargo publish --dry-run` stderr into a [`DryRunOutcome`].
///
/// - A "did not match any packages" / "package ID specification" line means
///   `-p <crate>` didn't resolve to a workspace member in THIS context (a
///   degenerate/test invocation, never a real release) → [`DryRunOutcome::Unavailable`].
/// - A "no matching package" / "failed to select a version" line naming a
///   crate is treated as a (potentially benign) missing-sibling signal: the
///   caller decides benign-vs-blocker by checking whether the named crate is
///   in the to-publish set.
/// - A compile/verify error (`error[E…]`, "cannot find …", "could not
///   compile") is a hard [`DryRunOutcome::CompileError`].
/// - Anything else non-zero is conservatively a `CompileError` (an unexpected
///   verify failure should abort, not pass) — except a pure registry/network
///   complaint, which degrades to `Unavailable`.
fn classify_dry_run_stderr(stderr: &str) -> DryRunOutcome {
    let lower = stderr.to_ascii_lowercase();

    // Invocation/environment artifact: the crate named by `-p` is not a
    // resolvable workspace member in THIS context (e.g. a synthetic test
    // fixture, or a sparse checkout). In a real release the crate IS a
    // workspace member, so this string only appears in degenerate contexts —
    // treat it as Unavailable (warn + fall back to the index check), never a
    // hard blocker.
    if lower.contains("did not match any packages") || lower.contains("package id specification") {
        return DryRunOutcome::Unavailable(first_nonempty_line(stderr));
    }

    // Missing-package signal (a dependency cargo could not resolve on the
    // registry). Carry the offending line; the caller distinguishes a
    // to-publish sibling (benign) from a genuinely-absent external dep.
    if lower.contains("no matching package")
        || lower.contains("failed to select a version")
        || lower.contains("could not find")
    {
        let line = first_line_matching(
            stderr,
            &["no matching package", "failed to select", "could not find"],
        );
        return DryRunOutcome::BenignSiblingMissing(line);
    }

    // Compile / verify failure — the dependent can't build against its
    // published dependency (the probe_dir case).
    if lower.contains("error[e")
        || lower.contains("cannot find function")
        || lower.contains("cannot find ")
        || lower.contains("could not compile")
        || lower.contains("unresolved import")
    {
        let line = first_line_matching(
            stderr,
            &[
                "error[e",
                "cannot find",
                "could not compile",
                "unresolved import",
            ],
        );
        return DryRunOutcome::CompileError(line);
    }

    // Pure registry/network failure → environmental, not a code problem.
    if lower.contains("failed to download")
        || lower.contains("network failure")
        || lower.contains("spurious network error")
        || lower.contains("error: failed to get successful http response")
    {
        return DryRunOutcome::Unavailable(first_nonempty_line(stderr));
    }

    // Unknown non-zero exit: conservatively treat as a compile/verify failure
    // so a real problem aborts rather than slipping past the gate.
    DryRunOutcome::CompileError(first_nonempty_line(stderr))
}

/// First stderr line containing any of `needles` (case-insensitive), trimmed.
/// Falls back to the first non-empty line.
fn first_line_matching(stderr: &str, needles: &[&str]) -> String {
    for line in stderr.lines() {
        let lower = line.to_ascii_lowercase();
        if needles.iter().any(|n| lower.contains(n)) {
            return line.trim().to_string();
        }
    }
    first_nonempty_line(stderr)
}

/// First non-empty, trimmed stderr line (or a placeholder when stderr is bare).
fn first_nonempty_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("non-zero exit, no diagnostic")
        .to_string()
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
    // Publisher `preflight()` hooks can perform live credential / repo probes
    // (cargo/npm token validity, GitHub-repo write scope, AUR ssh auth). The
    // production `run_preflight` enables them; the injected-factory test seams
    // disable them so unit tests stay hermetic (the rollback-scope branch below
    // is pure and always runs).
    live_publisher_preflight: bool,
) -> Result<()> {
    let publishers = crate::registry::configured_publishers(ctx);
    let mut required_missing_scope: Vec<String> = Vec::new();

    for p in &publishers {
        // Mirror the run path's skip set (`dispatch::dispatch`): a publisher
        // the release will not run must contribute nothing to the gate — no
        // live credential/repo probe AND no rollback-scope blocker. Probing a
        // deselected (`--skip`/`--publishers`) or nightly-skipped publisher can
        // manufacture a false Blocker against an irreversible door that never
        // opens this run.
        if ctx.publisher_deselected(p.name()) || (ctx.is_nightly() && p.skips_on_nightly()) {
            continue;
        }

        // ---- rollback scope check ------------------------------------
        if let Some(label) = p.rollback_scope_needed()
            && !crate::scope::scope_available_with_env(label, ctx.env_source())
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
        if !live_publisher_preflight {
            continue;
        }
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
                reason: format!("{e:#}"),
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

    // ---- percent_encode (GitHub search query encoder) -------------------

    #[test]
    fn percent_encode_space_becomes_plus() {
        // Spaces in the search query must become `+`, and the query operators
        // anodizer emits (`repo:`, `is:pr`, `in:title`) must round-trip
        // unescaped so the GitHub search syntax survives the encode.
        assert_eq!(
            percent_encode("repo:microsoft/winget-pkgs is:pr in:title"),
            "repo:microsoft/winget-pkgs+is:pr+in:title"
        );
    }

    #[test]
    fn percent_encode_passes_through_unreserved() {
        // Alphanumerics plus the explicit safe set `-._~/:` must NOT be
        // escaped — escaping them would corrupt package identifiers and the
        // search operators.
        let safe = "Abc123-._~/:";
        assert_eq!(percent_encode(safe), safe);
    }

    #[test]
    fn percent_encode_escapes_reserved_and_unicode_bytes() {
        // A reserved ASCII char (`#`) and a multi-byte UTF-8 char (`é`,
        // 0xC3 0xA9) must each percent-escape every byte as uppercase hex.
        assert_eq!(percent_encode("a#é"), "a%23%C3%A9");
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

    #[test]
    fn chocolatey_checker_present_without_hash_is_published() {
        // A 200 OData entry that exists but omits PackageHash maps to
        // FeedHashResult::PresentNoHash → the version is taken (Published),
        // never Clean — an unreadable hash must not let a published version
        // slip the preflight gate.
        let body = r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<entry>
  <id>http://example.com/api/v2/Packages(Id='foo',Version='1.0.0')</id>
  <m:properties><d:PackageStatus>Approved</d:PackageStatus></m:properties>
</entry>"#
            .to_string();
        let (addr, _calls) = spawn_oneshot_http_responder(vec![choco_http_resp(body)]);
        let source = format!("http://{}/", addr);

        let checker = Chocolatey::new(source, fast_retry());
        let state = checker.check("foo", "1.0.0");
        assert!(
            matches!(state, PublisherState::Published),
            "present-but-hashless row must be Published, got: {:?}",
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
    // These tests resolve rollback-scope token availability
    // (CARGO_REGISTRY_TOKEN, GITHUB_TOKEN, ANODIZER_GITHUB_TOKEN) through
    // the Context's injected `EnvSource` (`scope_available_with_env`), so
    // they inject or omit tokens via a `MapEnvSource` installed with
    // `ctx.set_env_source(..)` rather than mutating process-wide env. No
    // shared-lock serialization is needed.

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
    fn preflight_warns_on_missing_rollback_scope() {
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut ctx = fixture_cargo_publisher(false, None);
        // Omit CARGO_REGISTRY_TOKEN so the scope reads as missing.
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
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
    fn preflight_blocks_on_missing_rollback_scope_when_strict() {
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut ctx = fixture_cargo_publisher(true, None);
        // Omit CARGO_REGISTRY_TOKEN so the scope reads as missing.
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
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
    fn preflight_bails_when_required_publisher_missing_scope_and_rollback_best_effort() {
        use anodizer_core::context::RollbackMode;
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut ctx = fixture_cargo_publisher(false, Some(RollbackMode::BestEffort));
        // Omit CARGO_REGISTRY_TOKEN so the scope reads as missing.
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
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

    #[test]
    fn deselected_publisher_is_not_preflighted() {
        use anodizer_core::context::RollbackMode;
        use anodizer_core::log::{StageLogger, Verbosity};

        // Same fixture that bails in
        // `preflight_bails_when_required_publisher_missing_scope_and_rollback_best_effort`
        // — except cargo is now deselected via `--skip cargo`, so the run path
        // would never run it and the gate must not bail (nor warn) on it.
        let mut ctx = fixture_cargo_publisher(false, Some(RollbackMode::BestEffort));
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
        ctx.options.skip_stages = vec!["cargo".to_string()];
        let log = StageLogger::new("preflight", Verbosity::Normal);
        let factory = empty_factory();

        let report = run_preflight_with_factory(&mut ctx, &log, &factory)
            .expect("a deselected required publisher must not bail the rollback-scope gate");
        assert!(
            report.blockers.is_empty(),
            "deselected cargo must contribute no blocker: {:?}",
            report.blockers
        );
        assert!(
            !report.warnings.iter().any(|w| w.contains("cargo")),
            "deselected cargo must contribute no scope warning: {:?}",
            report.warnings
        );
    }

    #[test]
    fn nightly_skipped_publisher_is_not_preflighted() {
        use anodizer_core::context::RollbackMode;
        use anodizer_core::log::{StageLogger, Verbosity};

        // cargo `skips_on_nightly() == true`; under `--nightly` it never runs,
        // so its missing rollback scope must not bail the best-effort gate.
        let mut ctx = fixture_cargo_publisher(false, Some(RollbackMode::BestEffort));
        ctx.set_env_source(anodizer_core::MapEnvSource::new());
        ctx.options.nightly = true;
        let log = StageLogger::new("preflight", Verbosity::Normal);
        let factory = empty_factory();

        let report = run_preflight_with_factory(&mut ctx, &log, &factory)
            .expect("a nightly-skipped required publisher must not bail the rollback-scope gate");
        assert!(
            report.blockers.is_empty(),
            "nightly-skipped cargo must contribute no blocker: {:?}",
            report.blockers
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
        fn skips_on_nightly(&self) -> bool {
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
    fn preflight_honors_anodizer_github_token_fallback() {
        use anodizer_core::config::{
            Config, CrateConfig, HomebrewConfig, PublishConfig, RepositoryConfig,
        };
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

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
        // Omit GITHUB_TOKEN but provide ANODIZER_GITHUB_TOKEN: the fallback
        // must satisfy the GITHUB_TOKEN scope through the injected source.
        ctx.set_env_source(
            anodizer_core::MapEnvSource::new().with("ANODIZER_GITHUB_TOKEN", "fallback-token"),
        );
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

    // -----------------------------------------------------------------------
    // crates.io publish-simulation preflight (task #25)
    // -----------------------------------------------------------------------
    mod publish_simulation {
        use super::super::*;
        use anodizer_core::config::{CargoPublishConfig, CrateConfig, PublishConfig};
        use anodizer_core::context::Context;
        use anodizer_core::log::{StageLogger, Verbosity};
        use anodizer_core::preflight::{PreflightReport, PublisherState};
        use anodizer_core::test_helpers::TestContextBuilder;

        fn quiet_log() -> StageLogger {
            StageLogger::new("publish-sim-test", Verbosity::Normal)
        }

        /// A checker factory whose `.cargo()` checker panics if ever invoked —
        /// proves the real-release gate short-circuits before the index query
        /// (or the dry-run runner) is touched.
        struct PanicFactory;

        struct PanicChecker;
        impl PreflightChecker for PanicChecker {
            fn publisher_name(&self) -> &str {
                "cargo"
            }
            fn check(&self, _package: &str, _version: &str) -> PublisherState {
                panic!("gated-out simulation must never query the index")
            }
        }

        impl CheckerFactory for PanicFactory {
            fn cargo(&self, _policy: RetryPolicy) -> Box<dyn PreflightChecker> {
                Box::new(PanicChecker)
            }
            fn chocolatey(&self, _src: String, _p: RetryPolicy) -> Box<dyn PreflightChecker> {
                Box::new(PanicChecker)
            }
            fn winget(&self, _t: Option<String>, _p: RetryPolicy) -> Box<dyn PreflightChecker> {
                Box::new(PanicChecker)
            }
            fn aur(&self, _p: RetryPolicy) -> Box<dyn PreflightChecker> {
                Box::new(PanicChecker)
            }
        }

        /// A dry-run runner that panics if invoked — paired with [`PanicFactory`]
        /// so a gated-out simulation proves it spawns nothing.
        fn panic_runner(_krate: &str) -> DryRunOutcome {
            panic!("gated-out simulation must never spawn cargo")
        }

        /// A cargo-eligible crate with the given workspace-internal deps.
        fn cargo_crate(name: &str, deps: &[&str]) -> CrateConfig {
            CrateConfig {
                name: name.to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
                publish: Some(PublishConfig {
                    cargo: Some(CargoPublishConfig::default()),
                    ..Default::default()
                }),
                ..Default::default()
            }
        }

        fn two_crate_ctx() -> Context {
            TestContextBuilder::new()
                .crates(vec![
                    cargo_crate("anodizer-stage-blob", &["anodizer-core"]),
                    cargo_crate("anodizer-core", &[]),
                ])
                .build()
        }

        // ---- (1) partial-publish abort ----------------------------------

        #[test]
        fn partial_publish_mixed_state_aborts() {
            let mut ctx = two_crate_ctx();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            // core already on the index, stage-blob not — the exact poison.
            let index = |krate: &str, _v: &str| {
                if krate == "anodizer-core" {
                    PublisherState::Published
                } else {
                    PublisherState::Clean
                }
            };
            // Dry-run must never run once partial-publish aborts.
            let dry = |krate: &str| -> DryRunOutcome {
                panic!("dry-run must not run on a mixed index state (ran for {krate})")
            };
            run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);

            assert_eq!(report.blockers.len(), 1, "exactly one partial blocker");
            let b = &report.blockers[0];
            assert!(b.contains("partially published"), "blocker: {b}");
            assert!(
                b.contains("anodizer-core"),
                "names the published crate: {b}"
            );
            assert!(
                b.contains("anodizer-stage-blob"),
                "names the clean crate: {b}"
            );
            assert!(b.contains("bump to a new version"), "actionable: {b}");
        }

        #[test]
        fn all_clean_proceeds_no_blocker() {
            let mut ctx = two_crate_ctx();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            let index = |_krate: &str, _v: &str| PublisherState::Clean;
            // All clean → dry-run runs for both, all succeed.
            let dry = |_krate: &str| DryRunOutcome::Ok;
            run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);
            assert!(
                report.blockers.is_empty(),
                "all-clean must not block: {:?}",
                report.blockers
            );
        }

        #[test]
        fn all_published_idempotent_proceeds_no_blocker() {
            let mut ctx = two_crate_ctx();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            let index = |_krate: &str, _v: &str| PublisherState::Published;
            // Every crate already published → dry-run must be skipped entirely.
            let dry = |krate: &str| -> DryRunOutcome {
                panic!("dry-run must skip already-published crates (ran for {krate})")
            };
            run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);
            assert!(
                report.blockers.is_empty(),
                "all-published is idempotent, must not block: {:?}",
                report.blockers
            );
        }

        #[test]
        fn unknown_transport_error_is_surfaced_not_silently_passed() {
            let mut ctx = two_crate_ctx();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            let index = |krate: &str, _v: &str| {
                if krate == "anodizer-core" {
                    PublisherState::Unknown {
                        reason: "connection reset".into(),
                    }
                } else {
                    PublisherState::Clean
                }
            };
            let dry = |_krate: &str| DryRunOutcome::Ok;
            run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);
            assert_eq!(report.blockers.len(), 1, "Unknown surfaces a blocker");
            let b = &report.blockers[0];
            assert!(b.contains("could not determine crates.io state"), "{b}");
            assert!(b.contains("connection reset"), "carries the reason: {b}");
        }

        // ---- (2) dry-run classification ---------------------------------

        #[test]
        fn dry_run_compile_error_aborts() {
            let mut ctx = two_crate_ctx();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            let index = |_krate: &str, _v: &str| PublisherState::Clean;
            let dry = |krate: &str| {
                if krate == "anodizer-stage-blob" {
                    DryRunOutcome::CompileError(
                        "error[E0425]: cannot find function `probe_dir`".into(),
                    )
                } else {
                    DryRunOutcome::Ok
                }
            };
            run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);
            assert_eq!(report.blockers.len(), 1, "compile error aborts");
            let b = &report.blockers[0];
            assert!(b.contains("failed to build"), "{b}");
            assert!(
                b.contains("probe_dir"),
                "carries the compiler diagnostic: {b}"
            );
            assert!(b.contains("anodizer-stage-blob"), "names the crate: {b}");
        }

        #[test]
        fn dry_run_missing_sibling_in_set_is_benign() {
            let mut ctx = two_crate_ctx();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            let index = |_krate: &str, _v: &str| PublisherState::Clean;
            // stage-blob can't resolve anodizer-core (a sibling published first
            // in the real run) — benign, must NOT abort.
            let dry = |krate: &str| {
                if krate == "anodizer-stage-blob" {
                    DryRunOutcome::BenignSiblingMissing(
                        "no matching package named `anodizer-core` found".into(),
                    )
                } else {
                    DryRunOutcome::Ok
                }
            };
            run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);
            assert!(
                report.blockers.is_empty(),
                "missing in-set sibling is benign: {:?}",
                report.blockers
            );
        }

        #[test]
        fn dry_run_missing_external_dep_aborts() {
            let mut ctx = two_crate_ctx();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            let index = |_krate: &str, _v: &str| PublisherState::Clean;
            // A missing crate that is NOT in the to-publish set is a real
            // resolution failure that would also break the real publish.
            let dry = |krate: &str| {
                if krate == "anodizer-stage-blob" {
                    DryRunOutcome::BenignSiblingMissing(
                        "no matching package named `some-external-crate` found".into(),
                    )
                } else {
                    DryRunOutcome::Ok
                }
            };
            run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);
            assert_eq!(report.blockers.len(), 1, "missing external dep aborts");
            assert!(
                report.blockers[0].contains("could not resolve a dependency"),
                "{}",
                report.blockers[0]
            );
        }

        #[test]
        fn dry_run_unavailable_falls_back_to_index_check_no_block() {
            let mut ctx = two_crate_ctx();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            let index = |_krate: &str, _v: &str| PublisherState::Clean;
            // cargo unavailable → warn + fall back to (1), which already passed.
            let dry = |_krate: &str| DryRunOutcome::Unavailable("cargo not on PATH".into());
            run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);
            assert!(
                report.blockers.is_empty(),
                "infrastructure failure must not hard-fail the release: {:?}",
                report.blockers
            );
        }

        // ---- gating ------------------------------------------------------

        #[test]
        fn snapshot_skips_simulation_entirely() {
            let mut ctx = TestContextBuilder::new()
                .crates(vec![cargo_crate("anodizer-core", &[])])
                .snapshot(true)
                .build();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            // The wrapper owns the gate; PanicFactory + panic_runner prove it
            // never queries the index or spawns cargo under snapshot.
            run_cargo_publish_simulation(&mut ctx, &log, &mut report, &PanicFactory, &panic_runner);
            assert!(report.blockers.is_empty());
        }

        #[test]
        fn dry_run_mode_skips_simulation_entirely() {
            let mut ctx = TestContextBuilder::new()
                .crates(vec![cargo_crate("anodizer-core", &[])])
                .dry_run(true)
                .build();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            run_cargo_publish_simulation(&mut ctx, &log, &mut report, &PanicFactory, &panic_runner);
            assert!(report.blockers.is_empty());
        }

        #[test]
        fn nightly_skips_simulation_entirely() {
            let mut ctx = TestContextBuilder::new()
                .crates(vec![cargo_crate("anodizer-core", &[])])
                .build();
            ctx.options.nightly = true;
            let log = quiet_log();
            let mut report = PreflightReport::new();
            run_cargo_publish_simulation(&mut ctx, &log, &mut report, &PanicFactory, &panic_runner);
            assert!(report.blockers.is_empty());
        }

        #[test]
        fn skipped_publish_stage_skips_simulation_entirely() {
            let mut ctx = TestContextBuilder::new()
                .crates(vec![cargo_crate("anodizer-core", &[])])
                .skip_stages(vec!["publish".to_string()])
                .build();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            run_cargo_publish_simulation(&mut ctx, &log, &mut report, &PanicFactory, &panic_runner);
            assert!(report.blockers.is_empty());
        }

        /// Regression: `run_preflight_with_factory` (the test seam used by the
        /// rollback-scope / publisher-state tests) must NOT spawn cargo or hit
        /// the network for a configured cargo crate. The injected factory
        /// reports the crate Clean; the default no-op dry-run runner contributes
        /// no blocker. A single-crate Clean config cannot be a partial publish,
        /// so the simulation adds ZERO blockers — exactly what the rollback-scope
        /// tests assume.
        #[test]
        fn factory_seam_runs_no_op_dry_runner_no_spurious_blocker() {
            use anodizer_core::config::{Config, CrateConfig, PublishConfig};
            use anodizer_core::context::{Context, ContextOptions};

            let crate_cfg = CrateConfig {
                name: "mytool".to_string(),
                publish: Some(PublishConfig {
                    cargo: Some(CargoPublishConfig::default()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let config = Config {
                project_name: "mytool".to_string(),
                crates: vec![crate_cfg],
                ..Default::default()
            };
            let mut ctx = Context::new(config, ContextOptions::default());
            ctx.template_vars_mut().set("Version", "1.0.0");
            let log = quiet_log();

            // A factory reporting the cargo crate Clean (no network).
            let factory = super::CannedFactory {
                cargo_state: PublisherState::Clean,
                choco_state: PublisherState::Clean,
                winget_state: PublisherState::Clean,
                aur_state: PublisherState::Clean,
            };
            let report = run_preflight_with_factory(&mut ctx, &log, &factory).expect("ok");
            assert!(
                report.blockers.is_empty(),
                "factory seam must not produce a simulation blocker: {:?}",
                report.blockers
            );
        }

        // ---- classify_dry_run_stderr unit coverage ----------------------

        #[test]
        fn classify_stderr_compile_error() {
            let out = classify_dry_run_stderr(
                "   Compiling anodizer-stage-blob v0.6.0\nerror[E0425]: cannot find function `probe_dir` in module `path_util`\n",
            );
            match out {
                DryRunOutcome::CompileError(line) => {
                    assert!(line.contains("E0425"), "line: {line}")
                }
                other => panic!("expected CompileError, got {other:?}"),
            }
        }

        #[test]
        fn classify_stderr_no_matching_package() {
            let out = classify_dry_run_stderr(
                "error: failed to verify package tarball\n\nCaused by:\n  no matching package named `anodizer-core` found\n",
            );
            match out {
                DryRunOutcome::BenignSiblingMissing(line) => {
                    assert!(line.contains("anodizer-core"), "line: {line}")
                }
                other => panic!("expected BenignSiblingMissing, got {other:?}"),
            }
        }

        #[test]
        fn classify_stderr_network_failure_is_unavailable() {
            let out = classify_dry_run_stderr(
                "error: failed to download from registry\nCaused by:\n  spurious network error\n",
            );
            assert!(
                matches!(out, DryRunOutcome::Unavailable(_)),
                "network failure → Unavailable, got {out:?}"
            );
        }

        #[test]
        fn classify_stderr_unknown_nonzero_is_compile_error_conservative() {
            let out = classify_dry_run_stderr("error: something unexpected went wrong\n");
            assert!(
                matches!(out, DryRunOutcome::CompileError(_)),
                "unknown non-zero conservatively aborts, got {out:?}"
            );
        }

        #[test]
        fn classify_stderr_package_id_mismatch_is_unavailable_not_blocker() {
            // The exact string a degenerate/test invocation produces when `-p`
            // names a crate that is not a workspace member here. Must NOT be a
            // CompileError blocker — in a real release the crate IS a member.
            let out = classify_dry_run_stderr(
                "error: package ID specification `mytool` did not match any packages\n",
            );
            assert!(
                matches!(out, DryRunOutcome::Unavailable(_)),
                "package-ID mismatch → Unavailable (env artifact), got {out:?}"
            );
        }

        #[test]
        fn classify_stderr_could_not_find_is_benign_sibling() {
            // "could not find" is checked in the missing-package block (BEFORE
            // the compile block, which also contains "cannot find"), so a
            // `could not find crate` line must classify as a benign-sibling
            // signal the caller resolves against the to-publish set — never a
            // hard compile blocker.
            let out = classify_dry_run_stderr(
                "error: could not find `anodizer-core` in registry `crates-io`\n",
            );
            match out {
                DryRunOutcome::BenignSiblingMissing(line) => {
                    assert!(line.contains("could not find"), "line: {line}")
                }
                other => panic!("expected BenignSiblingMissing, got {other:?}"),
            }
        }

        #[test]
        fn classify_stderr_failed_to_select_version_is_benign_sibling() {
            let out = classify_dry_run_stderr(
                "error: failed to select a version for the requirement `anodizer-core = \"^0.6\"`\n",
            );
            match out {
                DryRunOutcome::BenignSiblingMissing(line) => {
                    assert!(line.contains("failed to select"), "line: {line}")
                }
                other => panic!("expected BenignSiblingMissing, got {other:?}"),
            }
        }

        #[test]
        fn classify_stderr_unresolved_import_is_compile_error() {
            let out = classify_dry_run_stderr(
                "   Compiling anodizer-stage-blob v0.6.0\nerror[E0432]: unresolved import `anodizer_core::probe`\n",
            );
            // Both the `error[e` and `unresolved import` needles match; the
            // `error[e` line wins because it precedes the import line and
            // `first_line_matching` returns the first matching line.
            match out {
                DryRunOutcome::CompileError(line) => {
                    assert!(line.contains("E0432"), "line: {line}")
                }
                other => panic!("expected CompileError, got {other:?}"),
            }
        }

        #[test]
        fn classify_stderr_cannot_find_function_is_compile_error() {
            // A bare `cannot find function` line (no `error[E…]` prefix) must
            // still reach the compile-error block via the dedicated needle.
            let out = classify_dry_run_stderr("cannot find function `probe_dir` in this scope\n");
            match out {
                DryRunOutcome::CompileError(line) => {
                    assert!(line.contains("probe_dir"), "line: {line}")
                }
                other => panic!("expected CompileError, got {other:?}"),
            }
        }

        #[test]
        fn classify_stderr_could_not_compile_is_compile_error() {
            let out = classify_dry_run_stderr(
                "error: could not compile `anodizer-stage-blob` due to 2 previous errors\n",
            );
            match out {
                DryRunOutcome::CompileError(line) => {
                    assert!(line.contains("could not compile"), "line: {line}")
                }
                other => panic!("expected CompileError, got {other:?}"),
            }
        }

        #[test]
        fn classify_stderr_failed_to_download_is_unavailable() {
            let out = classify_dry_run_stderr(
                "error: failed to download `serde v1.0.0`\nCaused by:\n  timed out\n",
            );
            match out {
                DryRunOutcome::Unavailable(line) => {
                    assert!(line.contains("failed to download"), "line: {line}")
                }
                other => panic!("expected Unavailable, got {other:?}"),
            }
        }

        #[test]
        fn classify_stderr_http_response_failure_is_unavailable() {
            let out = classify_dry_run_stderr(
                "error: failed to get successful HTTP response from `https://index.crates.io`\n",
            );
            assert!(
                matches!(out, DryRunOutcome::Unavailable(_)),
                "registry HTTP failure → Unavailable, got {out:?}"
            );
        }

        #[test]
        fn classify_stderr_blank_only_uses_placeholder_diagnostic() {
            // Stderr with no non-empty line falls through every needle block to
            // the conservative CompileError, and `first_nonempty_line` must
            // yield the bare-diagnostic placeholder rather than an empty string.
            let out = classify_dry_run_stderr("\n   \n\t\n");
            match out {
                DryRunOutcome::CompileError(line) => {
                    assert_eq!(line, "non-zero exit, no diagnostic")
                }
                other => panic!("expected CompileError placeholder, got {other:?}"),
            }
        }

        #[test]
        fn noop_dry_run_runner_reports_unavailable() {
            // The default test-seam runner never spawns and always degrades to
            // the index-only check; carry a reason so the caller's warn line is
            // honest about why the dry-run was skipped.
            match noop_dry_run_runner("anodizer-core") {
                DryRunOutcome::Unavailable(reason) => {
                    assert!(reason.contains("disabled"), "reason: {reason}")
                }
                other => panic!("expected Unavailable, got {other:?}"),
            }
        }

        #[test]
        fn partial_publish_non_index_state_is_treated_as_published() {
            // crates.io never yields InModeration, but the partial-publish
            // classifier must treat ANY non-Clean/non-Unknown state as
            // "present" so a mixed set (one present, one Clean) still aborts.
            let mut ctx = two_crate_ctx();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            let index = |krate: &str, _v: &str| {
                if krate == "anodizer-core" {
                    PublisherState::InModeration {
                        reason: "unexpected moderation state".into(),
                    }
                } else {
                    PublisherState::Clean
                }
            };
            let dry = |krate: &str| -> DryRunOutcome {
                panic!("dry-run must not run once a mixed state aborts (ran for {krate})")
            };
            run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);
            assert_eq!(report.blockers.len(), 1, "mixed state aborts");
            let b = &report.blockers[0];
            assert!(b.contains("partially published"), "blocker: {b}");
            assert!(
                b.contains("anodizer-core") && b.contains("anodizer-stage-blob"),
                "names both crates: {b}"
            );
        }

        #[test]
        fn render_failure_in_skip_template_surfaces_as_blocker() {
            use anodizer_core::config::StringOrBool;
            // An unterminated `skip:` template breaks `cargo_publish_plan` — the
            // same failure the real publish would hit — so the simulation must
            // surface it as a blocker, never silently skip the gate.
            let mut blob = cargo_crate("anodizer-stage-blob", &["anodizer-core"]);
            if let Some(ref mut p) = blob.publish
                && let Some(ref mut c) = p.cargo
            {
                c.skip = Some(StringOrBool::String("{{ unterminated".to_string()));
            }
            let mut ctx = TestContextBuilder::new()
                .crates(vec![blob, cargo_crate("anodizer-core", &[])])
                .build();
            let log = quiet_log();
            let mut report = PreflightReport::new();
            // Neither seam may run: the plan fails before any state query.
            let index = |krate: &str, _v: &str| -> PublisherState {
                panic!("index must not be queried when the plan fails (queried {krate})")
            };
            let dry = |krate: &str| -> DryRunOutcome {
                panic!("dry-run must not run when the plan fails (ran for {krate})")
            };
            run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);
            assert_eq!(report.blockers.len(), 1, "plan render failure blocks");
            assert!(
                report.blockers[0].contains("cargo publish-simulation:"),
                "blocker is tagged with the simulation prefix: {}",
                report.blockers[0]
            );
        }
    }

    // -----------------------------------------------------------------------
    // FakeToolDir-driven `cargo publish --dry-run` spawn coverage.
    //
    // Drives the REAL spawn against a fake `cargo` binary addressed by absolute
    // path (`run_cargo_dry_run_with_binary`). Each test fork+execs a
    // freshly-written stub, so they share the `path_env` serial group: a
    // sibling test's concurrent `fork()` would otherwise duplicate the
    // in-flight write FD of this stub and make the subsequent `exec` fail with
    // ETXTBSY ("Text file busy"). Serializing every fork+exec-of-fresh-binary
    // test under one group closes that window. Asserts argv shape + outcome
    // classification.
    // -----------------------------------------------------------------------
    #[cfg(unix)]
    mod publish_simulation_spawn {
        use super::super::*;
        use anodizer_core::log::{StageLogger, Verbosity};
        use anodizer_core::test_helpers::fake_tool::FakeToolDir;
        use serial_test::serial;

        fn quiet_log() -> StageLogger {
            StageLogger::new("publish-sim-spawn-test", Verbosity::Normal)
        }

        #[test]
        #[serial(path_env)]
        fn dry_run_exit_zero_is_ok_and_argv_is_publish_dry_run() {
            let fake = FakeToolDir::new();
            fake.tool("cargo").exit(0).install();

            let out = run_cargo_dry_run_with_binary(
                &fake.tool_path("cargo"),
                "anodizer-core",
                &quiet_log(),
            );
            assert_eq!(out, DryRunOutcome::Ok);

            let calls = fake.calls("cargo");
            assert_eq!(calls.len(), 1, "cargo invoked exactly once");
            assert_eq!(
                calls[0],
                vec!["publish", "--dry-run", "-p", "anodizer-core"],
                "argv must be `cargo publish --dry-run -p <crate>`"
            );
        }

        #[test]
        #[serial(path_env)]
        fn dry_run_compile_error_on_stderr_aborts() {
            let fake = FakeToolDir::new();
            fake.tool("cargo")
                .exit(101)
                .stderr("error[E0425]: cannot find function `probe_dir` in this scope")
                .install();

            let out = run_cargo_dry_run_with_binary(
                &fake.tool_path("cargo"),
                "anodizer-stage-blob",
                &quiet_log(),
            );
            match out {
                DryRunOutcome::CompileError(line) => {
                    assert!(line.contains("probe_dir"), "line: {line}")
                }
                other => panic!("expected CompileError, got {other:?}"),
            }
        }

        #[test]
        #[serial(path_env)]
        fn dry_run_missing_sibling_on_stderr_is_benign_signal() {
            let fake = FakeToolDir::new();
            fake.tool("cargo")
                .exit(101)
                .stderr("error: no matching package named `anodizer-core` found")
                .install();

            let out = run_cargo_dry_run_with_binary(
                &fake.tool_path("cargo"),
                "anodizer-stage-blob",
                &quiet_log(),
            );
            match out {
                DryRunOutcome::BenignSiblingMissing(line) => {
                    assert!(line.contains("anodizer-core"), "line: {line}")
                }
                other => panic!("expected BenignSiblingMissing, got {other:?}"),
            }
        }

        #[test]
        #[serial(path_env)]
        fn dry_run_spawn_failure_is_unavailable() {
            // A nonexistent cargo binary makes the spawn fail (cargo
            // absent / not on PATH). The runner must degrade to
            // Unavailable — never abort the release on a missing
            // toolchain — and carry the spawn-error reason so the warn
            // line is honest. Driven through the binary-path seam:
            // emptying the process-wide PATH instead would make every
            // concurrent PATH-resolved spawn in this binary flaky.
            let tmp = tempfile::TempDir::new().expect("temp dir");
            let missing = tmp.path().join("nonexistent-cargo");

            let out = run_cargo_dry_run_with_binary(&missing, "anodizer-core", &quiet_log());

            match out {
                DryRunOutcome::Unavailable(reason) => {
                    assert!(reason.contains("spawn cargo"), "reason: {reason}")
                }
                other => panic!("expected Unavailable on spawn failure, got {other:?}"),
            }
        }
    }
}
