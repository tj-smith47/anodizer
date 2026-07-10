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

#[test]
fn chocolatey_poller_404_throughout_window_returns_timeout_not_error() {
    // A package sitting in the human-moderator queue is invisible
    // (HTTP 404) by design. A chronic 404 across the entire poll
    // budget must NOT promote to Error — moderation queues routinely
    // span days, so the 404 is the expected steady state on a fresh
    // submission and is not actionable for the operator.
    let nf = http_response("HTTP/1.1 404 Not Found", "<html>not found</html>");
    let leaked: &'static str = Box::leak(nf.into_boxed_str());
    let responses = vec![leaked; 200];
    let (addr, _calls) = spawn_oneshot_http_responder(responses);
    let base = format!("http://{}", addr);

    let cfg = PostPublishPollConfig {
        enabled: true,
        interval: HumanDuration(Duration::from_millis(5)),
        timeout: HumanDuration(Duration::from_millis(30)),
    };
    let status = super::chocolatey::poll(&base, "anodizer", "0.3.0", cfg, &quiet_log());
    match status {
        PostPublishStatus::Timeout { last_state, .. } => {
            assert!(
                last_state.contains("not indexed") || last_state.contains("404"),
                "timeout must preserve 404 diagnostic in last_state: {last_state}"
            );
        }
        other => panic!(
            "expected Timeout for chronic 404 (not Error — moderation queue is expected state), \
             got {other:?}"
        ),
    }
}

#[test]
fn chocolatey_poller_visible_then_404_returns_error() {
    // Regression detection: the page resolved (any 200 OK with a
    // pending callout) earlier in the run and then went 404. That
    // IS unexpected and surfaces as Error so the operator sees the
    // takedown.
    let visible = http_response(
        "HTTP/1.1 200 OK",
        r#"<div class="callout callout-warning">
            <div class="callout-header">WARNING</div>
            <p>awaiting moderation</p>
           </div>"#,
    );
    let gone = http_response("HTTP/1.1 404 Not Found", "<html>not found</html>");
    let visible: &'static str = Box::leak(visible.into_boxed_str());
    let gone: &'static str = Box::leak(gone.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![visible, gone]);
    let base = format!("http://{}", addr);

    let status =
        super::chocolatey::poll(&base, "anodizer", "0.3.0", tight_poll_cfg(), &quiet_log());
    match status {
        PostPublishStatus::Error { reason } => {
            assert!(
                reason.contains("previously visible") || reason.contains("delisted"),
                "regression error must mention prior visibility: {reason}"
            );
        }
        other => panic!("expected Error on visible→404 regression, got {other:?}"),
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
        &super::winget::PollTarget {
            upstream_slug: "microsoft/winget-pkgs".to_string(),
            package_identifier: "TJSmith.Anodizer".to_string(),
            version: "0.2.0".to_string(),
            search_in_title: true,
        },
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
        &super::winget::PollTarget {
            upstream_slug: "microsoft/winget-pkgs".to_string(),
            package_identifier: "TJSmith.Anodizer".to_string(),
            version: "0.2.0".to_string(),
            search_in_title: true,
        },
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
    let status = super::winget::poll(
        &api_base,
        &super::winget::PollTarget {
            upstream_slug: "microsoft/winget-pkgs".to_string(),
            package_identifier: "X.Y".to_string(),
            version: "1.0.0".to_string(),
            search_in_title: true,
        },
        None,
        cfg,
        &quiet_log(),
    );
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

#[test]
fn winget_poller_found_then_search_empty_returns_error() {
    // Regression detection: a matching PR was located on the first
    // search, but a subsequent search returns empty (and the cached
    // PR URL hits a NetworkError that forces a re-search). The
    // disappearance is a regression and surfaces as Error.
    let pr_body = r#"{"state":"open","merged":false,"labels":[{"name":"New-Manifest"}]}"#;
    let (pr_addr, _pr_calls) = spawn_oneshot_http_responder(vec![
        Box::leak(http_response("HTTP/1.1 200 OK", pr_body).into_boxed_str()),
        // Second hit on the PR URL: 500 forces the poller to re-search.
        Box::leak(http_response("HTTP/1.1 500 Internal Server Error", "boom").into_boxed_str()),
    ]);
    let pr_url = format!("http://{}/repos/microsoft/winget-pkgs/pulls/77", pr_addr);
    let search_found = format!(
        r#"{{"total_count":1,"items":[{{"number":77,"pull_request":{{"url":"{}"}}}}]}}"#,
        pr_url
    );
    let search_empty = r#"{"total_count":0,"items":[]}"#.to_string();
    let leaked_found: &'static str =
        Box::leak(http_response("HTTP/1.1 200 OK", &search_found).into_boxed_str());
    let leaked_empty: &'static str =
        Box::leak(http_response("HTTP/1.1 200 OK", &search_empty).into_boxed_str());
    let (search_addr, _) = spawn_oneshot_http_responder(vec![leaked_found, leaked_empty]);
    let api_base = format!("http://{}", search_addr);

    let status = super::winget::poll(
        &api_base,
        &super::winget::PollTarget {
            upstream_slug: "microsoft/winget-pkgs".to_string(),
            package_identifier: "TJSmith.Anodizer".to_string(),
            version: "0.3.0".to_string(),
            search_in_title: true,
        },
        None,
        tight_poll_cfg(),
        &quiet_log(),
    );
    match status {
        PostPublishStatus::Error { reason } => {
            assert!(
                reason.contains("previously located") || reason.contains("disappeared"),
                "regression error must mention prior visibility: {reason}"
            );
        }
        other => panic!("expected Error on found→search-empty regression, got {other:?}"),
    }
}

#[test]
fn winget_poller_closed_unmerged_returns_rejected() {
    // Confirmed-rejection signal: PR closed without merge with a
    // recognized rejection label. The poller must classify this as
    // Rejected (an actionable terminal failure) and not Pending or
    // Error — distinguishing it from the noise-suppressed
    // "no PR found yet" path.
    let pr_body = r#"{"state":"closed","merged":false,"labels":[{"name":"Needs-CLA"}]}"#;
    let (pr_addr, _pr_calls) = spawn_oneshot_http_responder(vec![Box::leak(
        http_response("HTTP/1.1 200 OK", pr_body).into_boxed_str(),
    )]);
    let pr_url = format!("http://{}/repos/microsoft/winget-pkgs/pulls/55", pr_addr);
    let search_body = format!(
        r#"{{"total_count":1,"items":[{{"number":55,"pull_request":{{"url":"{}"}}}}]}}"#,
        pr_url
    );
    let leaked_search: &'static str =
        Box::leak(http_response("HTTP/1.1 200 OK", &search_body).into_boxed_str());
    let (search_addr, _) = spawn_oneshot_http_responder(vec![leaked_search]);
    let api_base = format!("http://{}", search_addr);

    let status = super::winget::poll(
        &api_base,
        &super::winget::PollTarget {
            upstream_slug: "microsoft/winget-pkgs".to_string(),
            package_identifier: "TJSmith.Anodizer".to_string(),
            version: "0.3.0".to_string(),
            search_in_title: true,
        },
        None,
        tight_poll_cfg(),
        &quiet_log(),
    );
    match status {
        PostPublishStatus::Rejected { detail } => {
            assert!(detail.contains("Needs-CLA"), "got: {detail}");
        }
        other => panic!("expected Rejected on closed-without-merge, got {other:?}"),
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
fn resolve_poll_config_returns_none_when_unset() {
    // Polling defaults to disabled — moderation-queue waits (Chocolatey,
    // winget-pkgs) routinely take hours to days, and blocking a CI job on
    // that is wrong by default. An unset `post_publish_poll:` block
    // resolves to `None` so the publisher emits `NotPolled` rather than
    // entering the polling fan-out with an unwanted 30-min budget.
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let cfg = super::resolve_poll_config(&ctx, None);
    assert!(
        cfg.is_none(),
        "unset block should default to disabled (no polling)"
    );
}

// ---------------------------------------------------------------------------
// Burn-detection probes (published-state guard evidence)
// ---------------------------------------------------------------------------

#[test]
fn chocolatey_burn_probe_pending_moderation_blocks() {
    let pending_html = r#"<html><body>
        <div class="callout callout-danger">
          <div class="callout-header">IMPORTANT</div>
          <p>This version is in <a href="/moderation">moderation</a> and has not yet been approved.</p>
        </div>
    </body></html>"#;
    let body = http_response("HTTP/1.1 200 OK", pending_html);
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let blocked =
        super::chocolatey::version_blocked_on_gallery(&base, "anodizer", "0.2.0", &quiet_log())
            .expect("probe must succeed against a resolvable page");
    let detail = blocked.expect("a pending-moderation version must block");
    assert!(detail.contains("moderation"), "got: {detail}");
}

#[test]
fn chocolatey_burn_probe_approved_version_blocks() {
    let approved_html = r#"<html><body>
        <div class="callout callout-success">
          <div class="callout-header">Package Approved</div>
        </div>
    </body></html>"#;
    let body = http_response("HTTP/1.1 200 OK", approved_html);
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let blocked =
        super::chocolatey::version_blocked_on_gallery(&base, "git", "2.50.1", &quiet_log())
            .expect("probe must succeed");
    let detail = blocked.expect("an approved version must block");
    assert!(detail.contains("approved"), "got: {detail}");
}

#[test]
fn chocolatey_burn_probe_absent_version_is_clear() {
    let body = http_response("HTTP/1.1 404 Not Found", "not here");
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let blocked =
        super::chocolatey::version_blocked_on_gallery(&base, "anodizer", "9.9.9", &quiet_log())
            .expect("404 is a definitive answer, not an error");
    assert!(blocked.is_none(), "a never-submitted version must be clear");
}

#[test]
fn chocolatey_burn_probe_unreachable_gallery_is_an_error() {
    // Bind-then-drop yields a port with nothing listening: connection
    // refused, which the retry classifier fast-fails.
    let addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let base = format!("http://{}", addr);
    let err = super::chocolatey::version_blocked_on_gallery(&base, "x", "1.0.0", &quiet_log())
        .expect_err("an unreachable gallery is indeterminate, never a clear/blocked verdict");
    let msg = format!("{err:#}");
    assert!(!msg.is_empty());
}

#[test]
fn winget_burn_probe_open_pr_blocks() {
    let search_json = r#"{
        "total_count": 1,
        "items": [{
            "number": 12345,
            "state": "open",
            "html_url": "https://github.com/microsoft/winget-pkgs/pull/12345",
            "pull_request": {"url": "https://api.github.com/repos/microsoft/winget-pkgs/pulls/12345", "merged_at": null}
        }]
    }"#;
    let body = http_response("HTTP/1.1 200 OK", search_json);
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let blocked = super::winget::version_pr_blocking(
        &base,
        "microsoft/winget-pkgs",
        "Acme.Tool",
        "1.2.3",
        true,
        None,
        &quiet_log(),
    )
    .expect("probe must succeed");
    let detail = blocked.expect("an open manifest PR must block");
    assert!(detail.contains("open manifest PR"), "got: {detail}");
    assert!(detail.contains("pull/12345"), "got: {detail}");
}

#[test]
fn winget_burn_probe_merged_pr_blocks() {
    let search_json = r#"{
        "total_count": 1,
        "items": [{
            "number": 777,
            "state": "closed",
            "html_url": "https://github.com/microsoft/winget-pkgs/pull/777",
            "pull_request": {"url": "https://api.github.com/repos/microsoft/winget-pkgs/pulls/777", "merged_at": "2026-07-01T00:00:00Z"}
        }]
    }"#;
    let body = http_response("HTTP/1.1 200 OK", search_json);
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let blocked = super::winget::version_pr_blocking(
        &base,
        "microsoft/winget-pkgs",
        "Acme.Tool",
        "1.2.3",
        true,
        None,
        &quiet_log(),
    )
    .expect("probe must succeed");
    let detail = blocked.expect("a merged manifest PR must block");
    assert!(detail.contains("merged"), "got: {detail}");
}

#[test]
fn winget_burn_probe_closed_unmerged_pr_is_clear() {
    let search_json = r#"{
        "total_count": 1,
        "items": [{
            "number": 778,
            "state": "closed",
            "html_url": "https://github.com/microsoft/winget-pkgs/pull/778",
            "pull_request": {"url": "https://api.github.com/repos/microsoft/winget-pkgs/pulls/778", "merged_at": null}
        }]
    }"#;
    let body = http_response("HTTP/1.1 200 OK", search_json);
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let blocked = super::winget::version_pr_blocking(
        &base,
        "microsoft/winget-pkgs",
        "Acme.Tool",
        "1.2.3",
        true,
        None,
        &quiet_log(),
    )
    .expect("probe must succeed");
    assert!(
        blocked.is_none(),
        "a rejected (closed-unmerged) PR released the version"
    );
}

#[test]
fn winget_burn_probe_no_match_is_clear() {
    let search_json = r#"{"total_count": 0, "items": []}"#;
    let body = http_response("HTTP/1.1 200 OK", search_json);
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let blocked = super::winget::version_pr_blocking(
        &base,
        "microsoft/winget-pkgs",
        "Acme.Tool",
        "9.9.9",
        true,
        None,
        &quiet_log(),
    )
    .expect("probe must succeed");
    assert!(blocked.is_none(), "no PR at all must be clear");
}

#[test]
fn winget_burn_probe_unreachable_api_is_an_error() {
    let addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let base = format!("http://{}", addr);
    super::winget::version_pr_blocking(
        &base,
        "microsoft/winget-pkgs",
        "Acme.Tool",
        "1.0.0",
        true,
        None,
        &quiet_log(),
    )
    .expect_err("an unreachable search API is indeterminate, never a clear/blocked verdict");
}

#[test]
fn chocolatey_burn_probe_redirect_away_is_clear() {
    // The gallery redirecting away from the requested /<id>/<version> page
    // means that exact version page does not exist — clear, not blocked.
    let body = "HTTP/1.1 302 Found\r\nLocation: /packages/anodizer\r\nContent-Length: 0\r\n\r\n";
    let (addr, _calls) = spawn_oneshot_http_responder(vec![body]);
    let base = format!("http://{}", addr);

    let blocked =
        super::chocolatey::version_blocked_on_gallery(&base, "anodizer", "9.9.9", &quiet_log())
            .expect("a redirect is a definitive answer, not an error");
    assert!(
        blocked.is_none(),
        "a redirect away from the version page must be clear"
    );
}

#[test]
fn chocolatey_burn_probe_unrecognized_200_is_clear() {
    // A 200 page with no verified moderation/approval callout is NOT
    // positive evidence — the guard's contract is refuse only on proof.
    let html = "<html><body><p>nothing recognizable here</p></body></html>";
    let body = http_response("HTTP/1.1 200 OK", html);
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let blocked =
        super::chocolatey::version_blocked_on_gallery(&base, "anodizer", "1.2.3", &quiet_log())
            .expect("an uncertain 200 must not be an error");
    assert!(
        blocked.is_none(),
        "a page with no recognizable callout must warn and clear, not block"
    );
}

#[test]
fn winget_burn_probe_open_pr_behind_closed_one_blocks() {
    // Relevance-ordered search can rank a closed-unmerged PR first; the
    // probe must classify every returned item.
    let search_json = r#"{
        "total_count": 2,
        "items": [
            {
                "number": 1,
                "title": "New version: Acme.Tool version 1.2.3",
                "state": "closed",
                "html_url": "https://github.com/microsoft/winget-pkgs/pull/1",
                "pull_request": {"merged_at": null}
            },
            {
                "number": 2,
                "title": "New version: Acme.Tool version 1.2.3",
                "state": "open",
                "html_url": "https://github.com/microsoft/winget-pkgs/pull/2",
                "pull_request": {"merged_at": null}
            }
        ]
    }"#;
    let body = http_response("HTTP/1.1 200 OK", search_json);
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let blocked = super::winget::version_pr_blocking(
        &base,
        "microsoft/winget-pkgs",
        "Acme.Tool",
        "1.2.3",
        true,
        None,
        &quiet_log(),
    )
    .expect("probe must succeed");
    let detail = blocked.expect("the open PR ranked second must still block");
    assert!(detail.contains("pull/2"), "got: {detail}");
}

#[test]
fn winget_burn_probe_merged_removal_pr_is_clear() {
    let search_json = r#"{
        "total_count": 1,
        "items": [{
            "number": 3,
            "title": "Remove Acme.Tool 1.2.3",
            "state": "closed",
            "html_url": "https://github.com/microsoft/winget-pkgs/pull/3",
            "pull_request": {"merged_at": "2026-07-01T00:00:00Z"}
        }]
    }"#;
    let body = http_response("HTTP/1.1 200 OK", search_json);
    let leaked: &'static str = Box::leak(body.into_boxed_str());
    let (addr, _calls) = spawn_oneshot_http_responder(vec![leaked]);
    let base = format!("http://{}", addr);

    let blocked = super::winget::version_pr_blocking(
        &base,
        "microsoft/winget-pkgs",
        "Acme.Tool",
        "1.2.3",
        true,
        None,
        &quiet_log(),
    )
    .expect("probe must succeed");
    assert!(
        blocked.is_none(),
        "a merged removal PR freed the version slot"
    );
}

#[test]
fn winget_poller_locates_manifest_pr_past_removal_item() {
    // Relevance-ordered search ranks a removal PR first; the poller must
    // skip it and track the actual manifest PR.
    let search_body = r#"{"total_count":2,"items":[
        {"number":10,"title":"Remove Acme.Tool 1.2.3","pull_request":{"url":"http://127.0.0.1:1/repos/x/y/pulls/10"}},
        {"number":11,"title":"New version: Acme.Tool version 1.2.3","pull_request":{"url":"__PR_URL__"}}
    ]}"#;
    let pr_body = r#"{"state":"closed","merged":true,"labels":[]}"#;

    let (pr_addr, pr_calls) = spawn_oneshot_http_responder(vec![Box::leak(
        http_response("HTTP/1.1 200 OK", pr_body).into_boxed_str(),
    )]);
    let pr_url = format!("http://{}/repos/microsoft/winget-pkgs/pulls/11", pr_addr);
    let search_body = search_body.replace("__PR_URL__", &pr_url);
    let leaked_search: &'static str =
        Box::leak(http_response("HTTP/1.1 200 OK", &search_body).into_boxed_str());
    let (search_addr, _) = spawn_oneshot_http_responder(vec![leaked_search]);
    let api_base = format!("http://{}", search_addr);

    let status = super::winget::poll(
        &api_base,
        &super::winget::PollTarget {
            upstream_slug: "microsoft/winget-pkgs".to_string(),
            package_identifier: "Acme.Tool".to_string(),
            version: "1.2.3".to_string(),
            search_in_title: true,
        },
        None,
        tight_poll_cfg(),
        &quiet_log(),
    );
    match status {
        PostPublishStatus::Approved { detail } => {
            assert!(detail.contains("merged"), "got: {detail}");
        }
        other => panic!("expected Approved via the manifest PR, got {other:?}"),
    }
    assert_eq!(
        pr_calls.load(Ordering::SeqCst),
        1,
        "the poller must fetch the manifest PR, not the removal PR"
    );
}
