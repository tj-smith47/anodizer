use super::*;
use anodizer_core::Publisher;
use anodizer_core::config::{ArtifactoryConfig, Config};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

fn make_ctx(addr: std::net::SocketAddr, deselect: bool) -> Context {
    let cfg = ArtifactoryConfig {
        name: Some("prod".into()),
        target: Some(format!("http://{addr}/repo/app.tar.gz")),
        username: Some("deployer".into()),
        password: Some("hunter2".into()),
        if_condition: if deselect { Some("false".into()) } else { None },
        ..Default::default()
    };
    let config = Config {
        project_name: "app".into(),
        artifactories: Some(vec![cfg]),
        ..Default::default()
    };
    Context::new(config, ContextOptions::default())
}

#[test]
fn artifactory_preflight_warns_on_auth_rejected() {
    let (addr, _c) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n"]);
    let ctx = make_ctx(addr, false);
    match ArtifactoryPublisher::new()
        .preflight(&ctx)
        .expect("preflight ok")
    {
        anodizer_core::PreflightCheck::Warning(m) => assert!(m.contains("artifactory"), "{m}"),
        other => panic!("expected Warning, got {other:?}"),
    }
}

#[test]
fn artifactory_preflight_passes_on_reachable_origin() {
    let (addr, _c) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"]);
    let ctx = make_ctx(addr, false);
    assert!(matches!(
        ArtifactoryPublisher::new()
            .preflight(&ctx)
            .expect("preflight ok"),
        anodizer_core::PreflightCheck::Pass
    ));
}

#[test]
fn artifactory_preflight_skips_deselected_without_request() {
    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"]);
    let ctx = make_ctx(addr, true);
    assert!(matches!(
        ArtifactoryPublisher::new()
            .preflight(&ctx)
            .expect("preflight ok"),
        anodizer_core::PreflightCheck::Pass
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}
