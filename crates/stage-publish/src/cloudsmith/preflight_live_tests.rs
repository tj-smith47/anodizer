use super::*;
use anodizer_core::Publisher;
use anodizer_core::config::{CloudSmithConfig, Config};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

fn make_ctx(base: &str, token: Option<&str>, deselect: bool) -> Context {
    let cfg = CloudSmithConfig {
        organization: Some("myorg".into()),
        repository: Some("myrepo".into()),
        if_condition: if deselect { Some("false".into()) } else { None },
        ..Default::default()
    };
    let config = Config {
        project_name: "app".into(),
        cloudsmiths: Some(vec![cfg]),
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let mut env =
        anodizer_core::MapEnvSource::new().with("ANODIZE_CLOUDSMITH_API_BASE", base.to_string());
    if let Some(t) = token {
        env = env.with("CLOUDSMITH_TOKEN", t.to_string());
    }
    ctx.set_env_source(env);
    ctx
}

#[test]
fn cloudsmith_preflight_blocks_on_invalid_token_when_required() {
    let (addr, _c) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
    ]);
    let ctx = make_ctx(&format!("http://{addr}"), Some("bad-token"), false);
    match CloudsmithPublisher::with_overrides(Some(true), None)
        .preflight(&ctx)
        .expect("preflight ok")
    {
        anodizer_core::PreflightCheck::Blocker(m) => assert!(m.contains("cloudsmith"), "{m}"),
        other => panic!("expected Blocker, got {other:?}"),
    }
}

#[test]
fn cloudsmith_preflight_passes_on_reachable() {
    let (addr, _c) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]"]);
    let ctx = make_ctx(&format!("http://{addr}"), Some("good-token"), false);
    assert!(matches!(
        CloudsmithPublisher::new()
            .preflight(&ctx)
            .expect("preflight ok"),
        anodizer_core::PreflightCheck::Pass
    ));
}

#[test]
fn cloudsmith_preflight_skips_deselected_without_request() {
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]"]);
    let ctx = make_ctx(&format!("http://{addr}"), Some("good-token"), true);
    assert!(matches!(
        CloudsmithPublisher::new()
            .preflight(&ctx)
            .expect("preflight ok"),
        anodizer_core::PreflightCheck::Pass
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// The preflight probe sends the entry's token without the whitespace around
/// it, and a token that is only whitespace counts as unset, so the entry is
/// not probed at all.
#[test]
fn cloudsmith_preflight_sends_the_env_token_trimmed() {
    use anodizer_core::test_helpers::scripted_responder::{
        ScriptedRoute, spawn_scripted_responder,
    };
    let probe_with = |token: &str| {
        let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
            method: "GET",
            path_pattern: "/packages/myorg/myrepo/?page_size=1",
            response: "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]",
            times: None,
        }]);
        let ctx = make_ctx(&format!("http://{addr}"), Some(token), false);
        let check = CloudsmithPublisher::new()
            .preflight(&ctx)
            .expect("preflight ok");
        let headers: Vec<Option<String>> = log_recorder
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.header("Authorization").map(str::to_string))
            .collect();
        (check, headers)
    };

    let (check, headers) = probe_with("  good \n");
    assert!(
        matches!(check, anodizer_core::PreflightCheck::Pass),
        "{check:?}"
    );
    assert_eq!(headers, vec![Some("Token good".to_string())]);

    let (check, headers) = probe_with(" \t\n");
    assert!(
        matches!(check, anodizer_core::PreflightCheck::Pass),
        "{check:?}"
    );
    assert!(
        headers.is_empty(),
        "a blank token skips the probe: {headers:?}"
    );
}
