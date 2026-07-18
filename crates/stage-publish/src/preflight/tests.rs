use super::*;
use anodizer_core::log::StageLogger;
use anodizer_core::preflight::{PreflightEntry, PreflightReport, PublisherState};
use std::time::Duration;

// Minimal mock checker for report-aggregation tests.
struct MockChecker {
    name: &'static str,
    state: PublisherState,
}

impl PreflightChecker for MockChecker {
    fn publisher_name(&self) -> &str {
        self.name
    }
    fn check(&self, _package: &str, _version: &str, _log: &StageLogger) -> PublisherState {
        self.state.clone()
    }
}

fn run_mocks(checkers: Vec<(&'static str, PublisherState)>) -> PreflightReport {
    let mut report = PreflightReport::new();
    for (name, state) in checkers {
        let checker = MockChecker { name, state };
        let s = checker.check(
            "testpkg",
            "1.0.0",
            anodizer_core::test_helpers::test_logger(),
        );
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
    let (addr, _calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let url = format!("http://{}/", addr);
    let result = query_crates_io(
        &url,
        "foo",
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    );
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
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
    let url = format!("http://{}/", addr);
    let result = query_crates_io(
        &url,
        "foo",
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    );
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
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
    let url = format!("http://{}/", addr);
    let result = query_crates_io(
        &url,
        "foo",
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    );
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
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
    let url = format!("http://{}/rpc/v5/info?arg[]=mypkg", addr);
    // query_aur_rpc does GET to the URL directly; reuse it with overridden URL
    // by calling the lower-level function with the mock address.
    let result = query_aur_rpc_at(
        &url,
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    );
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
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
    let url = format!("http://{}/rpc/v5/info?arg[]=mypkg", addr);
    let result = query_aur_rpc_at(
        &url,
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    );
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
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
    let url = format!(
        "http://{}/search/issues?q=mypkg+1.0.0+in%3Atitle&per_page=1",
        addr
    );
    let result = query_winget_pr_at(
        &url,
        None,
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("ok");
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
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
    let url = format!(
        "http://{}/search/issues?q=mypkg+1.0.0+in%3Atitle&per_page=1",
        addr
    );
    let result = query_winget_pr_at(
        &url,
        None,
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("ok");
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
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
    let url = format!("http://{}/search/issues", addr);
    let result = query_winget_pr_at(
        &url,
        None,
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("ok");
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
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
    let url = format!("http://{}/search/issues", addr);
    let err = query_winget_pr_at(
        &url,
        None,
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect_err("must be Err");
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
    let (addr, _calls) = spawn_oneshot_http_responder(vec![Box::leak(response.into_boxed_str())]);
    let url = format!("http://{}/rpc/v5/info?arg[]=mypkg", addr);
    let err = query_aur_rpc_at(
        &url,
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect_err("must be Err");
    assert!(
        err.to_string().contains("malformed AUR RPC response"),
        "{err}"
    );
}

// ---- AUR: 404 → Ok(false) (Clean) ------------------------------------

#[test]
fn aur_rpc_absent_on_404() {
    let (addr, _calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let url = format!("http://{}/rpc/v5/info?arg[]=mypkg", addr);
    let result = query_aur_rpc_at(
        &url,
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("ok");
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
    let result = query_crates_io(
        &url,
        "foo",
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    );
    let err = result.expect_err("must be Err on connect-refused");

    // The trait-level wrapper would surface this as Unknown { reason } —
    // exercise the path explicitly to confirm.
    let checker_state = match query_crates_io(
        &url,
        "foo",
        "1.0.0",
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    ) {
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
    query_winget_pr_at(
        &url,
        Some("secret-token"),
        &fast_retry(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("ok");

    // reqwest lowercases header names on the wire (HTTP/2 style); match
    // case-insensitively so the assertion isn't brittle to that detail.
    let req = captured.lock().unwrap().clone();
    let lower = req.to_ascii_lowercase();
    assert!(
        lower.contains("authorization: bearer secret-token"),
        "Authorization header missing or malformed; request was:\n{req}"
    );
}

// ---- GitHub search query encoding ------------------------------------

#[test]
fn winget_search_query_encoding_round_trips_operators() {
    // The core encoder percent-escapes spaces and the `:`/`/` in search
    // operators — forms GitHub's search API decodes back to the intended
    // query — while package identifiers and versions (dots, dashes)
    // survive verbatim.
    assert_eq!(
        anodizer_core::url::percent_encode_unreserved(
            "repo:microsoft/winget-pkgs is:pr is:open Acme.Tool 1.2.3 in:title"
        ),
        "repo%3Amicrosoft%2Fwinget-pkgs%20is%3Apr%20is%3Aopen%20Acme.Tool%201.2.3%20in%3Atitle"
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
    let state = checker.check("foo", "1.0.0", anodizer_core::test_helpers::test_logger());
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
    let state = checker.check("foo", "1.0.0", anodizer_core::test_helpers::test_logger());
    assert!(
        matches!(state, PublisherState::Published),
        "approved row must be Published, got: {:?}",
        state
    );
}

#[test]
fn chocolatey_checker_404_is_clean() {
    // The OData entry endpoint returns 404 when the row is absent.
    let (addr, _calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let source = format!("http://{}/", addr);

    let checker = Chocolatey::new(source, fast_retry());
    let state = checker.check("foo", "1.0.0", anodizer_core::test_helpers::test_logger());
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
    let state = checker.check("foo", "1.0.0", anodizer_core::test_helpers::test_logger());
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
    fn check(&self, _package: &str, _version: &str, _log: &StageLogger) -> PublisherState {
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
    fn winget(&self, _token: Option<String>, _policy: RetryPolicy) -> Box<dyn PreflightChecker> {
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

#[test]
fn deselected_publisher_contributes_no_state_probe_entry() {
    use anodizer_core::config::{
        AurConfig, CargoPublishConfig, ChocolateyConfig, Config, CrateConfig, PublishConfig,
        WingetConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    // All four publishers configured, but the invocation is scoped to
    // `--publishers cargo` (the shape the GH-hosted `publish-npm` job uses,
    // minus npm). The winget PR that the earlier `Publish Release` job left
    // pending must NOT be probed — a door this run does not touch. Only the
    // allowlisted publisher yields a state-probe entry.
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
    ctx.options.publisher_allowlist = vec!["cargo".to_string()];
    let log = StageLogger::new("preflight", Verbosity::Normal);

    // winget/choco/aur states are deliberately PRPending/InModeration —
    // if the guard regressed they would surface as blockers.
    let factory = CannedFactory {
        cargo_state: PublisherState::Clean,
        choco_state: PublisherState::InModeration {
            reason: "package in moderation queue".into(),
        },
        winget_state: PublisherState::PRPending(
            "https://github.com/microsoft/winget-pkgs/pull/1".into(),
        ),
        aur_state: PublisherState::Unknown {
            reason: "AUR is informational".into(),
        },
    };

    let report = run_preflight_with_factory(&mut ctx, &log, &factory).expect("ok");

    let order: Vec<&str> = report
        .entries
        .iter()
        .map(|e| e.publisher.as_str())
        .collect();
    assert_eq!(
        order,
        vec!["cargo"],
        "only the allowlisted publisher may be probed; deselected doors must not gate the run"
    );
    assert!(
        report.blockers(false).is_empty(),
        "no deselected publisher may contribute a blocker: {:?}",
        report.blockers(false)
    );
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
        report.warnings[0].contains("cargo") && report.warnings[0].contains("CARGO_REGISTRY_TOKEN"),
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
        fn check(&self, _package: &str, _version: &str, _log: &StageLogger) -> PublisherState {
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
            tag_template: Some("v{{ .Version }}".to_string()),
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

    // ---- (1) partial-publish probe (resumable) ----------------------

    #[test]
    fn partial_publish_mixed_state_resumes_when_dry_run_ok() {
        // core already on the index, stage-blob not — a RESUMABLE partial
        // publish (the exact v0.19.0 case: 30 libs published, the CLI +
        // one stage crate still Clean, byte-identical content). The mixed
        // state must WARN and defer to the dry-run, not abort.
        let mut ctx = two_crate_ctx();
        let log = quiet_log();
        let mut report = PreflightReport::new();
        let index = |krate: &str, _v: &str| {
            if krate == "anodizer-core" {
                PublisherState::Published
            } else {
                PublisherState::Clean
            }
        };
        // The Clean dependent builds fine against the published dep → the
        // resume completes. (Published crates are skipped by the dry-run.)
        let ran_for = std::cell::RefCell::new(Vec::<String>::new());
        let dry = |krate: &str| -> DryRunOutcome {
            ran_for.borrow_mut().push(krate.to_string());
            DryRunOutcome::Ok
        };
        run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);

        assert!(
            report.blockers.is_empty(),
            "a resumable partial publish must not block: {:?}",
            report.blockers
        );
        assert_eq!(report.warnings.len(), 1, "one resume warning");
        let w = &report.warnings[0];
        assert!(w.contains("partially published"), "warns: {w}");
        assert!(w.contains("resuming"), "explains the resume: {w}");
        assert_eq!(
            *ran_for.borrow(),
            vec!["anodizer-stage-blob"],
            "dry-run verified only the Clean crate (proceeded past the probe)"
        );
    }

    #[test]
    fn partial_publish_mixed_state_blocks_on_stale_dep() {
        // Same mixed state, but the Clean dependent cannot build against the
        // stale published dep — the genuine v0.11.3 poison. The dry-run's
        // CompileError (not the coarse partial-publish probe) is what blocks.
        let mut ctx = two_crate_ctx();
        let log = quiet_log();
        let mut report = PreflightReport::new();
        let index = |krate: &str, _v: &str| {
            if krate == "anodizer-core" {
                PublisherState::Published
            } else {
                PublisherState::Clean
            }
        };
        let dry =
            |_krate: &str| DryRunOutcome::CompileError("anodizer-core 0.19.0 API mismatch".into());
        run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);

        assert_eq!(
            report.blockers.len(),
            1,
            "stale dep is caught by the dry-run"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("partially published")),
            "still warns about the partial state: {:?}",
            report.warnings
        );
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
                DryRunOutcome::CompileError("error[E0425]: cannot find function `probe_dir`".into())
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

    #[test]
    fn cargo_deselected_surface_skips_simulation_entirely() {
        // `--publishers npm` (or `--skip cargo`) leaves the irreversible
        // cargo door out of this run's surface, so the simulation has
        // nothing to guard. PanicFactory + panic_runner prove the wrapper
        // takes the gate before querying the index or spawning cargo —
        // otherwise a cargo-less release would abort on a cargo-only probe.
        let mut ctx = TestContextBuilder::new()
            .crates(vec![cargo_crate("anodizer-core", &[])])
            .build();
        ctx.options.publisher_allowlist = vec!["npm".to_string()];
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
        // "present" so it still forms a mixed set (one present, one Clean).
        // The mixed set is a resumable partial publish: it warns and defers
        // to the dry-run rather than aborting.
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
        let dry = |_krate: &str| DryRunOutcome::Ok;
        run_cargo_publish_simulation_with(&mut ctx, &log, &mut report, &index, &dry);
        assert!(
            report.blockers.is_empty(),
            "the mixed set resumes, not aborts: {:?}",
            report.blockers
        );
        let w = report
            .warnings
            .iter()
            .find(|w| w.contains("partially published"))
            .expect("warns about the partial state");
        assert!(
            w.contains("anodizer-core") && w.contains("anodizer-stage-blob"),
            "names both crates: {w}"
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
// Drives the REAL spawn+classify against a fake `cargo` binary addressed by
// absolute path (`run_cargo_dry_run_spawning`). Each test fork+execs a
// freshly-written stub, so a sibling test thread's `fork()` can duplicate
// this stub's in-flight write FD and make the subsequent `exec` fail with
// ETXTBSY ("Text file busy"). The `path_env` serial group only orders these
// tests against each other — it does NOT bound the thousands of other
// spawning tests in the crate whose fork windows race this exec — so the
// stub-execing tests spawn through `output_retrying_etxtbsy`, which retries
// the exec until the racing child clears the FD. The spawn-FAILURE test
// keeps the raw path (it wants the immediate ENOENT). Asserts argv shape +
// outcome classification.
// -----------------------------------------------------------------------
#[cfg(unix)]
mod publish_simulation_spawn {
    use super::super::*;
    use anodizer_core::log::{StageLogger, Verbosity};
    use anodizer_core::test_helpers::fake_tool::{FakeToolDir, output_retrying_etxtbsy};
    use serial_test::serial;

    fn quiet_log() -> StageLogger {
        StageLogger::new("publish-sim-spawn-test", Verbosity::Normal)
    }

    /// Drive the real spawn+classify path against a freshly-installed stub,
    /// retrying the exec on `ETXTBSY` (the stub is written-then-exec'd, so a
    /// sibling test thread's `fork` window can hold its fd briefly). Without
    /// this the raw production `.output()` would surface the transient
    /// `ETXTBSY` as `Unavailable` and flake the assertion.
    fn dry_run_via_stub(fake: &FakeToolDir, crate_name: &str) -> DryRunOutcome {
        run_cargo_dry_run_spawning(&fake.tool_path("cargo"), crate_name, &quiet_log(), |cmd| {
            Ok(output_retrying_etxtbsy(cmd))
        })
    }

    #[test]
    #[serial(path_env)]
    fn dry_run_exit_zero_is_ok_and_argv_is_publish_dry_run() {
        let fake = FakeToolDir::new();
        fake.tool("cargo").exit(0).install();

        let out = dry_run_via_stub(&fake, "anodizer-core");
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

        let out = dry_run_via_stub(&fake, "anodizer-stage-blob");
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

        let out = dry_run_via_stub(&fake, "anodizer-stage-blob");
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
