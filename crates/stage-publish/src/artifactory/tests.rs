#![allow(clippy::field_reassign_with_default)]

use super::*;
use anodizer_core::config::{ArtifactoryConfig, Config, StringOrBool};
use anodizer_core::context::{Context, ContextOptions};
use std::path::PathBuf;

fn dry_run_ctx(config: Config) -> Context {
    Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    )
}

#[test]
fn test_artifactory_skips_when_no_config() {
    let config = Config::default();
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_skips_when_empty_vec() {
    let mut config = Config::default();
    config.artifactories = Some(vec![]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_skips_when_skipped() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_default_checksum_header_in_dry_run() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("chk".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        checksum_header: None,
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_mode_validation() {
    assert!(validate_upload_mode("archive").is_ok());
    assert!(validate_upload_mode("binary").is_ok());
    assert!(validate_upload_mode("invalid").is_err());
}

#[test]
fn test_artifactory_mode_validation_error_message() {
    let err = validate_upload_mode("foobar").unwrap_err();
    assert!(
        err.to_string().contains("invalid upload mode 'foobar'"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_artifactory_requires_target() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: None,
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    let err = publish_to_artifactory(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("missing required 'target'"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_artifactory_requires_target_nonempty() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some(String::new()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_err());
}

#[test]
fn test_artifactory_dry_run() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some("https://artifactory.example.com/repo/myapp/1.0.0/".to_string()),
        mode: Some("archive".to_string()),
        username: Some("deployer".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_dry_run_with_custom_headers() {
    let mut headers = HashMap::new();
    headers.insert("X-Custom".to_string(), "value".to_string());

    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("staging".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        custom_headers: Some(headers),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

/// Defense-in-depth: a custom header whose value is a rendered env-var
/// secret (e.g. `X-Api-Key: {{ .Env.JFROG_TOKEN }}`) must NOT leak the
/// actual token value into dry-run log output. The fix wraps the rendered
/// value in `log.redact()` before the status call.
#[test]
fn test_artifactory_dry_run_custom_header_token_is_redacted() {
    let capture = anodizer_core::log::LogCapture::new();
    let mut headers = HashMap::new();
    // Literal header value (not a template) — simulates the rendered output
    // of `{{ .Env.JFROG_TOKEN }}` after template expansion.
    headers.insert(
        "X-Api-Key".to_string(),
        "ghp_ARTIFACTORY_FAKE_SECRET_TOKEN".to_string(),
    );
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        custom_headers: Some(headers),
        ..Default::default()
    }]);
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    // Inject the secret into the template-vars env so the logger's
    // redaction engine knows to replace its value.
    ctx.template_vars_mut()
        .set_env("JFROG_TOKEN", "ghp_ARTIFACTORY_FAKE_SECRET_TOKEN");
    let log = ctx
        .logger("artifactory")
        .with_capture_handle(capture.clone());
    assert!(publish_to_artifactory(&ctx, &log).is_ok());

    let all_msgs: Vec<String> = capture.all_messages().into_iter().map(|(_, m)| m).collect();
    for msg in &all_msgs {
        assert!(
            !msg.contains("ghp_ARTIFACTORY_FAKE_SECRET_TOKEN"),
            "secret token must not appear in dry-run log output: {msg}"
        );
    }
}

#[test]
fn test_artifactory_dry_run_with_client_cert() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("secure".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        client_x509_cert: Some("/path/to/cert.pem".to_string()),
        client_x509_key: Some("/path/to/key.pem".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_invalid_mode_errors() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        mode: Some("invalid".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    let err = publish_to_artifactory(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("invalid upload mode"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_artifactory_binary_mode_accepted() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        mode: Some("binary".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_sha256_file() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.bin");
    fs::write(&file_path, b"hello world").unwrap();
    let hash = sha256_file(&file_path).unwrap();
    assert_eq!(
        hash,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

#[test]
fn test_sha256_file_missing() {
    let result = sha256_file(std::path::Path::new("/nonexistent/file.bin"));
    assert!(result.is_err());
}

#[test]
fn test_artifactory_multiple_entries() {
    let mut config = Config::default();
    config.artifactories = Some(vec![
        ArtifactoryConfig {
            name: Some("prod".to_string()),
            target: Some("https://art.example.com/prod/".to_string()),
            ..Default::default()
        },
        ArtifactoryConfig {
            name: Some("staging".to_string()),
            target: Some("https://art.example.com/staging/".to_string()),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        },
    ]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_requires_name() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: None,
        target: Some("https://art.example.com/repo/".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    let err = publish_to_artifactory(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("missing required 'name'"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_artifactory_requires_name_nonempty() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some(String::new()),
        target: Some("https://art.example.com/repo/".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    let err = publish_to_artifactory(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("missing required 'name'"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_artifactory_skips_when_skip_string_true() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        skip: Some(StringOrBool::String("true".to_string())),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_default_method_is_put() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("test".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        method: None,
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    // Should succeed with default PUT method
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_custom_method() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("test".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        method: Some("POST".to_string()),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_trusted_certificates_in_dry_run() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("test".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        trusted_certificates: Some(
            "-----BEGIN CERTIFICATE-----\nfake\n-----END CERTIFICATE-----".to_string(),
        ),
        ..Default::default()
    }]);
    let ctx = dry_run_ctx(config);
    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

#[test]
fn test_artifactory_username_without_password_errors_in_live_mode() {
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("test".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        username: Some("deployer".to_string()),
        password: None,
        ..Default::default()
    }]);
    let ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false,
            ..Default::default()
        },
    );
    let log = ctx.logger("artifactory");
    let err = publish_to_artifactory(&ctx, &log).unwrap_err();
    assert!(
        err.to_string().contains("has username set but no password"),
        "unexpected error: {}",
        err
    );
}

#[test]
fn test_artifact_kinds_for_mode_archive() {
    let kinds = artifact_kinds_for_mode("archive");
    assert!(kinds.contains(&ArtifactKind::Archive));
    assert!(kinds.contains(&ArtifactKind::SourceArchive));
    assert!(kinds.contains(&ArtifactKind::LinuxPackage));
    assert!(!kinds.contains(&ArtifactKind::UploadableBinary));
}

#[test]
fn test_artifact_kinds_for_mode_binary() {
    let kinds = artifact_kinds_for_mode("binary");
    assert!(kinds.contains(&ArtifactKind::UploadableBinary));
    assert!(!kinds.contains(&ArtifactKind::Archive));
}

#[test]
fn test_collect_upload_artifacts_by_mode() {
    let mut config = Config::default();
    config.project_name = "testapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());

    // Add archive artifact
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/testapp-1.0.0.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "testapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    // Add binary artifact
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::UploadableBinary,
        name: String::new(),
        path: PathBuf::from("dist/testapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "testapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    // Archive mode should find archive but not binary
    let archive_arts =
        collect_upload_artifacts(&ctx, "archive", None, None, None, CollectFlags::default());
    assert_eq!(archive_arts.len(), 1);
    assert_eq!(archive_arts[0].kind, ArtifactKind::Archive);

    // Binary mode should find binary but not archive
    let binary_arts =
        collect_upload_artifacts(&ctx, "binary", None, None, None, CollectFlags::default());
    assert_eq!(binary_arts.len(), 1);
    assert_eq!(binary_arts[0].kind, ArtifactKind::UploadableBinary);
}

#[test]
fn test_collect_upload_artifacts_excludes_appbundle_directory() {
    use anodizer_core::artifact::{FORMAT_APPBUNDLE, FORMAT_META};

    let mut config = Config::default();
    config.project_name = "testapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());

    // A macOS `.app` bundle: Installer kind + format=appbundle, a directory.
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Installer,
        name: "TestApp.app".to_string(),
        path: PathBuf::from("dist/TestApp.app"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "testapp".to_string(),
        metadata: HashMap::from([(FORMAT_META.to_string(), FORMAT_APPBUNDLE.to_string())]),
        size: None,
    });
    // A sibling Installer FILE (e.g. an MSI) must still be uploaded.
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Installer,
        name: "testapp-1.0.0.msi".to_string(),
        path: PathBuf::from("dist/testapp-1.0.0.msi"),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "testapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let arts = collect_upload_artifacts(&ctx, "archive", None, None, None, CollectFlags::default());
    assert_eq!(
        arts.len(),
        1,
        "the directory `.app` bundle must be excluded, the MSI file kept"
    );
    assert_eq!(arts[0].name(), "testapp-1.0.0.msi");
    assert!(
        !arts
            .iter()
            .any(|a| anodizer_core::artifact::is_directory_bundle_artifact(a)),
        "no directory bundle may survive selection"
    );
}

#[test]
fn test_collect_upload_artifacts_with_ext_filter() {
    let mut config = Config::default();
    config.project_name = "testapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: "testapp-1.0.0.tar.gz".to_string(),
        path: PathBuf::from("dist/testapp-1.0.0.tar.gz"),
        target: None,
        crate_name: "testapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: "testapp-1.0.0.zip".to_string(),
        path: PathBuf::from("dist/testapp-1.0.0.zip"),
        target: None,
        crate_name: "testapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let exts = vec!["zip".to_string()];
    let arts = collect_upload_artifacts(
        &ctx,
        "archive",
        None,
        None,
        Some(&exts),
        CollectFlags::default(),
    );
    assert_eq!(arts.len(), 1);
    assert!(arts[0].name().ends_with(".zip"));
}

#[test]
fn test_collect_upload_artifacts_includes_checksums() {
    let mut config = Config::default();
    config.project_name = "testapp".to_string();
    let mut ctx = Context::new(config, ContextOptions::default());

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/testapp-1.0.0.tar.gz"),
        target: None,
        crate_name: "testapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: PathBuf::from("dist/checksums.txt"),
        target: None,
        crate_name: "testapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    // Without include_checksum
    let arts = collect_upload_artifacts(&ctx, "archive", None, None, None, CollectFlags::default());
    assert_eq!(arts.len(), 1);

    // With include_checksum
    let arts = collect_upload_artifacts(
        &ctx,
        "archive",
        None,
        None,
        None,
        CollectFlags {
            checksum: true,
            ..CollectFlags::default()
        },
    );
    assert_eq!(arts.len(), 2);
}

#[test]
fn test_render_artifact_url_appends_name() {
    let config = Config::default();
    let ctx = Context::new(config, ContextOptions::default());
    let artifact = Artifact {
        kind: ArtifactKind::Archive,
        name: "myapp-1.0.0.tar.gz".to_string(),
        path: PathBuf::from("dist/myapp-1.0.0.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    };

    // Without custom_artifact_name, appends artifact name to URL
    let url = render_artifact_url(&ctx, "https://art.example.com/repo", &artifact, false).unwrap();
    assert!(url.ends_with("/myapp-1.0.0.tar.gz"));

    // With custom_artifact_name, does NOT append
    let url = render_artifact_url(&ctx, "https://art.example.com/repo", &artifact, true).unwrap();
    assert!(!url.ends_with("/myapp-1.0.0.tar.gz"));
}

fn deb_artifact(name: &str, target: Option<&str>) -> Artifact {
    Artifact {
        kind: ArtifactKind::LinuxPackage,
        name: name.to_string(),
        path: PathBuf::from(format!("dist/linux/{name}")),
        target: target.map(str::to_string),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    }
}

/// A `.deb` with default config gets `stable`/`main` and a target-derived
/// architecture appended as Artifactory matrix params — the exact shape
/// JFrog's Debian-repo upload docs require for indexing.
#[test]
fn deb_matrix_params_default_amd64() {
    let entry = ArtifactoryConfig::default();
    let art = deb_artifact("myapp_1.0.0_amd64.deb", Some("x86_64-unknown-linux-gnu"));
    let url = append_deb_matrix_params("https://art.example.com/deb-repo/myapp.deb", &art, &entry)
        .unwrap();
    assert_eq!(
        url,
        "https://art.example.com/deb-repo/myapp.deb;deb.distribution=stable;deb.component=main;deb.architecture=amd64"
    );
}

/// arm64 / armhf / i386 derive the correct Debian arch from the target.
#[test]
fn deb_matrix_params_derives_debian_arch() {
    let entry = ArtifactoryConfig::default();
    for (target, want) in [
        ("aarch64-unknown-linux-gnu", "arm64"),
        ("armv7-unknown-linux-gnueabihf", "armhf"),
        ("i686-unknown-linux-gnu", "i386"),
    ] {
        let art = deb_artifact("p.deb", Some(target));
        let url = append_deb_matrix_params("https://a/p.deb", &art, &entry).unwrap();
        assert!(
            url.ends_with(&format!(";deb.architecture={want}")),
            "target {target} → {want}, got: {url}"
        );
    }
}

/// Multiple distributions/components are emitted comma-separated
/// (Artifactory's list form), and an explicit architecture override wins
/// over the target-derived value.
#[test]
fn deb_matrix_params_multi_and_override() {
    let entry = ArtifactoryConfig {
        deb_distributions: Some(vec!["bookworm".to_string(), "bullseye".to_string()]),
        deb_components: Some(vec!["main".to_string(), "contrib".to_string()]),
        deb_architecture: Some("all".to_string()),
        ..Default::default()
    };
    let art = deb_artifact("p.deb", Some("x86_64-unknown-linux-gnu"));
    let url = append_deb_matrix_params("https://a/p.deb", &art, &entry).unwrap();
    assert_eq!(
        url,
        "https://a/p.deb;deb.distribution=bookworm,bullseye;deb.component=main,contrib;deb.architecture=all"
    );
}

/// A `.deb` with no target and no override omits the architecture param
/// (Artifactory then reads it from the package control file) but still
/// carries distribution + component.
#[test]
fn deb_matrix_params_no_target_omits_arch() {
    let entry = ArtifactoryConfig::default();
    let art = deb_artifact("p.deb", None);
    let url = append_deb_matrix_params("https://a/p.deb", &art, &entry).unwrap();
    assert_eq!(
        url,
        "https://a/p.deb;deb.distribution=stable;deb.component=main"
    );
}

/// Non-`.deb` artifacts are never touched — no matrix params, URL
/// returned verbatim (rpm/archive uploads keep their plain PUT path).
#[test]
fn deb_matrix_params_noop_for_non_deb() {
    let entry = ArtifactoryConfig::default();
    for name in ["myapp-1.0.0.tar.gz", "myapp-1.0.0.x86_64.rpm", "myapp.apk"] {
        let art = deb_artifact(name, Some("x86_64-unknown-linux-gnu"));
        let url = append_deb_matrix_params("https://a/repo/file", &art, &entry).unwrap();
        assert_eq!(url, "https://a/repo/file", "{name} must be untouched");
    }
}

/// A distribution/component slug containing matrix-param-breaking
/// characters (`;`, whitespace, `/`) hard-errors with an actionable
/// message, before any upload — so a corrupt slug can't silently land the
/// .deb at the wrong path.
#[test]
fn deb_matrix_slug_validation_rejects_breaking_chars() {
    for bad in ["stable;evil=1", "two words", "deb/bookworm", "with\ttab"] {
        let err = validate_deb_matrix_slug("deb_distributions", bad)
            .expect_err(&format!("'{bad}' must be rejected"));
        let msg = err.to_string();
        assert!(msg.contains("deb_distributions"), "names the field: {msg}");
        assert!(msg.contains(bad), "quotes the bad value: {msg}");
        assert!(
            msg.contains("matrix") || msg.contains("codename"),
            "explains the fix: {msg}"
        );
    }
}

/// The normal Debian distribution/component charset (alnum + `-`/`.`/`_`)
/// passes.
#[test]
fn deb_matrix_slug_validation_accepts_valid_slugs() {
    for ok in [
        "bookworm",
        "stable",
        "bullseye-backports",
        "1.0",
        "main",
        "non_free",
    ] {
        assert!(
            validate_deb_matrix_slug("deb_components", ok).is_ok(),
            "'{ok}' should be accepted"
        );
    }
}

/// Entry-level validation rejects a bad slug anywhere in the
/// distribution/component lists or the architecture override.
#[test]
fn artifactory_deb_slug_entry_validation() {
    let bad_dist = ArtifactoryConfig {
        deb_distributions: Some(vec!["stable".to_string(), "bad;x=1".to_string()]),
        ..Default::default()
    };
    assert!(validate_artifactory_deb_slugs(&bad_dist).is_err());

    let bad_comp = ArtifactoryConfig {
        deb_components: Some(vec!["main with space".to_string()]),
        ..Default::default()
    };
    assert!(validate_artifactory_deb_slugs(&bad_comp).is_err());

    let bad_arch = ArtifactoryConfig {
        deb_architecture: Some("amd64;rm".to_string()),
        ..Default::default()
    };
    assert!(validate_artifactory_deb_slugs(&bad_arch).is_err());

    let good = ArtifactoryConfig {
        deb_distributions: Some(vec!["bookworm".to_string(), "bullseye".to_string()]),
        deb_components: Some(vec!["main".to_string(), "contrib".to_string()]),
        deb_architecture: Some("arm64".to_string()),
        ..Default::default()
    };
    assert!(validate_artifactory_deb_slugs(&good).is_ok());
}

/// An empty or whitespace-only slug must be rejected: it joins into a
/// trailing-comma matrix param (e.g. `deb.distribution=bookworm,`) that
/// mis-indexes the .deb into an empty-named distribution. The charset check
/// alone would pass `""` vacuously, so the emptiness guard must run first.
#[test]
fn deb_matrix_slug_validation_rejects_empty_and_whitespace() {
    for bad in ["", "  ", "\t"] {
        let err = validate_deb_matrix_slug("deb_distributions", bad)
            .expect_err(&format!("{bad:?} must be rejected"));
        let msg = err.to_string();
        assert!(msg.contains("deb_distributions"), "names the field: {msg}");
        assert!(msg.contains("empty"), "explains it is empty: {msg}");
        assert!(
            msg.contains("trailing-comma") || msg.contains("codename"),
            "explains the fix: {msg}"
        );
    }
}

/// Entry-level validation propagates the emptiness rejection: a list with
/// `["bookworm", ""]` (or a whitespace-only element) must hard-error.
#[test]
fn artifactory_deb_slug_entry_rejects_empty_list_element() {
    let empty_in_dist = ArtifactoryConfig {
        deb_distributions: Some(vec!["bookworm".to_string(), "".to_string()]),
        ..Default::default()
    };
    assert!(validate_artifactory_deb_slugs(&empty_in_dist).is_err());

    let ws_in_comp = ArtifactoryConfig {
        deb_components: Some(vec!["main".to_string(), "  ".to_string()]),
        ..Default::default()
    };
    assert!(validate_artifactory_deb_slugs(&ws_in_comp).is_err());
}

/// An unmapped / exotic build target (e.g. a user-supplied `prebuilt`
/// triple) must HARD-ERROR when its Debian arch is derived for the matrix
/// param, never silently inject a raw triple fragment as
/// `deb.architecture=` (which would mis-index the .deb into a wrong slice).
#[test]
fn deb_matrix_params_unmapped_target_hard_errors() {
    let entry = ArtifactoryConfig::default();
    let art = deb_artifact("p.deb", Some("frob-unknown-linux-gnu"));
    let err = append_deb_matrix_params("https://a/p.deb", &art, &entry)
        .expect_err("unmapped target must hard-fail, not inject a raw fragment");
    let msg = err.to_string();
    assert!(
        msg.contains("frob-unknown-linux-gnu"),
        "names the offending triple: {msg}"
    );
    assert!(
        msg.contains("deb_architecture"),
        "names the override field as the fix: {msg}"
    );
}

#[test]
fn render_artifact_url_interpolates_os_arch_target_ext() {
    let config = Config::default();
    let ctx = Context::new(config, ContextOptions::default());
    let artifact = Artifact {
        kind: ArtifactKind::Archive,
        name: "myapp-1.0.0.tar.gz".to_string(),
        path: PathBuf::from("dist/myapp-1.0.0.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    };
    // ArtifactExt already carries its leading dot (".tar.gz").
    let url = render_artifact_url(
        &ctx,
        "https://art.example.com/{{ .Os }}/{{ .Arch }}/{{ .Target }}{{ .ArtifactExt }}",
        &artifact,
        true,
    )
    .unwrap();
    assert_eq!(
        url,
        "https://art.example.com/linux/amd64/x86_64-unknown-linux-gnu.tar.gz"
    );
}

#[test]
fn render_artifact_url_template_referencing_artifact_name_suppresses_append() {
    let config = Config::default();
    let ctx = Context::new(config, ContextOptions::default());
    let artifact = Artifact {
        kind: ArtifactKind::Archive,
        name: "myapp-1.0.0.tar.gz".to_string(),
        path: PathBuf::from("dist/myapp-1.0.0.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    };
    // custom_artifact_name=false BUT the template names ArtifactName ->
    // no second append (the name appears exactly once).
    let url = render_artifact_url(
        &ctx,
        "https://art.example.com/repo/{{ .ArtifactName }}",
        &artifact,
        false,
    )
    .unwrap();
    assert_eq!(url, "https://art.example.com/repo/myapp-1.0.0.tar.gz");
}

#[test]
fn render_artifact_url_keeps_single_slash_when_template_trailing_slashed() {
    let config = Config::default();
    let ctx = Context::new(config, ContextOptions::default());
    let artifact = Artifact {
        kind: ArtifactKind::Archive,
        name: "myapp.tar.gz".to_string(),
        path: PathBuf::from("dist/myapp.tar.gz"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    };
    let url = render_artifact_url(&ctx, "https://art.example.com/repo/", &artifact, false).unwrap();
    assert_eq!(url, "https://art.example.com/repo/myapp.tar.gz");
}

#[test]
fn test_dry_run_lists_matching_artifacts() {
    let mut config = Config::default();
    config.project_name = "testapp".to_string();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some("https://art.example.com/repo/".to_string()),
        ..Default::default()
    }]);
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/testapp-1.0.0.tar.gz"),
        target: None,
        crate_name: "testapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let log = ctx.logger("artifactory");
    assert!(publish_to_artifactory(&ctx, &log).is_ok());
}

/// Defense-in-depth: an Artifactory response body that echoes the
/// `Authorization: Bearer <PAT>` header back must not leak the token
/// into the user-visible error chain. The decode helper sits on the
/// JSON-parse fallback, raw-body fallback, and joined-output paths;
/// this test pins all three.
#[test]
fn decode_artifactory_error_body_redacts_bearer_tokens() {
    // Path 1: raw body when JSON parsing fails entirely.
    let raw = "plain-text error: Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg leaked";
    let out = decode_artifactory_error_body(raw);
    assert!(
        !out.contains("ghp_FAKETOKEN1234567890abcdefg"),
        "raw fallback: {out}"
    );
    assert!(
        out.contains("<redacted>"),
        "raw fallback should contain redaction marker: {out}"
    );

    // Path 2: JSON without the expected `errors` envelope.
    let no_errors = r#"{"trace":"Bearer ghp_FAKETOKEN1234567890abcdefg"}"#;
    let out = decode_artifactory_error_body(no_errors);
    assert!(
        !out.contains("ghp_FAKETOKEN1234567890abcdefg"),
        "no-errors fallback: {out}"
    );
    assert!(
        out.contains("<redacted>"),
        "no-errors fallback should contain redaction marker: {out}"
    );

    // Path 3: well-formed envelope where the message itself echoes the
    // bearer token (the realistic Artifactory misbehaviour).
    let envelope = r#"{"errors":[{"status":401,"message":"bad header Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg"}]}"#;
    let out = decode_artifactory_error_body(envelope);
    assert!(
        !out.contains("ghp_FAKETOKEN1234567890abcdefg"),
        "joined path: {out}"
    );
    assert!(
        out.contains("<redacted>"),
        "joined path should contain redaction marker: {out}"
    );
    // The non-secret prefix of the message is preserved so debugging
    // doesn't lose the upstream-supplied context.
    assert!(
        out.contains("status=401"),
        "status should survive redaction: {out}"
    );
}

// -----------------------------------------------------------------
// Live HTTP path tests (scripted responder)
//
// The in-process responder records (method, path, body) for every
// request. Header capture is not available, so credential/checksum
// header assertions go through `resolve_http_credentials` directly
// (covered below) while the wire-shape assertions here pin method,
// path, uploaded body bytes, and retry count.
// -----------------------------------------------------------------

use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::test_helpers::scripted_responder::{ScriptedRoute, spawn_scripted_responder};
use std::net::SocketAddr;
use std::time::Duration;

fn fast_policy(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
    }
}

/// Build a throwaway file artifact on disk so `upload_single_artifact`
/// can hash + read it. Returns the tempdir guard (keep alive) and the
/// constructed `Artifact`.
fn file_artifact(contents: &[u8], name: &str) -> (tempfile::TempDir, Artifact) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    fs::write(&path, contents).unwrap();
    let art = Artifact {
        kind: ArtifactKind::Archive,
        name: name.to_string(),
        path,
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    };
    (dir, art)
}

fn upload_ctx() -> Context {
    Context::new(Config::default(), ContextOptions::default())
}

fn no_headers() -> HashMap<String, String> {
    HashMap::new()
}

/// PUT upload to a 201-route: the responder records exactly one
/// request, with method PUT, the rendered path, and the file bytes as
/// the body.
#[test]
fn upload_put_sends_file_body_to_target() {
    let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "PUT",
        path_pattern: "/repo/myapp.tar.gz",
        response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let (_dir, art) = file_artifact(b"payload-bytes", "myapp.tar.gz");
    let ctx = upload_ctx();
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let url = format!("http://{addr}/repo/myapp.tar.gz");
    let custom = no_headers();
    let client = build_reqwest_client(None, None, None).unwrap();
    upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "PUT",
            url: &url,
            checksum_header: "X-Checksum-SHA256",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        true,
        &ctx,
        &fast_policy(3),
        &log,
    )
    .expect("201 upload succeeds");

    let entries = log_recorder.lock().unwrap();
    assert_eq!(entries.len(), 1, "exactly one request: {entries:?}");
    assert_eq!(entries[0].method, "PUT");
    assert_eq!(entries[0].path, "/repo/myapp.tar.gz");
    assert_eq!(
        entries[0].body, "payload-bytes",
        "the file bytes are the request body"
    );
}

/// POST method routes the request as a POST (not PUT). Pins that the
/// configured method actually selects `client.post`.
#[test]
fn upload_post_uses_post_verb() {
    let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "POST",
        path_pattern: "/repo/app.bin",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let (_dir, art) = file_artifact(b"x", "app.bin");
    let ctx = upload_ctx();
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let url = format!("http://{addr}/repo/app.bin");
    let custom = no_headers();
    let client = build_reqwest_client(None, None, None).unwrap();
    upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "POST",
            url: &url,
            checksum_header: "",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        true,
        &ctx,
        &fast_policy(1),
        &log,
    )
    .expect("200 POST succeeds");
    let entries = log_recorder.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].method, "POST");
}

/// A 503 on the first attempt retries and the second attempt (200)
/// succeeds — exactly two requests reach the wire.
#[test]
fn upload_retries_5xx_then_succeeds() {
    let (addr, log_recorder) = spawn_scripted_responder(vec![
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/repo/r.tar.gz",
            response: "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            times: Some(1),
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/repo/r.tar.gz",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
    ]);
    let (_dir, art) = file_artifact(b"retry-body", "r.tar.gz");
    let ctx = upload_ctx();
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let url = format!("http://{addr}/repo/r.tar.gz");
    let custom = no_headers();
    let client = build_reqwest_client(None, None, None).unwrap();
    upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "PUT",
            url: &url,
            checksum_header: "X-Checksum-SHA256",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        true,
        &ctx,
        &fast_policy(3),
        &log,
    )
    .expect("retry recovers from 503");
    let entries = log_recorder.lock().unwrap();
    assert_eq!(entries.len(), 2, "one 503 + one 201 = two attempts");
}

/// 5xx on every attempt exhausts the retry budget and surfaces an
/// error naming the artifact, method, and status. The number of
/// requests equals `max_attempts`.
#[test]
fn upload_5xx_exhausts_retries_and_errors() {
    let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "PUT",
        path_pattern: "/repo/e.tar.gz",
        response: "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let (_dir, art) = file_artifact(b"e", "e.tar.gz");
    let ctx = upload_ctx();
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let url = format!("http://{addr}/repo/e.tar.gz");
    let custom = no_headers();
    let client = build_reqwest_client(None, None, None).unwrap();
    let err = upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "PUT",
            url: &url,
            checksum_header: "X-Checksum-SHA256",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        true,
        &ctx,
        &fast_policy(3),
        &log,
    )
    .expect_err("persistent 500 must exhaust and error");
    let chain = format!("{err:#}");
    assert!(chain.contains("e.tar.gz"), "names artifact: {chain}");
    assert!(chain.contains("500"), "carries upstream status: {chain}");

    let entries = log_recorder.lock().unwrap();
    assert_eq!(entries.len(), 3, "all three attempts hit the wire");
}

/// A 4xx (e.g. 403) fast-fails: no retry, exactly one request, and the
/// decoded Artifactory error envelope reaches the error message.
#[test]
fn upload_4xx_fast_fails_without_retry() {
    let body = r#"{"errors":[{"status":403,"message":"forbidden path"}]}"#;
    let resp: &'static str = Box::leak(
        format!(
            "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_boxed_str(),
    );
    let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "PUT",
        path_pattern: "/repo/f.tar.gz",
        response: resp,
        times: None,
    }]);
    let (_dir, art) = file_artifact(b"f", "f.tar.gz");
    let ctx = upload_ctx();
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let url = format!("http://{addr}/repo/f.tar.gz");
    let custom = no_headers();
    let client = build_reqwest_client(None, None, None).unwrap();
    let err = upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "PUT",
            url: &url,
            checksum_header: "X-Checksum-SHA256",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        true,
        &ctx,
        &fast_policy(5),
        &log,
    )
    .expect_err("403 must fast-fail");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("forbidden path"),
        "decoded envelope message present: {chain}"
    );
    assert!(chain.contains("403"), "status present: {chain}");

    let entries = log_recorder.lock().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "4xx must NOT retry despite max_attempts=5: {entries:?}"
    );
}

/// An unsupported HTTP method fails fast OUTSIDE the retry loop — no
/// request is ever sent.
#[test]
fn upload_rejects_unsupported_method() {
    let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "PUT",
        path_pattern: "/repo/x.tar.gz",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let (_dir, art) = file_artifact(b"x", "x.tar.gz");
    let ctx = upload_ctx();
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let url = format!("http://{addr}/repo/x.tar.gz");
    let custom = no_headers();
    let client = build_reqwest_client(None, None, None).unwrap();
    let err = upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "DELETE",
            url: &url,
            checksum_header: "",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        true,
        &ctx,
        &fast_policy(3),
        &log,
    )
    .expect_err("DELETE is not a supported upload method");
    assert!(
        err.to_string().contains("unsupported HTTP method"),
        "unexpected: {err}"
    );
    assert!(
        log_recorder.lock().unwrap().is_empty(),
        "no request must reach the wire for a bad method"
    );
}

// The full upload URL is a request echo: at default verbosity an upload
// emits exactly one concise per-artifact RESULT line; the destination URL
// and HTTP status code ride at verbose.
#[test]
fn upload_routes_url_echo_to_verbose_keeps_concise_result_at_default() {
    use anodizer_core::log::LogLevel;

    let (addr, _rec) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "PUT",
        path_pattern: "/repo/myapp.tar.gz",
        response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let (_dir, art) = file_artifact(b"payload-bytes", "myapp.tar.gz");
    let ctx = upload_ctx();
    let (log, cap) = StageLogger::with_capture("artifactory", Verbosity::Normal);
    let url = format!("http://{addr}/repo/myapp.tar.gz");
    let custom = no_headers();
    let client = build_reqwest_client(None, None, None).unwrap();
    upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "PUT",
            url: &url,
            checksum_header: "X-Checksum-SHA256",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        true,
        &ctx,
        &fast_policy(3),
        &log,
    )
    .expect("201 upload succeeds");

    let status: Vec<String> = cap
        .all_messages()
        .into_iter()
        .filter(|(lvl, _)| *lvl == LogLevel::Status)
        .map(|(_, m)| m)
        .collect();
    let verbose: Vec<String> = cap
        .all_messages()
        .into_iter()
        .filter(|(lvl, _)| *lvl == LogLevel::Verbose)
        .map(|(_, m)| m)
        .collect();

    // Per-artifact upload detail is verbose-only — the default-verbosity
    // RESULT for an upload entry is the aggregate count-summary the shared
    // driver emits (see `upload_summary`), not one line per artifact. So
    // `upload_single_artifact_prepared` emits nothing at Status.
    assert!(
        status.is_empty(),
        "no per-artifact line at default; the entry summary carries the \
         RESULT: {status:?}"
    );
    assert!(
        verbose
            .iter()
            .any(|m| m.contains(&url) && m.contains("uploading")),
        "the request echo (with URL) must ride at verbose: {verbose:?}"
    );
    assert!(
        verbose
            .iter()
            .any(|m| m.contains("uploaded myapp.tar.gz") && m.contains("201")),
        "the per-artifact upload (with status code) rides at verbose: {verbose:?}"
    );
}

// An idempotent skip keeps a concise default RESULT; the matched URL is
// verbose-only.
#[test]
fn upload_skip_keeps_concise_result_url_at_verbose() {
    use anodizer_core::log::LogLevel;

    let (_dir, art) = file_artifact(b"already-there", "dup.tar.gz");
    // The remote checksum the HEAD probe reports must equal the local
    // file's sha256 to drive PresentMatching → skip.
    let checksum = sha256_file(&art.path).unwrap();
    let head_resp: &'static str = Box::leak(
        format!("HTTP/1.1 200 OK\r\nX-Checksum-Sha256: {checksum}\r\nContent-Length: 0\r\n\r\n")
            .into_boxed_str(),
    );
    let (addr, _rec) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "HEAD",
        path_pattern: "/repo/dup.tar.gz",
        response: head_resp,
        times: None,
    }]);
    let ctx = upload_ctx();
    let (log, cap) = StageLogger::with_capture("artifactory", Verbosity::Normal);
    let url = format!("http://{addr}/repo/dup.tar.gz");
    let custom = no_headers();
    let client = build_reqwest_client(None, None, None).unwrap();
    let outcome = upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "PUT",
            url: &url,
            checksum_header: "X-Checksum-SHA256",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        false,
        &ctx,
        &fast_policy(3),
        &log,
    )
    .expect("identical artifact present ⇒ idempotent skip");
    assert!(matches!(outcome, UploadOutcome::AlreadyPresent));

    let status: Vec<String> = cap
        .all_messages()
        .into_iter()
        .filter(|(lvl, _)| *lvl == LogLevel::Status)
        .map(|(_, m)| m)
        .collect();
    let verbose: Vec<String> = cap
        .all_messages()
        .into_iter()
        .filter(|(lvl, _)| *lvl == LogLevel::Verbose)
        .map(|(_, m)| m)
        .collect();

    // The idempotent-skip detail is verbose-only; the default-verbosity
    // RESULT is the entry's aggregate count-summary (driver-emitted), so
    // `upload_single_artifact_prepared` itself emits nothing at Status.
    assert!(
        status.is_empty(),
        "no per-artifact skip line at default; the entry summary carries \
         the RESULT: {status:?}"
    );
    assert!(
        verbose
            .iter()
            .any(|m| m.contains("skipped dup.tar.gz") && m.contains(&url)),
        "the per-artifact skip (with matched URL) rides at verbose: {verbose:?}"
    );
}

/// A missing artifact file bails before any network activity.
#[test]
fn upload_missing_file_bails() {
    let ctx = upload_ctx();
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let art = Artifact {
        kind: ArtifactKind::Archive,
        name: "gone.tar.gz".to_string(),
        path: PathBuf::from("/nonexistent/anodizer/gone.tar.gz"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    };
    let custom = no_headers();
    let client = build_reqwest_client(None, None, None).unwrap();
    let err = upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "PUT",
            url: "http://127.0.0.1:1/repo/gone.tar.gz",
            checksum_header: "",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        true,
        &ctx,
        &fast_policy(1),
        &log,
    )
    .expect_err("missing file must bail");
    assert!(
        err.to_string().contains("artifact file not found"),
        "unexpected: {err}"
    );
}

/// A directory passed as an artifact path is rejected (can't upload a
/// directory) before any network call.
#[test]
fn upload_directory_path_bails() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = upload_ctx();
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let art = Artifact {
        kind: ArtifactKind::Archive,
        name: "adir".to_string(),
        path: dir.path().to_path_buf(),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    };
    let custom = no_headers();
    let client = build_reqwest_client(None, None, None).unwrap();
    let err = upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "PUT",
            url: "http://127.0.0.1:1/repo/adir",
            checksum_header: "",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        true,
        &ctx,
        &fast_policy(1),
        &log,
    )
    .expect_err("directory upload must bail");
    assert!(
        err.to_string().contains("can't be a directory"),
        "unexpected: {err}"
    );
}

/// A custom header carrying broken template syntax fails fast (outside
/// the retry loop) rather than pushing an unrendered `{{ }}` literal
/// onto the wire.
#[test]
fn upload_bad_custom_header_template_fails_fast() {
    let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "PUT",
        path_pattern: "/repo/h.tar.gz",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let (_dir, art) = file_artifact(b"h", "h.tar.gz");
    let ctx = upload_ctx();
    let log = StageLogger::new("artifactory", Verbosity::Quiet);
    let url = format!("http://{addr}/repo/h.tar.gz");
    let mut custom = HashMap::new();
    // Unknown filter is a hard render error (not undefined-var leniency).
    custom.insert(
        "X-Bad".to_string(),
        "{{ ArtifactName | nonexistent_filter }}".to_string(),
    );
    let client = build_reqwest_client(None, None, None).unwrap();
    let err = upload_single_artifact(
        &client,
        &UploadHeaders {
            publisher: "artifactory",
            method: "PUT",
            url: &url,
            checksum_header: "",
        },
        &UploadAuth {
            username: "",
            password: "",
        },
        &custom,
        &art,
        true,
        &ctx,
        &fast_policy(3),
        &log,
    )
    .expect_err("bad header template must fail-fast");
    assert!(
        err.to_string().contains("custom header 'X-Bad'"),
        "unexpected: {err}"
    );
    assert!(
        log_recorder.lock().unwrap().is_empty(),
        "render failure must abort before any request"
    );
}

/// End-to-end through `publish_to_artifactory` in LIVE mode: the
/// per-entry name + ArtifactName rendering produces the correct PUT
/// path against the responder, exercising client build + render +
/// upload in one flow. Credentials come from config (so no env race).
#[test]
fn publish_live_uploads_artifact_to_responder() {
    let (addr, log_recorder) = spawn_scripted_responder(vec![
        // Idempotency probe: path is empty (404) → upload proceeds.
        ScriptedRoute {
            method: "HEAD",
            path_pattern: "/repo/live-app.tar.gz",
            response: "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
        ScriptedRoute {
            method: "PUT",
            path_pattern: "/repo/live-app.tar.gz",
            response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
            times: None,
        },
    ]);
    let dir = tempfile::tempdir().unwrap();
    let art_path = dir.path().join("live-app.tar.gz");
    fs::write(&art_path, b"live-bytes").unwrap();

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.retry = Some(anodizer_core::config::RetryConfig {
        attempts: 2,
        delay: anodizer_core::config::HumanDuration(Duration::from_millis(1)),
        max_delay: anodizer_core::config::HumanDuration(Duration::from_millis(2)),
        max_elapsed: None,
    });
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some(format!("http://{addr}/repo/")),
        username: Some("deployer".to_string()),
        password: Some("hunter2".to_string()),
        ..Default::default()
    }]);
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false,
            ..Default::default()
        },
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: "live-app.tar.gz".to_string(),
        path: art_path,
        target: None,
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    let log = ctx.logger("artifactory");
    publish_to_artifactory(&ctx, &log).expect("live publish succeeds");

    let entries = log_recorder.lock().unwrap();
    assert_eq!(entries.len(), 2, "HEAD probe + PUT upload: {entries:?}");
    assert_eq!(entries[0].method, "HEAD", "presence probe first");
    assert_eq!(entries[0].path, "/repo/live-app.tar.gz");
    assert_eq!(entries[1].method, "PUT");
    assert_eq!(entries[1].path, "/repo/live-app.tar.gz");
    assert_eq!(entries[1].body, "live-bytes");
}

/// Build a single-artifact live publish context against `addr`, with the
/// given `overwrite` setting. Returns the context and the on-disk bytes'
/// hex SHA-256 so the test can script a matching / differing HEAD probe.
fn live_publish_ctx(addr: SocketAddr, overwrite: Option<bool>) -> (Context, String) {
    let dir = tempfile::tempdir().unwrap();
    let art_path = dir.path().join("idem-app.tar.gz");
    fs::write(&art_path, b"idem-bytes").unwrap();
    let checksum = sha256_file(&art_path).unwrap();

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.retry = Some(anodizer_core::config::RetryConfig {
        attempts: 2,
        delay: anodizer_core::config::HumanDuration(Duration::from_millis(1)),
        max_delay: anodizer_core::config::HumanDuration(Duration::from_millis(2)),
        max_elapsed: None,
    });
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some(format!("http://{addr}/repo/")),
        username: Some("deployer".to_string()),
        password: Some("hunter2".to_string()),
        overwrite,
        ..Default::default()
    }]);
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false,
            ..Default::default()
        },
    );
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: "idem-app.tar.gz".to_string(),
        path: art_path,
        target: None,
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    // Keep the tempdir alive for the duration of the test by leaking it;
    // the file must outlive the upload read. Tests are short-lived.
    std::mem::forget(dir);
    (ctx, checksum)
}

/// Idempotent re-run: the path already holds an artifact whose sha256
/// matches the local file → HEAD probe returns the match, the PUT is
/// skipped entirely, and the run is a no-op upload.
#[test]
fn publish_skips_when_identical_artifact_already_present() {
    // Bind first so the responder can echo back the right checksum header.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (ctx, checksum) = live_publish_ctx(addr, None);
    let head_resp: &'static str = Box::leak(
        format!("HTTP/1.1 200 OK\r\nX-Checksum-Sha256: {checksum}\r\nContent-Length: 0\r\n\r\n")
            .into_boxed_str(),
    );
    let (_addr, log_recorder) =
        anodizer_core::test_helpers::scripted_responder::spawn_scripted_responder_on(
            listener,
            move |_| {
                vec![ScriptedRoute {
                    method: "HEAD",
                    path_pattern: "/repo/idem-app.tar.gz",
                    response: head_resp,
                    times: None,
                }]
            },
        );

    let log = ctx.logger("artifactory");
    let summary = publish_to_artifactory(&ctx, &log).expect("idempotent re-run is ok");
    assert_eq!(summary.uploaded, 0, "nothing uploaded");
    assert_eq!(summary.already_present, 1, "one artifact skipped");
    assert!(summary.is_fully_idempotent_skip());

    let entries = log_recorder.lock().unwrap();
    assert_eq!(
        entries.len(),
        1,
        "only the HEAD probe — no PUT: {entries:?}"
    );
    assert_eq!(entries[0].method, "HEAD");
}

/// A path already holding a *different* artifact for the same version is
/// immutable-version drift: the publish must hard-error, NOT silently
/// overwrite, when `overwrite` is unset.
#[test]
fn publish_bails_on_content_drift_without_overwrite() {
    let (addr, _log) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "HEAD",
        path_pattern: "/repo/idem-app.tar.gz",
        response: "HTTP/1.1 200 OK\r\nX-Checksum-Sha256: \
                   0000000000000000000000000000000000000000000000000000000000000000\
                   \r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let (ctx, _checksum) = live_publish_ctx(addr, None);
    let log = ctx.logger("artifactory");
    let err = publish_to_artifactory(&ctx, &log).expect_err("content drift must error");
    let chain = format!("{err:#}");
    assert!(chain.contains("different sha256"), "{chain}");
    assert!(chain.contains("overwrite: true"), "{chain}");
}

/// `overwrite: true` skips the existence probe and PUTs unconditionally —
/// restoring blind-overwrite for repos that allow it.
#[test]
fn publish_overwrite_true_skips_probe_and_puts() {
    let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "PUT",
        path_pattern: "/repo/idem-app.tar.gz",
        response: "HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let (ctx, _checksum) = live_publish_ctx(addr, Some(true));
    let log = ctx.logger("artifactory");
    let summary = publish_to_artifactory(&ctx, &log).expect("overwrite publish ok");
    assert_eq!(summary.uploaded, 1);
    assert_eq!(summary.already_present, 0);

    let entries = log_recorder.lock().unwrap();
    assert_eq!(entries.len(), 1, "no HEAD probe, just the PUT: {entries:?}");
    assert_eq!(entries[0].method, "PUT");
}

/// Live mode with no matching artifacts short-circuits without firing
/// any HTTP request (the "no matching artifacts" branch).
#[test]
fn publish_live_no_artifacts_makes_no_request() {
    let (addr, log_recorder) = spawn_scripted_responder(vec![ScriptedRoute {
        method: "PUT",
        path_pattern: "/repo/whatever",
        response: "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        times: None,
    }]);
    let mut config = Config::default();
    config.artifactories = Some(vec![ArtifactoryConfig {
        name: Some("prod".to_string()),
        target: Some(format!("http://{addr}/repo/")),
        username: Some("u".to_string()),
        password: Some("p".to_string()),
        ..Default::default()
    }]);
    let ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false,
            ..Default::default()
        },
    );
    let log = ctx.logger("artifactory");
    publish_to_artifactory(&ctx, &log).expect("no artifacts is ok");
    assert!(
        log_recorder.lock().unwrap().is_empty(),
        "no artifacts => no upload request"
    );
}

// -----------------------------------------------------------------
// build_reqwest_client — mTLS + trusted-CA error/success paths
// -----------------------------------------------------------------

/// A non-existent client cert path surfaces a read error naming the
/// path (the `failed to read client cert` branch).
#[test]
fn build_client_missing_cert_file_errors() {
    let err = build_reqwest_client(
        Some("/nonexistent/anodizer/cert.pem"),
        Some("/nonexistent/anodizer/key.pem"),
        None,
    )
    .expect_err("missing cert file must error");
    assert!(
        err.to_string().contains("failed to read client cert"),
        "unexpected: {err}"
    );
}

/// A cert file that exists but holds garbage (not a PEM identity)
/// fails at `Identity::from_pem` with the identity-load message.
#[test]
fn build_client_bad_pem_identity_errors() {
    let dir = tempfile::tempdir().unwrap();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    fs::write(&cert, b"not a real pem").unwrap();
    fs::write(&key, b"also not a pem").unwrap();
    let err = build_reqwest_client(
        Some(cert.to_str().unwrap()),
        Some(key.to_str().unwrap()),
        None,
    )
    .expect_err("garbage PEM must fail identity load");
    assert!(
        err.to_string()
            .contains("failed to load client certificate identity"),
        "unexpected: {err}"
    );
}

/// Only one of cert/key set is rejected as an incoherent mTLS pair.
#[test]
fn build_client_half_mtls_pair_errors() {
    let err = build_reqwest_client(Some("/tmp/cert.pem"), None, None)
        .expect_err("half mTLS pair must error");
    assert!(
        err.to_string().contains("must both be set"),
        "unexpected: {err}"
    );
}

/// A set-but-empty (whitespace) trusted-certificates bundle is
/// rejected with the copy-paste-accident guidance rather than
/// installing an empty trust store.
#[test]
fn build_client_empty_trusted_certs_errors() {
    let err =
        build_reqwest_client(None, None, Some("   \n\t ")).expect_err("blank CA bundle must error");
    assert!(
        err.to_string()
            .contains("trusted_certificates is set but empty"),
        "unexpected: {err}"
    );
}

/// A non-blank trusted-certificates value that contains no parseable
/// PEM certificate is rejected with the truncation guidance.
#[test]
fn build_client_unparseable_trusted_certs_errors() {
    let err = build_reqwest_client(None, None, Some("garbage-not-a-cert"))
        .expect_err("unparseable CA bundle must error");
    let msg = err.to_string();
    assert!(
        msg.contains("trusted_certificates"),
        "error must name the field: {msg}"
    );
}

/// No mTLS and no CA bundle builds a plain client successfully — the
/// happy path through `build_reqwest_client`.
#[test]
fn build_client_plain_succeeds() {
    assert!(build_reqwest_client(None, None, None).is_ok());
}

// -----------------------------------------------------------------
// Credential cascade via resolve_http_credentials (env override)
// -----------------------------------------------------------------

/// With no config credentials, the per-entry env vars
/// `ARTIFACTORY_PROD_USERNAME` / `_SECRET` resolve the basic-auth
/// pair. Confirms the prefix + uppercased-name env ladder.
#[test]
fn credentials_resolve_from_named_env_vars() {
    let mut ctx = upload_ctx();
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("ARTIFACTORY_PROD_USERNAME", "envuser")
            .with("ARTIFACTORY_PROD_SECRET", "envsecret"),
    );
    let (u, p) = crate::http_upload::resolve_http_credentials(
        &ctx,
        &crate::http_upload::CredentialResolveSpec {
            publisher: "artifactory",
            entry_name: "prod",
            config_username: None,
            config_password: None,
            env_prefix: "ARTIFACTORY",
            anonymous_ok: false,
        },
    )
    .expect("env creds resolve");
    assert_eq!(u, "envuser");
    assert_eq!(p, "envsecret");
}

/// A hyphenated entry name is folded to `_` and upper-cased for the
/// env lookup, so `my-repo` reads `ARTIFACTORY_MY_REPO_SECRET`.
#[test]
fn credentials_fold_hyphen_in_entry_name() {
    let mut ctx = upload_ctx();
    ctx.set_env_source(
        anodizer_core::MapEnvSource::new()
            .with("ARTIFACTORY_MY_REPO_USERNAME", "hu")
            .with("ARTIFACTORY_MY_REPO_SECRET", "hp"),
    );
    let (u, p) = crate::http_upload::resolve_http_credentials(
        &ctx,
        &crate::http_upload::CredentialResolveSpec {
            publisher: "artifactory",
            entry_name: "my-repo",
            config_username: None,
            config_password: None,
            env_prefix: "ARTIFACTORY",
            anonymous_ok: false,
        },
    )
    .expect("hyphen-folded env creds resolve");
    assert_eq!(u, "hu");
    assert_eq!(p, "hp");
}

/// Anonymous resolution (no config, no env) is refused when
/// `anonymous_ok = false` — the live artifactory path's guard.
#[test]
fn credentials_refuse_anonymous_when_required() {
    let mut ctx = upload_ctx();
    // An empty env source carries neither ARTIFACTORY_LONELY_USERNAME nor
    // _SECRET, so the resolver sees both as unset without touching process env.
    ctx.set_env_source(anodizer_core::MapEnvSource::new());
    let err = crate::http_upload::resolve_http_credentials(
        &ctx,
        &crate::http_upload::CredentialResolveSpec {
            publisher: "artifactory",
            entry_name: "lonely",
            config_username: None,
            config_password: None,
            env_prefix: "ARTIFACTORY",
            anonymous_ok: false,
        },
    )
    .expect_err("anonymous must be refused");
    assert!(
        err.to_string().contains("anonymous upload is refused"),
        "unexpected: {err}"
    );
}
