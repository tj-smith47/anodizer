//! End-to-end coverage for [`run_github_backend`] dispatch paths.
//!
//! These tests drive the orchestrator against a scripted in-process
//! HTTP responder so the create-vs-update-vs-replace branching,
//! upload-asset happy path, and 422 `already_exists` recovery arms
//! are pinned against the production wiring — not just the helper
//! classifiers (which have their own unit tests).
//!
//! ## Fixture wiring
//!
//! Every test points two URL surfaces at the loopback responder:
//!
//! - `ctx.config.github_urls.api` / `.upload` — the octocrab
//!   builder honors these, so every API call (list / create /
//!   PATCH / asset list / asset delete) routes through
//!   `http://addr/`. The release JSON returned by POST /releases
//!   carries `upload_url: http://addr/...` so `upload_asset(...)`
//!   POSTs to the same loopback.
//! - `ANODIZER_GITHUB_API_BASE` — the rate-limit poll honors this
//!   override. `build_ctx` seeds it through the [`Context`]'s
//!   injected [`MapEnvSource`](anodizer_core::MapEnvSource) so
//!   the proactive `/rate_limit` poll either matches a scripted
//!   route or silently degrades on 404, never delaying the test.
//!
//! Env injection is per-[`Context`], so parallel tests cannot race
//! and no global env-mutex is required.

use super::*;
use anodizer_core::config::{CrateConfig, GitHubUrlsConfig, ReleaseConfig, ScmRepoConfig};
use anodizer_core::context::Context;
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder_on};
use octocrab::repos::releases::MakeLatest;
use std::net::SocketAddr;
use std::path::PathBuf;
use tempfile::TempDir;

/// Wrap a JSON body in a `200 OK` HTTP response with the correct
/// `Content-Length`. Leaks the formatted string because the responder
/// requires `&'static str`; harmless in tests.
fn http_ok(body: String) -> &'static str {
    let len = body.len();
    Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        )
}

/// Same as [`http_ok`] but emits `201 Created`. GitHub returns 201 for
/// release create + asset upload; the orchestrator does not distinguish
/// 200 vs 201, but using the realistic status keeps the fixture honest.
fn http_201(body: String) -> &'static str {
    let len = body.len();
    Box::leak(
            format!(
                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}"
            )
            .into_boxed_str(),
        )
}

/// `204 No Content` for successful DELETE.
const HTTP_204: &str = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";

/// Build a minimal Release JSON octocrab can deserialize into
/// `models::repos::Release`. The `upload_url` field is the load-bearing
/// one: `upload_asset(...).send()` does a GET on the release and reads
/// `upload_url` to determine where to POST the asset bytes.
fn release_json(addr: SocketAddr, id: u64, draft: bool, name: &str) -> String {
    serde_json::json!({
        "id": id,
        "node_id": format!("RL_{id}"),
        "tag_name": "v1.2.3",
        "target_commitish": "main",
        "name": name,
        "draft": draft,
        "prerelease": false,
        "created_at": "2026-01-01T00:00:00Z",
        "published_at": null,
        "author": null,
        "assets": [],
        "tarball_url": null,
        "zipball_url": null,
        "body": null,
        "url": format!("http://{addr}/repos/o/r/releases/{id}"),
        "html_url": format!("http://{addr}/o/r/releases/{id}"),
        "assets_url": format!("http://{addr}/repos/o/r/releases/{id}/assets"),
        // upload_url MUST carry the `{?name,label}` template that
        // octocrab strips before appending `?name=<file>`. Without the
        // template, octocrab leaves the URL malformed and the upload
        // POSTs to the wrong path.
        "upload_url": format!("http://{addr}/upload/{id}{{?name,label}}"),
    })
    .to_string()
}

/// Like [`release_json`] but with an explicit `tag_name` (distinct nightly
/// tags such as `…-nightly.<build>` need their own tag for the retention
/// sweep's tag-delete assertions). Targets owner=o/repo=r for the API URLs,
/// matching the override-repo responder used by the retention tests.
fn release_json_named(addr: SocketAddr, id: u64, name: &str, tag: &str) -> String {
    serde_json::json!({
        "id": id,
        "node_id": format!("RL_{id}"),
        "tag_name": tag,
        "target_commitish": "main",
        "name": name,
        "draft": false,
        "prerelease": false,
        "created_at": "2026-01-01T00:00:00Z",
        "published_at": null,
        "author": null,
        "assets": [],
        "tarball_url": null,
        "zipball_url": null,
        "body": null,
        "url": format!("http://{addr}/repos/o/r/releases/{id}"),
        "html_url": format!("http://{addr}/o/r/releases/{id}"),
        "assets_url": format!("http://{addr}/repos/o/r/releases/{id}/assets"),
        "upload_url": format!("http://{addr}/upload/{id}{{?name,label}}"),
    })
    .to_string()
}

/// Minimal Asset JSON for the 201 response of an asset-upload POST.
fn asset_json(id: u64, name: &str, size: u64) -> String {
    serde_json::json!({
        "url": format!("http://example.test/asset/{id}"),
        "browser_download_url": format!("http://example.test/dl/{name}"),
        "id": id,
        "node_id": format!("RA_{id}"),
        "name": name,
        "label": null,
        "state": "uploaded",
        "content_type": "application/octet-stream",
        "size": size,
        "download_count": 0,
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "uploader": null,
    })
    .to_string()
}

/// 422 already_exists body. Pairs with HTTP status 422; the upload
/// classifier matches `errors[].code == "already_exists"`.
fn http_422_already_exists() -> &'static str {
    let body = r#"{"message":"Validation Failed","errors":[{"resource":"ReleaseAsset","code":"already_exists","field":"name"}]}"#;
    let len = body.len();
    Box::leak(
            format!("HTTP/1.1 422 Unprocessable Entity\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}")
                .into_boxed_str(),
        )
}

/// Build a [`Context`] with `github_urls` pointing at `addr` so every
/// production octocrab call routes through the loopback responder, and
/// a fast retry policy (millisecond delays) so the upload retry loop
/// in [`run_github_backend`] doesn't pad test runs with the production
/// 10-second default backoff.
fn build_ctx(addr: SocketAddr) -> Context {
    let base = format!("http://{addr}");
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .token(Some("test-token".to_string()))
        .env("ANODIZER_GITHUB_API_BASE", &base)
        .build();
    ctx.config.github_urls = Some(GitHubUrlsConfig {
        api: Some(base.clone()),
        upload: Some(base.clone()),
        download: Some(base),
        skip_tls_verify: None,
    });
    ctx.config.retry = Some(anodizer_core::config::RetryConfig {
        attempts: 5,
        delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(1)),
        max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(2)),
        max_elapsed: None,
    });
    ctx
}

/// Build a `CrateConfig` whose `release.github` points at owner=o, name=r.
fn build_crate_cfg() -> CrateConfig {
    let mut crate_cfg = CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ Version }}".to_string()),
        ..Default::default()
    };
    crate_cfg.release = Some(ReleaseConfig {
        github: Some(ScmRepoConfig {
            owner: "o".to_string(),
            name: "r".to_string(),
            token: None,
        }),
        mode: Some("replace".to_string()),
        ..Default::default()
    });
    crate_cfg
}

/// Write a small artifact file and return its path. The `run_github_backend`
/// upload loop calls `std::fs::read` and uses the file's bytes (and
/// length) for the upload POST + 422 size-compare branch.
fn write_artifact(dir: &std::path::Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write artifact");
    path
}

/// Owned ancillary fields that [`GithubReleaseSpec`] borrows. Bind in
/// the test scope then pass into [`make_spec`] so the borrows outlive
/// the spec struct.
struct SpecAncillary {
    make_latest: Option<MakeLatest>,
    target_commitish: Option<String>,
    discussion_category: Option<String>,
}

fn spec_ancillary_default() -> SpecAncillary {
    SpecAncillary {
        make_latest: None,
        target_commitish: None,
        discussion_category: None,
    }
}

/// Common spec: tag=v1.2.3, draft=true (so `user_wants_draft` short-circuits
/// the publish PATCH), mode=replace (so `get_by_tag` lookup is skipped).
fn make_spec(anc: &SpecAncillary) -> GithubReleaseSpec<'_> {
    GithubReleaseSpec {
        tag: "v1.2.3",
        name: "v1.2.3",
        body: "release body",
        mode: "replace",
        draft: true,
        prerelease: false,
        make_latest: &anc.make_latest,
        target_commitish: &anc.target_commitish,
        discussion_category: &anc.discussion_category,
    }
}

/// Default upload opts: every flag off.
fn base_opts() -> UploadOpts {
    UploadOpts {
        skip_upload: false,
        replace_existing_draft: false,
        replace_existing_artifacts: false,
        use_existing_draft: false,
        resume_release: false,
        retention_keep_last: None,
        publish_repo_override: None,
    }
}

/// `run_github_backend`'s success payload: `(html_url, download_base,
/// owner, repo)` or `None` when the backend signals skip.
type BackendOutcome = Result<Option<(String, String, String, String)>>;

/// Build the four ambient handles `run_github_backend` consumes.
fn run_backend(
    rt: &tokio::runtime::Runtime,
    ctx: &Context,
    token: &Option<String>,
    crate_cfg: &CrateConfig,
    spec: &GithubReleaseSpec<'_>,
    opts: &UploadOpts,
    artifacts: &[(PathBuf, Option<String>)],
) -> BackendOutcome {
    let log = StageLogger::new("release", Verbosity::Normal);
    let env = BackendEnv {
        rt,
        ctx,
        log: &log,
        token,
    };
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg present");
    run_github_backend(&env, crate_cfg, release_cfg, spec, opts, artifacts)
}

/// Like [`run_backend`] but attaches a [`LogCapture`] so a test can assert
/// on the status lines the backend emits (not just the HTTP calls it makes).
#[allow(clippy::too_many_arguments)]
fn run_backend_capturing(
    rt: &tokio::runtime::Runtime,
    ctx: &Context,
    token: &Option<String>,
    crate_cfg: &CrateConfig,
    spec: &GithubReleaseSpec<'_>,
    opts: &UploadOpts,
    artifacts: &[(PathBuf, Option<String>)],
) -> (BackendOutcome, anodizer_core::log::LogCapture) {
    let (log, capture) = StageLogger::with_capture("release", Verbosity::Normal);
    let env = BackendEnv {
        rt,
        ctx,
        log: &log,
        token,
    };
    let release_cfg = crate_cfg.release.as_ref().expect("release cfg present");
    let result = run_github_backend(&env, crate_cfg, release_cfg, spec, opts, artifacts);
    (result, capture)
}

// ---------------------------------------------------------------------
// 1. Happy path — create new release, upload one asset.
// ---------------------------------------------------------------------
#[test]
fn create_release_and_upload_one_asset_succeeds() {
    let tmp = TempDir::new().expect("tempdir");
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"hello world");
    let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

    // Reserve an ephemeral port then drop the listener so the scripted
    // responder can claim the same port — the release_json fixture
    // needs to embed the bound addr into `upload_url`, which the
    // upload_asset() flow reads back to route its POST.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let release = release_json(addr, 42, true, "v1.2.3");

    let routes = vec![
        // (1) Create-release POST.
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(release.clone()),
            times: Some(1),
        },
        // (2) upload_asset() first GETs the release to read upload_url.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(release),
            times: None,
        },
        // (3) The asset POST itself.
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
    let anc = spec_ancillary_default();

    let result = run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &base_opts(),
        &artifacts,
    )
    .expect("run_github_backend succeeds");
    let (html_url, dl_base, owner, repo) = result.expect("returns Some on success");
    assert_eq!(owner, "o");
    assert_eq!(repo, "r");
    // gh_download_base derives from github_urls.download (set to
    // the loopback by build_ctx); html_url composes deterministically
    // from it.
    assert!(
        html_url.contains("/o/r/releases/tag/v1.2.3"),
        "got: {html_url}"
    );
    assert!(dl_base.starts_with("http://"), "got: {dl_base}");

    let entries = log.lock().expect("log mutex");
    let post_create = entries
        .iter()
        .find(|e| e.method == "POST" && e.path == "/repos/o/r/releases")
        .expect("must POST /repos/o/r/releases to create the release");
    assert!(
        post_create.body.contains("\"name\":\"v1.2.3\""),
        "create body must include the release name: {}",
        post_create.body
    );
    assert!(
        post_create.body.contains("\"draft\":true"),
        "create body must request draft=true (draft-then-publish): {}",
        post_create.body
    );
    let upload_call = entries
        .iter()
        .find(|e| e.method == "POST" && e.path == "/upload/42?name=demo.tar.gz")
        .expect("must POST the asset to the upload_url returned in the release JSON");
    assert_eq!(
        upload_call.body, "hello world",
        "upload body must equal the file bytes"
    );
}

// ---------------------------------------------------------------------
// 2. replace_existing_draft = true — find existing draft, delete it,
// then create a new release.
// ---------------------------------------------------------------------
#[test]
fn replace_existing_draft_deletes_then_creates() {
    let tmp = TempDir::new().expect("tempdir");
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"payload");
    let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    // Existing draft (id=99) returned by list-releases.
    let list_body = format!("[{}]", release_json(addr, 99, true, "v1.2.3"));
    // New draft (id=42) created after the delete.
    let new_release = release_json(addr, 42, true, "v1.2.3");

    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases?per_page=100&page=1",
            response: http_ok(list_body),
            times: Some(1),
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repos/o/r/releases/99",
            response: HTTP_204,
            times: Some(1),
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(new_release.clone()),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(new_release),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

    let mut opts = base_opts();
    opts.replace_existing_draft = true;
    let anc = spec_ancillary_default();
    run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &opts,
        &artifacts,
    )
    .expect("backend succeeds")
    .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        entries
            .iter()
            .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/99"),
        "must DELETE the existing draft (id=99); calls: {entries:?}",
    );
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/repos/o/r/releases"),
        "must POST a fresh release after the delete; calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// nightly publish_repo: the release create, asset upload, AND the
// composed html_url all target the OVERRIDE repo (nushell/nightly),
// not the source repo (o/r) resolved from release.github.
// ---------------------------------------------------------------------
#[test]
fn publish_repo_override_redirects_create_and_upload() {
    let tmp = TempDir::new().expect("tempdir");
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"hello world");
    let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    // Override repo's API URLs use /repos/nushell/nightly/...
    let release = serde_json::json!({
        "id": 42, "node_id": "RL_42", "tag_name": "v1.2.3",
        "target_commitish": "main", "name": "v1.2.3", "draft": true,
        "prerelease": false, "created_at": "2026-01-01T00:00:00Z",
        "published_at": null, "author": null, "assets": [],
        "tarball_url": null, "zipball_url": null, "body": null,
        "url": format!("http://{addr}/repos/nushell/nightly/releases/42"),
        "html_url": format!("http://{addr}/nushell/nightly/releases/42"),
        "assets_url": format!("http://{addr}/repos/nushell/nightly/releases/42/assets"),
        "upload_url": format!("http://{addr}/upload/42{{?name,label}}"),
    })
    .to_string();

    let routes = vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/nushell/nightly/releases",
            response: http_201(release.clone()),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/nushell/nightly/releases/42",
            response: http_ok(release),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
    ];
    let (_a, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
    let anc = spec_ancillary_default();

    let mut opts = base_opts();
    opts.publish_repo_override = Some(("nushell".to_string(), "nightly".to_string()));

    let result = run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &opts,
        &artifacts,
    )
    .expect("backend succeeds");
    let (html_url, _dl, owner, repo) = result.expect("returns Some");
    // Returned owner/repo + html_url reflect the OVERRIDE repo.
    assert_eq!(owner, "nushell");
    assert_eq!(repo, "nightly");
    assert!(
        html_url.contains("/nushell/nightly/releases/tag/v1.2.3"),
        "got: {html_url}"
    );

    let entries = log.lock().expect("log mutex");
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/repos/nushell/nightly/releases"),
        "create must target the override repo; calls: {entries:?}",
    );
    // No call may touch the source repo (o/r).
    assert!(
        !entries.iter().any(|e| e.path.starts_with("/repos/o/r/")),
        "no call may target the source repo o/r; calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// nightly retention keep_last=2: list nightly releases by name, keep the
// newest 1 existing (the new one becomes the 2nd), DELETE the older
// release AND its distinct git tag.
// ---------------------------------------------------------------------
#[test]
fn retention_keep_last_prunes_old_release_and_tag() {
    let tmp = TempDir::new().expect("tempdir");
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"x");
    let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    // The retention sweep now runs AFTER the new release is created, so the
    // list-by-name returns the just-created release (id=42) alongside the
    // two existing nightly releases. Newest-first: 42, 11, 10. With
    // keep_last=2 the kept set is {42, 11}; id=10 + its tag "nightly.0" is
    // pruned. The new release id=42 must NEVER be pruned.
    let new_release = release_json_named(addr, 42, "demo-nightly", "v1.2.3");
    let list_body = format!(
        "[{},{},{}]",
        release_json_named(addr, 42, "demo-nightly", "v1.2.3"),
        release_json_named(addr, 11, "demo-nightly", "nightly.1"),
        release_json_named(addr, 10, "demo-nightly", "nightly.0"),
    );

    let routes = vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(new_release.clone()),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(new_release),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases?per_page=100&page=1",
            response: http_ok(list_body),
            times: Some(1),
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repos/o/r/releases/10",
            response: HTTP_204,
            times: Some(1),
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repos/o/r/git/refs/tags/nightly.0",
            response: HTTP_204,
            times: Some(1),
        },
    ];
    let (_a, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
    let anc = spec_ancillary_default();
    // The nightly release name the sweep matches on.
    let spec = GithubReleaseSpec {
        name: "demo-nightly",
        ..make_spec(&anc)
    };

    let mut opts = base_opts();
    opts.retention_keep_last = Some(2);

    run_backend(&rt, &ctx, &token, &crate_cfg, &spec, &opts, &artifacts)
        .expect("backend succeeds")
        .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        entries
            .iter()
            .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/10"),
        "must delete the pruned release id=10; calls: {entries:?}",
    );
    assert!(
        entries
            .iter()
            .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/git/refs/tags/nightly.0"),
        "must delete the pruned release's distinct git tag; calls: {entries:?}",
    );
    // The kept release (id=11) must NOT be deleted.
    assert!(
        !entries
            .iter()
            .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/11"),
        "must KEEP the newest existing release id=11; calls: {entries:?}",
    );
    // The just-created release (id=42) must NEVER be deleted by the sweep.
    assert!(
        !entries
            .iter()
            .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/42"),
        "the just-created release id=42 must never be pruned; calls: {entries:?}",
    );

    // M6 ordering: the new release must be created (and its asset uploaded)
    // BEFORE any retention delete fires. Pruning before the new release is
    // live is irreversible-before-reversible.
    let create_pos = entries
        .iter()
        .position(|e| e.method == "POST" && e.path == "/repos/o/r/releases")
        .expect("create POST must occur");
    let upload_pos = entries
        .iter()
        .position(|e| e.method == "POST" && e.path == "/upload/42?name=demo.tar.gz")
        .expect("asset upload POST must occur");
    let first_delete_pos = entries
        .iter()
        .position(|e| e.method == "DELETE" && e.path.starts_with("/repos/o/r/releases/"))
        .expect("a retention delete must occur");
    assert!(
        create_pos < first_delete_pos,
        "release must be created before any retention delete; calls: {entries:?}",
    );
    assert!(
        upload_pos < first_delete_pos,
        "asset upload must complete before any retention delete; calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// replace_existing_draft = true with the NEW release published
// (`draft: false`): the leftover draft must still be deleted. This pins
// the self-heal path: publishes while replacing a stale
// draft from a prior failed run; gating the delete on the new release's
// draft flag would skip cleanup and the stale id later 404s on upload.
// ---------------------------------------------------------------------
#[test]
fn replace_existing_draft_deletes_when_publishing() {
    let tmp = TempDir::new().expect("tempdir");
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"payload");
    let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    // Existing draft (id=99) returned by list-releases.
    let list_body = format!("[{}]", release_json(addr, 99, true, "v1.2.3"));
    // New PUBLISHED release (id=42, draft=false) created after the delete.
    let new_release = release_json(addr, 42, false, "v1.2.3");

    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases?per_page=100&page=1",
            response: http_ok(list_body),
            times: Some(1),
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repos/o/r/releases/99",
            response: HTTP_204,
            times: Some(1),
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(new_release.clone()),
            times: Some(1),
        },
        // Un-draft PATCH: the release is created as a draft then flipped
        // live because the spec requests `draft: false`.
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(new_release.clone()),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(new_release),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

    let mut opts = base_opts();
    opts.replace_existing_draft = true;
    let anc = spec_ancillary_default();
    // Publish (draft: false) while replacing a stale draft — the self-heal recovery path.
    let mut spec = make_spec(&anc);
    spec.draft = false;
    run_backend(&rt, &ctx, &token, &crate_cfg, &spec, &opts, &artifacts)
        .expect("backend succeeds")
        .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        entries
            .iter()
            .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/99"),
        "must DELETE the stale draft (id=99) even when publishing; calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// 3. use_existing_draft = true — find existing draft, PATCH it (no POST).
// ---------------------------------------------------------------------
#[test]
fn use_existing_draft_patches_instead_of_posting() {
    let tmp = TempDir::new().expect("tempdir");
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"data");
    let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let existing = release_json(addr, 55, true, "v1.2.3");
    let list_body = format!("[{}]", existing.clone());

    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases?per_page=100&page=1",
            response: http_ok(list_body),
            times: Some(1),
        },
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/repos/o/r/releases/55",
            response: http_ok(existing.clone()),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/55",
            response: http_ok(existing),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/55?name=demo.tar.gz",
            response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

    let mut opts = base_opts();
    opts.use_existing_draft = true;
    let anc = spec_ancillary_default();
    run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &opts,
        &artifacts,
    )
    .expect("backend succeeds")
    .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        entries
            .iter()
            .any(|e| e.method == "PATCH" && e.path == "/repos/o/r/releases/55"),
        "use_existing_draft must PATCH the existing release; calls: {entries:?}",
    );
    assert!(
        !entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/repos/o/r/releases"),
        "use_existing_draft must NOT POST a new release (would 422 on duplicate tag); calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// 3b. keep-existing re-touch of an already-live release — the publish
//     pipeline pass that runs after the release stage already created and
//     published the release. The PATCH stays idempotent, but the
//     create/publish log lines collapse to a single `release already live`.
// ---------------------------------------------------------------------
#[test]
fn keep_existing_retouch_of_live_release_logs_already_live_only() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    // An already-published (draft=false) release found by tag.
    let live = release_json(addr, 77, false, "v1.2.3");

    let routes = vec![
        // get_by_tag lookup finds the live release.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/tags/v1.2.3",
            response: http_ok(live.clone()),
            times: Some(1),
        },
        // PATCH the existing release (idempotent update).
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/repos/o/r/releases/77",
            response: http_ok(live.clone()),
            times: None,
        },
    ];
    let (_addr2, _log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts: Vec<(PathBuf, Option<String>)> = Vec::new();

    // mode=keep-existing, draft=false (user wants the release live).
    let spec = GithubReleaseSpec {
        tag: "v1.2.3",
        name: "v1.2.3",
        body: "release body",
        mode: "keep-existing",
        draft: false,
        prerelease: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    };

    let (result, capture) = run_backend_capturing(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &spec,
        &base_opts(),
        &artifacts,
    );
    result.expect("backend succeeds").expect("returns Some");

    let messages: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
    assert!(
        messages
            .iter()
            .any(|m| m == "release 'v1.2.3' already live (id=77, mode=keep-existing)"),
        "re-touch of a live release must log the concise already-live line; got: {messages:?}"
    );
    assert!(
        !messages
            .iter()
            .any(|m| m.contains("created GitHub Release")),
        "re-touch must NOT re-emit the create line; got: {messages:?}"
    );
    assert!(
        !messages.iter().any(|m| m.contains("published release")),
        "re-touch must NOT re-emit the publish line; got: {messages:?}"
    );
}

/// A live release found by tag, carrying its assets, that is re-touched by
/// the publish pipeline pass (`--publish-only` → `resume_release=true`) must
/// NOT re-POST any asset when no overwrite was requested. Every re-POST
/// returns `422 already_exists`; the redundant ~115-asset burst trips
/// GitHub's secondary rate limit. With `replace_existing_artifacts=false`,
/// the retouch path skips the upload loop entirely (zero `/upload/` POSTs,
/// zero DELETEs).
#[test]
fn publish_only_retouch_live_without_replace_uploads_nothing() {
    let tmp = TempDir::new().expect("tempdir");
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"already-attached");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    // A live (draft=false) release found by tag whose `demo.tar.gz` is
    // already attached — exactly the shape the publish-only pass sees after
    // the release stage created, uploaded, and published it.
    let attached = asset_json(9, "demo.tar.gz", 16);
    let live_with_asset = serde_json::json!({
        "id": 77,
        "node_id": "RL_77",
        "tag_name": "v1.2.3",
        "target_commitish": "main",
        "name": "v1.2.3",
        "draft": false,
        "prerelease": false,
        "created_at": "2026-01-01T00:00:00Z",
        "published_at": "2026-01-01T00:00:00Z",
        "author": null,
        "assets": [serde_json::from_str::<serde_json::Value>(&attached).unwrap()],
        "tarball_url": null,
        "zipball_url": null,
        "body": null,
        "url": format!("http://{addr}/repos/o/r/releases/77"),
        "html_url": format!("http://{addr}/o/r/releases/77"),
        "assets_url": format!("http://{addr}/repos/o/r/releases/77/assets"),
        "upload_url": format!("http://{addr}/upload/77{{?name,label}}"),
    })
    .to_string();

    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/tags/v1.2.3",
            response: http_ok(live_with_asset.clone()),
            times: None,
        },
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/repos/o/r/releases/77",
            response: http_ok(live_with_asset.clone()),
            times: None,
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

    // keep-existing + live release => retouch_live; publish-only sets
    // resume_release=true (so the leftover-assets pre-check does not bail).
    let spec = GithubReleaseSpec {
        tag: "v1.2.3",
        name: "v1.2.3",
        body: "release body",
        mode: "keep-existing",
        draft: false,
        prerelease: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    };
    let mut opts = base_opts();
    opts.resume_release = true;

    run_backend(&rt, &ctx, &token, &crate_cfg, &spec, &opts, &artifacts)
        .expect("backend succeeds")
        .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        !entries.iter().any(|e| e.path.starts_with("/upload/")),
        "retouch of a live release without --replace must NOT re-POST any \
             asset; calls: {entries:?}",
    );
    assert!(
        !entries.iter().any(|e| e.method == "DELETE"),
        "no overwrite requested => no asset DELETE; calls: {entries:?}",
    );
}

/// The same live-release retouch WITH `replace_existing_artifacts=true`
/// (operator asked for a real overwrite) keeps the full upload loop: each
/// asset is re-uploaded (the 422 already_exists size-mismatch path deletes
/// the stale asset and retries). Proves the fix suppresses uploads ONLY on
/// the no-replace path.
#[test]
fn publish_only_retouch_live_with_replace_reuploads() {
    let tmp = TempDir::new().expect("tempdir");
    let bytes = b"fresh bytes";
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", bytes);
    let artifact_len = bytes.len() as u64;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let attached = asset_json(9, "demo.tar.gz", 9999);
    let live_with_asset = serde_json::json!({
        "id": 77,
        "node_id": "RL_77",
        "tag_name": "v1.2.3",
        "target_commitish": "main",
        "name": "v1.2.3",
        "draft": false,
        "prerelease": false,
        "created_at": "2026-01-01T00:00:00Z",
        "published_at": "2026-01-01T00:00:00Z",
        "author": null,
        "assets": [serde_json::from_str::<serde_json::Value>(&attached).unwrap()],
        "tarball_url": null,
        "zipball_url": null,
        "body": null,
        "url": format!("http://{addr}/repos/o/r/releases/77"),
        "html_url": format!("http://{addr}/o/r/releases/77"),
        "assets_url": format!("http://{addr}/repos/o/r/releases/77/assets"),
        "upload_url": format!("http://{addr}/upload/77{{?name,label}}"),
    })
    .to_string();
    let stale_list = format!("[{}]", asset_json(9, "demo.tar.gz", 9999));

    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/tags/v1.2.3",
            response: http_ok(live_with_asset.clone()),
            times: None,
        },
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/repos/o/r/releases/77",
            response: http_ok(live_with_asset.clone()),
            times: None,
        },
        // readiness probe before uploads.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/77",
            response: http_ok(live_with_asset.clone()),
            times: None,
        },
        // First upload 422s (asset already present, size mismatch).
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/77?name=demo.tar.gz",
            response: http_422_already_exists(),
            times: Some(1),
        },
        // Size-probe lists the stale asset; DELETE clears it.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/77/assets?per_page=100&page=1",
            response: http_ok(stale_list),
            times: None,
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repos/o/r/releases/assets/9",
            response: HTTP_204,
            times: None,
        },
        // Retry upload succeeds.
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/77?name=demo.tar.gz",
            response: http_201(asset_json(11, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

    let spec = GithubReleaseSpec {
        tag: "v1.2.3",
        name: "v1.2.3",
        body: "release body",
        mode: "keep-existing",
        draft: false,
        prerelease: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    };
    let mut opts = base_opts();
    opts.resume_release = true;
    opts.replace_existing_artifacts = true;

    run_backend(&rt, &ctx, &token, &crate_cfg, &spec, &opts, &artifacts)
        .expect("backend succeeds")
        .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    let uploads = entries
        .iter()
        .filter(|e| e.method == "POST" && e.path == "/upload/77?name=demo.tar.gz")
        .count();
    assert!(
        uploads >= 1,
        "with --replace the upload loop must run and re-POST the asset; \
             calls: {entries:?}",
    );
    assert!(
        entries
            .iter()
            .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/assets/9"),
        "with --replace the stale asset must be DELETEd before re-upload; \
             calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// 4. No artifacts — release is created but upload loop runs zero times.
// ---------------------------------------------------------------------
#[test]
fn empty_artifacts_creates_release_but_uploads_nothing() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let routes = vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repos/o/r/releases",
        response: http_201(release_json(addr, 42, true, "v1.2.3")),
        times: Some(1),
    }];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts: Vec<(PathBuf, Option<String>)> = Vec::new();
    let anc = spec_ancillary_default();

    run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &base_opts(),
        &artifacts,
    )
    .expect("backend succeeds")
    .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/repos/o/r/releases"),
        "must still POST create-release even with no artifacts; calls: {entries:?}",
    );
    assert!(
        !entries.iter().any(|e| e.path.starts_with("/upload/")),
        "empty artifact list must skip every upload POST; calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// 5. 422 already_exists + matching remote size → SkipIdempotent (no
// delete, no error, success).
// ---------------------------------------------------------------------
#[test]
fn upload_422_with_matching_remote_size_is_idempotent_skip() {
    let tmp = TempDir::new().expect("tempdir");
    let bytes = b"identical bytes";
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", bytes);
    let artifact_len = bytes.len() as u64;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let release = release_json(addr, 42, true, "v1.2.3");

    let assets_page = format!("[{}]", asset_json(9, "demo.tar.gz", artifact_len));

    let routes = vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(release.clone()),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(release),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_422_already_exists(),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42/assets?per_page=100&page=1",
            response: http_ok(assets_page),
            times: None,
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
    let anc = spec_ancillary_default();

    run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &base_opts(),
        &artifacts,
    )
    .expect("422 + size match must succeed as SkipIdempotent")
    .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        !entries.iter().any(|e| e.method == "DELETE"),
        "SkipIdempotent must NOT issue a DELETE; calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// 6. 422 already_exists + size mismatch + replace_existing_artifacts=false
// → BailReplaceForbidden surfaces an error.
// ---------------------------------------------------------------------
#[test]
fn upload_422_size_mismatch_without_replace_forbidden_bails() {
    let tmp = TempDir::new().expect("tempdir");
    let bytes = b"local content";
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", bytes);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let release = release_json(addr, 42, true, "v1.2.3");

    // Remote asset reports a DIFFERENT size (9999 vs local len).
    let assets_page = format!("[{}]", asset_json(9, "demo.tar.gz", 9999));

    let routes = vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(release.clone()),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(release),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_422_already_exists(),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42/assets?per_page=100&page=1",
            response: http_ok(assets_page),
            times: None,
        },
    ];
    let (_addr2, _log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
    let anc = spec_ancillary_default();

    // replace_existing_artifacts left false (default base_opts).
    let err = run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &base_opts(),
        &artifacts,
    )
    .expect_err("size-mismatch with replace=false must Err");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("replace_existing_artifacts: false")
            || msg.contains("already exists")
            || msg.contains("upload artifact"),
        "error must explain the conflict: {msg}",
    );
}

// ---------------------------------------------------------------------
// 7. 422 already_exists + size mismatch + replace_existing_artifacts=true
// → DeleteAndRetry succeeds on the second attempt.
// ---------------------------------------------------------------------
#[test]
fn upload_422_size_mismatch_with_replace_allowed_deletes_and_retries() {
    let tmp = TempDir::new().expect("tempdir");
    let bytes = b"new content";
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", bytes);
    let artifact_len = bytes.len() as u64;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let release = release_json(addr, 42, true, "v1.2.3");

    // First upload hits 422. The size probe returns 9999 (existing)
    // vs 11 (local) — classify_already_exists routes to
    // DeleteAndRetry, the stale asset_id=9 is deleted, and the
    // second upload succeeds.
    let stale_asset = asset_json(9, "demo.tar.gz", 9999);
    let stale_list = format!("[{stale_asset}]");

    let routes = vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(release.clone()),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(release),
            times: None,
        },
        // Size-probe + recovery delete (size mismatch path,
        // triggered by the 422 below): GET assets returns the
        // stale asset; DELETE asset_id=9 clears the way; second
        // upload below succeeds.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42/assets?per_page=100&page=1",
            response: http_ok(stale_list),
            times: None,
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repos/o/r/releases/assets/9",
            response: HTTP_204,
            times: None,
        },
        // First upload attempt: 422.
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_422_already_exists(),
            times: Some(1),
        },
        // Second upload attempt (after recovery delete): success.
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_201(asset_json(11, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

    let mut opts = base_opts();
    opts.replace_existing_artifacts = true;
    let anc = spec_ancillary_default();
    run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &opts,
        &artifacts,
    )
    .expect("delete+retry must recover and succeed")
    .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    let delete_count = entries
        .iter()
        .filter(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/assets/9")
        .count();
    assert!(
        delete_count >= 1,
        "replace_existing_artifacts=true must DELETE the stale asset at least once; calls: {entries:?}",
    );
    let upload_count = entries
        .iter()
        .filter(|e| e.method == "POST" && e.path == "/upload/42?name=demo.tar.gz")
        .count();
    assert_eq!(
        upload_count, 2,
        "expected exactly 2 upload POSTs (first 422, second 201); calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// 7b. A DRAFT release found by tag with leftover assets auto-resumes:
// replace_existing_artifacts AND resume_release both false, yet the stale
// asset is overwritten (DELETE + re-upload) and the backend succeeds — no
// "left by a prior failed attempt" bail. A draft is never publicly
// downloadable (draft-then-publish invariant), so it is debris from an
// incomplete prior attempt and a CI retry must self-heal without an
// operator passing --resume-release / --replace-existing.
// ---------------------------------------------------------------------
#[test]
fn draft_found_by_tag_auto_resumes_overwriting_leftover_assets() {
    let tmp = TempDir::new().expect("tempdir");
    let bytes = b"fresh content";
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", bytes);
    let artifact_len = bytes.len() as u64;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    // find-by-tag returns a DRAFT (id=88) already carrying a stale
    // demo.tar.gz (size 9999) left by a prior failed attempt.
    let stale_asset: serde_json::Value =
        serde_json::from_str(&asset_json(9, "demo.tar.gz", 9999)).expect("asset json");
    let draft_with_stale = serde_json::json!({
        "id": 88,
        "node_id": "RL_88",
        "tag_name": "v1.2.3",
        "target_commitish": "main",
        "name": "v1.2.3",
        "draft": true,
        "prerelease": false,
        "created_at": "2026-01-01T00:00:00Z",
        "published_at": null,
        "author": null,
        "assets": [stale_asset],
        "tarball_url": null,
        "zipball_url": null,
        "body": null,
        "url": format!("http://{addr}/repos/o/r/releases/88"),
        "html_url": format!("http://{addr}/o/r/releases/88"),
        "assets_url": format!("http://{addr}/repos/o/r/releases/88/assets"),
        "upload_url": format!("http://{addr}/upload/88{{?name,label}}"),
    })
    .to_string();
    let stale_list = format!("[{}]", asset_json(9, "demo.tar.gz", 9999));

    let routes = vec![
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/tags/v1.2.3",
            response: http_ok(draft_with_stale.clone()),
            times: None,
        },
        // PATCH the existing draft (update body, draft state preserved).
        ScriptedRoute {
            method: "PATCH",
            path_pattern: "/repos/o/r/releases/88",
            response: http_ok(draft_with_stale.clone()),
            times: None,
        },
        // readability guard + per-upload reads.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/88",
            response: http_ok(draft_with_stale.clone()),
            times: None,
        },
        // size-probe assets list (stale 9999 vs local) → DeleteAndRetry.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/88/assets?per_page=100&page=1",
            response: http_ok(stale_list),
            times: None,
        },
        ScriptedRoute {
            method: "DELETE",
            path_pattern: "/repos/o/r/releases/assets/9",
            response: HTTP_204,
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/88?name=demo.tar.gz",
            response: http_422_already_exists(),
            times: Some(1),
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/88?name=demo.tar.gz",
            response: http_201(asset_json(11, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

    // mode != "replace" so the find-by-tag lookup runs; the draft is kept
    // as a draft (no un-draft publish PATCH). base_opts leaves
    // replace_existing_artifacts AND resume_release FALSE — the draft
    // detection alone must enable the overwrite.
    let spec = GithubReleaseSpec {
        tag: "v1.2.3",
        name: "v1.2.3",
        body: "release body",
        mode: "keep-existing",
        draft: true,
        prerelease: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    };

    run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &spec,
        &base_opts(),
        &artifacts,
    )
    .expect("draft auto-resume must NOT bail and must succeed")
    .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        entries
            .iter()
            .any(|e| e.method == "DELETE" && e.path == "/repos/o/r/releases/assets/9"),
        "a draft found by tag must overwrite its leftover asset (DELETE + reupload), \
             proving auto-resume despite replace=false/resume=false; calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// 8. Missing token surfaces a clear error without any HTTP traffic.
// ---------------------------------------------------------------------
#[test]
fn missing_token_errs_before_any_http_call() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    // Spawn the responder with no routes; ANY HTTP call lands in the
    // request log and fails the test.
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| Vec::new());

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token: Option<String> = None;
    let artifacts: Vec<(PathBuf, Option<String>)> = Vec::new();
    let anc = spec_ancillary_default();

    let err = run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &base_opts(),
        &artifacts,
    )
    .expect_err("missing token must Err");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("GITHUB_TOKEN") || msg.contains("token"),
        "error must mention the missing token: {msg}",
    );
    let entries = log.lock().expect("log mutex");
    assert!(
        entries.is_empty(),
        "token check must short-circuit BEFORE any HTTP call; calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// 9. Missing artifact file surfaces a clear error after release create.
// ---------------------------------------------------------------------
#[test]
fn missing_artifact_file_errs_with_path_in_message() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let routes = vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repos/o/r/releases",
        response: http_201(release_json(addr, 42, true, "v1.2.3")),
        times: Some(1),
    }];
    let (_addr2, _log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    // Point at a path that does not exist.
    let missing = PathBuf::from("/nonexistent/anodizer-test/does-not-exist.tar.gz");
    let artifacts = vec![(missing.clone(), Some("does-not-exist.tar.gz".to_string()))];
    let anc = spec_ancillary_default();

    let err = run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &base_opts(),
        &artifacts,
    )
    .expect_err("missing-on-disk artifact must Err");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing") && msg.contains("does-not-exist.tar.gz"),
        "missing-file error must name the offending path: {msg}",
    );
}

// ---------------------------------------------------------------------
// 10. skip_upload = true creates the release but skips every upload POST.
// ---------------------------------------------------------------------
#[test]
fn skip_upload_creates_release_but_skips_uploads() {
    let tmp = TempDir::new().expect("tempdir");
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"unused");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let routes = vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repos/o/r/releases",
        response: http_201(release_json(addr, 42, true, "v1.2.3")),
        times: Some(1),
    }];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];

    let mut opts = base_opts();
    opts.skip_upload = true;
    let anc = spec_ancillary_default();
    run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &opts,
        &artifacts,
    )
    .expect("backend succeeds")
    .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        !entries.iter().any(|e| e.path.starts_with("/upload/")),
        "skip_upload=true must NOT POST any asset; calls: {entries:?}",
    );
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/repos/o/r/releases"),
        "skip_upload=true must still create the release; calls: {entries:?}",
    );
}

/// `404 Not Found` carrying a GitHub-shaped JSON body, so octocrab maps
/// it to `Error::GitHub { status_code: 404 }` (the read-after-write lag
/// shape) rather than a transport error.
fn http_404() -> &'static str {
    let body = r#"{"message":"Not Found","documentation_url":"https://docs.github.com/rest"}"#;
    let len = body.len();
    Box::leak(
            format!("HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}")
                .into_boxed_str(),
        )
}

/// Force `retry.attempts: 1` to reproduce the stateful-mode policy
/// (`--publish-only`), under which a single transient failure is
/// otherwise unrecoverable. The readiness guard and the per-upload
/// bounded-404 retry must both work despite this cap.
fn build_ctx_attempts_one(addr: SocketAddr) -> Context {
    let mut ctx = build_ctx(addr);
    ctx.config.retry = Some(anodizer_core::config::RetryConfig {
        attempts: 1,
        delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(1)),
        max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(2)),
        max_elapsed: None,
    });
    ctx
}

// ---------------------------------------------------------------------
// Post-create read-after-write lag: the readiness guard must absorb a
// transient 404 on `GET /releases/{id}` before uploads start, even when
// the resolved policy caps attempts at 1 (stateful `--publish-only`).
// Without the guard the first `upload_asset` GET 404s and the run dies.
// ---------------------------------------------------------------------
#[test]
fn readiness_guard_absorbs_transient_404_before_upload() {
    let tmp = TempDir::new().expect("tempdir");
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"hello world");
    let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let release = release_json(addr, 42, true, "v1.2.3");

    let routes = vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(release.clone()),
            times: Some(1),
        },
        // The readiness guard's first probe hits the replica before it
        // has observed the create: a transient 404 (served once).
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_404(),
            times: Some(1),
        },
        // Every subsequent GET (the guard's retry, then upload_asset's
        // own upload_url read) sees the release.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(release),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx_attempts_one(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
    let anc = spec_ancillary_default();

    run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &base_opts(),
        &artifacts,
    )
    .expect("readiness guard must absorb the transient 404 and let the upload succeed")
    .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/upload/42?name=demo.tar.gz"),
        "the asset upload must reach the POST after the readiness guard recovers; calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// Backstop: even past the readiness guard, a parallel replica can lag
// independently and 404 the `GET` inside `upload_asset(...).send()`. With
// the stateful policy (attempts=1) that single 404 used to be fatal; the
// per-upload bounded-404 floor must retry it instead.
// ---------------------------------------------------------------------
#[test]
fn per_upload_404_retries_under_stateful_attempts_one() {
    let tmp = TempDir::new().expect("tempdir");
    let artifact_path = write_artifact(tmp.path(), "demo.tar.gz", b"hello world");
    let artifact_len = std::fs::metadata(&artifact_path).expect("meta").len();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let release = release_json(addr, 42, true, "v1.2.3");

    let routes = vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(release.clone()),
            times: Some(1),
        },
        // (1) Readiness guard GET — readable on the first probe.
        // (2) upload_asset's upload_url GET on the FIRST attempt — 404
        //     (independent replica still lagging). attempts=1 would make
        //     this fatal without the per-upload bounded-404 floor.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(release.clone()),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_404(),
            times: Some(1),
        },
        // upload_asset's GET on the retry attempt, and any further reads.
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(release),
            times: None,
        },
        ScriptedRoute {
            method: "POST",
            path_pattern: "/upload/42?name=demo.tar.gz",
            response: http_201(asset_json(7, "demo.tar.gz", artifact_len)),
            times: Some(1),
        },
    ];
    let (_addr2, log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx_attempts_one(addr);
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![(artifact_path, Some("demo.tar.gz".to_string()))];
    let anc = spec_ancillary_default();

    run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &base_opts(),
        &artifacts,
    )
    .expect("per-upload bounded-404 retry must recover under attempts=1")
    .expect("returns Some");

    let entries = log.lock().expect("log mutex");
    assert!(
        entries
            .iter()
            .any(|e| e.method == "POST" && e.path == "/upload/42?name=demo.tar.gz"),
        "the asset upload must reach the POST after the per-upload 404 retry; calls: {entries:?}",
    );
}

// ---------------------------------------------------------------------
// Proactive upload pace — the minimum interval between upload STARTS.
// ---------------------------------------------------------------------

/// Build a [`Context`] like [`build_ctx`] but also seed the
/// `ANODIZER_GITHUB_UPLOAD_PACE_MS` override so the pace timing tests can
/// drive the inter-upload-start interval without touching config.
fn build_ctx_with_pace_ms(addr: SocketAddr, pace_ms: &str) -> Context {
    let base = format!("http://{addr}");
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .tag("v1.2.3")
        .token(Some("test-token".to_string()))
        .env("ANODIZER_GITHUB_API_BASE", &base)
        .env("ANODIZER_GITHUB_UPLOAD_PACE_MS", pace_ms)
        .build();
    ctx.config.github_urls = Some(GitHubUrlsConfig {
        api: Some(base.clone()),
        upload: Some(base.clone()),
        download: Some(base),
        skip_tls_verify: None,
    });
    ctx.config.retry = Some(anodizer_core::config::RetryConfig {
        attempts: 5,
        delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(1)),
        max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(2)),
        max_elapsed: None,
    });
    ctx
}

/// Route set for an N-asset happy-path upload against release id 42:
/// create POST, a reusable GET on the release (readiness + per-upload
/// `upload_url` read), and one upload POST per asset name.
fn multi_asset_routes(release: String, names: &[(&'static str, u64)]) -> Vec<ScriptedRoute> {
    let mut routes = vec![
        ScriptedRoute {
            method: "POST",
            path_pattern: "/repos/o/r/releases",
            response: http_201(release.clone()),
            times: Some(1),
        },
        ScriptedRoute {
            method: "GET",
            path_pattern: "/repos/o/r/releases/42",
            response: http_ok(release),
            times: None,
        },
    ];
    for (name, id) in names {
        routes.push(ScriptedRoute {
            method: "POST",
            path_pattern: Box::leak(format!("/upload/42?name={name}").into_boxed_str()),
            response: http_201(asset_json(*id, name, 5)),
            times: Some(1),
        });
    }
    routes
}

/// With a non-zero pace, successive upload STARTS are spaced by at least
/// the (jittered) pace interval. Three assets => two inter-start gaps, so
/// total wall-clock must be at least `2 * pace * 0.8` (the jitter floor).
/// A 120 ms pace yields a >= ~192 ms floor — comfortably above scheduler
/// noise yet fast enough to keep the test cheap.
#[test]
fn upload_pace_spaces_successive_upload_starts() {
    use std::time::Instant;

    let tmp = TempDir::new().expect("tempdir");
    let a = write_artifact(tmp.path(), "a.tar.gz", b"aaaaa");
    let b = write_artifact(tmp.path(), "b.tar.gz", b"bbbbb");
    let c = write_artifact(tmp.path(), "c.tar.gz", b"ccccc");

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let release = release_json(addr, 42, true, "v1.2.3");
    let routes = multi_asset_routes(
        release,
        &[("a.tar.gz", 1), ("b.tar.gz", 2), ("c.tar.gz", 3)],
    );
    let (_addr2, _log) = spawn_scripted_responder_on(listener, |_| routes);

    let ctx = build_ctx_with_pace_ms(addr, "120");
    let crate_cfg = build_crate_cfg();
    let rt = tokio::runtime::Runtime::new().expect("rt");
    let token = Some("test-token".to_string());
    let artifacts = vec![
        (a, Some("a.tar.gz".to_string())),
        (b, Some("b.tar.gz".to_string())),
        (c, Some("c.tar.gz".to_string())),
    ];
    let anc = spec_ancillary_default();

    let t0 = Instant::now();
    run_backend(
        &rt,
        &ctx,
        &token,
        &crate_cfg,
        &make_spec(&anc),
        &base_opts(),
        &artifacts,
    )
    .expect("paced upload succeeds")
    .expect("returns Some");
    let elapsed = t0.elapsed();

    // 2 gaps * 120 ms * 0.8 jitter floor = 192 ms.
    assert!(
        elapsed >= std::time::Duration::from_millis(192),
        "upload pace must space the 3 starts by >= 2 * 120ms * 0.8; elapsed: {elapsed:?}"
    );
}

// The pace=0 no-op invariant is proven deterministically in
// `forge::tests::upload_pace_delay_tests` (pure `upload_pace_delay`), not
// by comparing two wall-clock runs here — that comparison was load-flaky
// under concurrent test hosts and false-reds the release gate.
