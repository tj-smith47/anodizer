//! End-to-end tests for the post-publish polling fan-out.
//!
//! Strategy mirrors `mcp::tests`: bind an ephemeral-port TCP listener,
//! enqueue a sequence of canned HTTP responses, point the publisher at
//! `http://127.0.0.1:<port>`. The polling config uses 1ms intervals + a
//! tight timeout so a multi-round poll completes in single-digit ms.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anodizer_core::config::{HumanDuration, PostPublishPollConfig};
use anodizer_core::log::{StageLogger, Verbosity};

use super::status::PostPublishStatus;
use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

fn tight_poll_cfg() -> PostPublishPollConfig {
    PostPublishPollConfig {
        enabled: true,
        interval: HumanDuration(Duration::from_millis(5)),
        // 5s is generous enough that even a heavily-contended shared CI
        // runner (notably macOS GH Actions runners, which have flaked at
        // ~250ms under load) won't trip false timeouts; the happy-path
        // tests still complete in single-digit ms because the polling
        // client returns as soon as it gets an Approved response. The
        // `chocolatey_poller_times_out_on_persistent_pending` test below
        // declares its own short-timeout config (30ms) — it WANTS the
        // timeout to fire, so it stays separate from `tight_poll_cfg`.
        timeout: HumanDuration(Duration::from_secs(5)),
    }
}

fn quiet_log() -> StageLogger {
    StageLogger::new("post-publish-test", Verbosity::Quiet)
}

fn http_response(status_line: &str, body: &str) -> String {
    format!(
        "{}\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
        status_line,
        body.len(),
        body
    )
}

// ---------------------------------------------------------------------------
// Chocolatey poller
// ---------------------------------------------------------------------------

#[test]
fn chocolatey_poller_resolves_approved_on_first_call() {
    let approved_html = r#"<html><body>
        <div class="callout callout-success">
          <div class="callout-header">Package Approved</div>
          <p>This package was approved as a trusted package.</p>
        </div>
    </body></html>"#;
    let body = http_response("HTTP/1.1 200 OK", approved_html);
    // Leak so the static lifetime matches the responder signature.
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let status = super::chocolatey::poll(&base, "git", "2.50.1", tight_poll_cfg(), &quiet_log());
    match status {
        PostPublishStatus::Approved { detail } => {
            assert!(detail.contains("Approved"), "got: {detail}");
        }
        other => panic!("expected Approved, got {other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "approved page should resolve on first request"
    );
}

#[test]
fn chocolatey_poller_pending_then_approved() {
    let pending = http_response(
        "HTTP/1.1 200 OK",
        r#"<div class="callout callout-danger"><div class="callout-header">IMPORTANT</div>
           <p>This version is in <a>moderation</a> and has not yet been approved.</p></div>"#,
    );
    let approved = http_response(
        "HTTP/1.1 200 OK",
        r#"<div class="callout-header">Package Approved</div>"#,
    );
    let pending: &'static str = Box::leak(pending.into_boxed_str());
    let approved: &'static str = Box::leak(approved.into_boxed_str());
    let (addr, calls) = spawn_oneshot_http_responder(vec![pending, approved]);
    let base = format!("http://{}", addr);

    let status =
        super::chocolatey::poll(&base, "anodizer", "0.2.0", tight_poll_cfg(), &quiet_log());
    match status {
        PostPublishStatus::Approved { .. } => {}
        other => panic!("expected Approved after pending→approved, got {other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "must poll twice (pending then approved)"
    );
}

#[test]
fn chocolatey_poller_times_out_on_persistent_pending() {
    // Pre-stage many pending responses so the listener can serve the
    // poller until it gives up on the tight timeout.
    let pending = http_response(
        "HTTP/1.1 200 OK",
        r#"<div class="callout callout-warning">
            <div class="callout-header">WARNING</div>
            <p>awaiting moderation</p>
           </div>"#,
    );
    let pending: &'static str = Box::leak(pending.into_boxed_str());
    let responses = vec![pending; 200];
    let (addr, _calls) = spawn_oneshot_http_responder(responses);
    let base = format!("http://{}", addr);

    let cfg = PostPublishPollConfig {
        enabled: true,
        interval: HumanDuration(Duration::from_millis(5)),
        timeout: HumanDuration(Duration::from_millis(30)),
    };
    let status = super::chocolatey::poll(&base, "x", "1.0.0", cfg, &quiet_log());
    match status {
        PostPublishStatus::Timeout { last_state, .. } => {
            assert!(
                last_state.contains("awaiting moderation") || last_state.contains("moderation"),
                "timeout must preserve last pending state: {last_state}"
            );
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// WinGet poller
// ---------------------------------------------------------------------------

#[test]
fn winget_poller_resolves_merged_pr() {
    // Round 1: GET /search/issues -> { total_count: 1, items: [{ pull_request: { url: "<pr_api>" } }] }
    // Round 2: GET <pr_api>      -> { state: closed, merged: true }
    let search_body =
        r#"{"total_count":1,"items":[{"number":42,"pull_request":{"url":"__PR_URL__"}}]}"#;
    let pr_body = r#"{"state":"closed","merged":true,"labels":[{"name":"Moderator-Approved"}]}"#;

    // We need to bind two listeners — one for search, one for the PR
    // fetch — because the URL extracted from the search response must
    // point back to the same loopback for the second request.
    let (pr_addr, _pr_calls) = spawn_oneshot_http_responder(vec![Box::leak(
        http_response("HTTP/1.1 200 OK", pr_body).into_boxed_str(),
    )]);
    let pr_url = format!("http://{}/repos/microsoft/winget-pkgs/pulls/42", pr_addr);
    let search_body = search_body.replace("__PR_URL__", &pr_url);
    let leaked_search: &'static str =
        Box::leak(http_response("HTTP/1.1 200 OK", &search_body).into_boxed_str());
    let (search_addr, search_calls) = spawn_oneshot_http_responder(vec![leaked_search]);
    let api_base = format!("http://{}", search_addr);

    let status = super::winget::poll(
        &api_base,
        "TJSmith.Anodizer",
        "0.2.0",
        None,
        tight_poll_cfg(),
        &quiet_log(),
    );
    match status {
        PostPublishStatus::Approved { detail } => {
            assert!(detail.contains("merged"), "got: {detail}");
        }
        other => panic!("expected Approved, got {other:?}"),
    }
    assert_eq!(
        search_calls.load(Ordering::SeqCst),
        1,
        "exactly one search call needed before falling through to direct PR fetch"
    );
}

#[test]
fn winget_poller_rejects_validation_error() {
    let pr_body = r#"{"state":"closed","merged":false,"labels":[{"name":"Validation-Hash-Verification-Failed"}]}"#;
    let (pr_addr, _pr_calls) = spawn_oneshot_http_responder(vec![Box::leak(
        http_response("HTTP/1.1 200 OK", pr_body).into_boxed_str(),
    )]);
    let pr_url = format!("http://{}/repos/microsoft/winget-pkgs/pulls/99", pr_addr);
    let search_body = format!(
        r#"{{"total_count":1,"items":[{{"number":99,"pull_request":{{"url":"{}"}}}}]}}"#,
        pr_url
    );
    let leaked_search: &'static str =
        Box::leak(http_response("HTTP/1.1 200 OK", &search_body).into_boxed_str());
    let (search_addr, _) = spawn_oneshot_http_responder(vec![leaked_search]);
    let api_base = format!("http://{}", search_addr);

    let status = super::winget::poll(
        &api_base,
        "TJSmith.Anodizer",
        "0.2.0",
        None,
        tight_poll_cfg(),
        &quiet_log(),
    );
    match status {
        PostPublishStatus::Rejected { detail } => {
            assert!(detail.contains("closed without merge"), "got: {detail}");
            assert!(
                detail.contains("Validation-Hash-Verification-Failed"),
                "got: {detail}"
            );
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[test]
fn winget_poller_times_out_when_pr_never_found() {
    // Every search returns total_count: 0 — the poller should keep
    // sampling until the budget runs out, then emit Timeout.
    let empty_search = http_response("HTTP/1.1 200 OK", r#"{"total_count":0,"items":[]}"#);
    let leaked: &'static str = Box::leak(empty_search.into_boxed_str());
    let responses = vec![leaked; 200];
    let (addr, _) = spawn_oneshot_http_responder(responses);
    let api_base = format!("http://{}", addr);

    let cfg = PostPublishPollConfig {
        enabled: true,
        interval: HumanDuration(Duration::from_millis(5)),
        timeout: HumanDuration(Duration::from_millis(30)),
    };
    let status = super::winget::poll(&api_base, "X.Y", "1.0.0", None, cfg, &quiet_log());
    match status {
        PostPublishStatus::Timeout { last_state, .. } => {
            assert!(
                last_state.contains("no matching PR") || last_state.contains("no PR"),
                "expected last_state mentions missing PR; got: {last_state}"
            );
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Fan-out / parallel execution
// ---------------------------------------------------------------------------

#[test]
fn run_post_publish_polls_returns_results_in_input_order() {
    // Two chocolatey jobs, each pointed at independent listeners that
    // both serve an Approved page. The fan-out runner schedules them on
    // parallel threads but the returned `Vec<PostPublishResult>` must
    // preserve input order so the release-summary renderer doesn't have
    // to re-sort.
    let approved = http_response(
        "HTTP/1.1 200 OK",
        r#"<div class="callout-header">Package Approved</div>"#,
    );
    let approved_a: &'static str = Box::leak(approved.clone().into_boxed_str());
    let approved_b: &'static str = Box::leak(approved.into_boxed_str());
    let (addr_a, _) = spawn_oneshot_http_responder(vec![approved_a]);
    let (addr_b, _) = spawn_oneshot_http_responder(vec![approved_b]);

    let jobs = vec![
        super::PollJob::Chocolatey {
            package: "first".to_string(),
            version: "1.0.0".to_string(),
            page_base_url: format!("http://{}", addr_a),
            cfg: tight_poll_cfg(),
        },
        super::PollJob::Chocolatey {
            package: "second".to_string(),
            version: "2.0.0".to_string(),
            page_base_url: format!("http://{}", addr_b),
            cfg: tight_poll_cfg(),
        },
    ];
    let results = super::run_post_publish_polls(jobs, &quiet_log());
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].package, "first");
    assert_eq!(results[1].package, "second");
    for r in &results {
        match &r.status {
            PostPublishStatus::Approved { .. } => {}
            other => panic!("job for {} returned {other:?}", r.package),
        }
    }
}

#[test]
fn run_post_publish_polls_empty_returns_empty() {
    let results = super::run_post_publish_polls(Vec::new(), &quiet_log());
    assert!(results.is_empty());
}

// ---------------------------------------------------------------------------
// resolve_poll_config
// ---------------------------------------------------------------------------

#[test]
fn resolve_poll_config_returns_none_when_cli_skip_set() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(
        Config::default(),
        ContextOptions {
            skip_post_publish_poll: true,
            ..Default::default()
        },
    );
    assert!(super::resolve_poll_config(&ctx, None).is_none());
    assert!(
        super::resolve_poll_config(&ctx, Some(PostPublishPollConfig::default())).is_none(),
        "CLI flag must override per-publisher config"
    );
}

#[test]
fn resolve_poll_config_returns_none_when_disabled() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let disabled = PostPublishPollConfig {
        enabled: false,
        ..PostPublishPollConfig::default()
    };
    assert!(super::resolve_poll_config(&ctx, Some(disabled)).is_none());
}

#[test]
fn resolve_poll_config_returns_default_when_unset() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let cfg = super::resolve_poll_config(&ctx, None);
    assert!(cfg.is_some(), "unset block should default to enabled");
    let cfg = cfg.unwrap();
    assert!(cfg.enabled);
    assert_eq!(
        cfg.interval.duration(),
        PostPublishPollConfig::DEFAULT_INTERVAL
    );
    assert_eq!(
        cfg.timeout.duration(),
        PostPublishPollConfig::DEFAULT_TIMEOUT
    );
}
