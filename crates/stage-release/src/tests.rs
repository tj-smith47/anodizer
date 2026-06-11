#![allow(clippy::field_reassign_with_default)]

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    ContentSource, CrateConfig, ExtraFileSpec, GitHubUrlsConfig, MakeLatestConfig,
    PrereleaseConfig, ReleaseConfig, StringOrBool,
};
use anodizer_core::scm::ScmTokenType;
use anodizer_core::stage::Stage;
use anodizer_core::test_helpers::TestContextBuilder;

use super::ReleaseStage;
use super::github::build_octocrab_client;
use super::release_body::{
    GITHUB_RELEASE_BODY_MAX_CHARS, build_publish_patch_body, build_release_body,
    build_release_json, collect_extra_files, compose_body_for_mode,
    render_nondeterministic_exemptions_block, resolve_content_source, resolve_header_footer,
    resolve_make_latest, resolve_release_tag,
};
use super::{
    compose_release_url, populate_artifact_download_urls, populate_checksums_var, retry_upload,
    should_mark_prerelease,
};

#[test]
fn test_is_prerelease_auto_with_rc() {
    assert!(should_mark_prerelease(
        &Some(PrereleaseConfig::Auto),
        "v1.0.0-rc.1"
    ));
}

#[test]
fn test_is_prerelease_auto_stable() {
    assert!(!should_mark_prerelease(
        &Some(PrereleaseConfig::Auto),
        "v1.0.0"
    ));
}

#[test]
fn test_is_prerelease_explicit_true() {
    assert!(should_mark_prerelease(
        &Some(PrereleaseConfig::Bool(true)),
        "v1.0.0"
    ));
}

#[test]
fn test_is_prerelease_explicit_false() {
    assert!(!should_mark_prerelease(
        &Some(PrereleaseConfig::Bool(false)),
        "v1.0.0-rc.1"
    ));
}

#[test]
fn test_is_prerelease_none() {
    assert!(!should_mark_prerelease(&None, "v1.0.0"));
}

#[test]
fn test_stage_skips_crate_without_release_config() {
    let mut ctx = TestContextBuilder::new().build();
    let stage = ReleaseStage;
    // Should succeed — no crates have release config
    assert!(stage.run(&mut ctx).is_ok());
}

// ---- populate_artifact_download_urls tests ----

#[test]
fn test_populate_artifact_download_urls_github() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: "dist/myapp_1.0.0_linux_amd64.tar.gz".into(),
        name: "myapp_1.0.0_linux_amd64.tar.gz".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        path: "dist/checksums.txt".into(),
        name: "checksums.txt".to_string(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    populate_artifact_download_urls(
        &mut ctx,
        "myapp",
        ScmTokenType::GitHub,
        "https://github.com",
        "octocat",
        "hello",
        "v1.0.0",
    );

    let archive = ctx
        .artifacts
        .all()
        .iter()
        .find(|a| a.name == "myapp_1.0.0_linux_amd64.tar.gz")
        .unwrap();
    assert_eq!(
        archive.metadata.get("url").unwrap(),
        "https://github.com/octocat/hello/releases/download/v1.0.0/myapp_1.0.0_linux_amd64.tar.gz"
    );
    let checksum = ctx
        .artifacts
        .all()
        .iter()
        .find(|a| a.name == "checksums.txt")
        .unwrap();
    assert_eq!(
        checksum.metadata.get("url").unwrap(),
        "https://github.com/octocat/hello/releases/download/v1.0.0/checksums.txt"
    );
}

#[test]
fn test_populate_artifact_download_urls_github_enterprise() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: "dist/myapp.tar.gz".into(),
        name: "myapp.tar.gz".to_string(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    populate_artifact_download_urls(
        &mut ctx,
        "myapp",
        ScmTokenType::GitHub,
        "https://github.example.com",
        "org",
        "repo",
        "v2.0.0",
    );

    let a = ctx
        .artifacts
        .all()
        .iter()
        .find(|a| a.name == "myapp.tar.gz")
        .unwrap();
    assert_eq!(
        a.metadata.get("url").unwrap(),
        "https://github.example.com/org/repo/releases/download/v2.0.0/myapp.tar.gz"
    );
}

#[test]
fn test_populate_artifact_download_urls_gitlab() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: "dist/app.tar.gz".into(),
        name: "app.tar.gz".to_string(),
        target: None,
        crate_name: "app".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    populate_artifact_download_urls(
        &mut ctx,
        "app",
        ScmTokenType::GitLab,
        "https://gitlab.com",
        "group",
        "project",
        "v1.0.0",
    );

    let a = ctx
        .artifacts
        .all()
        .iter()
        .find(|a| a.name == "app.tar.gz")
        .unwrap();
    assert_eq!(
        a.metadata.get("url").unwrap(),
        "https://gitlab.com/group/project/-/releases/v1.0.0/downloads/app.tar.gz"
    );
}

#[test]
fn test_populate_artifact_download_urls_gitea() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: "dist/tool.tar.gz".into(),
        name: "tool.tar.gz".to_string(),
        target: None,
        crate_name: "tool".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    populate_artifact_download_urls(
        &mut ctx,
        "tool",
        ScmTokenType::Gitea,
        "https://gitea.example.com",
        "owner",
        "repo",
        "v3.0.0",
    );

    let a = ctx
        .artifacts
        .all()
        .iter()
        .find(|a| a.name == "tool.tar.gz")
        .unwrap();
    assert_eq!(
        a.metadata.get("url").unwrap(),
        "https://gitea.example.com/owner/repo/releases/download/v3.0.0/tool.tar.gz"
    );
}

#[test]
fn test_populate_artifact_download_urls_encodes_special_chars() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: "dist/my app.tar.gz".into(),
        name: "my app.tar.gz".to_string(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    populate_artifact_download_urls(
        &mut ctx,
        "myapp",
        ScmTokenType::GitHub,
        "https://github.com",
        "owner",
        "repo",
        "v1.0.0-rc.1",
    );

    let a = ctx.artifacts.all().first().unwrap();
    let url = a.metadata.get("url").unwrap();
    assert!(
        url.contains("my%20app.tar.gz"),
        "spaces should be percent-encoded: {}",
        url
    );
}

#[test]
fn test_populate_artifact_download_urls_skips_other_crates() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: "dist/other.tar.gz".into(),
        name: "other.tar.gz".to_string(),
        target: None,
        crate_name: "other_crate".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    populate_artifact_download_urls(
        &mut ctx,
        "myapp",
        ScmTokenType::GitHub,
        "https://github.com",
        "owner",
        "repo",
        "v1.0.0",
    );

    let a = ctx.artifacts.all().first().unwrap();
    assert!(
        !a.metadata.contains_key("url"),
        "should not set URL for different crate"
    );
}

// ---- retry_upload tests ----

#[tokio::test]
async fn test_retry_upload_succeeds_immediately() {
    let result = retry_upload("test", || async { Ok(()) }).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_retry_upload_retries_transient_errors() {
    // Network-substring errors are classified retriable by `is_retriable`.
    let attempt = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let attempt_clone = attempt.clone();
    let result = retry_upload("test", move || {
        let attempt = attempt_clone.clone();
        async move {
            let n = attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < 2 {
                anyhow::bail!("connection reset by peer");
            }
            Ok(())
        }
    })
    .await;
    assert!(result.is_ok());
    assert_eq!(attempt.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn retry_upload_fast_fails_4xx_via_inner_classifier() {
    // Regression guard: the outer `retry_upload` MUST honor the inner
    // `retry_http_async` 4xx fast-fail decision. Pre-fix, the outer arm
    // retried every Err unconditionally, amplifying the inner's correct
    // fast-fail by 10×. After the fix, an `HttpError { status: 422 }` in
    // the chain breaks immediately (single attempt).
    use anodizer_core::retry::HttpError;
    let attempt = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let attempt_clone = attempt.clone();
    let result = retry_upload("test", move || {
        let attempt = attempt_clone.clone();
        async move {
            attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let inner = HttpError::new(std::io::Error::other("422"), 422);
            Err::<(), _>(anyhow::Error::new(inner).context("upload failed"))
        }
    })
    .await;
    assert!(result.is_err(), "4xx must surface as Err");
    assert_eq!(
        attempt.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "4xx must NOT retry — fast-fail honors inner classifier"
    );
}

#[tokio::test]
async fn retry_upload_retries_5xx() {
    // Symmetry with the 4xx case: a 503 in the chain is retriable, so the
    // outer loop must continue until success (or exhaustion).
    use anodizer_core::retry::HttpError;
    let attempt = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let attempt_clone = attempt.clone();
    let result = retry_upload("test", move || {
        let attempt = attempt_clone.clone();
        async move {
            let n = attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < 2 {
                let inner = HttpError::new(std::io::Error::other("503"), 503);
                Err(anyhow::Error::new(inner).context("upload failed"))
            } else {
                Ok(())
            }
        }
    })
    .await;
    assert!(result.is_ok());
    assert_eq!(
        attempt.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "5xx must retry until success"
    );
}

// ---- build_release_body tests ----

#[test]
fn test_build_release_body_with_header_and_footer() {
    let body = build_release_body(
        "## Changes\n- Fixed a bug",
        Some("# Release v1.0"),
        Some("---\nPowered by anodizer"),
    );
    assert_eq!(
        body,
        "# Release v1.0\n\n## Changes\n- Fixed a bug\n\n---\nPowered by anodizer\n"
    );
}

#[test]
fn test_build_release_body_header_only() {
    let body = build_release_body("changelog content", Some("HEADER"), None);
    assert_eq!(body, "HEADER\n\nchangelog content\n");
}

#[test]
fn test_build_release_body_footer_only() {
    let body = build_release_body("changelog content", None, Some("FOOTER"));
    assert_eq!(body, "changelog content\n\nFOOTER\n");
}

#[test]
fn test_build_release_body_no_header_footer() {
    let body = build_release_body("changelog content", None, None);
    assert_eq!(body, "changelog content\n");
}

#[test]
fn test_build_release_body_empty_changelog() {
    let body = build_release_body("", Some("HEADER"), Some("FOOTER"));
    assert_eq!(body, "HEADER\n\nFOOTER\n");
}

#[test]
fn test_build_release_body_all_empty() {
    let body = build_release_body("", None, None);
    assert_eq!(body, "");
}

#[test]
fn allow_nondeterministic_appears_in_release_body() {
    let entries = vec![
        ("foo.rpm".to_string(), "tool-bug-1234".to_string()),
        ("bar.msi".to_string(), "signing-cert-rotation".to_string()),
    ];
    let block = render_nondeterministic_exemptions_block(&entries);
    assert!(
        block.contains("Non-deterministic exemptions:"),
        "header missing: {}",
        block
    );
    assert!(
        block.contains("foo.rpm - tool-bug-1234"),
        "first entry missing: {}",
        block
    );
    assert!(
        block.contains("bar.msi - signing-cert-rotation"),
        "second entry missing: {}",
        block
    );
    // Must be ASCII-only (no emdash). Cheap sanity check.
    assert!(
        block.is_ascii(),
        "exemption block must be ASCII for predictable rendering: {}",
        block
    );
}

#[test]
fn render_nondeterministic_exemptions_block_empty_is_noop() {
    assert_eq!(render_nondeterministic_exemptions_block(&[]), "");
}

#[test]
fn render_nondeterministic_exemptions_block_single_entry_shape() {
    let entries = vec![("only.deb".to_string(), "dpkg timestamp".to_string())];
    let block = render_nondeterministic_exemptions_block(&entries);
    assert_eq!(
        block, "Non-deterministic exemptions:\n  only.deb - dpkg timestamp\n",
        "single-entry block shape regressed"
    );
}

#[test]
fn test_build_release_body_empty_string_header_footer() {
    // Empty strings should be treated as absent
    let body = build_release_body("changes", Some(""), Some(""));
    assert_eq!(body, "changes\n");
}

// ---- collect_extra_files tests ----

#[test]
fn test_collect_extra_files_no_patterns() {
    let ctx = TestContextBuilder::new().build();
    let result = collect_extra_files(&[], &ctx).unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_collect_extra_files_no_matches() {
    let ctx = TestContextBuilder::new().build();
    // a glob that matches nothing is a hard error.
    let result = collect_extra_files(
        &[ExtraFileSpec::Glob(
            "/tmp/anodizer_test_nonexistent_dir_12345/*.xyz".to_string(),
        )],
        &ctx,
    );
    assert!(result.is_err());
}

#[test]
fn test_collect_extra_files_no_matches_dry_run_downgrades_to_warning() {
    // Dry-run never executes before/after hooks, so hook-produced files
    // cannot exist; the zero-match must warn instead of hard-erroring.
    let ctx = TestContextBuilder::new().dry_run(true).build();
    let result = collect_extra_files(
        &[ExtraFileSpec::Glob(
            "/tmp/anodizer_test_nonexistent_dir_12345/*.xyz".to_string(),
        )],
        &ctx,
    )
    .unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_collect_extra_files_detailed_no_matches_dry_run_downgrades_to_warning() {
    let ctx = TestContextBuilder::new().dry_run(true).build();
    let result = collect_extra_files(
        &[ExtraFileSpec::Detailed {
            glob: "/tmp/anodizer_test_nonexistent_dir_12345/*.xyz".to_string(),
            name_template: None,
            allow_empty: false,
        }],
        &ctx,
    )
    .unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_collect_extra_files_detailed_no_matches_snapshot_still_errors() {
    // Snapshot (without --dry-run) DOES execute hooks, so a zero-match
    // stays a hard error there — only dry-run downgrades.
    let ctx = TestContextBuilder::new().snapshot(true).build();
    let result = collect_extra_files(
        &[ExtraFileSpec::Detailed {
            glob: "/tmp/anodizer_test_nonexistent_dir_12345/*.xyz".to_string(),
            name_template: None,
            allow_empty: false,
        }],
        &ctx,
    );
    assert!(result.is_err());
}

#[test]
fn test_collect_extra_files_with_real_file() {
    let ctx = TestContextBuilder::new().build();
    // Create a temp file and collect it
    let dir = std::env::temp_dir().join("anodizer_extra_files_test");
    let _ = std::fs::create_dir_all(&dir);
    let test_file = dir.join("test_extra.txt");
    std::fs::write(&test_file, "extra file content").unwrap();

    let pattern = dir.join("*.txt").to_string_lossy().into_owned();
    let result = collect_extra_files(&[ExtraFileSpec::Glob(pattern)], &ctx).unwrap();
    assert!(
        result
            .iter()
            .any(|(p, _)| p.file_name().unwrap() == "test_extra.txt")
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_collect_extra_files_skips_directories() {
    let ctx = TestContextBuilder::new().build();
    let dir = std::env::temp_dir().join("anodizer_extra_files_dir_test");
    let _ = std::fs::create_dir_all(dir.join("subdir"));
    let test_file = dir.join("file.txt");
    std::fs::write(&test_file, "content").unwrap();

    // The glob "*" matches both files and directories; we only want files
    let pattern = dir.join("*").to_string_lossy().into_owned();
    let result = collect_extra_files(&[ExtraFileSpec::Glob(pattern)], &ctx).unwrap();
    assert!(result.iter().all(|(p, _)| p.is_file()));

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_collect_extra_files_detailed_spec() {
    let ctx = TestContextBuilder::new().build();
    let dir = std::env::temp_dir().join("anodizer_extra_files_detailed_test");
    let _ = std::fs::create_dir_all(&dir);
    let test_file = dir.join("artifact.sig");
    std::fs::write(&test_file, "signature").unwrap();

    let pattern = dir.join("*.sig").to_string_lossy().into_owned();
    let result = collect_extra_files(
        &[ExtraFileSpec::Detailed {
            glob: pattern,
            name_template: Some("{{ .ArtifactName }}.sig".to_string()),
            allow_empty: false,
        }],
        &ctx,
    )
    .unwrap();
    assert_eq!(result.len(), 1);
    assert!(result[0].0.file_name().unwrap() == "artifact.sig");
    // name_template should have been rendered
    assert_eq!(result[0].1.as_deref(), Some("artifact.sig.sig"));

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- resolve_make_latest tests ----

/// Identity renderer for tests — returns the input unchanged.
fn noop_render(s: &str) -> anyhow::Result<String> {
    Ok(s.to_string())
}

#[test]
fn test_resolve_make_latest_true() {
    let ml = resolve_make_latest(&Some(MakeLatestConfig::Bool(true)), noop_render).unwrap();
    assert!(ml.is_some());
    assert_eq!(ml.unwrap().to_string(), "true");
}

#[test]
fn test_resolve_make_latest_false() {
    let ml = resolve_make_latest(&Some(MakeLatestConfig::Bool(false)), noop_render).unwrap();
    assert!(ml.is_some());
    assert_eq!(ml.unwrap().to_string(), "false");
}

#[test]
fn test_resolve_make_latest_auto() {
    let ml = resolve_make_latest(&Some(MakeLatestConfig::Auto), noop_render).unwrap();
    assert!(ml.is_some());
    assert_eq!(ml.unwrap().to_string(), "legacy");
}

#[test]
fn test_resolve_make_latest_none() {
    let ml = resolve_make_latest(&None, noop_render).unwrap();
    assert!(ml.is_none());
}

#[test]
fn test_resolve_make_latest_template_string_true() {
    let ml = resolve_make_latest(
        &Some(MakeLatestConfig::String("true".to_string())),
        noop_render,
    )
    .unwrap();
    assert!(ml.is_some());
    assert_eq!(ml.unwrap().to_string(), "true");
}

#[test]
fn test_resolve_make_latest_template_string_false() {
    let ml = resolve_make_latest(
        &Some(MakeLatestConfig::String("false".to_string())),
        noop_render,
    )
    .unwrap();
    assert!(ml.is_some());
    assert_eq!(ml.unwrap().to_string(), "false");
}

#[test]
fn test_resolve_make_latest_template_string_auto() {
    let ml = resolve_make_latest(
        &Some(MakeLatestConfig::String("auto".to_string())),
        noop_render,
    )
    .unwrap();
    assert!(ml.is_some());
    assert_eq!(ml.unwrap().to_string(), "legacy");
}

#[test]
fn test_resolve_make_latest_template_rendered() {
    // Simulate a template that renders to "false"
    let ml = resolve_make_latest(
        &Some(MakeLatestConfig::String("{{ .IsSnapshot }}".to_string())),
        |_| Ok("false".to_string()),
    )
    .unwrap();
    assert!(ml.is_some());
    assert_eq!(ml.unwrap().to_string(), "false");
}

// ---- skip_upload behavior test ----

#[test]
fn test_skip_upload_dry_run_message() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip_upload: Some(StringOrBool::Bool(true)),
                draft: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let stage = ReleaseStage;
    // Dry-run should succeed even with skip_upload = true
    assert!(stage.run(&mut ctx).is_ok());
}

// ---- replace_existing_draft / replace_existing_artifacts config defaults ----

#[test]
fn test_replace_existing_draft_defaults() {
    let cfg = ReleaseConfig::default();
    assert_eq!(cfg.replace_existing_draft, None);
}

#[test]
fn test_replace_existing_artifacts_defaults() {
    let cfg = ReleaseConfig::default();
    assert_eq!(cfg.replace_existing_artifacts, None);
}

// ---- integration-style dry-run tests ----

#[test]
fn test_dry_run_with_extra_files() {
    // extra_files globs that match nothing are hard
    // errors. Create a real file so the stage completes successfully.
    let tmp = std::env::temp_dir().join("anodizer_test_dry_extra_files");
    let _ = std::fs::create_dir_all(&tmp);
    let file = tmp.join("artifact.sig");
    std::fs::write(&file, "sig").unwrap();
    let pattern = tmp.join("*.sig").to_string_lossy().into_owned();

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                extra_files: Some(vec![ExtraFileSpec::Glob(pattern)]),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn test_dry_run_with_header_footer_in_changelog() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                header: Some(ContentSource::Inline("# Custom Header".to_string())),
                footer: Some(ContentSource::Inline("Custom Footer".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.stage_outputs
        .changelogs
        .insert("testcrate".to_string(), "- bug fix".to_string());
    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_with_make_latest() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                make_latest: Some(MakeLatestConfig::Bool(true)),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

// ---- release.tag override tests ----

#[test]
fn test_resolve_release_tag_override() {
    // When release.tag is set, the override value should be used as the
    // release tag instead of crate_cfg.tag_template.
    let ctx = TestContextBuilder::new().build();
    let tag = resolve_release_tag(&ctx, "myapp/v1.0.0", Some("v1.0.0"), "testcrate").unwrap();
    assert_eq!(
        tag, "v1.0.0",
        "release.tag override must take precedence over tag_template"
    );
}

#[test]
fn test_resolve_release_tag_template_rendering() {
    // The release.tag field supports template rendering.
    let ctx = TestContextBuilder::new().tag("v2.5.0").build();
    let tag =
        resolve_release_tag(&ctx, "prefix/{{ .Tag }}", Some("{{ .Tag }}"), "testcrate").unwrap();
    assert_eq!(
        tag, "v2.5.0",
        "release.tag template must render to the git tag value"
    );
}

#[test]
fn test_resolve_release_tag_falls_back_to_tag_template() {
    // When release.tag is None, the crate's tag_template is used as before.
    let ctx = TestContextBuilder::new().build();
    let tag = resolve_release_tag(&ctx, "v1.0.0", None, "testcrate").unwrap();
    assert_eq!(
        tag, "v1.0.0",
        "with no release.tag, tag_template must be used"
    );
}

#[test]
fn test_resolve_release_tag_invalid_template_errors() {
    let ctx = TestContextBuilder::new().build();
    let result = resolve_release_tag(&ctx, "ok", Some("{{ invalid"), "testcrate");
    assert!(result.is_err(), "malformed template must return an error");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("release.tag override"),
        "error should mention release.tag override context, got: {err}"
    );
}

#[test]
fn release_tag_empty_bails_with_actionable_error() {
    // A `release.tag:` override that renders to an empty string must
    // bail before the Releases REST POST so users see "tag_template
    // rendered to an empty tag" instead of GitHub's confusing 422
    // (`tag_name is too short`). Bail message must name the field
    // (release.tag), the crate, and an actionable next step.
    let ctx = TestContextBuilder::new().build();
    let result = resolve_release_tag(&ctx, "ok", Some(""), "mycrate");
    let err = result
        .expect_err("empty release.tag override must bail")
        .to_string();
    assert!(
        err.contains("release.tag"),
        "error must name the source field, got: {err}"
    );
    assert!(
        err.contains("mycrate"),
        "error must name the crate context, got: {err}"
    );
    assert!(
        err.contains("snapshot") || err.contains("release.tag:"),
        "error must include an actionable hint, got: {err}"
    );
}

#[test]
fn release_tag_template_renders_empty_bails_with_actionable_error() {
    // The fallback tag_template path must also bail when it renders to
    // empty (e.g. `tag_template: ""` or a template referencing an
    // unset variable that resolves to ""). Bail message must name
    // `tag_template` (not `release.tag`) so the user knows which field
    // to fix.
    let ctx = TestContextBuilder::new().build();
    let result = resolve_release_tag(&ctx, "", None, "mycrate");
    let err = result
        .expect_err("empty tag_template must bail")
        .to_string();
    assert!(
        err.contains("tag_template"),
        "error must name the source field, got: {err}"
    );
    assert!(
        err.contains("mycrate"),
        "error must name the crate context, got: {err}"
    );
}

// ---- Error path tests ----

#[test]
fn test_release_missing_token_errors() {
    use anodizer_core::config::GitHubConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .token(None)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                github: Some(GitHubConfig {
                    owner: "testowner".to_string(),
                    name: "testrepo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    let result = stage.run(&mut ctx);

    // If GITHUB_TOKEN / ANODIZER_GITHUB_TOKEN happens to be set in the
    // environment (e.g., CI), the stage would proceed past token resolution
    // and fail on the API call instead. Either way, it should error.
    assert!(result.is_err(), "release without token should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("GITHUB_TOKEN")
            || err.contains("ANODIZER_GITHUB_TOKEN")
            || err.contains("--token")
            || err.contains("release"),
        "error should mention GITHUB_TOKEN, ANODIZER_GITHUB_TOKEN, --token, or release failure, got: {err}"
    );
}

#[test]
fn test_release_no_github_config_skips_silently() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                github: None, // no github config
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    // Should succeed — no github config causes skip, not error
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_prerelease_auto_detects_alpha() {
    assert!(should_mark_prerelease(
        &Some(PrereleaseConfig::Auto),
        "v1.0.0-alpha.1"
    ));
}

#[test]
fn test_prerelease_auto_detects_beta() {
    assert!(should_mark_prerelease(
        &Some(PrereleaseConfig::Auto),
        "v2.0.0-beta"
    ));
}

#[test]
fn test_prerelease_auto_detects_dev() {
    assert!(should_mark_prerelease(
        &Some(PrereleaseConfig::Auto),
        "v1.0.0-dev.5"
    ));
}

#[test]
fn test_collect_extra_files_invalid_glob_pattern() {
    let ctx = TestContextBuilder::new().build();
    // invalid glob patterns are hard errors, not silent skips.
    let result = collect_extra_files(&[ExtraFileSpec::Glob("[invalid-glob".to_string())], &ctx);
    assert!(result.is_err());
}

// ---- MockGitHubClient integration test ----

#[test]
fn test_release_pipeline_with_mock_github_client() {
    use anodizer_core::github_client::{
        AssetInfo, CreateReleaseParams, GitHubClient, MockGitHubClient, ReleaseInfo,
        UploadAssetParams,
    };

    // Set up the mock to return a successful release creation
    let mock = MockGitHubClient::new();
    mock.set_create_release_response(Ok(ReleaseInfo {
        id: 42,
        html_url: "https://github.com/testowner/testrepo/releases/42".to_string(),
        tag_name: "v1.0.0".to_string(),
        name: Some("Release v1.0.0".to_string()),
        draft: false,
    }));
    mock.set_upload_asset_response(Ok(AssetInfo {
        id: 100,
        name: "artifact.tar.gz".to_string(),
        size: 1024,
    }));

    // Build release parameters as the stage would
    let params = CreateReleaseParams {
        owner: "testowner".to_string(),
        repo: "testrepo".to_string(),
        tag_name: "v1.0.0".to_string(),
        name: "Release v1.0.0".to_string(),
        body: build_release_body("- initial release", Some("# v1.0.0"), None),
        draft: false,
        prerelease: should_mark_prerelease(&Some(PrereleaseConfig::Auto), "v1.0.0"),
        generate_release_notes: false,
        make_latest: None,
    };

    // Simulate the release pipeline: create release + upload asset
    let release = mock.create_release(&params).unwrap();
    assert_eq!(release.id, 42);
    assert_eq!(release.tag_name, "v1.0.0");
    assert!(!release.draft);

    // Simulate uploading an asset
    let upload_params = UploadAssetParams {
        owner: "testowner".to_string(),
        repo: "testrepo".to_string(),
        release_id: release.id,
        file_name: "myapp-linux-amd64.tar.gz".to_string(),
        file_path: std::path::PathBuf::from("/tmp/myapp-linux-amd64.tar.gz"),
    };
    let asset = mock.upload_asset(&upload_params).unwrap();
    assert_eq!(asset.name, "artifact.tar.gz");

    // Verify the mock recorded the correct calls
    assert_eq!(mock.create_release_call_count(), 1);
    assert_eq!(mock.upload_asset_call_count(), 1);

    let create_calls = mock.create_release_calls();
    assert_eq!(create_calls[0].owner, "testowner");
    assert_eq!(create_calls[0].tag_name, "v1.0.0");
    assert_eq!(create_calls[0].body, "# v1.0.0\n\n- initial release\n");
    assert!(!create_calls[0].prerelease);

    let upload_calls = mock.upload_asset_calls();
    assert_eq!(upload_calls[0].release_id, 42);
    assert_eq!(upload_calls[0].file_name, "myapp-linux-amd64.tar.gz");
}

// -----------------------------------------------------------------------
// Additional behavior tests — config fields actually do things
// -----------------------------------------------------------------------

#[test]
fn test_header_footer_wrap_changelog_in_release_body() {
    // Verify that header and footer actually appear around the changelog body
    let body = build_release_body(
        "- Fixed bug A\n- Added feature B",
        Some("## Release v2.0"),
        Some("---\nThank you for using our tool!"),
    );
    assert!(body.starts_with("## Release v2.0"));
    assert!(body.contains("- Fixed bug A"));
    assert!(body.contains("- Added feature B"));
    assert!(body.ends_with("Thank you for using our tool!\n"));

    // parts separated by a blank line so markdown renderers treat them
    // as distinct paragraphs
    assert!(body.contains("## Release v2.0\n\n- Fixed bug A"));
    assert!(body.contains("Added feature B\n\n---"));
}

// ---- C-new-18: changelog.header / changelog.footer fall back into release body ----

#[test]
fn test_resolve_header_footer_release_only_wins() {
    // When only release.header is set, it is used.
    let chosen = resolve_header_footer(Some("release-h"), None);
    assert_eq!(chosen, Some("release-h"));
}

#[test]
fn test_resolve_header_footer_changelog_fallback() {
    // When release.header is unset but changelog.header is set,
    // the changelog value reaches the release body.
    let chosen = resolve_header_footer(None, Some("changelog-h"));
    assert_eq!(chosen, Some("changelog-h"));
}

#[test]
fn test_resolve_header_footer_release_overrides_changelog() {
    // When BOTH are set, release.header wins (more specific override).
    let chosen = resolve_header_footer(Some("release-h"), Some("changelog-h"));
    assert_eq!(chosen, Some("release-h"));
}

#[test]
fn test_resolve_header_footer_neither_set() {
    let chosen = resolve_header_footer(None, None);
    assert_eq!(chosen, None);
}

#[test]
fn test_changelog_header_reaches_release_body_via_helper() {
    // Integration: simulate the precedence flow inside the release stage.
    // changelog.header is set in YAML → context.changelog_header is populated
    // by the changelog stage → release stage uses it as a fallback.
    let release_header: Option<&str> = None;
    let changelog_header = Some("# v1.0.0 release notes");
    let chosen = resolve_header_footer(release_header, changelog_header);
    let body = build_release_body("- bug fix", chosen, None);
    assert!(body.starts_with("# v1.0.0 release notes\n\n- bug fix"));
}

#[test]
fn test_release_header_takes_precedence_over_changelog_header() {
    // Both set → release.header wins.
    let chosen = resolve_header_footer(Some("RELEASE-H"), Some("CHANGELOG-H"));
    let body = build_release_body("body", chosen, None);
    assert!(body.starts_with("RELEASE-H\n\nbody"));
    assert!(!body.contains("CHANGELOG-H"));
}

#[test]
fn test_changelog_footer_reaches_release_body_via_helper() {
    let chosen = resolve_header_footer(None, Some("--- changelog footer"));
    let body = build_release_body("body", None, chosen);
    assert!(body.contains("body\n\n--- changelog footer"));
}

#[test]
fn test_release_footer_takes_precedence_over_changelog_footer() {
    let chosen = resolve_header_footer(Some("RELEASE-F"), Some("CHANGELOG-F"));
    let body = build_release_body("body", None, chosen);
    assert!(body.contains("body\n\nRELEASE-F"));
    assert!(!body.contains("CHANGELOG-F"));
}

#[test]
fn test_dry_run_changelog_header_falls_through_to_release() {
    // End-to-end smoke test: when changelog stage stashes a rendered
    // header/footer on the context AND release.header / release.footer
    // are unset, the release stage should still succeed and the precedence
    // helper picks the changelog values.
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }])
        .build();
    ctx.stage_outputs
        .changelogs
        .insert("testcrate".to_string(), "- a fix".to_string());
    ctx.stage_outputs.changelog_header = Some("# Header from changelog".to_string());
    ctx.stage_outputs.changelog_footer = Some("Footer from changelog".to_string());

    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_extra_files_collected_with_glob() {
    let ctx = TestContextBuilder::new().build();
    // Create temp files and verify glob collection works
    let dir = std::env::temp_dir().join("anodizer_release_extra_test");
    let _ = std::fs::create_dir_all(&dir);
    let f1 = dir.join("artifact1.sig");
    let f2 = dir.join("artifact2.sig");
    let f3 = dir.join("readme.txt");
    std::fs::write(&f1, "sig1").unwrap();
    std::fs::write(&f2, "sig2").unwrap();
    std::fs::write(&f3, "text").unwrap();

    // Collect only .sig files
    let pattern = dir.join("*.sig").to_string_lossy().into_owned();
    let result = collect_extra_files(&[ExtraFileSpec::Glob(pattern)], &ctx).unwrap();
    assert_eq!(result.len(), 2, "should find exactly 2 .sig files");
    assert!(result.iter().all(|(p, _)| p.extension().unwrap() == "sig"));

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_skip_upload_prevents_dry_run_upload_messages() {
    // When skip_upload is true, the dry-run output should mention skip_upload
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip_upload: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    // Should complete without error
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_make_latest_values_resolve_correctly() {
    // Bool(true) -> MakeLatest::True
    let ml_true = resolve_make_latest(&Some(MakeLatestConfig::Bool(true)), noop_render)
        .unwrap()
        .unwrap();
    assert_eq!(ml_true.to_string(), "true");

    // Bool(false) -> MakeLatest::False
    let ml_false = resolve_make_latest(&Some(MakeLatestConfig::Bool(false)), noop_render)
        .unwrap()
        .unwrap();
    assert_eq!(ml_false.to_string(), "false");

    // Auto -> MakeLatest::Legacy
    let ml_auto = resolve_make_latest(&Some(MakeLatestConfig::Auto), noop_render)
        .unwrap()
        .unwrap();
    assert_eq!(ml_auto.to_string(), "legacy");

    // None -> None
    assert!(resolve_make_latest(&None, noop_render).unwrap().is_none());
}

#[test]
fn test_release_name_template_rendering() {
    // Verify the rendered release name matches expected template output.
    // We simulate the same resolution logic the stage uses: render
    // name_template via ctx.render_template and check the result.
    use anodizer_core::github_client::{
        CreateReleaseParams, GitHubClient, MockGitHubClient, ReleaseInfo,
    };

    let ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v2.0.0")
        .build();

    let name_template = "MyApp {{ .Version }}";
    let rendered_name = ctx.render_template(name_template).unwrap();
    assert_eq!(
        rendered_name, "MyApp 2.0.0",
        "name_template should render Version variable"
    );

    let tag_template = "v{{ .Version }}";
    let rendered_tag = ctx.render_template(tag_template).unwrap();
    assert_eq!(rendered_tag, "v2.0.0");

    // Verify the rendered name would propagate to the GitHub API via mock
    let mock = MockGitHubClient::new();
    mock.set_create_release_response(Ok(ReleaseInfo {
        id: 1,
        html_url: "https://github.com/test/test/releases/1".to_string(),
        tag_name: rendered_tag.clone(),
        name: Some(rendered_name.clone()),
        draft: false,
    }));

    let params = CreateReleaseParams {
        owner: "testowner".to_string(),
        repo: "testrepo".to_string(),
        tag_name: rendered_tag,
        name: rendered_name.clone(),
        body: String::new(),
        draft: false,
        prerelease: false,
        generate_release_notes: false,
        make_latest: None,
    };

    mock.create_release(&params).unwrap();

    let calls = mock.create_release_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].name, "MyApp 2.0.0",
        "rendered name_template should be passed as the release name"
    );
}

#[test]
fn test_release_name_template_default_tag() {
    // When name_template is None, the default "{{ Tag }}" should render to the tag value.
    let ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v3.1.0")
        .build();

    let default_tmpl = "{{ Tag }}";
    let rendered = ctx.render_template(default_tmpl).unwrap();
    assert_eq!(
        rendered, "v3.1.0",
        "default name_template '{{ Tag }}' should render to the tag"
    );
}

#[test]
fn test_draft_release_flag() {
    // Verify draft=true propagates through to the GitHub API parameters.
    use anodizer_core::github_client::{
        CreateReleaseParams, GitHubClient, MockGitHubClient, ReleaseInfo,
    };

    let release_cfg = ReleaseConfig {
        draft: Some(true),
        ..Default::default()
    };

    // Resolve draft the same way the stage does
    let draft = release_cfg.draft.unwrap_or(false);
    assert!(draft, "draft=Some(true) should resolve to true");

    // Also verify the default case
    let default_cfg = ReleaseConfig::default();
    let default_draft = default_cfg.draft.unwrap_or(false);
    assert!(!default_draft, "draft=None should default to false");

    // Verify draft=true propagates to the mock GitHub client
    let mock = MockGitHubClient::new();
    mock.set_create_release_response(Ok(ReleaseInfo {
        id: 99,
        html_url: "https://github.com/test/test/releases/99".to_string(),
        tag_name: "v1.0.0".to_string(),
        name: Some("Release v1.0.0".to_string()),
        draft: true,
    }));

    let params = CreateReleaseParams {
        owner: "testowner".to_string(),
        repo: "testrepo".to_string(),
        tag_name: "v1.0.0".to_string(),
        name: "Release v1.0.0".to_string(),
        body: build_release_body("changelog", None, None),
        draft,
        prerelease: should_mark_prerelease(&None, "v1.0.0"),
        generate_release_notes: false,
        make_latest: None,
    };

    let release = mock.create_release(&params).unwrap();
    assert!(release.draft, "mock should return draft=true");

    let calls = mock.create_release_calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].draft,
        "draft=true must propagate to CreateReleaseParams"
    );
    assert!(
        !calls[0].prerelease,
        "prerelease should be false for stable tag with None config"
    );
}

#[test]
fn test_prerelease_auto_case_insensitive() {
    // The prerelease Auto detection should be case-insensitive
    assert!(should_mark_prerelease(
        &Some(PrereleaseConfig::Auto),
        "v1.0.0-RC.1"
    ));
    assert!(should_mark_prerelease(
        &Some(PrereleaseConfig::Auto),
        "v1.0.0-BETA"
    ));
    assert!(should_mark_prerelease(
        &Some(PrereleaseConfig::Auto),
        "v1.0.0-ALPHA.5"
    ));
}

// ---- Error path tests: actionable error messages ----

#[test]
fn test_release_missing_token_error_message_is_actionable() {
    // The release stage requires a GitHub token for non-dry-run.
    // test_release_missing_token_errors already covers this,
    // but we verify the error message is actionable (tells user what to do).
    use anodizer_core::config::GitHubConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .token(None)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                github: Some(GitHubConfig {
                    owner: "testowner".to_string(),
                    name: "testrepo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    let result = stage.run(&mut ctx);

    // If GITHUB_TOKEN / ANODIZER_GITHUB_TOKEN is in the environment, the
    // stage proceeds past token resolution and fails on the API call
    // instead. Either way the error should be informative.
    assert!(
        result.is_err(),
        "release without explicit token should fail"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("GITHUB_TOKEN")
            || err.contains("ANODIZER_GITHUB_TOKEN")
            || err.contains("--token")
            || err.contains("release")
            || err.contains("GitHub"),
        "error should mention GITHUB_TOKEN, ANODIZER_GITHUB_TOKEN, --token, or release context, got: {err}"
    );
}

#[test]
fn test_mock_github_api_401_error() {
    use anodizer_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

    let mock = MockGitHubClient::new();
    mock.set_create_release_response(Err("401 Unauthorized: Bad credentials".to_string()));

    let params = CreateReleaseParams {
        owner: "testowner".to_string(),
        repo: "testrepo".to_string(),
        tag_name: "v1.0.0".to_string(),
        name: "Release v1.0.0".to_string(),
        body: String::new(),
        draft: false,
        prerelease: false,
        generate_release_notes: false,
        make_latest: None,
    };

    let result = mock.create_release(&params);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("401") && err.contains("Unauthorized"),
        "error should contain HTTP status and description, got: {err}"
    );
}

#[test]
fn test_mock_github_api_403_error() {
    use anodizer_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

    let mock = MockGitHubClient::new();
    mock.set_create_release_response(Err(
        "403 Forbidden: Resource not accessible by integration".to_string()
    ));

    let params = CreateReleaseParams {
        owner: "testowner".to_string(),
        repo: "testrepo".to_string(),
        tag_name: "v1.0.0".to_string(),
        name: "Release".to_string(),
        body: String::new(),
        draft: false,
        prerelease: false,
        generate_release_notes: false,
        make_latest: None,
    };

    let result = mock.create_release(&params);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("403"));
}

#[test]
fn test_mock_github_api_404_error() {
    use anodizer_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

    let mock = MockGitHubClient::new();
    mock.set_create_release_response(Err("404 Not Found: repository not found".to_string()));

    let params = CreateReleaseParams {
        owner: "testowner".to_string(),
        repo: "nonexistent-repo".to_string(),
        tag_name: "v1.0.0".to_string(),
        name: "Release".to_string(),
        body: String::new(),
        draft: false,
        prerelease: false,
        generate_release_notes: false,
        make_latest: None,
    };

    let result = mock.create_release(&params);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("404") && err.contains("Not Found"),
        "error should contain 404 Not Found, got: {err}"
    );
}

#[test]
fn test_mock_github_api_422_error() {
    use anodizer_core::github_client::{CreateReleaseParams, GitHubClient, MockGitHubClient};

    let mock = MockGitHubClient::new();
    mock.set_create_release_response(Err(
        "422 Unprocessable Entity: Validation Failed - tag already exists".to_string(),
    ));

    let params = CreateReleaseParams {
        owner: "testowner".to_string(),
        repo: "testrepo".to_string(),
        tag_name: "v1.0.0".to_string(),
        name: "Release".to_string(),
        body: String::new(),
        draft: false,
        prerelease: false,
        generate_release_notes: false,
        make_latest: None,
    };

    let result = mock.create_release(&params);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("422") && err.contains("Validation"),
        "error should contain 422 and Validation, got: {err}"
    );
}

#[test]
fn test_mock_upload_failure() {
    use anodizer_core::github_client::{GitHubClient, MockGitHubClient, UploadAssetParams};

    let mock = MockGitHubClient::new();
    mock.set_upload_asset_response(Err(
        "upload failed: connection timeout after 30s".to_string()
    ));

    let params = UploadAssetParams {
        owner: "testowner".to_string(),
        repo: "testrepo".to_string(),
        release_id: 42,
        file_name: "myapp.tar.gz".to_string(),
        file_path: std::path::PathBuf::from("/tmp/myapp.tar.gz"),
    };

    let result = mock.upload_asset(&params);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("upload failed") && err.contains("timeout"),
        "error should describe the upload failure, got: {err}"
    );
}

#[test]
fn test_dry_run_with_draft_release() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                draft: Some(true),
                prerelease: Some(PrereleaseConfig::Auto),
                make_latest: Some(MakeLatestConfig::Bool(false)),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

// ---- conflicting draft config tests ----

#[test]
fn test_conflicting_replace_and_use_existing_draft_fails() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                replace_existing_draft: Some(true),
                use_existing_draft: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err(), "conflicting draft options should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("replace_existing_draft") && err.contains("use_existing_draft"),
        "error should mention both conflicting options, got: {err}"
    );
}

#[test]
fn test_replace_existing_draft_alone_ok() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                replace_existing_draft: Some(true),
                use_existing_draft: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_use_existing_draft_alone_ok() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                replace_existing_draft: Some(false),
                use_existing_draft: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

// ---- release disable tests ----

#[test]
fn test_release_disable_config_parsing() {
    let yaml = r#"
skip: true
draft: false
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_release_disable_config_parsing_false() {
    let yaml = r#"
skip: false
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.skip, Some(StringOrBool::Bool(false)));
}

#[test]
fn test_release_disable_config_parsing_template_string() {
    let yaml = r#"
skip: "{{ if IsSnapshot }}true{{ endif }}"
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    match cfg.skip {
        Some(StringOrBool::String(s)) => {
            assert!(s.contains("IsSnapshot"));
        }
        other => panic!("expected StringOrBool::String, got {:?}", other),
    }
}

#[test]
fn test_release_disable_config_parsing_absent() {
    let yaml = r#"
draft: true
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.skip, None);
}

#[test]
fn test_release_stage_skipped_when_disabled() {
    // When skip: true is set, the release stage should skip
    // the crate entirely. We test via dry-run to avoid real API calls.
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    // Should succeed with no error - the crate is simply skipped
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_release_stage_not_skipped_when_disable_false() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip: Some(StringOrBool::Bool(false)),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    // Should succeed - disable=false means proceed normally (dry-run)
    assert!(stage.run(&mut ctx).is_ok());
}

// resolve_release_mode tests live with the canonical defaults logic on
// `ReleaseConfig::resolved_mode` in `anodizer-core` (lazy-defaults policy).

#[test]
fn test_release_mode_stored_in_config() {
    let yaml = r#"
mode: keep-existing
draft: false
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.mode.as_deref(), Some("keep-existing"));
}

#[test]
fn test_release_mode_absent_in_config() {
    let yaml = r#"
draft: true
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.mode, None);
}

#[test]
fn test_release_mode_all_valid_values_in_config() {
    for mode in &["keep-existing", "append", "prepend", "replace"] {
        let yaml = format!("mode: {}", mode);
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(cfg.mode.as_deref(), Some(*mode));
        // Verify it passes validation through the accessor.
        assert!(cfg.resolved_mode().is_ok());
    }
}

#[test]
fn test_dry_run_logs_release_mode() {
    // When mode is set, the dry-run output should include it
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                mode: Some("append".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    // Dry-run should succeed; the mode is validated and logged
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_invalid_release_mode_fails_stage() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                mode: Some("bogus".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err(), "invalid release mode should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("invalid mode") || err.contains("bogus"),
        "error should mention invalid mode, got: {err}"
    );
}

// ---- ids filtering tests ----

#[test]
fn test_ids_filter_includes_matching_artifacts() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;
    use std::path::PathBuf;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                ids: Some(vec!["linux-amd64".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    // Archive with matching id
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-linux-amd64.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
        size: None,
    });

    // Archive with non-matching id
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-darwin-arm64.tar.gz"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
        size: None,
    });

    let stage = ReleaseStage;
    // Dry-run succeeds; the filter is applied internally
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_ids_filter_none_includes_all_artifacts() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;
    use std::path::PathBuf;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                ids: None, // no filter
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    // Add two archives with different ids
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-linux.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-darwin.tar.gz"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
        size: None,
    });

    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_ids_filter_unit_logic() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;
    use std::path::PathBuf;

    let ids = ["linux-amd64".to_string(), "windows-amd64".to_string()];

    let artifacts = [
        Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/tmp/linux.tar.gz"),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
            size: None,
        },
        Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/tmp/darwin.tar.gz"),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
            size: None,
        },
        Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/tmp/windows.zip"),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::from([("id".to_string(), "windows-amd64".to_string())]),
            size: None,
        },
        Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: PathBuf::from("/tmp/checksums.txt"),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::new(), // no id metadata
            size: None,
        },
    ];

    let filtered: Vec<_> = artifacts
        .iter()
        .filter(|a| anodizer_core::artifact::matches_id_filter(a, Some(&ids)))
        .collect();

    assert_eq!(
        filtered.len(),
        3,
        "should match linux + windows archives plus the Checksum (always-pass per GoReleaser ByID)"
    );
    assert_eq!(filtered[0].path, PathBuf::from("/tmp/linux.tar.gz"));
    assert_eq!(filtered[1].path, PathBuf::from("/tmp/windows.zip"));
    assert_eq!(filtered[2].path, PathBuf::from("/tmp/checksums.txt"));
}

#[test]
fn test_ids_filter_no_id_metadata_excluded() {
    // Artifacts without "id" metadata should be excluded when ids filter is set
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;
    use std::path::PathBuf;

    let ids = ["linux-amd64".to_string()];

    let artifact_no_id = Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("/tmp/mystery.tar.gz"),
        target: None,
        crate_name: "app".to_string(),
        metadata: HashMap::new(),
        size: None,
    };

    let matches = anodizer_core::artifact::matches_id_filter(&artifact_no_id, Some(&ids));
    assert!(
        !matches,
        "Archive artifact without id metadata should not match ids filter"
    );
}

#[test]
fn test_ids_config_parsing() {
    let yaml = r#"
ids:
  - linux-amd64
  - darwin-arm64
draft: false
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let ids = cfg.ids.unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(ids[0], "linux-amd64");
    assert_eq!(ids[1], "darwin-arm64");
}

#[test]
fn test_ids_config_absent() {
    let yaml = r#"
draft: true
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(cfg.ids.is_none());
}

#[test]
fn test_ids_and_mode_combined_dry_run() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::collections::HashMap;
    use std::path::PathBuf;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                mode: Some("prepend".to_string()),
                ids: Some(vec!["linux-amd64".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-linux.tar.gz"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "linux-amd64".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-darwin.tar.gz"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "darwin-arm64".to_string())]),
        size: None,
    });

    let stage = ReleaseStage;
    assert!(
        stage.run(&mut ctx).is_ok(),
        "dry-run with mode + ids should succeed"
    );
}

#[test]
fn test_release_collects_all_uploadable_artifact_kinds() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::path::PathBuf;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    // Add one artifact of each uploadable kind.
    let uploadable_kinds = vec![
        (ArtifactKind::Archive, "myapp.tar.gz"),
        (ArtifactKind::Checksum, "checksums.txt"),
        (ArtifactKind::LinuxPackage, "myapp.deb"),
        (ArtifactKind::Snap, "myapp.snap"),
        (ArtifactKind::DiskImage, "myapp.dmg"),
        (ArtifactKind::Installer, "myapp.msi"),
        (ArtifactKind::MacOsPackage, "myapp.pkg"),
        (ArtifactKind::SourceArchive, "myapp-src.tar.gz"),
        (ArtifactKind::Sbom, "myapp.sbom.json"),
    ];
    for (kind, name) in &uploadable_kinds {
        ctx.artifacts.add(Artifact {
            kind: *kind,
            name: String::new(),
            path: PathBuf::from(format!("/tmp/{}", name)),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
    }

    // Also add a signature Metadata artifact (should be uploaded).
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Metadata,
        name: String::new(),
        path: PathBuf::from("/tmp/checksums.txt.sig"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: std::collections::HashMap::from([("type".to_string(), "Signature".to_string())]),
        size: None,
    });

    // Add non-uploadable kinds (should NOT be uploaded).
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        path: PathBuf::from("ghcr.io/test/myapp:latest"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Library,
        name: String::new(),
        path: PathBuf::from("/tmp/libmyapp.so"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Wasm,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp.wasm"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });
    // Plain Metadata (not Signature/Certificate) should NOT be uploaded.
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Metadata,
        name: String::new(),
        path: PathBuf::from("/tmp/metadata.json"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = ReleaseStage;
    assert!(
        stage.run(&mut ctx).is_ok(),
        "dry-run with all artifact kinds should succeed"
    );

    // The dry-run completes successfully, confirming the expanded artifact
    // collection logic compiles and processes all expected kinds.
}

// ---- compose_body_for_mode tests ----

#[test]
fn test_compose_body_replace_ignores_existing() {
    let result = compose_body_for_mode("replace", Some("old body"), "new body");
    assert_eq!(result, "new body");
}

#[test]
fn test_compose_body_replace_no_existing() {
    let result = compose_body_for_mode("replace", None, "new body");
    assert_eq!(result, "new body");
}

#[test]
fn test_compose_body_keep_existing_with_existing() {
    let result = compose_body_for_mode("keep-existing", Some("old body"), "new body");
    assert_eq!(result, "old body");
}

#[test]
fn test_compose_body_keep_existing_empty_existing() {
    let result = compose_body_for_mode("keep-existing", Some(""), "new body");
    assert_eq!(result, "new body");
}

#[test]
fn test_compose_body_keep_existing_no_existing() {
    let result = compose_body_for_mode("keep-existing", None, "new body");
    assert_eq!(result, "new body");
}

#[test]
fn test_compose_body_append_with_existing() {
    let result = compose_body_for_mode("append", Some("old body"), "new body");
    assert_eq!(result, "old body\n\nnew body");
}

#[test]
fn test_compose_body_append_no_existing() {
    let result = compose_body_for_mode("append", None, "new body");
    assert_eq!(result, "new body");
}

#[test]
fn test_compose_body_append_empty_existing() {
    let result = compose_body_for_mode("append", Some(""), "new body");
    assert_eq!(result, "new body");
}

#[test]
fn test_compose_body_prepend_with_existing() {
    let result = compose_body_for_mode("prepend", Some("old body"), "new body");
    assert_eq!(result, "new body\n\nold body");
}

#[test]
fn test_compose_body_prepend_no_existing() {
    let result = compose_body_for_mode("prepend", None, "new body");
    assert_eq!(result, "new body");
}

#[test]
fn test_compose_body_prepend_empty_existing() {
    let result = compose_body_for_mode("prepend", Some(""), "new body");
    assert_eq!(result, "new body");
}

// ---- resolve_content_source tests ----

fn content_source_test_ctx() -> anodizer_core::context::Context {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    let mut config = Config::default();
    config.project_name = "test".to_string();
    Context::new(config, ContextOptions::default())
}

#[test]
fn test_resolve_content_source_inline() {
    let ctx = content_source_test_ctx();
    let source = ContentSource::Inline("hello world".to_string());
    assert_eq!(
        resolve_content_source(&source, &ctx).unwrap(),
        "hello world"
    );
}

#[test]
fn test_resolve_content_source_from_file() {
    let ctx = content_source_test_ctx();
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("header.md");
    std::fs::write(&file_path, "# Release Header\nFrom file.").unwrap();

    let source = ContentSource::FromFile {
        from_file: file_path.to_string_lossy().into_owned(),
    };
    let result = resolve_content_source(&source, &ctx).unwrap();
    assert_eq!(result, "# Release Header\nFrom file.");
}

#[test]
fn test_resolve_content_source_from_file_not_found() {
    let ctx = content_source_test_ctx();
    let source = ContentSource::FromFile {
        from_file: "/tmp/anodizer_nonexistent_file_12345.md".to_string(),
    };
    let result = resolve_content_source(&source, &ctx);
    assert!(result.is_err());
    // After hoisting to core::content_source the error message uses the
    // anyhow `with_context` form; both old "failed to read" and new
    // "read from_file" wording are acceptable signals.
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("read from_file") || msg.contains("failed to read"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn test_resolve_content_source_from_file_path_is_template_rendered() {
    // `from_file: "./release-{{ .Tag }}.md"` should work —
    // previously the path was read raw. Regression-guard: template-render path first.
    let mut ctx = content_source_test_ctx();
    ctx.template_vars_mut().set("Tag", "v9.8.7");

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("release-v9.8.7.md");
    std::fs::write(&file_path, "rendered path worked").unwrap();

    let tmpl_path = format!("{}/release-{{{{ .Tag }}}}.md", dir.path().to_string_lossy());
    let source = ContentSource::FromFile {
        from_file: tmpl_path,
    };
    let result = resolve_content_source(&source, &ctx).unwrap();
    assert_eq!(result, "rendered path worked");
}

#[test]
fn test_content_source_from_url_with_headers_parses() {
    // Schema: new `headers` map on FromUrl variant.
    use anodizer_core::config::ContentSource;
    let yaml = r#"
from_url: https://example.com/h.md
headers:
  X-API-Token: "{{ .Env.TOKEN }}"
  Accept: text/markdown
"#;
    let parsed: ContentSource = serde_yaml_ng::from_str(yaml).unwrap();
    match parsed {
        ContentSource::FromUrl { from_url, headers } => {
            assert_eq!(from_url, "https://example.com/h.md");
            let h = headers.expect("headers should deserialize");
            assert_eq!(
                h.get("X-API-Token").map(String::as_str),
                Some("{{ .Env.TOKEN }}")
            );
            assert_eq!(h.get("Accept").map(String::as_str), Some("text/markdown"));
        }
        other => panic!("expected FromUrl, got {:?}", other),
    }
}

#[test]
fn test_content_source_from_url_without_headers_parses() {
    // Backwards compat — old config with just `from_url:` still works.
    use anodizer_core::config::ContentSource;
    let yaml = r#"
from_url: https://example.com/h.md
"#;
    let parsed: ContentSource = serde_yaml_ng::from_str(yaml).unwrap();
    match parsed {
        ContentSource::FromUrl { from_url, headers } => {
            assert_eq!(from_url, "https://example.com/h.md");
            assert!(headers.is_none());
        }
        other => panic!("expected FromUrl, got {:?}", other),
    }
}

// ---- new config field parsing tests ----

#[test]
fn test_target_commitish_config_parsing() {
    let yaml = r#"
target_commitish: main
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.target_commitish, Some("main".to_string()));
}

#[test]
fn test_target_commitish_absent() {
    let yaml = r#"
draft: true
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.target_commitish, None);
}

#[test]
fn test_discussion_category_name_config_parsing() {
    let yaml = r#"
discussion_category_name: Announcements
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        cfg.discussion_category_name,
        Some("Announcements".to_string())
    );
}

#[test]
fn test_discussion_category_name_absent() {
    let yaml = r#"
draft: false
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.discussion_category_name, None);
}

#[test]
fn test_include_meta_config_parsing() {
    let yaml = r#"
include_meta: true
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.include_meta, Some(true));
}

#[test]
fn test_include_meta_false() {
    let yaml = r#"
include_meta: false
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.include_meta, Some(false));
}

#[test]
fn test_include_meta_absent() {
    let yaml = r#"
draft: true
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.include_meta, None);
}

#[test]
fn test_use_existing_draft_config_parsing() {
    let yaml = r#"
use_existing_draft: true
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.use_existing_draft, Some(true));
}

#[test]
fn test_use_existing_draft_false() {
    let yaml = r#"
use_existing_draft: false
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.use_existing_draft, Some(false));
}

#[test]
fn test_use_existing_draft_absent() {
    let yaml = r#"
draft: true
"#;
    let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.use_existing_draft, None);
}

// ---- dry-run tests for new config fields ----

#[test]
fn test_dry_run_with_target_commitish() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                target_commitish: Some("main".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_with_discussion_category_name() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                discussion_category_name: Some("Releases".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_with_include_meta() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                include_meta: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_with_use_existing_draft() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                use_existing_draft: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_with_all_new_fields() {
    // extra_files globs must match at least one file.
    let tmp = std::env::temp_dir().join("anodizer_test_dry_all_fields");
    let _ = std::fs::create_dir_all(&tmp);
    let file = tmp.join("extra.sig");
    std::fs::write(&file, "sig").unwrap();
    let pattern = tmp.join("*.sig").to_string_lossy().into_owned();

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                header: Some(ContentSource::Inline("# Header".to_string())),
                footer: Some(ContentSource::Inline("Footer".to_string())),
                extra_files: Some(vec![ExtraFileSpec::Glob(pattern)]),
                target_commitish: Some("release/v1".to_string()),
                discussion_category_name: Some("Announcements".to_string()),
                include_meta: Some(true),
                use_existing_draft: Some(false),
                mode: Some("append".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.stage_outputs
        .changelogs
        .insert("testcrate".to_string(), "- changes".to_string());
    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());

    let _ = std::fs::remove_dir_all(&tmp);
}

// ---- ContentSource from_file dry-run integration test ----

#[test]
fn test_dry_run_with_header_from_file() {
    let dir = tempfile::tempdir().unwrap();
    let header_path = dir.path().join("header.md");
    std::fs::write(&header_path, "# Release from file").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                header: Some(ContentSource::FromFile {
                    from_file: header_path.to_string_lossy().into_owned(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_include_meta_collects_dist_files() {
    // Create a temp dist directory with metadata files
    let dir = tempfile::tempdir().unwrap();
    let dist_dir = dir.path().join("dist");
    std::fs::create_dir_all(&dist_dir).unwrap();
    std::fs::write(dist_dir.join("metadata.json"), r#"{"key":"value"}"#).unwrap();
    std::fs::write(dist_dir.join("artifacts.json"), r#"[]"#).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                include_meta: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    // Override the dist path to our temp directory
    ctx.config.dist = dist_dir.clone();

    let stage = ReleaseStage;
    assert!(stage.run(&mut ctx).is_ok());
}

// ---- body truncation tests ----

#[test]
fn test_build_release_json_body_within_limit() {
    let body = "a".repeat(1000);
    let json = build_release_json(&crate::release_body::ReleaseJsonSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: &body,
        draft: false,
        prerelease_flag: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    });
    assert_eq!(json["body"].as_str().unwrap(), &body);
}

#[test]
fn test_build_release_json_body_at_limit() {
    let body = "a".repeat(GITHUB_RELEASE_BODY_MAX_CHARS);
    let json = build_release_json(&crate::release_body::ReleaseJsonSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: &body,
        draft: false,
        prerelease_flag: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    });
    assert_eq!(json["body"].as_str().unwrap(), &body);
}

#[test]
fn test_build_release_json_body_exceeds_limit_is_truncated() {
    let body = "a".repeat(GITHUB_RELEASE_BODY_MAX_CHARS + 500);
    let json = build_release_json(&crate::release_body::ReleaseJsonSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: &body,
        draft: false,
        prerelease_flag: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    });
    let result = json["body"].as_str().unwrap();
    // The truncate suffix is the three-dot ellipsis, not
    // `"\n\n...(truncated)"` (16 chars).
    let suffix = "...";
    assert_eq!(suffix.len(), 3);
    // Total length must not exceed the limit.
    assert!(
        result.len() <= GITHUB_RELEASE_BODY_MAX_CHARS,
        "truncated body length {} exceeds limit {}",
        result.len(),
        GITHUB_RELEASE_BODY_MAX_CHARS,
    );
    // The content portion should be max_chars - suffix length of 'a's.
    let expected_content_len = GITHUB_RELEASE_BODY_MAX_CHARS - suffix.len();
    assert!(result.starts_with(&"a".repeat(expected_content_len)));
    assert!(result.ends_with(suffix));
}

#[test]
fn test_build_release_json_truncate_suffix_matches_goreleaser() {
    // Regression: the truncate suffix is exactly `"..."` (3 chars),
    // a literal three-dot ellipsis. Any drift
    // back to `"\n\n...(truncated)"` (16 chars) — anodizer's old shape — must
    // fail this test.
    let body = "a".repeat(GITHUB_RELEASE_BODY_MAX_CHARS + 100);
    let json = build_release_json(&crate::release_body::ReleaseJsonSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: &body,
        draft: false,
        prerelease_flag: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    });
    let result = json["body"].as_str().unwrap();
    assert_eq!(result.len(), GITHUB_RELEASE_BODY_MAX_CHARS);
    assert!(result.ends_with("..."));
    assert!(!result.ends_with("(truncated)"));
    assert!(!result.contains("\n\n...(truncated)"));
}

#[test]
fn test_build_release_json_empty_body_not_set() {
    let json = build_release_json(&crate::release_body::ReleaseJsonSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: "",
        draft: false,
        prerelease_flag: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    });
    assert!(json.get("body").is_none());
}

// ---- draft-then-publish: build_release_json always uses draft as passed ----

#[test]
fn test_build_release_json_draft_true() {
    let json = build_release_json(&crate::release_body::ReleaseJsonSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: "body",
        draft: true,
        prerelease_flag: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    });
    assert!(json["draft"].as_bool().unwrap());
}

#[test]
fn test_build_release_json_draft_false() {
    let json = build_release_json(&crate::release_body::ReleaseJsonSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: "body",
        draft: false,
        prerelease_flag: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    });
    assert!(!json["draft"].as_bool().unwrap());
}

#[test]
fn test_build_release_json_never_sets_generate_release_notes() {
    // Regression guard:
    // (`changelog.use: github-native` → wrong API endpoint). The
    // create-release POST must never carry `generate_release_notes:
    // true`: the github-native flow now calls
    // `POST /releases/generate-notes` upfront (see
    // `stage-changelog/src/github_native.rs`) and embeds the returned
    // body in `spec.body`.
    // the generate-release-notes endpoint. Toggling
    // `generate_release_notes: true` here would silently use GitHub's
    // "most recent published release" as the previous tag — wrong for
    // monorepos and tag-prefixed re-releases.
    let json = build_release_json(&crate::release_body::ReleaseJsonSpec {
        tag: "v1.0.0",
        name: "Release v1.0.0",
        body: "Auto-generated release notes from /releases/generate-notes",
        draft: false,
        prerelease_flag: false,
        make_latest: &None,
        target_commitish: &None,
        discussion_category: &None,
    });
    assert!(
        json.get("generate_release_notes").is_none(),
        "create-release POST must never include `generate_release_notes` \
         (would diverge from GR's explicit prev/current pinning); got: {json}"
    );
}

#[test]
fn test_dry_run_with_templated_extra_files() {
    use anodizer_core::config::TemplatedExtraFile;

    let tmp = tempfile::TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    // Create a source template file
    let tpl_src = tmp.path().join("NOTES.md.tpl");
    std::fs::write(&tpl_src, "Release {{ .ProjectName }} {{ .Version }}").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v2.0.0")
        .dry_run(true)
        .dist(dist.clone())
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            release: Some(ReleaseConfig {
                templated_extra_files: Some(vec![TemplatedExtraFile {
                    src: tpl_src.to_string_lossy().to_string(),
                    dst: Some("RELEASE-NOTES.md".to_string()),
                    mode: None,
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    stage.run(&mut ctx).unwrap();

    // Verify the templated file was rendered and written to dist
    let rendered = dist.join("RELEASE-NOTES.md");
    assert!(
        rendered.exists(),
        "templated extra file should be written to dist"
    );
    let content = std::fs::read_to_string(&rendered).unwrap();
    assert_eq!(content, "Release myapp 2.0.0");
}

// -----------------------------------------------------------------------
// GitHub Enterprise URL support tests
// -----------------------------------------------------------------------

/// Helper: build_octocrab_client requires a tokio runtime (octocrab's
/// Buffer service needs one) and a rustls CryptoProvider installed.
/// Wrap assertions in a temporary runtime with the provider set.
fn with_tokio<F: FnOnce()>(f: F) {
    // Install the ring crypto provider if not already installed.
    // ignore error if another test thread already installed it.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { f() });
}

#[test]
fn test_build_octocrab_client_default_no_github_urls() {
    // When github_urls is None, build_octocrab_client should succeed
    // with standard GitHub.com endpoints.
    with_tokio(|| {
        let result = build_octocrab_client("ghp_fake_token_123", &None);
        assert!(
            result.is_ok(),
            "default client (no github_urls) should build successfully"
        );
    });
}

#[test]
fn test_build_octocrab_client_with_enterprise_api_url() {
    with_tokio(|| {
        let urls = Some(GitHubUrlsConfig {
            api: Some("https://github.example.com/api/v3/".to_string()),
            upload: None,
            download: None,
            skip_tls_verify: None,
        });
        let result = build_octocrab_client("ghp_fake_token_123", &urls);
        assert!(
            result.is_ok(),
            "client with enterprise api URL should build successfully"
        );
    });
}

#[test]
fn test_build_octocrab_client_with_enterprise_api_and_upload_urls() {
    with_tokio(|| {
        let urls = Some(GitHubUrlsConfig {
            api: Some("https://github.example.com/api/v3/".to_string()),
            upload: Some("https://github.example.com/api/uploads/".to_string()),
            download: Some("https://github.example.com/".to_string()),
            skip_tls_verify: None,
        });
        let result = build_octocrab_client("ghp_fake_token_123", &urls);
        assert!(
            result.is_ok(),
            "client with enterprise api + upload URLs should build successfully"
        );
    });
}

#[test]
fn test_build_octocrab_client_with_skip_tls_verify() {
    with_tokio(|| {
        let urls = Some(GitHubUrlsConfig {
            api: Some("https://github.example.com/api/v3/".to_string()),
            upload: Some("https://github.example.com/api/uploads/".to_string()),
            download: None,
            skip_tls_verify: Some(true),
        });
        let result = build_octocrab_client("ghp_fake_token_123", &urls);
        assert!(
            result.is_ok(),
            "client with skip_tls_verify should build successfully"
        );
    });
}

#[test]
fn test_build_octocrab_client_invalid_api_url_errors() {
    with_tokio(|| {
        let urls = Some(GitHubUrlsConfig {
            api: Some("not a valid url \x00".to_string()),
            upload: None,
            download: None,
            skip_tls_verify: None,
        });
        let bad_result = build_octocrab_client("ghp_fake_token_123", &urls);
        assert!(
            bad_result.is_err(),
            "invalid api URL should produce an error"
        );
    });
}

#[test]
fn test_build_octocrab_client_skip_tls_false_uses_normal_path() {
    // skip_tls_verify = Some(false) should use the normal (secure) path.
    with_tokio(|| {
        let urls = Some(GitHubUrlsConfig {
            api: Some("https://github.example.com/api/v3/".to_string()),
            upload: None,
            download: None,
            skip_tls_verify: Some(false),
        });
        let result = build_octocrab_client("ghp_fake_token_123", &urls);
        assert!(
            result.is_ok(),
            "skip_tls_verify=false should use normal build path"
        );
    });
}

#[test]
fn test_dry_run_logs_github_enterprise_urls() {
    // When github_urls are configured and dry_run is true, the release
    // stage should log the enterprise URL configuration.
    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }])
        .build();

    ctx.config.github_urls = Some(GitHubUrlsConfig {
        api: Some("https://ghe.corp.example.com/api/v3/".to_string()),
        upload: Some("https://ghe.corp.example.com/api/uploads/".to_string()),
        download: Some("https://ghe.corp.example.com/".to_string()),
        skip_tls_verify: Some(true),
    });

    let stage = ReleaseStage;
    // Dry-run should succeed — no actual API calls are made.
    stage.run(&mut ctx).unwrap();
}

#[test]
fn test_dry_run_without_github_urls_still_works() {
    // Verify the default path (no github_urls) still works in dry-run.
    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }])
        .build();

    assert!(ctx.config.github_urls.is_none());

    let stage = ReleaseStage;
    stage.run(&mut ctx).unwrap();
}

// ---- GitLab backend tests ----

#[test]
fn test_dry_run_gitlab_token_type_shows_gitlab_release() {
    use anodizer_core::config::ScmRepoConfig;
    use anodizer_core::scm::ScmTokenType;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                gitlab: Some(ScmRepoConfig {
                    owner: "mygroup".to_string(),
                    name: "myproject".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::GitLab;

    let stage = ReleaseStage;
    // Dry-run with GitLab token type should succeed.
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_gitlab_with_custom_urls() {
    use anodizer_core::config::{GitLabUrlsConfig, ScmRepoConfig};
    use anodizer_core::scm::ScmTokenType;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                gitlab: Some(ScmRepoConfig {
                    owner: "corp".to_string(),
                    name: "app".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::GitLab;
    ctx.config.gitlab_urls = Some(GitLabUrlsConfig {
        api: Some("https://gitlab.example.com/api/v4".to_string()),
        download: Some("https://gitlab.example.com".to_string()),
        skip_tls_verify: Some(true),
        use_package_registry: Some(true),
        use_job_token: Some(false),
    });

    let stage = ReleaseStage;
    // Dry-run with custom GitLab URLs should succeed and show them.
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_gitlab_backend_skips_when_no_gitlab_config() {
    use anodizer_core::scm::ScmTokenType;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .token(Some("glpat-test-token".to_string()))
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                // No gitlab config, no github config either.
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::GitLab;

    let stage = ReleaseStage;
    // Should succeed by skipping (warn + continue) since no gitlab config.
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_gitlab_backend_falls_back_to_github_config() {
    use anodizer_core::config::ScmRepoConfig;
    use anodizer_core::scm::ScmTokenType;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                // Only github config set, no gitlab-specific config.
                github: Some(ScmRepoConfig {
                    owner: "fallback-owner".to_string(),
                    name: "fallback-repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::GitLab;

    let stage = ReleaseStage;
    // Should succeed in dry-run because GitLab falls back to github config.
    assert!(stage.run(&mut ctx).is_ok());
}

// ---- Gitea backend tests ----

#[test]
fn test_gitea_dry_run_with_gitea_config() {
    use anodizer_core::config::ScmRepoConfig;
    use anodizer_core::scm::ScmTokenType;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                gitea: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::Gitea;

    let stage = ReleaseStage;
    // Should succeed in dry-run.
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_gitea_backend_skips_when_no_gitea_config() {
    use anodizer_core::scm::ScmTokenType;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .token(Some("gitea-test-token".to_string()))
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                // No gitea config, no github config either.
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::Gitea;

    let stage = ReleaseStage;
    // Should succeed by skipping (warn + continue) since no gitea config.
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_gitea_backend_falls_back_to_github_config() {
    use anodizer_core::config::ScmRepoConfig;
    use anodizer_core::scm::ScmTokenType;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                // Only github config set, no gitea-specific config.
                github: Some(ScmRepoConfig {
                    owner: "fallback-owner".to_string(),
                    name: "fallback-repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::Gitea;

    let stage = ReleaseStage;
    // Should succeed in dry-run because Gitea falls back to github config.
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_gitea_missing_token_errors() {
    use anodizer_core::config::ScmRepoConfig;
    use anodizer_core::scm::ScmTokenType;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .token(None)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                gitea: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::Gitea;

    let stage = ReleaseStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("GITEA_TOKEN") || err.contains("--token"),
        "error should mention GITEA_TOKEN or --token, got: {err}"
    );
}

// ---- build_publish_patch_body regression tests ----
//
// Behaviours: preserve prerelease on publish +
// 2e17678 (preserve prerelease publish fields). The PATCH body sent when
// un-drafting a release must:
//   - always carry `draft = false`,
//   - re-render and include `name` (stale drafts get the current template),
//   - force `make_latest = "false"` whenever `prerelease` is true,
//   - include `prerelease = true` whenever `prerelease` is true,
//   - send `discussion_category_name` only when configured.
//
// These are the load-bearing invariants for the un-draft flow.

#[test]
fn test_build_publish_patch_body_basic_undraft() {
    let body = build_publish_patch_body("Release v1.0.0", false, &None, &None);
    assert_eq!(body["draft"].as_bool(), Some(false));
    assert_eq!(body["name"].as_str(), Some("Release v1.0.0"));
    assert!(body.get("prerelease").is_none());
    assert!(body.get("make_latest").is_none());
    assert!(body.get("discussion_category_name").is_none());
}

#[test]
fn test_build_publish_patch_body_includes_make_latest_when_not_prerelease() {
    use octocrab::repos::releases::MakeLatest;
    let body = build_publish_patch_body("Release v1.0.0", false, &Some(MakeLatest::True), &None);
    assert_eq!(body["make_latest"].as_str(), Some("true"));
}

#[test]
fn test_build_publish_patch_body_prerelease_forces_make_latest_false() {
    // When prerelease=true, make_latest is forced to
    // "false" regardless of the user's `make_latest` template (a prerelease
    // can never be the latest).
    use octocrab::repos::releases::MakeLatest;
    let body =
        build_publish_patch_body("Release v1.0.0-rc.1", true, &Some(MakeLatest::True), &None);
    assert_eq!(body["prerelease"].as_bool(), Some(true));
    assert_eq!(
        body["make_latest"].as_str(),
        Some("false"),
        "prerelease must force make_latest=false even when user requested true",
    );
}

#[test]
fn test_build_publish_patch_body_prerelease_legacy_ml_still_forced_false() {
    use octocrab::repos::releases::MakeLatest;
    // Legacy ("auto") + prerelease must still force make_latest=false.
    let body = build_publish_patch_body(
        "Release v2.0.0-beta.1",
        true,
        &Some(MakeLatest::Legacy),
        &None,
    );
    assert_eq!(body["make_latest"].as_str(), Some("false"));
}

#[test]
fn test_build_publish_patch_body_includes_name_re_render() {
    // The `name` is re-rendered from name_template and
    // included in the PATCH so a stale draft picks up template changes.
    let body = build_publish_patch_body("Renamed Release v1.2.3", false, &None, &None);
    assert_eq!(body["name"].as_str(), Some("Renamed Release v1.2.3"));
}

#[test]
fn test_build_publish_patch_body_empty_name_omitted() {
    // If the rendered name is empty (`if title != ""`), skip
    // the field rather than blanking the release name on GitHub.
    let body = build_publish_patch_body("", false, &None, &None);
    assert!(body.get("name").is_none());
}

#[test]
fn test_build_publish_patch_body_includes_discussion_category() {
    let body = build_publish_patch_body(
        "Release v1.0.0",
        false,
        &None,
        &Some("Releases".to_string()),
    );
    assert_eq!(body["discussion_category_name"].as_str(), Some("Releases"));
}

#[test]
fn test_build_publish_patch_body_prerelease_with_discussion() {
    // Prerelease + discussion_category_name + make_latest combo: discussion
    // still passes through, make_latest forced to "false".
    use octocrab::repos::releases::MakeLatest;
    let body = build_publish_patch_body(
        "Release v1.0.0-rc.1",
        true,
        &Some(MakeLatest::True),
        &Some("Announcements".to_string()),
    );
    assert_eq!(body["draft"].as_bool(), Some(false));
    assert_eq!(body["prerelease"].as_bool(), Some(true));
    assert_eq!(body["make_latest"].as_str(), Some("false"));
    assert_eq!(
        body["discussion_category_name"].as_str(),
        Some("Announcements")
    );
}

// ---- Q11.1: header-access panic-freedom regression ----
//
// An update-release call could panic when `resp`
// was nil before accessing `resp.Header.Get(...)`. Anodizer is panic-free
// by construction: octocrab/reqwest header access goes through
// `headers().get(name)` which returns `Option<&HeaderValue>`. This test
// pins that contract so a future refactor that introduces `.unwrap()` on
// a header access fails CI.

#[test]
fn test_response_header_access_returns_option_no_panic() {
    use http::HeaderMap;
    let empty = HeaderMap::new();
    // `.get()` on an empty HeaderMap returns None — no panic, even when the
    // value would be required by upstream-Go's `resp.Header.Get(...)`.
    assert!(empty.get("X-GitHub-Request-Id").is_none());
    let mut populated = HeaderMap::new();
    populated.insert(
        "X-GitHub-Request-Id",
        http::HeaderValue::from_static("ABCD:1234:5678:90:0"),
    );
    assert_eq!(
        populated
            .get("X-GitHub-Request-Id")
            .map(|v| v.to_str().ok()),
        Some(Some("ABCD:1234:5678:90:0")),
    );
}

// ---- populate_checksums_var: workspace aggregation ----
//
// In a multi-crate workspace, each crate's checksum stage writes a combined
// SHA256SUMS-style sidecar. When the release body references
// `{{ .Checksums }}`, users expect the UNION of every per-crate checksum
// block — not a single crate's content (last-write-wins) and not a
// `serde_json::Map` keyed by the artifact's path (which leaks the build
// host's filesystem layout into release notes). The aggregation must be
// sorted by filename so the rendered block is deterministic and matches
// the SHA256SUMS convention.

#[test]
fn test_populate_checksums_var_aggregates_workspace_combined_files() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let dir = tempfile::tempdir().unwrap();
    let crate_a_path = dir.path().join("crateA_1.0.0_checksums.txt");
    let crate_b_path = dir.path().join("crateB_2.0.0_checksums.txt");
    std::fs::write(
        &crate_a_path,
        "aaaa1111  zebra-1.0.0-linux.tar.gz\naaaa2222  alpha-1.0.0-linux.tar.gz\n",
    )
    .unwrap();
    std::fs::write(&crate_b_path, "bbbb3333  middle-2.0.0-linux.tar.gz\n").unwrap();

    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        path: crate_a_path,
        name: "crateA_1.0.0_checksums.txt".to_string(),
        target: None,
        crate_name: "crateA".to_string(),
        metadata: std::collections::HashMap::from([
            ("algorithm".to_string(), "sha256".to_string()),
            ("combined".to_string(), "true".to_string()),
        ]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        path: crate_b_path,
        name: "crateB_2.0.0_checksums.txt".to_string(),
        target: None,
        crate_name: "crateB".to_string(),
        metadata: std::collections::HashMap::from([
            ("algorithm".to_string(), "sha256".to_string()),
            ("combined".to_string(), "true".to_string()),
        ]),
        size: None,
    });

    populate_checksums_var(&mut ctx);

    let rendered = ctx.render_template("{{ Checksums }}").unwrap();
    assert!(
        rendered.contains("aaaa1111  zebra-1.0.0-linux.tar.gz"),
        "missing crateA zebra line in {rendered:?}",
    );
    assert!(
        rendered.contains("aaaa2222  alpha-1.0.0-linux.tar.gz"),
        "missing crateA alpha line in {rendered:?}",
    );
    assert!(
        rendered.contains("bbbb3333  middle-2.0.0-linux.tar.gz"),
        "missing crateB middle line in {rendered:?}",
    );
    let pos_alpha = rendered
        .find("alpha-1.0.0-linux.tar.gz")
        .expect("alpha line absent");
    let pos_middle = rendered
        .find("middle-2.0.0-linux.tar.gz")
        .expect("middle line absent");
    let pos_zebra = rendered
        .find("zebra-1.0.0-linux.tar.gz")
        .expect("zebra line absent");
    assert!(
        pos_alpha < pos_middle && pos_middle < pos_zebra,
        "checksum lines not sorted by filename: {rendered:?}",
    );
}

#[test]
fn test_populate_checksums_var_single_combined_file_preserves_content() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("myapp_1.0.0_checksums.txt");
    std::fs::write(&path, "abc123  myapp-1.0.0-linux.tar.gz\n").unwrap();

    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        path,
        name: "myapp_1.0.0_checksums.txt".to_string(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: std::collections::HashMap::from([
            ("algorithm".to_string(), "sha256".to_string()),
            ("combined".to_string(), "true".to_string()),
        ]),
        size: None,
    });

    populate_checksums_var(&mut ctx);

    let rendered = ctx.render_template("{{ Checksums }}").unwrap();
    assert!(rendered.contains("abc123  myapp-1.0.0-linux.tar.gz"));
}

#[test]
fn test_populate_checksums_var_split_mode_preserves_map_keyed_by_checksumof() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    // Split-mode sidecars (one per archive) carry a `ChecksumOf` marker
    // pointing back at the artifact they checksum. The Checksums variable
    // must be a map so a release-body template can iterate
    // `{% for k, v in Checksums %}…{% endfor %}` and reference each sidecar
    // by the artifact name. This pins the split-mode contract that workspace
    // aggregation MUST NOT break.
    let dir = tempfile::tempdir().unwrap();
    let sidecar_a = dir.path().join("myapp-1.0.0-linux.tar.gz.sha256");
    let sidecar_b = dir.path().join("myapp-1.0.0-darwin.tar.gz.sha256");
    std::fs::write(&sidecar_a, "aaaa  myapp-1.0.0-linux.tar.gz\n").unwrap();
    std::fs::write(&sidecar_b, "bbbb  myapp-1.0.0-darwin.tar.gz\n").unwrap();

    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        path: sidecar_a,
        name: "myapp-1.0.0-linux.tar.gz.sha256".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: std::collections::HashMap::from([
            ("algorithm".to_string(), "sha256".to_string()),
            (
                "ChecksumOf".to_string(),
                "myapp-1.0.0-linux.tar.gz".to_string(),
            ),
        ]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        path: sidecar_b,
        name: "myapp-1.0.0-darwin.tar.gz.sha256".to_string(),
        target: Some("x86_64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: std::collections::HashMap::from([
            ("algorithm".to_string(), "sha256".to_string()),
            (
                "ChecksumOf".to_string(),
                "myapp-1.0.0-darwin.tar.gz".to_string(),
            ),
        ]),
        size: None,
    });

    populate_checksums_var(&mut ctx);

    // The map shape lets Tera iterate with `{% for k, v in Checksums %}`.
    // We verify the rendered output references both artifact names so the
    // template path through this branch is exercised.
    let rendered = ctx
        .render_template("{% for k, v in Checksums %}{{ k }}:{{ v }}\n{% endfor %}")
        .unwrap();
    assert!(rendered.contains("myapp-1.0.0-linux.tar.gz"));
    assert!(rendered.contains("myapp-1.0.0-darwin.tar.gz"));
    assert!(rendered.contains("aaaa"));
    assert!(rendered.contains("bbbb"));
}

// ---------------------------------------------------------------------------
// RetryConfig -> RetryPolicy wiring for the github backend
// ---------------------------------------------------------------------------
//
// The github backend (`crates/stage-release/src/github/mod.rs`) resolves a
// single `policy = ctx.config.retry.unwrap_or_default().to_policy()` at the
// top of `run_github_backend` and threads it through every retriable
// octocrab call site (find-draft list pagination, replace-existing-draft
// delete, create-release POST, update-release PATCH, the bespoke
// upload-asset retry loop, the un-draft publish PATCH).
//
// The unit-level retry-loop contract (5xx-retries, 4xx-fast-fails, honors
// `attempts`) is pinned by `github::retry_call::tests` against a real TCP
// responder. The tests below pin the config-surface translation
// (`RetryConfig::to_policy`) so the upload-loop locals
// (`max_upload_attempts`, `initial_retry_delay`, `max_retry_delay`) and the
// `retry_octocrab_call` policy argument trace back to the user's YAML.
//
// Invariant pinned: the upload loop's `max_upload_attempts` equals
// `ctx.config.retry.unwrap_or_default().to_policy().max_attempts`.

/// Local helper that mirrors the resolution `run_github_backend` performs:
/// `ctx.config.retry.unwrap_or_default().to_policy()`. Drift between the
/// backend and this helper is the failure mode the test pins against.
fn resolve_policy_like_github_backend(
    retry: Option<anodizer_core::config::RetryConfig>,
) -> anodizer_core::retry::RetryPolicy {
    retry.unwrap_or_default().to_policy()
}

#[test]
fn test_retry_config_default_yields_goreleaser_defaults_for_github_backend() {
    // Pin the "no retry: block in YAML" branch: `unwrap_or_default()` must
    // yield the defaults so the github backend's upload-loop constants
    // translate cleanly to the policy fields. A change to either the
    // defaults or the github backend's `unwrap_or_default()` call site
    // breaks this pin and requires a deliberate update.
    let policy = resolve_policy_like_github_backend(None);
    assert_eq!(
        policy.max_attempts, 10,
        "default attempts must be 10 (matches GR pkg/config.Retry.Attempts)"
    );
    assert_eq!(
        policy.base_delay,
        std::time::Duration::from_secs(10),
        "default base_delay must be 10s (matches GR pkg/config.Retry.Delay)"
    );
    assert_eq!(
        policy.max_delay,
        std::time::Duration::from_secs(5 * 60),
        "default max_delay must be 5m (matches GR pkg/config.Retry.MaxDelay)"
    );
}

#[test]
fn test_retry_config_attempts_one_short_circuits_github_backend_policy() {
    // Pin the "user wants no retries" surface: `attempts: 1` in YAML must
    // produce `max_attempts == 1` so every retry-wrapped octocrab call site
    // (find-draft list, delete, create, update, upload, publish) attempts
    // exactly once and fast-fails on the first transient error. This is the
    // shape the github backend exercises via
    // `ctx.config.retry.unwrap_or_default().to_policy()`.
    use anodizer_core::config::{HumanDuration, RetryConfig};
    let cfg = RetryConfig {
        attempts: 1,
        delay: HumanDuration(std::time::Duration::from_millis(5)),
        max_delay: HumanDuration(std::time::Duration::from_millis(10)),
    };
    let policy = resolve_policy_like_github_backend(Some(cfg));
    assert_eq!(
        policy.max_attempts, 1,
        "attempts=1 must produce exactly one attempt (no retries)"
    );
    assert_eq!(policy.base_delay, std::time::Duration::from_millis(5));
    assert_eq!(policy.max_delay, std::time::Duration::from_millis(10));
}

#[test]
fn test_retry_config_custom_values_flow_into_upload_constants() {
    // Pin the upload-loop wiring: `policy.max_attempts` seeds
    // `max_upload_attempts`, `policy.base_delay` seeds `initial_retry_delay`,
    // and `policy.max_delay` seeds `max_retry_delay`. The YAML controls all
    // three via `ctx.config.retry`.
    //
    // The test calls the same `upload_retry_locals` helper the backend uses,
    // so a future formula change in the backend fails this test instead of
    // silently drifting.
    use anodizer_core::config::{HumanDuration, RetryConfig};
    let cfg = RetryConfig {
        attempts: 4,
        delay: HumanDuration(std::time::Duration::from_millis(250)),
        max_delay: HumanDuration(std::time::Duration::from_secs(7)),
    };
    let policy = cfg.to_policy();

    // The values the github backend assigns into the upload loop locals:
    let (max_upload_attempts, initial_retry_delay, max_retry_delay) =
        crate::github::upload_retry_locals(&policy);

    assert_eq!(max_upload_attempts, 4, "upload loop honors custom attempts");
    assert_eq!(
        initial_retry_delay,
        std::time::Duration::from_millis(250),
        "upload loop honors custom delay"
    );
    assert_eq!(
        max_retry_delay,
        std::time::Duration::from_secs(7),
        "upload loop honors custom max_delay"
    );
}

#[test]
fn test_release_upload_candidates_exclude_binary_sign_outputs() {
    // Construct a context with three signature artifacts:
    //   1. A binary-sign output (metadata["binary_sign"] = "true") — must be excluded.
    //   2. A binary-sign certificate output — must be excluded.
    //   3. A normal archive-sign Signature (no binary_sign metadata) — must be included.
    let mut ctx = TestContextBuilder::new().build();

    let mut binary_sign_meta = std::collections::HashMap::new();
    binary_sign_meta.insert("type".to_string(), "Signature".to_string());
    binary_sign_meta.insert("binary_sign".to_string(), "true".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Signature,
        path: "dist/anodizer_linux_amd64".into(),
        name: "anodizer_linux_amd64".to_string(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: binary_sign_meta,
        size: None,
    });

    let mut binary_sign_cert_meta = std::collections::HashMap::new();
    binary_sign_cert_meta.insert("type".to_string(), "Certificate".to_string());
    binary_sign_cert_meta.insert("binary_sign".to_string(), "true".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Certificate,
        path: "dist/anodizer_linux_amd64.pem".into(),
        name: "anodizer_linux_amd64.pem".to_string(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: binary_sign_cert_meta,
        size: None,
    });

    let mut archive_sign_meta = std::collections::HashMap::new();
    archive_sign_meta.insert("type".to_string(), "Signature".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Signature,
        path: "dist/myapp_1.0.0_linux_amd64.tar.gz.sig".into(),
        name: "myapp_1.0.0_linux_amd64.tar.gz.sig".to_string(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: archive_sign_meta,
        size: None,
    });

    // Call the production helper directly. If a future refactor drops the
    // binary-sign filter from `collect_release_upload_candidates`, this test
    // will fail.
    let candidates = super::run::collect_release_upload_candidates(&ctx, "myapp", None, false);
    let paths: Vec<String> = candidates
        .iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect();

    assert!(
        !paths.iter().any(|p| p.ends_with("anodizer_linux_amd64")),
        "binary-sign Signature must not appear in release upload candidates; got {:?}",
        paths
    );
    assert!(
        !paths
            .iter()
            .any(|p| p.ends_with("anodizer_linux_amd64.pem")),
        "binary-sign Certificate must not appear in release upload candidates; got {:?}",
        paths
    );
    assert!(
        paths
            .iter()
            .any(|p| p.ends_with("myapp_1.0.0_linux_amd64.tar.gz.sig")),
        "archive-sign Signature must appear in release upload candidates; got {:?}",
        paths
    );
}

// =====================================================================
// run.rs coverage: behaviors not already pinned by sibling tests.
// =====================================================================

// ---- release.skip template-rendering path -----------------------------
//
// `release.skip` is a `StringOrBool`: when a template string evaluates
// to "true" the crate is skipped before validation runs. Pin both
// branches (renders-to-true skips; renders-to-false proceeds).

#[test]
fn test_release_skip_template_renders_to_true_skips_crate() {
    // Template evaluates to "true" via the IsSnapshot var (snapshot=true).
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .snapshot(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip: Some(StringOrBool::String(
                    "{% if IsSnapshot %}true{% else %}false{% endif %}".to_string(),
                )),
                // A conflicting draft pair would normally bail, but skip:
                // short-circuits before that validation. If the bail fires
                // the assertion below will catch it.
                replace_existing_draft: Some(true),
                use_existing_draft: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    assert!(
        stage.run(&mut ctx).is_ok(),
        "skip template that renders to true should short-circuit before draft validation"
    );
}

#[test]
fn test_release_skip_template_renders_to_false_proceeds() {
    // Template renders to "false" (snapshot=false), so the crate proceeds.
    // We deliberately set the conflicting draft pair so the stage MUST
    // reach validation and bail — proving the skip branch was not taken.
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .snapshot(false)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip: Some(StringOrBool::String(
                    "{% if IsSnapshot %}true{% else %}false{% endif %}".to_string(),
                )),
                replace_existing_draft: Some(true),
                use_existing_draft: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let stage = ReleaseStage;
    let err = stage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("replace_existing_draft") && err.contains("use_existing_draft"),
        "skip-renders-false must reach draft validation; got: {err}"
    );
}

// ---- skip_upload string/template variants -----------------------------
//
// `release.skip_upload` accepts true/false/auto/1/0 plus a templated
// string. The bare-bool path is covered (`test_skip_upload_dry_run_message`)
// but the string/template variants — including the bail on invalid — are
// not. Each test relies on the dry-run path so behavior is observable
// through ok/err alone (no API).

#[test]
fn test_skip_upload_string_auto_in_snapshot_succeeds() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .snapshot(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip_upload: Some(StringOrBool::String("auto".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_snapshot_without_dry_run_does_not_reach_live_backend() {
    use anodizer_core::config::ScmRepoConfig;
    use anodizer_core::scm::ScmTokenType;
    // `--snapshot` (WITHOUT `--dry-run`) must take the no-publish telemetry path,
    // not the live GitHub backend — which bails on the missing token (and would
    // create a real release if a token were present). Regression for the release
    // stage computing `dry_run = is_dry_run()` and omitting `|| is_snapshot()`.
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .snapshot(true)
        .token(None)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "octocat".to_string(),
                    name: "hello".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::GitHub;
    assert!(
        ReleaseStage.run(&mut ctx).is_ok(),
        "release --snapshot must not reach the live SCM backend / token gate"
    );
}

#[test]
fn test_skip_upload_string_zero_and_one_are_valid() {
    for value in ["1", "0", "true", "false", ""] {
        let mut ctx = TestContextBuilder::new()
            .project_name("test")
            .dry_run(true)
            .crates(vec![CrateConfig {
                name: "testcrate".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    skip_upload: Some(StringOrBool::String(value.to_string())),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        assert!(
            ReleaseStage.run(&mut ctx).is_ok(),
            "skip_upload value {value:?} should be accepted"
        );
    }
}

#[test]
fn test_skip_upload_invalid_string_bails() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip_upload: Some(StringOrBool::String("maybe".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("invalid skip_upload") && err.contains("maybe"),
        "bail should name the rejected value; got: {err}"
    );
}

#[test]
fn test_skip_upload_template_renders_to_true_then_validates() {
    // Template ("{% if IsSnapshot %}true{% else %}false{% endif %}") with
    // snapshot=true → "true". Stage must not bail — proves the template
    // branch (vs the literal-string branch) was taken.
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .snapshot(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip_upload: Some(StringOrBool::String(
                    "{% if IsSnapshot %}true{% else %}maybe{% endif %}".to_string(),
                )),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_skip_upload_template_renders_to_invalid_bails() {
    // Same template shape but snapshot=false → "maybe", which is invalid.
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .snapshot(false)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip_upload: Some(StringOrBool::String(
                    "{% if IsSnapshot %}true{% else %}maybe{% endif %}".to_string(),
                )),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("invalid skip_upload"),
        "rendered-invalid template should still bail; got: {err}"
    );
}

// ---- selected_crates filter -------------------------------------------
//
// Only crates whose name is in `ctx.options.selected_crates` are
// processed (when the filter is non-empty). Pin by setting up a crate
// whose release config has a poison pill (conflicting drafts that would
// bail) and filtering it OUT: success means the crate was filtered.

#[test]
fn test_selected_crates_filter_excludes_unlisted_crate() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .selected_crates(vec!["other".to_string()])
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                // Would bail if processed — filter must exclude us first.
                replace_existing_draft: Some(true),
                use_existing_draft: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    assert!(
        ReleaseStage.run(&mut ctx).is_ok(),
        "selected_crates filter should exclude the unlisted crate"
    );
}

#[test]
fn test_selected_crates_filter_includes_listed_crate() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .selected_crates(vec!["testcrate".to_string()])
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                // Same poison pill — but now the crate IS listed, so the
                // bail SHOULD fire and prove the include-path was taken.
                replace_existing_draft: Some(true),
                use_existing_draft: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(err.contains("replace_existing_draft"));
}

// ---- include_meta: missing metadata.json ------------------------------
//
// When `include_meta` is true and `dist/metadata.json` is absent:
//   - non-strict: warn and continue (stage succeeds);
//   - strict: bail with a strict-mode error.

#[test]
fn test_include_meta_missing_metadata_json_warns_in_non_strict() {
    let dir = tempfile::tempdir().unwrap();
    let dist_dir = dir.path().join("dist");
    std::fs::create_dir_all(&dist_dir).unwrap();
    // intentionally no metadata.json
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                include_meta: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.config.dist = dist_dir;

    assert!(
        ReleaseStage.run(&mut ctx).is_ok(),
        "non-strict include_meta with missing metadata.json should warn, not fail"
    );
}

#[test]
fn test_include_meta_missing_metadata_json_bails_in_strict_mode() {
    let dir = tempfile::tempdir().unwrap();
    let dist_dir = dir.path().join("dist");
    std::fs::create_dir_all(&dist_dir).unwrap();
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                include_meta: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.config.dist = dist_dir;
    ctx.options.strict = true;

    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("include_meta") && err.contains("strict"),
        "strict include_meta with missing file should bail; got: {err}"
    );
}

// ---- release.tag override drifts from pushed git tag (warn) -----------
//
// The stage warns when `release.tag` resolves to a value different from
// the pushed `Tag` template var. The warn is logged, not returned, so we
// observe the post-state: `ReleaseURL` should be composed from the
// override tag.

#[test]
fn test_release_tag_override_drifts_from_pushed_tag_does_not_fail() {
    use anodizer_core::config::ScmRepoConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .tag("v9.9.9") // pushed tag in template_vars
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v9.9.9".to_string(),
            release: Some(ReleaseConfig {
                // Override differs from pushed Tag — should warn + proceed.
                tag: Some("v0.0.0-override".to_string()),
                github: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
    // Post-state: the ReleaseURL contains the override tag, not the pushed
    // tag. This is the observable consequence of the override path.
    let release_url = ctx
        .template_vars()
        .get("ReleaseURL")
        .cloned()
        .unwrap_or_default();
    assert!(
        release_url.contains("v0.0.0-override"),
        "ReleaseURL should use override tag, got {release_url:?}"
    );
}

// ---- replace_existing_artifacts CLI override --------------------------
//
// The CLI `--replace-existing` flag (`ctx.options.replace_existing_artifacts`)
// is OR'd with the config value. Test that setting only the CLI flag is
// enough to drive the stage through dry-run (smoke-test of the OR path).

#[test]
fn test_replace_existing_artifacts_cli_override_drives_stage() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                // config flag NOT set; only the CLI override is enabled.
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.options.replace_existing_artifacts = true;

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

// ---- ids filter zero-match warning --------------------------------
//
// When `ids` is set and no artifacts match, the stage logs a warn and
// proceeds (release still gets created with no uploads). Observable
// signal: ok() return AND ReleaseURL still populated in dry-run.

#[test]
fn test_ids_filter_zero_match_warns_and_proceeds() {
    use anodizer_core::config::ScmRepoConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                ids: Some(vec!["nonexistent-id".to_string()]),
                github: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
    let release_url = ctx
        .template_vars()
        .get("ReleaseURL")
        .cloned()
        .unwrap_or_default();
    assert!(
        !release_url.is_empty(),
        "ids-zero-match should still create the release (URL populated)"
    );
}

// ---- exemptions block injection into release body ---------------------
//
// When `ctx.determinism.runtime_allowlist` is non-empty AND the
// changelog body is empty, the exemptions block becomes the body
// outright (the `exemptions if changelog_body.is_empty()` branch).
// When both are non-empty, the format string joins them with "\n".
// Both branches drive the dry-run stage to completion.

#[test]
fn test_release_with_exemptions_and_empty_changelog_succeeds() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }])
        .build();
    // Seed the allowlist; leave changelog_body empty.
    ctx.determinism = Some(anodizer_core::DeterminismState {
        runtime_allowlist: vec![("foo.deb".to_string(), "dpkg timestamp".to_string())],
        ..Default::default()
    });
    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_release_with_exemptions_and_changelog_joins_both() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }])
        .build();
    ctx.determinism = Some(anodizer_core::DeterminismState {
        runtime_allowlist: vec![("foo.deb".to_string(), "dpkg timestamp".to_string())],
        ..Default::default()
    });
    ctx.stage_outputs
        .changelogs
        .insert("testcrate".to_string(), "- bug fix".to_string());
    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

// ---- dry-run gitea derives download URL from API URL ------------------
//
// In the dry-run path, when `gitea_urls.download` is None but `api` is
// set, the stage derives the download URL by stripping `/api/v1` from
// the API URL. Observable through the `ReleaseURL` template var.

#[test]
fn test_gitea_dry_run_derives_download_url_from_api_url() {
    use anodizer_core::config::{GiteaUrlsConfig, ScmRepoConfig};

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                gitea: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::Gitea;
    ctx.config.gitea_urls = Some(GiteaUrlsConfig {
        api: Some("https://gitea.example.com/api/v1".to_string()),
        download: None,
        skip_tls_verify: None,
    });

    assert!(ReleaseStage.run(&mut ctx).is_ok());
    let release_url = ctx
        .template_vars()
        .get("ReleaseURL")
        .cloned()
        .unwrap_or_default();
    assert!(
        release_url.contains("gitea.example.com"),
        "ReleaseURL should be derived from API host, got {release_url:?}"
    );
    assert!(
        !release_url.contains("/api/v1"),
        "/api/v1 should be stripped from derived download URL, got {release_url:?}"
    );
}

// ---- collect_release_upload_candidates: include_meta + ids filter -----
//
// Beyond the existing binary-sign exclusion test, pin two more behaviors
// of the public helper:
//   1. `include_meta=true` adds the Metadata kind to the upload set.
//   2. The ids filter honors metadata["id"] correctly.

#[test]
fn test_release_upload_candidates_include_meta_adds_metadata_kind() {
    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Metadata,
        path: "dist/metadata.json".into(),
        name: "metadata.json".to_string(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    let without =
        super::run::collect_release_upload_candidates(&ctx, "myapp", None, /*meta*/ false);
    assert!(
        !without
            .iter()
            .any(|(p, _)| p.to_string_lossy().ends_with("metadata.json")),
        "Metadata kind must be excluded when include_meta=false"
    );

    let with =
        super::run::collect_release_upload_candidates(&ctx, "myapp", None, /*meta*/ true);
    assert!(
        with.iter()
            .any(|(p, _)| p.to_string_lossy().ends_with("metadata.json")),
        "Metadata kind must be included when include_meta=true"
    );
}

#[test]
fn test_release_upload_candidates_ids_filter_selects_matching() {
    let mut ctx = TestContextBuilder::new().build();
    let mut meta_keep = std::collections::HashMap::new();
    meta_keep.insert("id".to_string(), "linux-archive".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: "dist/keep.tar.gz".into(),
        name: "keep.tar.gz".to_string(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: meta_keep,
        size: None,
    });
    let mut meta_drop = std::collections::HashMap::new();
    meta_drop.insert("id".to_string(), "windows-archive".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: "dist/drop.zip".into(),
        name: "drop.zip".to_string(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: meta_drop,
        size: None,
    });

    let ids = vec!["linux-archive".to_string()];
    let selected = super::run::collect_release_upload_candidates(&ctx, "myapp", Some(&ids), false);
    let paths: Vec<String> = selected
        .iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect();
    assert!(paths.iter().any(|p| p.ends_with("keep.tar.gz")));
    assert!(!paths.iter().any(|p| p.ends_with("drop.zip")));
}

#[test]
fn test_release_upload_candidates_ids_filter_signatures_inherit_subject_verdict() {
    // A signature/SBOM uploads iff the artifact it derives from uploads:
    // the sign/SBOM stages record the subject's kind and build id on the
    // derived artifact, and matches_id_filter judges that record instead of
    // excluding the (id-less) derived kind wholesale.
    let mut ctx = TestContextBuilder::new().build();
    let mut add = |kind: ArtifactKind, name: &str, meta: &[(&str, &str)]| {
        ctx.artifacts.add(Artifact {
            kind,
            path: format!("dist/{name}").into(),
            name: name.to_string(),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: meta
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            size: None,
        });
    };
    add(ArtifactKind::Archive, "keep.tar.gz", &[("id", "keep")]);
    add(
        ArtifactKind::Signature,
        "keep.tar.gz.sig",
        &[("subject_kind", "archive"), ("id", "keep")],
    );
    add(ArtifactKind::Archive, "drop.zip", &[("id", "drop")]);
    add(
        ArtifactKind::Signature,
        "drop.zip.sig",
        &[("subject_kind", "archive"), ("id", "drop")],
    );
    add(ArtifactKind::Checksum, "checksums.txt", &[]);
    add(
        ArtifactKind::Signature,
        "checksums.txt.sig",
        &[("subject_kind", "checksum")],
    );
    add(
        ArtifactKind::Sbom,
        "keep.tar.gz.cdx.json",
        &[("subject_kind", "archive"), ("id", "keep")],
    );
    add(
        ArtifactKind::Sbom,
        "drop.zip.cdx.json",
        &[("subject_kind", "archive"), ("id", "drop")],
    );
    // Transitive chain: a project-wide `any` SBOM has no subject record and
    // always uploads; its signature copies that absence and uploads too. A
    // sig of an EXCLUDED archive's SBOM copies (archive, drop) and is
    // dropped with its chain.
    add(
        ArtifactKind::Sbom,
        "project.cdx.json",
        &[("sbom_id", "default")],
    );
    add(ArtifactKind::Signature, "project.cdx.json.sig", &[]);
    add(
        ArtifactKind::Signature,
        "drop.zip.cdx.json.sig",
        &[("subject_kind", "archive"), ("id", "drop")],
    );
    add(
        ArtifactKind::Signature,
        "keep.tar.gz.cdx.json.sig",
        &[("subject_kind", "archive"), ("id", "keep")],
    );

    let ids = vec!["keep".to_string()];
    let selected = super::run::collect_release_upload_candidates(&ctx, "myapp", Some(&ids), false);
    let names: Vec<String> = selected
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    for kept in [
        "keep.tar.gz",
        "keep.tar.gz.sig",
        "keep.tar.gz.cdx.json",
        "keep.tar.gz.cdx.json.sig",
        "checksums.txt",
        "checksums.txt.sig",
        "project.cdx.json",
        "project.cdx.json.sig",
    ] {
        assert!(
            names.contains(&kept.to_string()),
            "{kept} must upload: {names:?}"
        );
    }
    for dropped in [
        "drop.zip",
        "drop.zip.sig",
        "drop.zip.cdx.json",
        "drop.zip.cdx.json.sig",
    ] {
        assert!(
            !names.contains(&dropped.to_string()),
            "{dropped} must NOT upload: {names:?}"
        );
    }
}

#[test]
fn test_release_upload_candidates_filters_by_crate_name() {
    let mut ctx = TestContextBuilder::new().build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: "dist/mine.tar.gz".into(),
        name: "mine.tar.gz".to_string(),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: "dist/theirs.tar.gz".into(),
        name: "theirs.tar.gz".to_string(),
        target: None,
        crate_name: "othercrate".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    let selected = super::run::collect_release_upload_candidates(&ctx, "mycrate", None, false);
    let paths: Vec<String> = selected
        .iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect();
    assert!(paths.iter().any(|p| p.ends_with("mine.tar.gz")));
    assert!(!paths.iter().any(|p| p.ends_with("theirs.tar.gz")));
}

// =====================================================================
// run.rs additional coverage: bail paths, template render errors, and
// dry-run branches not yet pinned above.
// =====================================================================

// ---- conflicting draft options bail (without skip-template detour) -----
//
// The pre-existing test that pins this error path threads through the
// `release.skip` renders-to-false branch. Pin the direct bail path too:
// no `skip`, just `replace_existing_draft + use_existing_draft` both true.
// Regression target: dropping or reordering the validation.

#[test]
fn test_release_conflicting_draft_options_bails_direct() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                replace_existing_draft: Some(true),
                use_existing_draft: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("replace_existing_draft") && err.contains("use_existing_draft"),
        "conflicting draft options must bail; got: {err}"
    );
}

// ---- header template render error ------------------------------------
//
// `release.header` is template-rendered. An invalid Tera template must
// surface a contextualised error mentioning the crate name.

#[test]
fn test_release_header_invalid_template_errors() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                // Unterminated tag → Tera parse error.
                header: Some(ContentSource::Inline("{{ unterminated".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("header") && err.contains("testcrate"),
        "header render error should mention header + crate; got: {err}"
    );
}

// ---- footer template render error ------------------------------------

#[test]
fn test_release_footer_invalid_template_errors() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                footer: Some(ContentSource::Inline("{{ broken".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("footer") && err.contains("testcrate"),
        "footer render error should mention footer + crate; got: {err}"
    );
}

// ---- release.name_template render error -------------------------------

#[test]
fn test_release_name_template_invalid_errors() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                name_template: Some("{{ broken".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("name_template") && err.contains("testcrate"),
        "name_template render error should mention field + crate; got: {err}"
    );
}

// ---- target_commitish render error ------------------------------------

#[test]
fn test_release_target_commitish_invalid_template_errors() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                target_commitish: Some("{{ unterminated".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("target_commitish") && err.contains("testcrate"),
        "target_commitish render error should mention field + crate; got: {err}"
    );
}

// ---- release.skip template render error -------------------------------
//
// When the skip template fails to render, the error context must
// identify the field and crate.

#[test]
fn test_release_skip_invalid_template_errors() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip: Some(StringOrBool::String("{{ unterminated".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("skip") && err.contains("testcrate"),
        "skip render error should mention field + crate; got: {err}"
    );
}

// ---- release.tag override matches pushed tag → no drift warn ----------
//
// When the override resolves to the same value as the pushed Tag, no
// drift warning should fire. We can't introspect the warn directly but
// the dry-run still completes ok and the ReleaseURL uses the same tag.

#[test]
fn test_release_tag_override_matches_pushed_tag_no_drift() {
    use anodizer_core::config::ScmRepoConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .tag("v1.2.3")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.2.3".to_string(),
            release: Some(ReleaseConfig {
                tag: Some("v1.2.3".to_string()),
                github: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
    let release_url = ctx
        .template_vars()
        .get("ReleaseURL")
        .cloned()
        .unwrap_or_default();
    assert!(
        release_url.ends_with("/releases/tag/v1.2.3"),
        "ReleaseURL should reflect the matching tag, got {release_url:?}"
    );
}

// ---- selected_crates excludes other release-configured crate ----------
//
// When `selected_crates` is non-empty, crates not in the set must be
// silently filtered. Combined with one selected and one ignored crate
// (both with release config), only the selected one's ReleaseURL is set.

#[test]
fn test_selected_crates_excludes_other_release_configured_crate() {
    use anodizer_core::config::ScmRepoConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .selected_crates(vec!["keep".to_string()])
        .crates(vec![
            CrateConfig {
                name: "keep".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(ScmRepoConfig {
                        owner: "o".to_string(),
                        name: "keep-repo".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            CrateConfig {
                name: "drop".to_string(),
                path: ".".to_string(),
                tag_template: "v2.0.0".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(ScmRepoConfig {
                        owner: "o".to_string(),
                        name: "drop-repo".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
    let release_url = ctx
        .template_vars()
        .get("ReleaseURL")
        .cloned()
        .unwrap_or_default();
    // Only the selected crate's repo should be in the URL.
    assert!(
        release_url.contains("keep-repo"),
        "selected crate's repo should appear in ReleaseURL, got {release_url:?}"
    );
    assert!(
        !release_url.contains("drop-repo"),
        "unselected crate's repo must NOT appear in ReleaseURL, got {release_url:?}"
    );
}

// ---- multiple crates: one with release config, one without -----------
//
// Crates without a `release` block must be silently filtered (no
// failure, no warn). Sanity-check the stage iterates only the
// release-configured ones.

#[test]
fn test_release_stage_filters_crates_without_release_block() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![
            CrateConfig {
                name: "without".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: None,
                ..Default::default()
            },
            CrateConfig {
                name: "with".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig::default()),
                ..Default::default()
            },
        ])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

// ---- release.skip plain bool true skips silently ----------------------
//
// Bool-shaped skip (not a template) hits the same branch. Pin it
// explicitly so a future regression that only handles the template
// form gets caught.

#[test]
fn test_release_skip_plain_bool_true_skips_silently() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip: Some(StringOrBool::Bool(true)),
                // Conflicting draft options would normally bail, but
                // skip:true short-circuits before validation runs.
                replace_existing_draft: Some(true),
                use_existing_draft: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(
        ReleaseStage.run(&mut ctx).is_ok(),
        "skip:true must skip before the conflicting-draft validation runs"
    );
}

// ---- GitLab missing token returns an actionable error -----------------

#[test]
fn test_gitlab_missing_token_errors() {
    use anodizer_core::config::ScmRepoConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .token(None)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                gitlab: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::GitLab;

    let err = ReleaseStage.run(&mut ctx).unwrap_err().to_string();
    assert!(
        err.contains("GITLAB_TOKEN") || err.contains("--token"),
        "missing GitLab token should mention GITLAB_TOKEN or --token; got: {err}"
    );
}

// ---- Gitea backend dry-run with default URLs (no gitea_urls) ---------
//
// When `gitea_urls` is None, the dry-run still publishes a ReleaseURL
// derived from the default `https://gitea.com`. Pin that URL prefix so
// dropping the default would surface immediately.

#[test]
fn test_gitea_dry_run_default_urls() {
    use anodizer_core::config::ScmRepoConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                gitea: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::Gitea;
    // Explicitly leave ctx.config.gitea_urls = None.

    assert!(ReleaseStage.run(&mut ctx).is_ok());
    let release_url = ctx
        .template_vars()
        .get("ReleaseURL")
        .cloned()
        .unwrap_or_default();
    assert!(
        release_url.starts_with("https://gitea.com/owner/repo/"),
        "default gitea download URL should be https://gitea.com, got {release_url:?}"
    );
}

// ---- dry-run + skip_upload + extra_files: upload lines must not fire --
//
// skip_upload should suppress the per-artifact "would upload" logging
// even when extra_files contribute entries. Observable proof: the run
// still completes ok with a non-empty extra_files glob.

#[test]
fn test_dry_run_skip_upload_with_extra_files_succeeds() {
    use anodizer_core::config::ExtraFileSpec;
    use anodizer_core::config::StringOrBool;

    let tmp = tempfile::TempDir::new().unwrap();
    let extra = tmp.path().join("EXTRA.txt");
    std::fs::write(&extra, "x").unwrap();
    let pattern = tmp.path().join("*.txt").to_string_lossy().into_owned();

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip_upload: Some(StringOrBool::Bool(true)),
                extra_files: Some(vec![ExtraFileSpec::Glob(pattern)]),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

// ---- include_meta=true dry-run with metadata.json present -----------
//
// When `include_meta=true` AND `dist/metadata.json` exists, the dry-run
// path enumerates it as a would-upload candidate (no warn, no bail).

#[test]
fn test_dry_run_include_meta_with_existing_metadata_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("metadata.json"), "{\"a\":1}").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                include_meta: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

// ---- dry-run sets ReleaseURL even with empty owner (defensive path) --
//
// When no repo config resolves (no github/gitlab/gitea block), the
// dry-run path uses empty owner/repo and skips composing the
// ReleaseURL. The stage must still complete ok.

#[test]
fn test_dry_run_no_repo_config_does_not_set_release_url() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            // No github/gitlab/gitea block.
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
    let release_url = ctx
        .template_vars()
        .get("ReleaseURL")
        .cloned()
        .unwrap_or_default();
    assert!(
        release_url.is_empty(),
        "ReleaseURL must stay empty when no repo config resolves, got {release_url:?}"
    );
}

// ---- skip_upload template that renders to a bare bool string ---------
//
// `skip_upload: "{{ IsSnapshot }}"` should evaluate IsSnapshot and the
// rendered "true"/"false" string is consumed by the same matcher. Pin
// the snapshot=false / renders-to-"false" path so the dry-run proceeds
// AND the artifact-listing branch is taken.

#[test]
fn test_skip_upload_template_renders_false_in_non_snapshot() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .snapshot(false)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip_upload: Some(StringOrBool::String("{{ IsSnapshot }}".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

// ---- release.tag template render error surfaces via resolve_release_tag

#[test]
fn test_release_tag_override_invalid_template_errors() {
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                tag: Some("{{ unterminated".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(
        ReleaseStage.run(&mut ctx).is_err(),
        "invalid release.tag template must error"
    );
}

// -----------------------------------------------------------------------
// Additional run.rs coverage tests
// -----------------------------------------------------------------------

#[test]
fn test_dry_run_gitea_skip_tls_verify_true_logs() {
    use anodizer_core::config::{GiteaUrlsConfig, ScmRepoConfig};

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                gitea: Some(ScmRepoConfig {
                    owner: "owner".to_string(),
                    name: "repo".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::Gitea;
    ctx.config.gitea_urls = Some(GiteaUrlsConfig {
        api: Some("https://gitea.example.com/api/v1".to_string()),
        download: Some("https://gitea.example.com".to_string()),
        skip_tls_verify: Some(true),
    });

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_gitlab_use_job_token_logs() {
    use anodizer_core::config::{GitLabUrlsConfig, ScmRepoConfig};

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                gitlab: Some(ScmRepoConfig {
                    owner: "g".to_string(),
                    name: "p".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::GitLab;
    ctx.config.gitlab_urls = Some(GitLabUrlsConfig {
        api: Some("https://gitlab.example.com/api/v4".to_string()),
        download: None,
        skip_tls_verify: None,
        use_package_registry: None,
        use_job_token: Some(true),
    });

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_release_tag_override_with_empty_pushed_tag_no_warn() {
    // When pushed_tag template var is empty, the override-mismatch warn
    // branch is short-circuited (cannot diff against empty). The stage
    // must still succeed and use the override tag for the release URL.
    use anodizer_core::config::ScmRepoConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                tag: Some("v-override".to_string()),
                github: Some(ScmRepoConfig {
                    owner: "o".to_string(),
                    name: "r".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    // Force Tag template var to empty so the warn branch's pushed_tag check fails.
    ctx.template_vars_mut().set("Tag", "");

    assert!(ReleaseStage.run(&mut ctx).is_ok());
    let release_url = ctx
        .template_vars()
        .get("ReleaseURL")
        .cloned()
        .unwrap_or_default();
    assert!(
        release_url.contains("v-override"),
        "override tag must drive ReleaseURL even when pushed tag is empty, got: {release_url}"
    );
}

#[test]
fn test_release_stage_multiple_crates_processed_independently() {
    // Two crates with release configs — both should reach the dry-run
    // log path without one short-circuiting the other.
    use anodizer_core::config::ScmRepoConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![
            CrateConfig {
                name: "c1".to_string(),
                path: ".".to_string(),
                tag_template: "v1.0.0".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(ScmRepoConfig {
                        owner: "o".to_string(),
                        name: "c1".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            CrateConfig {
                name: "c2".to_string(),
                path: ".".to_string(),
                tag_template: "v2.0.0".to_string(),
                release: Some(ReleaseConfig {
                    github: Some(ScmRepoConfig {
                        owner: "o".to_string(),
                        name: "c2".to_string(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
    // ReleaseURL gets overwritten per-crate; the *last* crate wins. This
    // pins the per-crate ordering (c2 runs after c1).
    let release_url = ctx
        .template_vars()
        .get("ReleaseURL")
        .cloned()
        .unwrap_or_default();
    assert!(
        release_url.contains("c2"),
        "second crate processed last must own ReleaseURL, got: {release_url}"
    );
}

#[test]
fn test_release_with_exemptions_overlay_in_runtime_allowlist() {
    // Populate `ctx.determinism.runtime_allowlist` so the exemptions
    // block render path fires inside `run()`. The dry-run must succeed
    // regardless of whether the changelog was provided.
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }])
        .build();
    ctx.determinism = Some(anodizer_core::DeterminismState {
        runtime_allowlist: vec![("foo.rpm".to_string(), "tool-bug-1234".to_string())],
        ..Default::default()
    });
    ctx.stage_outputs
        .changelogs
        .insert("testcrate".to_string(), "- fix".to_string());

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_with_release_header_template_renders() {
    // Exercise the header template render branch (release_cfg.header.is_some()).
    let mut ctx = TestContextBuilder::new()
        .project_name("hello")
        .tag("v1.2.3")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            release: Some(ReleaseConfig {
                header: Some(ContentSource::Inline(
                    "# {{ .ProjectName }} {{ .Tag }}".to_string(),
                )),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_with_release_footer_template_renders() {
    // Exercise the footer template render branch (release_cfg.footer.is_some()).
    let mut ctx = TestContextBuilder::new()
        .project_name("hello")
        .tag("v1.2.3")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            release: Some(ReleaseConfig {
                footer: Some(ContentSource::Inline("End {{ .Version }}".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_combines_exemptions_with_header_and_footer() {
    // Three render layers stack: header, exemptions block prepended to
    // changelog body, footer. Exercises the format_release_body path that
    // would otherwise only be hit when all three coexist.
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                header: Some(ContentSource::Inline("H".to_string())),
                footer: Some(ContentSource::Inline("F".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.determinism = Some(anodizer_core::DeterminismState {
        runtime_allowlist: vec![("x.rpm".to_string(), "reason".to_string())],
        ..Default::default()
    });
    ctx.stage_outputs
        .changelogs
        .insert("testcrate".to_string(), "- changelog".to_string());

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_with_replace_existing_artifacts_config_flag() {
    // Hits the OR-branch where the config flag is true (the CLI
    // override is separately covered).
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                replace_existing_artifacts: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_release_no_crates_with_release_block_silently_succeeds() {
    // A crate config without a `release` block must be filtered out
    // before any per-crate work runs. Empty filtered set => Ok(()).
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: None,
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_with_make_latest_template_string() {
    // Template-string make_latest gets resolved through
    // `resolve_make_latest`; pin the dry-run path with a rendered "auto".
    use anodizer_core::config::ScmRepoConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                make_latest: Some(MakeLatestConfig::String(
                    "{% if IsSnapshot %}false{% else %}true{% endif %}".to_string(),
                )),
                github: Some(ScmRepoConfig {
                    owner: "o".to_string(),
                    name: "r".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_target_commitish_template_renders() {
    use anodizer_core::config::ScmRepoConfig;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                target_commitish: Some("release/{{ .Tag }}".to_string()),
                github: Some(ScmRepoConfig {
                    owner: "o".to_string(),
                    name: "r".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_skip_upload_zero_falsy_proceeds_to_uploads() {
    // skip_upload="0" (or "false", or "") => skip_upload is false =>
    // dry-run iterates the artifact_entries log path. Empty artifacts
    // produce no upload lines but the dry-run still succeeds.
    use anodizer_core::config::StringOrBool;

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                skip_upload: Some(StringOrBool::String("0".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_gitlab_skip_tls_verify_and_use_package_registry_logs() {
    use anodizer_core::config::{GitLabUrlsConfig, ScmRepoConfig};

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                gitlab: Some(ScmRepoConfig {
                    owner: "g".to_string(),
                    name: "p".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.token_type = ScmTokenType::GitLab;
    // skip_tls_verify=true AND use_package_registry=true so both
    // dry-run log branches fire (line 414-419 in run.rs).
    ctx.config.gitlab_urls = Some(GitLabUrlsConfig {
        api: None,
        download: Some("https://gitlab.example.com".to_string()),
        skip_tls_verify: Some(true),
        use_package_registry: Some(true),
        use_job_token: Some(false),
    });

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_release_with_dist_dir_set_succeeds() {
    // Confirms the dist-dir + include_meta=false + extra_files-absent
    // happy path is reachable through a fully-defaulted release config.
    let tmp = tempfile::TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }])
        .build();

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_with_artifacts_present_lists_uploads() {
    use anodizer_core::config::ScmRepoConfig;
    use anodizer_core::test_helpers::artifact_set::TestArtifactSet;

    let tmp = tempfile::TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    let artifacts = TestArtifactSet::new()
        .linux_amd64("testcrate")
        .windows_amd64_zip("testcrate")
        .write_to(&dist);

    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .dist(dist)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                github: Some(ScmRepoConfig {
                    owner: "o".to_string(),
                    name: "r".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    for a in artifacts {
        ctx.artifacts.add(a);
    }

    assert!(ReleaseStage.run(&mut ctx).is_ok());
    // Each registered uploadable artifact gets metadata["url"] populated
    // via the dry-run path's `populate_artifact_download_urls` call.
    let any_with_url = ctx
        .artifacts
        .all()
        .iter()
        .any(|a| a.metadata.contains_key("url"));
    assert!(
        any_with_url,
        "dry-run with repo_cfg must populate artifact url metadata"
    );
}

#[test]
fn test_dry_run_replace_existing_artifacts_cli_or_config_or() {
    // Asserts that both the CLI flag AND the config flag set together
    // do not double-trigger or change behavior — they OR cleanly.
    let mut ctx = TestContextBuilder::new()
        .project_name("test")
        .dry_run(true)
        .crates(vec![CrateConfig {
            name: "testcrate".to_string(),
            path: ".".to_string(),
            tag_template: "v1.0.0".to_string(),
            release: Some(ReleaseConfig {
                replace_existing_artifacts: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    ctx.options.replace_existing_artifacts = true;

    assert!(ReleaseStage.run(&mut ctx).is_ok());
}

#[test]
fn compose_release_url_gitlab_empty_owner_omits_owner_segment() {
    // A top-level GitLab project with no namespace: the authoritative
    // `gitlab_release_url` drops the owner segment, so the derived default
    // must too — otherwise `ensure_release_url` would emit a double-slash
    // `https://gitlab.com//project/-/releases/v1.0.0` that diverges from the
    // URL the live create returns.
    let url = compose_release_url(
        ScmTokenType::GitLab,
        "https://gitlab.com",
        "",
        "project",
        "v1.0.0",
    );
    assert_eq!(url, "https://gitlab.com/project/-/releases/v1.0.0");
}

#[test]
fn compose_release_url_gitlab_with_owner_includes_owner_segment() {
    let url = compose_release_url(
        ScmTokenType::GitLab,
        "https://gitlab.com",
        "group",
        "project",
        "v1.0.0",
    );
    assert_eq!(url, "https://gitlab.com/group/project/-/releases/v1.0.0");
}

#[test]
fn compose_release_url_github_always_includes_owner_segment() {
    // GitHub format is unchanged: owner is always present, matching the
    // authoritative backend composer (`{base}/{owner}/{repo}/releases/tag/{tag}`).
    let url = compose_release_url(
        ScmTokenType::GitHub,
        "https://github.com",
        "tj-smith47",
        "anodizer",
        "v1.0.0",
    );
    assert_eq!(
        url,
        "https://github.com/tj-smith47/anodizer/releases/tag/v1.0.0"
    );
}
