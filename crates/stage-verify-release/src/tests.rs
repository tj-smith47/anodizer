//! Stage-orchestration tests for [`VerifyReleaseStage`].
//!
//! The check LOGIC (asset-diff, glibc compare/extract, smoke argv) is unit-
//! tested in the respective modules without network or Docker. These tests
//! cover the stage wiring: enabled/skip/dry-run gating, the produced-set
//! derivation, the binary-name fallback, and the multi-crate (workspace)
//! fan-out — all offline (no real release exists, so the gating paths return
//! before any network call).

use super::*;
use anodizer_core::artifact::Artifact;
use anodizer_core::config::{
    CrateConfig, GitHubUrlsConfig, InstallSmokeConfig, VerifyReleaseConfig,
};
use anodizer_core::test_helpers::TestContextBuilder;
use anodizer_core::test_helpers::scripted_responder::{
    RequestLog, ScriptedRoute, spawn_scripted_responder,
};
use std::collections::HashMap;
use std::net::SocketAddr;

/// Deserialize a minimal crate with a GitHub release block so it counts as
/// "published" for the gate's crate iteration.
fn published_crate(name: &str, binary: Option<&str>) -> CrateConfig {
    let builds = match binary {
        Some(b) => format!("builds:\n  - binary: {b}\n"),
        None => String::new(),
    };
    let yaml = format!(
        "name: {name}\npath: .\ntag_template: \"v{{{{ .Version }}}}\"\n\
         release:\n  github: {{ owner: me, name: repo }}\n{builds}"
    );
    serde_yaml_ng::from_str(&yaml).expect("valid crate yaml")
}

fn add_artifact(ctx: &mut Context, kind: ArtifactKind, name: &str, crate_name: &str) {
    ctx.artifacts.add(Artifact {
        kind,
        name: name.to_string(),
        path: std::path::PathBuf::from(name),
        target: None,
        crate_name: crate_name.to_string(),
        metadata: HashMap::new(),
        size: None,
    });
}

/// Register a COMBINED `checksums.txt` artifact — the only Checksum kind that
/// `signs: artifacts: checksum` signs (split sidecars are never signed).
fn add_combined_checksum(ctx: &mut Context, name: &str, crate_name: &str) {
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: name.to_string(),
        path: std::path::PathBuf::from(name),
        target: None,
        crate_name: crate_name.to_string(),
        metadata: HashMap::from([(
            anodizer_core::artifact::COMBINED_CHECKSUM_META.to_string(),
            anodizer_core::artifact::COMBINED_CHECKSUM_VALUE.to_string(),
        )]),
        size: None,
    });
}

use anodizer_core::test_helpers::has_recursive_sidecar_chain;

#[test]
fn verify_release_never_demands_signature_of_a_signature_or_certificate() {
    use anodizer_core::config::SignConfig;

    // Mirror anodizer's own posture: `signs: artifacts: checksum`. Register an
    // archive, its COMBINED checksums file, a per-artifact split sidecar, plus a
    // Signature and Certificate (the dist state on a publish-only resume).
    //
    // GoReleaser parity: `artifacts: checksum` signs EVERY Checksum, so the
    // derivation legitimately demands BOTH the combined `checksums.txt.sig` AND
    // the split `app.tar.gz.sha256.sig` (the GR-legit second level). It must
    // NEVER demand a signature OF a signature or OF a certificate, and no
    // demanded name may form a forbidden recursive chain (a checksum of a sig,
    // a sig of a sig, etc.).
    let sign_cfg = SignConfig {
        id: Some("default".to_string()),
        artifacts: Some("checksum".to_string()),
        cmd: Some("true".to_string()),
        args: Some(vec![]),
        ..Default::default()
    };
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("app", None)])
        .signs(vec![sign_cfg])
        .build();

    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");
    add_combined_checksum(&mut ctx, "app_checksums.txt", "app");
    add_artifact(&mut ctx, ArtifactKind::Checksum, "app.tar.gz.sha256", "app");
    // A signature/certificate that must NOT be re-signed by the derivation.
    add_artifact(&mut ctx, ArtifactKind::Signature, "app.tar.gz.sig", "app");
    add_artifact(&mut ctx, ArtifactKind::Certificate, "app.tar.gz.pem", "app");

    let derived = config_expected_asset_names(&ctx, "app", None, None).expect("derivation");

    // Both the combined file and the split sidecar are signed (GR parity).
    assert!(
        derived.contains(&"app_checksums.txt.sig".to_string()),
        "must demand the combined checksums signature; got {derived:?}"
    );
    assert!(
        derived.contains(&"app.tar.gz.sha256.sig".to_string()),
        "must demand the split sidecar signature (GR parity); got {derived:?}"
    );
    // No demanded name may form a forbidden recursive chain.
    for name in &derived {
        assert!(
            !has_recursive_sidecar_chain(name),
            "verify-release demanded a forbidden recursive sidecar asset: {name}"
        );
    }
    // Specifically: never a signature OF a signature or OF a certificate.
    assert!(!derived.contains(&"app.tar.gz.sig.sig".to_string()));
    assert!(!derived.contains(&"app.tar.gz.pem.sig".to_string()));
}

#[test]
fn disabled_is_noop() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("myapp", None)])
        .build();
    // verify_release defaults to disabled.
    assert!(!ctx.config.verify_release.enabled);
    add_artifact(&mut ctx, ArtifactKind::Archive, "myapp.tar.gz", "myapp");
    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "disabled gate must be a no-op (no network)"
    );
}

#[test]
fn enabled_but_dry_run_is_noop() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .dry_run(true)
        .crates(vec![published_crate("myapp", None)])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        assert_assets: true,
        ..Default::default()
    };
    add_artifact(&mut ctx, ArtifactKind::Archive, "myapp.tar.gz", "myapp");
    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "dry-run has no published release to verify; must no-op without fetching"
    );
}

#[test]
fn enabled_but_snapshot_is_noop() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .snapshot(true)
        .crates(vec![published_crate("myapp", None)])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(VerifyReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn skip_flag_is_noop() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("myapp", None)])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        ..Default::default()
    };
    ctx.options.skip_stages = vec!["verify-release".to_string()];
    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "--skip=verify-release must short-circuit before any fetch"
    );
}

#[test]
fn no_published_crates_is_noop() {
    // A crate with no release block is not "published"; the gate finds
    // nothing to verify and returns Ok without touching the network.
    let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(VerifyReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn produced_asset_names_derives_from_registry_per_crate() {
    // Rule #11 evidence: the produced set comes from release_uploadable_kinds()
    // + by_kind_and_crate, with NO config. Per-crate isolation (workspace mode):
    // crate A's archive must not leak into crate B's produced set.
    let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
    add_artifact(&mut ctx, ArtifactKind::Archive, "a.tar.gz", "crate-a");
    add_artifact(&mut ctx, ArtifactKind::Checksum, "checksums.txt", "crate-a");
    add_artifact(&mut ctx, ArtifactKind::LinuxPackage, "a.deb", "crate-a");
    add_artifact(&mut ctx, ArtifactKind::Archive, "b.tar.gz", "crate-b");
    // A raw Binary is NOT in release_uploadable_kinds(); must be excluded.
    add_artifact(&mut ctx, ArtifactKind::Binary, "raw-bin", "crate-a");

    let a = produced_asset_names(&ctx, "crate-a", None, None);
    assert_eq!(a, vec!["a.deb", "a.tar.gz", "checksums.txt"]);
    let b = produced_asset_names(&ctx, "crate-b", None, None);
    assert_eq!(b, vec!["b.tar.gz"], "crate-b set is isolated from crate-a");
}

/// Add an artifact carrying an `id` in metadata so `release.ids` filtering can
/// select / exclude it (mirrors how upstream stages tag artifacts with `id`).
fn add_artifact_with_id(
    ctx: &mut Context,
    kind: ArtifactKind,
    name: &str,
    crate_name: &str,
    id: &str,
) {
    let mut metadata = HashMap::new();
    metadata.insert("id".to_string(), id.to_string());
    ctx.artifacts.add(Artifact {
        kind,
        name: name.to_string(),
        path: std::path::PathBuf::from(name),
        target: None,
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    });
}

#[test]
fn produced_asset_names_honors_release_ids_filter() {
    // The upload path applies `release.ids`; the asset-existence check must use
    // the SAME filter so an artifact intentionally filtered OUT of the upload
    // set is NOT reported as a missing asset (false post-release FAIL).
    let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
    add_artifact_with_id(
        &mut ctx,
        ArtifactKind::Archive,
        "linux.tar.gz",
        "app",
        "linux",
    );
    add_artifact_with_id(
        &mut ctx,
        ArtifactKind::Archive,
        "windows.zip",
        "app",
        "windows",
    );

    // No filter: both candidates are expected assets.
    let all = produced_asset_names(&ctx, "app", None, None);
    assert_eq!(all, vec!["linux.tar.gz", "windows.zip"]);

    // ids = [linux]: the windows artifact is filtered out of the upload set and
    // therefore must NOT appear in the expected (produced) asset names.
    let ids = vec!["linux".to_string()];
    let filtered = produced_asset_names(&ctx, "app", Some(&ids), None);
    assert_eq!(
        filtered,
        vec!["linux.tar.gz"],
        "ids-filtered-out artifact must not be reported as a produced asset"
    );
}

#[test]
fn crate_binary_name_prefers_build_binary_then_falls_back() {
    let with_bin = published_crate("mycrate", Some("mybin"));
    assert_eq!(crate_binary_name(&with_bin), "mybin");
    let without = published_crate("mycrate", None);
    assert_eq!(
        crate_binary_name(&without),
        "mycrate",
        "falls back to crate name when no build binary is set"
    );
}

#[test]
fn smoke_disabled_when_no_install_smoke_block() {
    // With install_smoke=None, docker_available() must never be consulted and
    // the stage must not hard-fail on a docker-less host. We force enabled but
    // dry-run so the whole run is a no-op regardless — the real assertion is
    // that the default config leaves smoke off.
    let cfg = VerifyReleaseConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(cfg.install_smoke.is_none(), "smoke off unless configured");
}

#[test]
fn libc_check_off_without_ceiling() {
    let cfg = VerifyReleaseConfig {
        enabled: true,
        glibc_ceiling: None,
        ..Default::default()
    };
    assert!(
        !cfg.glibc_check_enabled(),
        "no ceiling => libc check does not run"
    );
}

#[test]
fn install_smoke_resolves_per_type_images() {
    let smoke = InstallSmokeConfig::default();
    // All defaults when nothing configured.
    assert_eq!(smoke.deb_image(), "debian:stable-slim");
    assert_eq!(smoke.rpm_image(), "fedora:latest");
    assert_eq!(smoke.apk_image(), "alpine:latest");
}

#[test]
fn multi_crate_iteration_covers_all_published_crates() {
    // Workspace per-crate mode: two published crates, dry-run so no network.
    // The stage must consider BOTH (not silo to one) — verified indirectly by
    // produced_asset_names isolation plus the dry-run no-op completing for a
    // multi-crate config.
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .dry_run(true)
        .crates(vec![
            published_crate("crate-a", Some("bin-a")),
            published_crate("crate-b", Some("bin-b")),
        ])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        ..Default::default()
    };
    add_artifact(&mut ctx, ArtifactKind::Archive, "a.tar.gz", "crate-a");
    add_artifact(&mut ctx, ArtifactKind::Archive, "b.tar.gz", "crate-b");
    assert!(VerifyReleaseStage.run(&mut ctx).is_ok());
}

// ===========================================================================
// Asset-existence — the network half of the gate, driven against an
// in-process scripted GitHub responder. The published_crate fixture targets
// release.github { owner: me, name: repo }; with the default GitHub token type
// `find_release_by_tag` issues GET /repos/me/repo/releases/tags/<tag>. We point
// `github_urls.api` at the loopback so octocrab routes every call there.
// ===========================================================================

/// A `200 OK` JSON HTTP response with a correct `Content-Length`. Leaked to
/// satisfy the responder's `&'static str` contract (test-only).
fn http_ok(body: String) -> &'static str {
    let len = body.len();
    Box::leak(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}"
        )
        .into_boxed_str(),
    )
}

/// `404 Not Found` — what `GET /releases/tags/<tag>` returns when no release
/// exists for the tag. `find_release_by_tag` maps this to `Ok(None)`, which
/// `fetch_published_asset_names` turns into a "no release found" bail.
const HTTP_404: &str = "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 28\r\n\r\n{\"message\":\"Not Found\"}\r\n\r\n";

/// Build a minimal Release JSON octocrab deserializes into
/// `models::repos::Release`, with `asset_names` as the uploaded asset list —
/// the published set the asset-existence check diffs against.
fn release_json_with_assets(addr: SocketAddr, asset_names: &[&str]) -> String {
    let assets: Vec<_> = asset_names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            serde_json::json!({
                "url": format!("http://{addr}/asset/{i}"),
                "browser_download_url": format!("http://{addr}/dl/{name}"),
                "id": i as u64 + 1,
                "node_id": format!("RA_{i}"),
                "name": name,
                "label": null,
                "state": "uploaded",
                "content_type": "application/octet-stream",
                "size": 1u64,
                "download_count": 0,
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "uploader": null,
            })
        })
        .collect();
    serde_json::json!({
        "id": 1,
        "node_id": "RL_1",
        "tag_name": "v1.0.0",
        "target_commitish": "main",
        "name": "v1.0.0",
        "draft": false,
        "prerelease": false,
        "created_at": "2026-01-01T00:00:00Z",
        "published_at": "2026-01-01T00:00:00Z",
        "author": null,
        "assets": assets,
        "tarball_url": null,
        "zipball_url": null,
        "body": null,
        "url": format!("http://{addr}/repos/me/repo/releases/1"),
        "html_url": format!("http://{addr}/me/repo/releases/1"),
        "assets_url": format!("http://{addr}/repos/me/repo/releases/1/assets"),
        "upload_url": format!("http://{addr}/upload/1{{?name,label}}"),
    })
    .to_string()
}

/// Build a non-dry-run context whose octocrab client routes through `addr`,
/// carrying a token and an enabled asset-existence-only verify config.
fn asset_ctx(addr: SocketAddr, crates: Vec<CrateConfig>) -> Context {
    let base = format!("http://{addr}");
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .token(Some("test-token".to_string()))
        .env("ANODIZER_GITHUB_API_BASE", &base)
        .crates(crates)
        .build();
    ctx.config.github_urls = Some(GitHubUrlsConfig {
        api: Some(base.clone()),
        upload: Some(base.clone()),
        download: Some(base),
        skip_tls_verify: None,
    });
    ctx.config.retry = Some(anodizer_core::config::RetryConfig {
        attempts: 2,
        delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(1)),
        max_delay: anodizer_core::config::HumanDuration(std::time::Duration::from_millis(2)),
    });
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        assert_assets: true,
        glibc_ceiling: None,
        install_smoke: None,
    };
    ctx
}

/// Spawn a scripted responder answering `GET /repos/me/repo/releases/tags/
/// v1.0.0` with a 200 release JSON whose uploaded assets are `asset_names`.
/// Binds first so the bound addr can be baked into the asset URLs.
fn spawn_release_route(
    asset_names: &[&str],
) -> (
    SocketAddr,
    std::sync::Arc<std::sync::Mutex<Vec<RequestLog>>>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let body = release_json_with_assets(addr, asset_names);
    let routes = vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/repos/me/repo/releases/tags/v1.0.0",
        response: http_ok(body),
        times: None,
    }];
    anodizer_core::test_helpers::scripted_responder::spawn_scripted_responder_on(
        listener,
        move |_| routes,
    )
}

#[test]
fn asset_existence_passes_when_every_produced_asset_is_published() {
    // Produced {app.tar.gz, checksums.txt} all present on the release => no
    // issue, gate returns Ok despite running the live fetch + diff.
    let (addr, _log) = spawn_release_route(&["app.tar.gz", "checksums.txt"]);

    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");
    add_artifact(&mut ctx, ArtifactKind::Checksum, "checksums.txt", "app");

    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "all produced assets present on the release => gate passes"
    );
}

#[test]
fn asset_existence_bails_when_a_produced_asset_is_missing() {
    // Produced {app.tar.gz, checksums.txt} but the release only stores
    // app.tar.gz => checksums.txt is reported missing and the gate bails with
    // the published-note prefix and the missing name.
    let (addr, _log) = spawn_release_route(&["app.tar.gz"]);

    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");
    add_artifact(&mut ctx, ArtifactKind::Checksum, "checksums.txt", "app");

    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("a missing produced asset must fail the gate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("checksums.txt"),
        "error names the missing asset: {msg}"
    );
    assert!(
        msg.contains(PUBLISHED_NOTE),
        "error carries the already-published note: {msg}"
    );
}

#[test]
fn verify_release_records_summary_slot_on_pass() {
    // Clean pass: the gate stamps Some(issues:[]) — the Some encodes "the gate
    // ran" so the run-summary can render a passing verify-release row.
    let (addr, _log) = spawn_release_route(&["app.tar.gz", "checksums.txt"]);

    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");
    add_artifact(&mut ctx, ArtifactKind::Checksum, "checksums.txt", "app");

    assert!(VerifyReleaseStage.run(&mut ctx).is_ok());
    assert!(
        ctx.verify_release.is_some(),
        "a clean pass must still stamp the verify-release slot (Some encodes 'ran')"
    );
    let summary = ctx.verify_release.as_ref().unwrap();
    assert!(
        summary.issues.is_empty(),
        "a clean pass carries no issues: {:?}",
        summary.issues
    );
}

#[test]
fn verify_release_records_summary_slot_before_bail() {
    // Failure path: the slot is set with the issue(s) BEFORE the bail, so the
    // pipeline-end summary (emit_summary fires after the stage returns Err)
    // can render the FAILED verify-release row instead of a false all-green.
    let (addr, _log) = spawn_release_route(&["app.tar.gz"]);

    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");
    add_artifact(&mut ctx, ArtifactKind::Checksum, "checksums.txt", "app");

    VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("a missing produced asset must fail the gate");
    assert!(
        ctx.verify_release.is_some(),
        "the failure path must stamp the verify-release slot (Some) before bailing"
    );
    let summary = ctx.verify_release.as_ref().unwrap();
    assert_eq!(
        summary.issues.len(),
        1,
        "exactly one issue: {:?}",
        summary.issues
    );
    assert!(
        summary.issues[0].contains("checksums.txt"),
        "the recorded issue names the missing asset: {:?}",
        summary.issues
    );
}

#[test]
fn verify_release_slot_stays_none_when_disabled() {
    // Early-return paths (disabled / skip / dry-run / snapshot) must NOT stamp
    // the slot — no published release was verified, so no row should appear.
    let mut ctx = TestContextBuilder::new()
        .crates(vec![published_crate("app", None)])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: false,
        ..Default::default()
    };
    assert!(VerifyReleaseStage.run(&mut ctx).is_ok());
    assert!(
        ctx.verify_release.is_none(),
        "a disabled gate must leave the slot None"
    );
}

#[test]
fn asset_existence_orphan_published_asset_is_advisory_not_failure() {
    // The release stores an EXTRA asset (stale.txt) not produced this run. An
    // orphan is advisory only — the gate still passes when nothing produced is
    // missing.
    let (addr, _log) = spawn_release_route(&["app.tar.gz", "stale.txt"]);

    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");

    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "an orphan published asset must not fail the gate"
    );
}

#[test]
fn asset_existence_bails_when_release_not_found_for_tag() {
    // GET /releases/tags/<tag> returns 404 => find_release_by_tag yields None
    // => fetch_published_asset_names bails ("no release found"); the stage logs
    // that as a fetch issue and the gate fails. The publish should have created
    // the release, so its absence is a genuine post-publish defect.
    let routes = vec![ScriptedRoute {
        method: "GET",
        path_pattern: "/repos/me/repo/releases/tags/v1.0.0",
        response: HTTP_404,
        times: None,
    }];
    let (addr, _log) = spawn_scripted_responder(routes);

    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");

    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("a missing release for the tag must fail the gate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("could not fetch published release assets")
            || msg.contains("no GitHub release found"),
        "error surfaces the failed fetch: {msg}"
    );
}

#[test]
fn asset_existence_skipped_when_crate_has_no_github_repo() {
    // A crate with a release block but no `github` resolves to Ok(None) under
    // the default GitHub token type => the asset check is skipped with a notice
    // and NO network call is made (the responder has no routes; a hit would
    // 404 and is never made). The gate passes.
    let (addr, log) = spawn_scripted_responder(vec![]);

    // release block present but empty (no github sub-config).
    let yaml = "name: app\npath: .\ntag_template: \"v{{ .Version }}\"\nrelease: {}\n";
    let crate_cfg: CrateConfig = serde_yaml_ng::from_str(yaml).expect("valid crate yaml");

    let mut ctx = asset_ctx(addr, vec![crate_cfg]);
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");

    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "no github repo => asset check skipped, gate passes"
    );
    assert!(
        log.lock().expect("log mutex").is_empty(),
        "no GitHub repo configured => no live fetch is attempted"
    );
}

#[test]
fn asset_existence_bails_when_no_token_available() {
    // With assert_assets enabled but no token, fetch_published_asset_names
    // errors ("no GitHub token available"); the stage records that as a fetch
    // issue and the gate fails rather than silently skipping.
    let (addr, _log) = spawn_scripted_responder(vec![]);
    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    ctx.options.token = None;
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");

    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("no token must fail the asset fetch");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("could not fetch published release assets"),
        "error surfaces the fetch failure: {msg}"
    );
    assert!(
        msg.contains(PUBLISHED_NOTE),
        "carries the published note: {msg}"
    );
}

#[test]
fn asset_check_disabled_makes_no_network_call() {
    // assert_assets=false with the gate enabled and NOT dry-run: the asset
    // path must be skipped entirely (no fetch). The responder logs zero hits.
    let (addr, log) = spawn_scripted_responder(vec![]);
    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    ctx.config.verify_release.assert_assets = false;
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");

    assert!(VerifyReleaseStage.run(&mut ctx).is_ok());
    assert!(
        log.lock().expect("log mutex").is_empty(),
        "assert_assets=false => no live fetch"
    );
}

#[test]
fn multi_crate_asset_check_bails_naming_the_offending_crate() {
    // Workspace per-crate: crate-a fully present, crate-b missing one asset.
    // Both crates target their own owner/repo (me/repo here via the shared
    // fixture, distinguished by tag is unnecessary — both use the same route).
    // The gate must iterate BOTH and the failure must name crate-b. Both crates
    // resolve to me/repo + tag v1.0.0, so a single route (times: None) serves
    // both fetches; it stores a.tar.gz only — present for crate-a, missing b's.
    let (addr, _log) = spawn_release_route(&["a.tar.gz"]);

    let mut ctx = asset_ctx(
        addr,
        vec![
            published_crate("crate-a", None),
            published_crate("crate-b", None),
        ],
    );
    add_artifact(&mut ctx, ArtifactKind::Archive, "a.tar.gz", "crate-a");
    add_artifact(&mut ctx, ArtifactKind::Archive, "b.tar.gz", "crate-b");

    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("crate-b's missing asset must fail the gate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("crate 'crate-b'") && msg.contains("b.tar.gz"),
        "failure names crate-b and its missing asset: {msg}"
    );
}

// ===========================================================================
// libc-ceiling — the local-file half (check_one_deb_libc / extract_deb_main_elf
// / linux_packages). Synthetic .deb files are built on disk in a tempdir and
// the stage drives the real ELF extraction + glibc compare. assert_assets is
// turned OFF so these tests exercise only the libc path with no network.
// ===========================================================================

/// Build a tar archive in memory from `(path, bytes)` members.
fn make_tar(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in members {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, *data).unwrap();
    }
    builder.into_inner().unwrap()
}

/// Gzip-compress bytes.
fn gz(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// Build a minimal `.deb` ar archive carrying a single `data.tar.gz` member.
fn make_deb(data_tar_gz: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"!<arch>\n");
    let name = "data.tar.gz";
    let mut header = vec![b' '; 60];
    header[0..name.len()].copy_from_slice(name.as_bytes());
    let size_str = data_tar_gz.len().to_string();
    header[48..48 + size_str.len()].copy_from_slice(size_str.as_bytes());
    header[58] = b'\x60';
    header[59] = b'\n';
    out.extend_from_slice(&header);
    out.extend_from_slice(data_tar_gz);
    if data_tar_gz.len() % 2 == 1 {
        out.push(b'\n');
    }
    out
}

/// A structurally-valid 32-bit LE ELF declaring a `GLIBC_2.99` requirement via
/// `.gnu.version_r` (the `object` verneed walk extracts 2.99). Mirrors the
/// fixture proven in `libc_check.rs::elf32_le_with_glibc_2_99`.
fn elf32_le_with_glibc_2_99() -> Vec<u8> {
    const SHT_STRTAB: u32 = 3;
    const SHT_DYNSYM: u32 = 11;
    const SHT_GNU_VERSYM: u32 = 0x6fff_ffff;
    const SHT_GNU_VERNEED: u32 = 0x6fff_fffe;
    const VER_IDX: u16 = 2;
    let le32 = |buf: &mut Vec<u8>, v: u32| buf.extend_from_slice(&v.to_le_bytes());

    let mut dynstr = vec![0u8];
    let off_libc = dynstr.len() as u32;
    dynstr.extend_from_slice(b"libc.so.6\0");
    let off_glibc = dynstr.len() as u32;
    dynstr.extend_from_slice(b"GLIBC_2.99\0");
    let off_sym = dynstr.len() as u32;
    dynstr.extend_from_slice(b"glibc99\0");

    let mut dynsym = Vec::new();
    dynsym.extend_from_slice(&[0u8; 16]);
    le32(&mut dynsym, off_sym);
    le32(&mut dynsym, 0);
    le32(&mut dynsym, 0);
    dynsym.push((1 << 4) | 2);
    dynsym.push(0);
    dynsym.extend_from_slice(&1u16.to_le_bytes());

    let mut versym = Vec::new();
    versym.extend_from_slice(&0u16.to_le_bytes());
    versym.extend_from_slice(&VER_IDX.to_le_bytes());

    let mut verneed = Vec::new();
    verneed.extend_from_slice(&1u16.to_le_bytes());
    verneed.extend_from_slice(&1u16.to_le_bytes());
    le32(&mut verneed, off_libc);
    le32(&mut verneed, 16);
    le32(&mut verneed, 0);
    le32(&mut verneed, 0);
    verneed.extend_from_slice(&0u16.to_le_bytes());
    verneed.extend_from_slice(&VER_IDX.to_le_bytes());
    le32(&mut verneed, off_glibc);
    le32(&mut verneed, 0);

    let shstrtab = vec![0u8];

    let mut img = vec![0u8; 52];
    let place = |img: &mut Vec<u8>, body: &[u8]| -> (u32, u32) {
        let off = img.len() as u32;
        img.extend_from_slice(body);
        (off, body.len() as u32)
    };
    let (dynstr_off, dynstr_sz) = place(&mut img, &dynstr);
    let (dynsym_off, dynsym_sz) = place(&mut img, &dynsym);
    let (versym_off, versym_sz) = place(&mut img, &versym);
    let (verneed_off, verneed_sz) = place(&mut img, &verneed);
    let (shstr_off, shstr_sz) = place(&mut img, &shstrtab);

    let shoff = img.len() as u32;
    let sh = |img: &mut Vec<u8>,
              sh_type: u32,
              offset: u32,
              size: u32,
              link: u32,
              info: u32,
              entsize: u32| {
        le32(img, 0);
        le32(img, sh_type);
        le32(img, 0);
        le32(img, 0);
        le32(img, offset);
        le32(img, size);
        le32(img, link);
        le32(img, info);
        le32(img, 0);
        le32(img, entsize);
    };
    sh(&mut img, 0, 0, 0, 0, 0, 0);
    sh(&mut img, SHT_STRTAB, dynstr_off, dynstr_sz, 0, 0, 0);
    sh(&mut img, SHT_DYNSYM, dynsym_off, dynsym_sz, 1, 1, 16);
    sh(&mut img, SHT_GNU_VERSYM, versym_off, versym_sz, 2, 0, 2);
    sh(&mut img, SHT_GNU_VERNEED, verneed_off, verneed_sz, 1, 1, 0);
    sh(&mut img, SHT_STRTAB, shstr_off, shstr_sz, 0, 0, 0);
    let shnum: u16 = 6;
    let shstrndx: u16 = 5;

    img[0..4].copy_from_slice(b"\x7fELF");
    img[4] = 1;
    img[5] = 1;
    img[6] = 1;
    img[16..18].copy_from_slice(&3u16.to_le_bytes());
    img[18..20].copy_from_slice(&3u16.to_le_bytes());
    img[20..24].copy_from_slice(&1u32.to_le_bytes());
    img[32..36].copy_from_slice(&shoff.to_le_bytes());
    img[40..42].copy_from_slice(&52u16.to_le_bytes());
    img[46..48].copy_from_slice(&40u16.to_le_bytes());
    img[48..50].copy_from_slice(&shnum.to_le_bytes());
    img[50..52].copy_from_slice(&shstrndx.to_le_bytes());
    img
}

/// A minimal 32-bit LE ELF header with NO section table — parses as ELF but
/// carries no `.gnu.version` data, so the glibc scan finds no requirement
/// (the static/musl skip path).
fn minimal_elf32_le() -> Vec<u8> {
    let mut h = vec![0u8; 52];
    h[0..4].copy_from_slice(b"\x7fELF");
    h[4] = 1;
    h[5] = 1;
    h[6] = 1;
    h[16] = 3;
    h[18] = 3;
    h[20] = 1;
    h
}

/// Register a `.deb` file on disk and add it as a `LinuxPackage` artifact whose
/// path points at the real file (so `linux_packages` canonicalizes it and the
/// libc check can read it). Returns the directory to keep it alive.
fn register_deb(ctx: &mut Context, dir: &std::path::Path, name: &str, deb_bytes: &[u8]) {
    let path = dir.join(name);
    std::fs::write(&path, deb_bytes).expect("write deb");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        name: name.to_string(),
        path,
        target: None,
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
}

/// Build a libc-only context: gate enabled, assert_assets OFF (no network),
/// glibc ceiling set.
fn libc_ctx(ceiling: &str) -> Context {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("app", None)])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        assert_assets: false,
        glibc_ceiling: Some(ceiling.to_string()),
        install_smoke: None,
    };
    ctx
}

#[test]
fn libc_check_bails_when_deb_exceeds_ceiling() {
    // A .deb whose embedded ELF requires GLIBC_2.99 against a 2.36 ceiling must
    // be flagged and the gate must bail naming the excess version.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let elf = elf32_le_with_glibc_2_99();
    let deb = make_deb(&gz(&make_tar(&[("usr/bin/app", &elf)])));

    let mut ctx = libc_ctx("2.36");
    register_deb(&mut ctx, tmp.path(), "app_amd64.deb", &deb);

    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("a deb above the glibc ceiling must fail the gate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("2.99") && msg.contains("2.36"),
        "failure names the required and ceiling glibc: {msg}"
    );
    assert!(
        msg.contains(PUBLISHED_NOTE),
        "carries the published note: {msg}"
    );
}

#[test]
fn libc_check_passes_when_deb_has_no_glibc_requirement() {
    // A .deb whose ELF has no .gnu.version table (static/musl) is a SKIP, not a
    // failure — the gate passes with no issue.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let elf = minimal_elf32_le();
    let deb = make_deb(&gz(&make_tar(&[("usr/bin/app", &elf)])));

    let mut ctx = libc_ctx("2.36");
    register_deb(&mut ctx, tmp.path(), "app_amd64.deb", &deb);

    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "a deb with no glibc requirement must not fail the gate"
    );
}

#[test]
fn libc_check_skips_deb_with_no_inspectable_elf() {
    // A .deb whose data.tar contains only non-ELF members yields Ok(None) from
    // extract_deb_main_elf => the libc check is skipped (no issue), gate passes.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let deb = make_deb(&gz(&make_tar(&[("usr/share/doc/readme", b"plain text")])));

    let mut ctx = libc_ctx("2.36");
    register_deb(&mut ctx, tmp.path(), "data_amd64.deb", &deb);

    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "a deb with no inspectable ELF skips the libc check"
    );
}

#[test]
fn libc_check_bails_when_deb_unreadable() {
    // A LinuxPackage artifact whose path does not exist on disk: extract reads
    // the file and errors; the stage records that as a "could not read" issue
    // and the gate bails.
    let mut ctx = libc_ctx("2.36");
    // Register an artifact pointing at a nonexistent .deb (path not written).
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        name: "ghost_amd64.deb".to_string(),
        path: std::path::PathBuf::from("/nonexistent/dir/ghost_amd64.deb"),
        target: None,
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("an unreadable deb must fail the libc check");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("could not read") && msg.contains("ghost_amd64.deb"),
        "failure names the unreadable deb: {msg}"
    );
}

#[test]
fn libc_check_ignores_non_deb_linux_packages() {
    // The libc check only inspects `.deb`s; a `.rpm` LinuxPackage artifact must
    // be skipped at the extension filter — even a bogus rpm body must not error
    // the gate.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let mut ctx = libc_ctx("2.36");
    let path = tmp.path().join("app.x86_64.rpm");
    std::fs::write(&path, b"not really an rpm").expect("write rpm");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        name: "app.x86_64.rpm".to_string(),
        path,
        target: None,
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "a non-.deb package is skipped by the libc check"
    );
}

#[test]
fn libc_check_off_does_not_inspect_debs() {
    // With no glibc_ceiling, even a deb that WOULD exceed any ceiling is never
    // inspected: glibc_check_enabled() is false, so the gate passes.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let elf = elf32_le_with_glibc_2_99();
    let deb = make_deb(&gz(&make_tar(&[("usr/bin/app", &elf)])));

    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("app", None)])
        .build();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        assert_assets: false,
        glibc_ceiling: None,
        install_smoke: None,
    };
    register_deb(&mut ctx, tmp.path(), "app_amd64.deb", &deb);

    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "no ceiling => debs are never libc-inspected"
    );
}

#[test]
fn linux_packages_resolves_absolute_path_and_basename() {
    // linux_packages canonicalizes the registered path (so the smoke-test's
    // bind-mount gets an absolute host path) and surfaces the basename. A
    // relative registered path must come back absolute.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let deb = make_deb(&gz(&make_tar(&[("usr/bin/app", &minimal_elf32_le())])));
    let path = tmp.path().join("pkg_amd64.deb");
    std::fs::write(&path, &deb).expect("write deb");

    let mut ctx = TestContextBuilder::new().tag("v1.0.0").build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        name: "pkg_amd64.deb".to_string(),
        path: path.clone(),
        target: None,
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let pkgs = linux_packages(&ctx, "app");
    assert_eq!(pkgs.len(), 1, "the one LinuxPackage artifact is returned");
    let (abs, name, target) = &pkgs[0];
    assert!(abs.is_absolute(), "path is absolute: {}", abs.display());
    assert_eq!(name, "pkg_amd64.deb", "basename surfaced for the caller");
    assert_eq!(target, &None, "host build carries no target triple");
    // A non-existent crate must yield no packages (per-crate isolation).
    assert!(
        linux_packages(&ctx, "other").is_empty(),
        "packages are isolated per crate"
    );

    // A target-built package surfaces its triple so the smoke-test can pin
    // the container platform to the package's architecture.
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::LinuxPackage,
        name: "pkg_arm64.deb".to_string(),
        path: path.clone(),
        target: Some("aarch64-unknown-linux-gnu".to_string()),
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    let pkgs = linux_packages(&ctx, "app");
    let arm = pkgs
        .iter()
        .find(|(_, n, _)| n == "pkg_arm64.deb")
        .expect("arm64 package present");
    assert_eq!(
        arm.2.as_deref().and_then(docker_platform).as_deref(),
        Some("linux/arm64"),
        "triple maps to the docker platform the smoke job pins"
    );
}

#[test]
fn extract_deb_main_elf_picks_largest_elf_member() {
    // extract_deb_main_elf walks the .deb's data.tar and returns the LARGEST
    // ELF member (the shipped binary in the single-binary case), skipping
    // non-ELF members.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let small = [b"\x7fELF".as_slice(), &[1u8; 8]].concat();
    let big = [b"\x7fELF".as_slice(), &[2u8; 64]].concat();
    let deb = make_deb(&gz(&make_tar(&[
        ("usr/share/doc/readme", b"text"),
        ("usr/bin/small", &small),
        ("usr/bin/app", &big),
    ])));
    let path = tmp.path().join("multi_amd64.deb");
    std::fs::write(&path, &deb).expect("write deb");

    let elf = extract_deb_main_elf(&path)
        .expect("read deb")
        .expect("an ELF member");
    assert_eq!(elf, big, "the largest ELF (the binary) is selected");

    // A non-.deb file (no ar magic) yields Ok(None) rather than erroring.
    let txt = tmp.path().join("plain.bin");
    std::fs::write(&txt, b"not a deb").expect("write");
    assert!(
        extract_deb_main_elf(&txt).expect("read").is_none(),
        "a non-ar file degrades to None, not an error"
    );
}

// ---------------------------------------------------------------------------
// Config-derived signature/SBOM expectations (the v0.8.0 gap)
// ---------------------------------------------------------------------------

/// The repo's own `signs:` shape — gpg over checksum artifacts, gated on the
/// release-mode condition that silently mis-evaluated in v0.8.0.
fn checksum_gpg_sign() -> anodizer_core::config::SignConfig {
    anodizer_core::config::SignConfig {
        id: Some("default".to_string()),
        artifacts: Some("checksum".to_string()),
        cmd: Some("gpg".to_string()),
        if_condition: Some("{{ not IsSnapshot or IsHarness }}".to_string()),
        ..Default::default()
    }
}

#[test]
fn derived_expectations_include_per_artifact_sigs_when_signing_enabled() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("app", None)])
        .build();
    ctx.config.signs = vec![checksum_gpg_sign()];
    add_combined_checksum(&mut ctx, "app_checksums.txt", "app");
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");

    let derived = config_expected_asset_names(&ctx, "app", None, None).expect("derivation");
    assert_eq!(derived, vec!["app_checksums.txt.sig".to_string()]);
}

#[test]
fn derived_expectations_empty_when_signing_not_configured() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("app", None)])
        .build();
    add_artifact(&mut ctx, ArtifactKind::Checksum, "app_checksums.txt", "app");
    let derived = config_expected_asset_names(&ctx, "app", None, None).expect("derivation");
    assert!(derived.is_empty());
}

#[test]
fn derived_expectations_empty_when_sign_skipped_by_condition() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("app", None)])
        .build();
    ctx.config.signs = vec![anodizer_core::config::SignConfig {
        if_condition: Some("false".to_string()),
        ..checksum_gpg_sign()
    }];
    add_artifact(&mut ctx, ArtifactKind::Checksum, "app_checksums.txt", "app");
    let derived = config_expected_asset_names(&ctx, "app", None, None).expect("derivation");
    assert!(
        derived.is_empty(),
        "an if: that evaluated false must not create expectations"
    );
}

#[test]
fn derived_expectations_empty_when_run_recorded_intentional_skip() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("app", None)])
        .build();
    ctx.config.signs = vec![checksum_gpg_sign()];
    add_artifact(&mut ctx, ArtifactKind::Checksum, "app_checksums.txt", "app");
    // The run's own skip record is the authoritative waiver.
    ctx.remember_skip("sign", "default", "`if` condition evaluated falsy");
    let derived = config_expected_asset_names(&ctx, "app", None, None).expect("derivation");
    assert!(derived.is_empty());
}

#[test]
fn derived_expectations_follow_subject_verdict_under_release_ids() {
    // A signature inherits its SUBJECT's release.ids verdict: a sig of an
    // ids-excluded archive is not expected, a sig of an ids-included archive is.
    // Under `artifacts: all` the COMBINED checksum IS signed (GoReleaser parity)
    // and, being an always-pass subject, is expected regardless of ids.
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("app", None)])
        .build();
    ctx.config.signs = vec![anodizer_core::config::SignConfig {
        artifacts: Some("all".to_string()),
        ..checksum_gpg_sign()
    }];
    add_combined_checksum(&mut ctx, "app_checksums.txt", "app");
    let mut keep = HashMap::new();
    keep.insert("id".to_string(), "keep".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: "keep.tar.gz".to_string(),
        path: std::path::PathBuf::from("keep.tar.gz"),
        target: None,
        crate_name: "app".to_string(),
        metadata: keep,
        size: None,
    });
    let mut drop_meta = HashMap::new();
    drop_meta.insert("id".to_string(), "drop".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: "drop.tar.gz".to_string(),
        path: std::path::PathBuf::from("drop.tar.gz"),
        target: None,
        crate_name: "app".to_string(),
        metadata: drop_meta,
        size: None,
    });

    let ids = vec!["keep".to_string()];
    let derived = config_expected_asset_names(&ctx, "app", Some(&ids), None).expect("derivation");
    assert_eq!(
        derived,
        vec![
            "app_checksums.txt.sig".to_string(),
            "keep.tar.gz.sig".to_string()
        ],
        "the combined checksum (always-pass) and the ids-included archive are \
         signed under `all`; the ids-excluded archive contributes none"
    );
}

#[test]
fn derived_expectations_drop_excluded_signature() {
    // `release.exclude: ["*.sig"]` keeps a signature off the GitHub release, so
    // the verify-release gate must NOT expect it — otherwise an intentional
    // exclude triggers a false "missing asset" failure.
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![published_crate("app", None)])
        .build();
    ctx.config.signs = vec![anodizer_core::config::SignConfig {
        artifacts: Some("all".to_string()),
        ..checksum_gpg_sign()
    }];
    add_combined_checksum(&mut ctx, "app_checksums.txt", "app");
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: "app.tar.gz".to_string(),
        path: std::path::PathBuf::from("app.tar.gz"),
        target: None,
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    // No exclude: the .sig expectations are present.
    let without = config_expected_asset_names(&ctx, "app", None, None).expect("derivation");
    assert!(
        without.iter().any(|n| n.ends_with(".sig")),
        "precondition: signatures are expected without exclude; got {without:?}"
    );

    // With `exclude: ["*.sig"]`: every .sig expectation is filtered out.
    let exclude = vec!["*.sig".to_string()];
    let with = config_expected_asset_names(&ctx, "app", None, Some(&exclude)).expect("derivation");
    assert!(
        with.iter().all(|n| !n.ends_with(".sig")),
        "release.exclude must drop excluded signatures from the expected set; got {with:?}"
    );
}

#[test]
fn derived_expectations_resolve_per_crate() {
    // Workspace modes: each published crate's expectations come from its own
    // artifacts only.
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![
            published_crate("crate-a", None),
            published_crate("crate-b", None),
        ])
        .build();
    ctx.config.signs = vec![checksum_gpg_sign()];
    add_combined_checksum(&mut ctx, "a_checksums.txt", "crate-a");
    add_combined_checksum(&mut ctx, "b_checksums.txt", "crate-b");

    let a = config_expected_asset_names(&ctx, "crate-a", None, None).expect("derivation");
    let b = config_expected_asset_names(&ctx, "crate-b", None, None).expect("derivation");
    assert_eq!(a, vec!["a_checksums.txt.sig".to_string()]);
    assert_eq!(b, vec!["b_checksums.txt.sig".to_string()]);
}

#[test]
fn unsigned_release_fails_listing_missing_signature_assets() {
    // THE v0.8.0 regression: signing configured and not skipped, but the
    // sign stage registered nothing (no Signature artifacts in the registry)
    // and the published release stores none. The gate previously PASSED;
    // it must now fail naming the exact missing signature assets.
    let (addr, _log) = spawn_release_route(&["app.tar.gz", "app_checksums.txt"]);

    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    ctx.config.signs = vec![checksum_gpg_sign()];
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");
    add_combined_checksum(&mut ctx, "app_checksums.txt", "app");

    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("missing config-required signature assets must fail the gate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("app_checksums.txt.sig"),
        "error names the missing signature asset: {msg}"
    );
    assert!(
        msg.contains("required by the resolved signs/sboms config"),
        "error explains the expectation source: {msg}"
    );
    assert!(
        msg.contains(PUBLISHED_NOTE),
        "error carries the already-published note: {msg}"
    );
}

#[test]
fn signed_release_with_uploaded_sigs_passes() {
    // Healthy case: signing configured AND the sig asset is on the release.
    // No Signature artifact needs to be in the registry for the gate to pass —
    // the published set satisfies the config-derived expectation.
    let (addr, _log) =
        spawn_release_route(&["app.tar.gz", "app_checksums.txt", "app_checksums.txt.sig"]);

    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    ctx.config.signs = vec![checksum_gpg_sign()];
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");
    add_artifact(&mut ctx, ArtifactKind::Checksum, "app_checksums.txt", "app");

    assert!(
        VerifyReleaseStage.run(&mut ctx).is_ok(),
        "expected signature present on the release => gate passes"
    );
}

#[test]
fn skipped_sign_stage_does_not_fail_unsigned_release() {
    // --skip=sign is explicit operator intent: the release is knowingly
    // unsigned and the gate must not demand signatures.
    let (addr, _log) = spawn_release_route(&["app.tar.gz", "app_checksums.txt"]);

    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    ctx.config.signs = vec![checksum_gpg_sign()];
    ctx.options.skip_stages.push("sign".to_string());
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");
    add_artifact(&mut ctx, ArtifactKind::Checksum, "app_checksums.txt", "app");

    assert!(VerifyReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn sbom_expectations_fail_when_configured_sboms_never_uploaded() {
    // sboms: configured per-archive, but neither the registry nor the
    // release has the SBOM => the gate fails naming the missing document.
    let (addr, _log) = spawn_release_route(&["app.tar.gz"]);

    let mut ctx = asset_ctx(addr, vec![published_crate("app", None)]);
    ctx.config.project_name = "app".to_string();
    ctx.config.sboms = vec![anodizer_core::config::SbomConfig {
        documents: Some(vec!["{{ .ArtifactName }}.cdx.json".to_string()]),
        artifacts: Some("archive".to_string()),
        ..Default::default()
    }];
    add_artifact(&mut ctx, ArtifactKind::Archive, "app.tar.gz", "app");

    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("missing config-required SBOM assets must fail the gate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("app.tar.gz.cdx.json"),
        "error names the missing SBOM asset: {msg}"
    );
}

// ---------------------------------------------------------------------------
// v0.8.0 mirror — the live failure, reproduced byte-for-byte
// ---------------------------------------------------------------------------

/// The COMPLETE asset list of the real published v0.8.0 release (fetched via
/// `gh release view v0.8.0 --json assets` on 2026-06-11). Zero signature
/// assets: the sign stage was silently skipped by the Is*-string-compare
/// template-typing bug, and the gate passed because its expectations were
/// registry-derived.
const V080_PUBLISHED_ASSETS: &[&str] = &[
    "anodizer-0.8.0-darwin-amd64-extra.tar.xz",
    "anodizer-0.8.0-darwin-amd64-extra.tar.xz.cdx.json",
    "anodizer-0.8.0-darwin-amd64-extra.tar.xz.cdx.json.sha256",
    "anodizer-0.8.0-darwin-amd64-extra.tar.xz.sha256",
    "anodizer-0.8.0-darwin-amd64-extra.tar.zst",
    "anodizer-0.8.0-darwin-amd64-extra.tar.zst.cdx.json",
    "anodizer-0.8.0-darwin-amd64-extra.tar.zst.cdx.json.sha256",
    "anodizer-0.8.0-darwin-amd64-extra.tar.zst.sha256",
    "anodizer-0.8.0-darwin-amd64.tar.gz",
    "anodizer-0.8.0-darwin-amd64.tar.gz.cdx.json",
    "anodizer-0.8.0-darwin-amd64.tar.gz.cdx.json.sha256",
    "anodizer-0.8.0-darwin-amd64.tar.gz.sha256",
    "anodizer-0.8.0-darwin-arm64-extra.tar.xz",
    "anodizer-0.8.0-darwin-arm64-extra.tar.xz.cdx.json",
    "anodizer-0.8.0-darwin-arm64-extra.tar.xz.cdx.json.sha256",
    "anodizer-0.8.0-darwin-arm64-extra.tar.xz.sha256",
    "anodizer-0.8.0-darwin-arm64-extra.tar.zst",
    "anodizer-0.8.0-darwin-arm64-extra.tar.zst.cdx.json",
    "anodizer-0.8.0-darwin-arm64-extra.tar.zst.cdx.json.sha256",
    "anodizer-0.8.0-darwin-arm64-extra.tar.zst.sha256",
    "anodizer-0.8.0-darwin-arm64.tar.gz",
    "anodizer-0.8.0-darwin-arm64.tar.gz.cdx.json",
    "anodizer-0.8.0-darwin-arm64.tar.gz.cdx.json.sha256",
    "anodizer-0.8.0-darwin-arm64.tar.gz.sha256",
    "anodizer_0.8.0_linux_amd64.apk",
    "anodizer_0.8.0_linux_amd64.apk.sha256",
    "anodizer_0.8.0_linux_amd64.deb",
    "anodizer_0.8.0_linux_amd64.deb.sha256",
    "anodizer-0.8.0-linux-amd64-extra.tar.xz",
    "anodizer-0.8.0-linux-amd64-extra.tar.xz.cdx.json",
    "anodizer-0.8.0-linux-amd64-extra.tar.xz.cdx.json.sha256",
    "anodizer-0.8.0-linux-amd64-extra.tar.xz.sha256",
    "anodizer-0.8.0-linux-amd64-extra.tar.zst",
    "anodizer-0.8.0-linux-amd64-extra.tar.zst.cdx.json",
    "anodizer-0.8.0-linux-amd64-extra.tar.zst.cdx.json.sha256",
    "anodizer-0.8.0-linux-amd64-extra.tar.zst.sha256",
    "anodizer-0.8.0-linux-amd64-installer.run",
    "anodizer-0.8.0-linux-amd64-installer.run.sha256",
    "anodizer_0.8.0_linux_amd64.rpm",
    "anodizer_0.8.0_linux_amd64.rpm.sha256",
    "anodizer-0.8.0-linux-amd64.tar.gz",
    "anodizer-0.8.0-linux-amd64.tar.gz.cdx.json",
    "anodizer-0.8.0-linux-amd64.tar.gz.cdx.json.sha256",
    "anodizer-0.8.0-linux-amd64.tar.gz.sha256",
    "anodizer_0.8.0_linux_arm64.apk",
    "anodizer_0.8.0_linux_arm64.apk.sha256",
    "anodizer_0.8.0_linux_arm64.deb",
    "anodizer_0.8.0_linux_arm64.deb.sha256",
    "anodizer-0.8.0-linux-arm64-extra.tar.xz",
    "anodizer-0.8.0-linux-arm64-extra.tar.xz.cdx.json",
    "anodizer-0.8.0-linux-arm64-extra.tar.xz.cdx.json.sha256",
    "anodizer-0.8.0-linux-arm64-extra.tar.xz.sha256",
    "anodizer-0.8.0-linux-arm64-extra.tar.zst",
    "anodizer-0.8.0-linux-arm64-extra.tar.zst.cdx.json",
    "anodizer-0.8.0-linux-arm64-extra.tar.zst.cdx.json.sha256",
    "anodizer-0.8.0-linux-arm64-extra.tar.zst.sha256",
    "anodizer-0.8.0-linux-arm64-installer.run",
    "anodizer-0.8.0-linux-arm64-installer.run.sha256",
    "anodizer_0.8.0_linux_arm64.rpm",
    "anodizer_0.8.0_linux_arm64.rpm.sha256",
    "anodizer-0.8.0-linux-arm64.tar.gz",
    "anodizer-0.8.0-linux-arm64.tar.gz.cdx.json",
    "anodizer-0.8.0-linux-arm64.tar.gz.cdx.json.sha256",
    "anodizer-0.8.0-linux-arm64.tar.gz.sha256",
    "anodizer-0.8.0-source.tar.gz",
    "anodizer-0.8.0-source.tar.gz.sha256",
    "anodizer-0.8.0-windows-amd64-extra.tgz",
    "anodizer-0.8.0-windows-amd64-extra.tgz.cdx.json",
    "anodizer-0.8.0-windows-amd64-extra.tgz.cdx.json.sha256",
    "anodizer-0.8.0-windows-amd64-extra.tgz.sha256",
    "anodizer-0.8.0-windows-amd64.zip",
    "anodizer-0.8.0-windows-amd64.zip.cdx.json",
    "anodizer-0.8.0-windows-amd64.zip.cdx.json.sha256",
    "anodizer-0.8.0-windows-amd64.zip.sha256",
    "anodizer-0.8.0-windows-arm64-extra.tgz",
    "anodizer-0.8.0-windows-arm64-extra.tgz.cdx.json",
    "anodizer-0.8.0-windows-arm64-extra.tgz.cdx.json.sha256",
    "anodizer-0.8.0-windows-arm64-extra.tgz.sha256",
    "anodizer-0.8.0-windows-arm64.zip",
    "anodizer-0.8.0-windows-arm64.zip.cdx.json",
    "anodizer-0.8.0-windows-arm64.zip.cdx.json.sha256",
    "anodizer-0.8.0-windows-arm64.zip.sha256",
    "anodizer.1",
    "anodizer-apk-signing-key.rsa.pub",
    "attestation-subjects.json",
    "install.sh",
    "install.sh.sha256",
    "metadata.json",
];

/// Map a v0.8.0 asset name to the artifact kind the real run registered it
/// under. Suffix order matters: `.cdx.json.sha256` is a Checksum, not an Sbom.
fn v080_kind(name: &str) -> ArtifactKind {
    if name.ends_with(".sha256") {
        ArtifactKind::Checksum
    } else if name.ends_with(".cdx.json") {
        ArtifactKind::Sbom
    } else if name.ends_with(".apk") || name.ends_with(".deb") || name.ends_with(".rpm") {
        ArtifactKind::LinuxPackage
    } else if name.ends_with(".run") {
        ArtifactKind::Makeself
    } else if name.ends_with("-source.tar.gz") {
        ArtifactKind::SourceArchive
    } else if name.ends_with(".tar.gz")
        || name.ends_with(".tar.xz")
        || name.ends_with(".tar.zst")
        || name.ends_with(".tgz")
        || name.ends_with(".zip")
    {
        ArtifactKind::Archive
    } else {
        ArtifactKind::UploadableFile
    }
}

/// Register the v0.8.0 produced artifact set (exactly the published assets —
/// the upload itself was complete; only the signatures were never produced).
fn register_v080_produced_set(ctx: &mut Context, crate_name: &str) {
    for name in V080_PUBLISHED_ASSETS {
        add_artifact(ctx, v080_kind(name), name, crate_name);
    }
}

/// The repo's real `sboms:` shape (built-in CycloneDX per archive).
fn real_sboms_config() -> Vec<anodizer_core::config::SbomConfig> {
    vec![anodizer_core::config::SbomConfig {
        id: Some("default".to_string()),
        documents: Some(vec!["{{ .ArtifactName }}.cdx.json".to_string()]),
        artifacts: Some("archive".to_string()),
        ..Default::default()
    }]
}

#[test]
fn v080_mirror_split_checksum_signing_demands_second_level_sigs_no_recursion() {
    // Reproduction of the v0.8.0 asset shape: split (`split: true`) per-artifact
    // `.sha256` sidecars, the real signs (`artifacts: checksum`) / sboms
    // (`artifacts: archive`) config, and the real published set — which has
    // every `.sha256` checksum but NO `.sig` (the v0.8.0 sign-skip bug).
    //
    // GoReleaser parity: `artifacts: checksum` signs EVERY Checksum, so the
    // derivation demands one legit `X.sha256.sig` (second level) per checksum
    // asset (42). None were uploaded, so the gate must FAIL listing them. It
    // must NEVER demand a third-level `X.sha256.sig.sha256` — that forbidden
    // recursion stays unrepresentable (checksum input is primary-only, refresh
    // skips derived sidecars).
    let (addr, _log) = spawn_release_route(V080_PUBLISHED_ASSETS);

    let mut ctx = asset_ctx(addr, vec![published_crate("anodizer", None)]);
    ctx.config.project_name = "anodizer".to_string();
    ctx.config.signs = vec![checksum_gpg_sign()];
    ctx.config.sboms = real_sboms_config();
    register_v080_produced_set(&mut ctx, "anodizer");

    let derived = config_expected_asset_names(&ctx, "anodizer", None, None).expect("derivation");

    // Every demanded asset is a legit terminal — no forbidden recursive chain.
    for name in &derived {
        assert!(
            !has_recursive_sidecar_chain(name),
            "config demanded a forbidden recursive sidecar asset: {name}"
        );
    }
    // The 42 second-level checksum signatures ARE demanded (GR parity)...
    let checksum_sigs = derived
        .iter()
        .filter(|n| n.ends_with(".sha256.sig"))
        .count();
    assert_eq!(
        checksum_sigs, 42,
        "one X.sha256.sig per checksum asset is demanded (GR parity); got {derived:?}"
    );
    assert!(
        derived
            .iter()
            .any(|n| n == "anodizer-0.8.0-linux-amd64.tar.gz.sha256.sig"),
        "the legit second-level checksum signature is demanded: {derived:?}"
    );
    // ...but NO third-level checksum-of-a-signature is ever demanded.
    assert!(
        !derived.iter().any(|n| n.ends_with(".sha256.sig.sha256")),
        "the forbidden third-level chain must never be demanded: {derived:?}"
    );

    // The gate FAILS: the 42 demanded `.sha256.sig` assets were never produced
    // or uploaded (the v0.8.0 sign-skip bug), and that is precisely what the
    // config-derived expectation catches.
    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("the unsigned v0.8.0 asset set must fail the gate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("signature/SBOM asset(s) required by the resolved signs/sboms config"),
        "the gate names the missing config-required signatures: {msg}"
    );
    assert!(
        msg.contains("anodizer-0.8.0-linux-amd64.tar.gz.sha256.sig"),
        "the missing second-level signatures are named precisely: {msg}"
    );
}

#[test]
#[ignore = "live read-only probe of the real v0.8.0 GitHub release; needs GITHUB_TOKEN. \
            Run: cargo test -p anodizer-stage-verify-release --lib -- --ignored live_v080"]
fn live_v080_real_release_fails_missing_signature_assets() {
    // Live evidence path: same context as the mirror test but fetching the
    // REAL release assets from api.github.com (read-only GET). There is no
    // standalone CLI entry that runs the gate against an existing release,
    // so this is the closest real-code-path probe.
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .expect("GITHUB_TOKEN required for the live probe");

    let yaml = "name: anodizer\npath: .\ntag_template: \"v{{ .Version }}\"\n\
                release:\n  github: { owner: tj-smith47, name: anodizer }\n";
    let crate_cfg: CrateConfig = serde_yaml_ng::from_str(yaml).expect("valid crate yaml");

    let mut ctx = TestContextBuilder::new()
        .tag("v0.8.0")
        .token(Some(token))
        .crates(vec![crate_cfg])
        .build();
    ctx.config.project_name = "anodizer".to_string();
    ctx.config.verify_release = VerifyReleaseConfig {
        enabled: true,
        assert_assets: true,
        glibc_ceiling: None,
        install_smoke: None,
    };
    ctx.config.signs = vec![checksum_gpg_sign()];
    ctx.config.sboms = real_sboms_config();
    register_v080_produced_set(&mut ctx, "anodizer");

    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("the real v0.8.0 release is unsigned and must fail the gate");
    let msg = format!("{err:#}");
    eprintln!("live v0.8.0 verify-release output:\n{msg}");
    assert!(
        msg.contains("42 signature/SBOM asset(s) required by the resolved signs/sboms config"),
        "live release is missing exactly the 42 checksum signatures: {msg}"
    );
    assert!(
        msg.contains("anodizer-0.8.0-linux-amd64.tar.gz.sha256.sig"),
        "live error names the missing sig assets: {msg}"
    );
}

#[test]
fn gate_demands_sig_of_subjectless_sbom_under_release_ids() {
    // release.ids + a project-wide (subject-less) SBOM + signs over sboms:
    // the any-SBOM uploads regardless of the ids filter, so its signature
    // must be expected — transitively record-less, never stranded behind a
    // subject_kind:"sbom"/empty-id record. The sig of an ids-EXCLUDED
    // archive's SBOM must NOT be expected.
    let (addr, _log) = spawn_release_route(&["keep.tar.gz", "project.cdx.json"]);

    let yaml = "name: app\npath: .\ntag_template: \"v{{ .Version }}\"\n\
                release:\n  github: { owner: me, name: repo }\n  ids: [keep]\n";
    let crate_cfg: CrateConfig = serde_yaml_ng::from_str(yaml).expect("valid crate yaml");
    let mut ctx = asset_ctx(addr, vec![crate_cfg]);
    ctx.config.signs = vec![anodizer_core::config::SignConfig {
        artifacts: Some("sbom".to_string()),
        ..checksum_gpg_sign()
    }];

    let mut add_with_meta = |kind: ArtifactKind, name: &str, meta: &[(&str, &str)]| {
        ctx.artifacts.add(Artifact {
            kind,
            name: name.to_string(),
            path: std::path::PathBuf::from(name),
            target: None,
            crate_name: "app".to_string(),
            metadata: meta
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            size: None,
        });
    };
    add_with_meta(ArtifactKind::Archive, "keep.tar.gz", &[("id", "keep")]);
    add_with_meta(
        ArtifactKind::Sbom,
        "project.cdx.json",
        &[("sbom_id", "default")],
    );
    add_with_meta(ArtifactKind::Archive, "drop.zip", &[("id", "drop")]);
    add_with_meta(
        ArtifactKind::Sbom,
        "drop.zip.cdx.json",
        &[("subject_kind", "archive"), ("id", "drop")],
    );

    let err = VerifyReleaseStage
        .run(&mut ctx)
        .expect_err("missing sig of the uploaded any-SBOM must fail the gate");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("project.cdx.json.sig"),
        "the subject-less SBOM's signature is demanded: {msg}"
    );
    assert!(
        !msg.contains("drop.zip.cdx.json.sig"),
        "the excluded archive's SBOM sig must NOT be demanded: {msg}"
    );
}
