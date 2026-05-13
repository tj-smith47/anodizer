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
            Err(e) => PublisherState::Unknown(e.to_string()),
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
        use crate::chocolatey::package::{FeedHashResult, package_feed_hash};

        match package_feed_hash(&self.source, package, version, &self.policy) {
            FeedHashResult::Present { listed, status, .. } => {
                if listed == Some(false) {
                    let label = status.as_deref().unwrap_or("Unknown");
                    if label.eq_ignore_ascii_case("Rejected") {
                        // Rejected is a hard blocker too — can't re-push.
                        return PublisherState::InModeration;
                    }
                    return PublisherState::InModeration;
                }
                // listed == Some(true) or None → version is live
                PublisherState::Published
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
        // `<PackageIdentifier> <version>` — the conventional winget PR title
        // format (GoReleaser parity: "New version: <id> <version>").
        match query_winget_pr(package, version, self.token.as_deref(), &self.policy) {
            Ok(Some(url)) => PublisherState::PRPending(url),
            Ok(None) => PublisherState::Clean,
            Err(e) => PublisherState::Unknown(e.to_string()),
        }
    }
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
) -> Result<Option<String>> {
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
) -> Result<Option<String>> {
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
                return Ok(None);
            }
            return Err(err);
        }
    };

    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let total = v.get("total_count").and_then(|n| n.as_u64()).unwrap_or(0);

    if total == 0 {
        return Ok(None);
    }

    let pr_url = v
        .get("items")
        .and_then(|items| items.get(0))
        .and_then(|item| item.get("html_url"))
        .and_then(|u| u.as_str())
        .map(str::to_string);

    // Fall back to the pulls listing URL when items[0] is present but its
    // `html_url` is missing (defensive: live GitHub always populates this).
    Ok(pr_url.or_else(|| Some("https://github.com/microsoft/winget-pkgs/pulls".to_string())))
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
            Ok(true) => PublisherState::Published, // informational — AUR allows overwrites
            Ok(false) => PublisherState::Clean,
            Err(e) => PublisherState::Unknown(e.to_string()),
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

    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
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

/// Run all enabled one-way-door publisher checks and return an aggregated
/// [`PreflightReport`].
///
/// Checkers run sequentially. Each checker is only constructed when the
/// corresponding publisher is configured for at least one selected crate.
pub fn run_preflight(ctx: &Context, log: &StageLogger) -> Result<PreflightReport> {
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
            let checker = CargoCratesIo::new(policy);
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
            let checker = Chocolatey::new(source, policy);
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
            let checker = Winget::new(token, policy);
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
            let checker = Aur::new(policy);
            let state = checker.check(&pkg_name, &version);
            report.push(PreflightEntry {
                publisher: checker.publisher_name().to_string(),
                package: pkg_name,
                version: version.clone(),
                state,
            });
        }
    }

    Ok(report)
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
            ("chocolatey", PublisherState::InModeration),
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
            PublisherState::Unknown("timeout connecting to AUR".into()),
        )]);
        assert!(!report.has_blockers(false));
        assert!(report.has_blockers(true));
    }

    // ---- HTTP-mock tests for crates.io index check ------------------------

    fn spawn_oneshot_http_responder(
        responses: Vec<&'static str>,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::atomic::AtomicU32>,
    ) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicU32, Ordering};

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let counter = std::sync::Arc::new(AtomicU32::new(0));
        let counter_inner = counter.clone();
        std::thread::spawn(move || {
            for (i, resp) in responses.iter().enumerate() {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                counter_inner.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
                if i == responses.len() - 1 {
                    break;
                }
            }
        });
        (addr, counter)
    }

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
        let result = query_winget_pr_at(&url, None, &fast_retry());
        assert!(result.is_ok());
        assert!(result.unwrap().is_none(), "no PR when total_count=0");
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
        let result = query_winget_pr_at(&url, None, &fast_retry());
        assert!(result.is_ok());
        let pr_url = result.unwrap();
        assert!(pr_url.is_some(), "PR found");
        assert!(
            pr_url.unwrap().contains("pull/9999"),
            "correct PR URL returned"
        );
    }
}
