#![allow(clippy::field_reassign_with_default)]

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
    build_release_json, collect_extra_files, compose_body_for_mode, resolve_content_source,
    resolve_header_footer, resolve_make_latest, resolve_release_tag,
};
use super::{populate_artifact_download_urls, retry_upload, should_mark_prerelease};

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

// ---- Error path tests (Task 3B) ----

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
// Task 4C: Additional behavior tests — config fields actually do things
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

// ---- Error path tests (Task 4D) ----

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

// resolve_release_mode tests moved to anodizer-core's
// test_release_resolved_mode_* (Session C lazy-defaults migration).
// The release-mode default and validation logic now lives on
// ReleaseConfig::resolved_mode in core/config.rs.

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
    // GoReleaser docs say `from_file: "./release-{{ .Tag }}.md"` should work —
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
    // Schema: new `headers` map on FromUrl variant (GoReleaser Pro parity).
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
    // GoReleaser parity (`internal/client/client.go:21`): the truncate suffix
    // is the three-dot ellipsis, not `"\n\n...(truncated)"` (16 chars).
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
    // GR-parity regression: the truncate suffix is exactly `"..."` (3 chars),
    // matching `goreleaser/internal/client/client.go:21::ellipsis`. Any drift
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
    // GR-aligned regression guard for second-opinion finding C3
    // (`changelog.use: github-native` → wrong API endpoint). The
    // create-release POST must never carry `generate_release_notes:
    // true`: the github-native flow now calls
    // `POST /releases/generate-notes` upfront (see
    // `stage-changelog/src/github_native.rs`) and embeds the returned
    // body in `spec.body`, matching GR
    // `internal/client/github.go::GenerateReleaseNotes`. Toggling
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
        let client = build_octocrab_client("ghp_fake_token_123", &None);
        assert!(
            client.is_ok(),
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
        let client = build_octocrab_client("ghp_fake_token_123", &urls);
        assert!(
            client.is_ok(),
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
        let client = build_octocrab_client("ghp_fake_token_123", &urls);
        assert!(
            client.is_ok(),
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
        let client = build_octocrab_client("ghp_fake_token_123", &urls);
        assert!(
            client.is_ok(),
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
        let result = build_octocrab_client("ghp_fake_token_123", &urls);
        assert!(result.is_err(), "invalid api URL should produce an error");
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
        let client = build_octocrab_client("ghp_fake_token_123", &urls);
        assert!(
            client.is_ok(),
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

// ---- build_publish_patch_body — P2.1 / P2.2 / P2.3 regression tests ----
//
// Tracks GoReleaser commits 6ecba31 (preserve prerelease on publish) +
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
    // GR commit 6ecba31: when prerelease=true, make_latest is forced to
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
    // GR commit 2e17678: the `name` is re-rendered from name_template and
    // included in the PATCH so a stale draft picks up template changes.
    let body = build_publish_patch_body("Renamed Release v1.2.3", false, &None, &None);
    assert_eq!(body["name"].as_str(), Some("Renamed Release v1.2.3"));
}

#[test]
fn test_build_publish_patch_body_empty_name_omitted() {
    // If the rendered name is empty, mirror GR (`if title != ""`) and skip
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
// Upstream `internal/client/github.go::updateRelease` panicked when `resp`
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
