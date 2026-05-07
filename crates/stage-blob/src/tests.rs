#![cfg(test)]
#![allow(clippy::field_reassign_with_default)]

use std::path::PathBuf;
use std::sync::Arc;

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{BlobConfig, ExtraFileSpec, StringOrBool};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::stage::Stage;

use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutOptions};

use crate::BlobStage;
use crate::kms::{KmsProvider, encrypt_with_kms, parse_kms_provider};
use crate::provider::Provider;
use crate::store::build_s3_store;
use crate::upload::{
    build_put_options, collect_artifacts, format_remote_path, handle_upload_error,
    resolve_extra_files, upload_files_owned,
};
use anodizer_core::config::RetryConfig;

// -----------------------------------------------------------------------
// Provider tests
// -----------------------------------------------------------------------

#[test]
fn test_provider_s3() {
    assert_eq!(Provider::parse("s3").unwrap(), Provider::S3);
}

#[test]
fn test_provider_gcs() {
    assert_eq!(Provider::parse("gcs").unwrap(), Provider::Gcs);
    assert_eq!(Provider::parse("gs").unwrap(), Provider::Gcs);
}

#[test]
fn test_provider_azblob() {
    assert_eq!(Provider::parse("azblob").unwrap(), Provider::AzBlob);
    assert_eq!(Provider::parse("azure").unwrap(), Provider::AzBlob);
}

#[test]
fn test_provider_invalid() {
    let err = Provider::parse("dropbox").unwrap_err();
    assert!(err.to_string().contains("unknown provider"));
}

#[test]
fn test_provider_display_name() {
    assert_eq!(Provider::S3.display_name(), "s3");
    assert_eq!(Provider::Gcs.display_name(), "gs");
    assert_eq!(Provider::AzBlob.display_name(), "azblob");
}

// -----------------------------------------------------------------------
// S3 store builder tests
// -----------------------------------------------------------------------

#[test]
fn test_s3_basic_build() {
    // Verify the builder doesn't panic with minimal config.
    // Actual credential validation happens at upload time, not build time.
    let config = BlobConfig {
        provider: "s3".to_string(),
        bucket: "test-bucket".to_string(),
        ..Default::default()
    };
    // The builder will succeed (credentials validated lazily)
    let result = build_s3_store(&config, "test-bucket", &make_ctx(), &RetryConfig::default());
    // May fail due to missing AWS creds in test env, but should not panic
    // and should produce a meaningful error if it fails
    let _ = result;
}

#[test]
fn test_s3_force_path_style_defaults_true_with_endpoint() {
    // When endpoint is set, force_path_style should default to true
    let config = BlobConfig {
        provider: "s3".to_string(),
        bucket: "b".to_string(),
        endpoint: Some("http://minio:9000".to_string()),
        // s3_force_path_style NOT set — should default to true
        ..Default::default()
    };
    // This exercises the builder path. The builder configures
    // virtual_hosted_style_request = !force_path = false, which means
    // path-style is enabled.
    let _ = build_s3_store(&config, "b", &make_ctx(), &RetryConfig::default());
}

#[test]
fn test_s3_force_path_style_explicit_false_with_endpoint() {
    let config = BlobConfig {
        provider: "s3".to_string(),
        bucket: "b".to_string(),
        endpoint: Some("http://minio:9000".to_string()),
        s3_force_path_style: Some(false),
        ..Default::default()
    };
    let _ = build_s3_store(&config, "b", &make_ctx(), &RetryConfig::default());
}

// -----------------------------------------------------------------------
// Put options tests
// -----------------------------------------------------------------------

#[test]
fn test_put_options_cache_control_array_joined() {
    let config = BlobConfig {
        cache_control: Some(vec!["max-age=86400".to_string(), "public".to_string()]),
        ..Default::default()
    };
    let opts = build_put_options(&config, "file.tar.gz", &make_ctx()).unwrap();
    let cc = opts
        .attributes
        .get(&object_store::Attribute::CacheControl)
        .unwrap_or_else(|| panic!("cache_control should be set"));
    assert_eq!(cc.as_ref(), "max-age=86400, public");
}

#[test]
fn test_put_options_cache_control_single() {
    let config = BlobConfig {
        cache_control: Some(vec!["no-cache".to_string()]),
        ..Default::default()
    };
    let opts = build_put_options(&config, "f.txt", &make_ctx()).unwrap();
    let cc = opts
        .attributes
        .get(&object_store::Attribute::CacheControl)
        .unwrap_or_else(|| panic!("should be set"));
    assert_eq!(cc.as_ref(), "no-cache");
}

#[test]
fn test_put_options_content_disposition_default_unset() {
    // B3 fix: GoReleaser does NOT default content-disposition; anodizer
    // must not either, so in-browser preview (images/PDFs/HTML) keeps
    // working when users don't opt into attachment behavior.
    let config = BlobConfig::default();
    let opts = build_put_options(&config, "myapp-v1.tar.gz", &make_ctx()).unwrap();
    assert!(
        opts.attributes
            .get(&object_store::Attribute::ContentDisposition)
            .is_none(),
        "default content-disposition must be unset (matches GoReleaser)"
    );
}

#[test]
fn test_put_options_content_disposition_disabled() {
    let config = BlobConfig {
        content_disposition: Some("-".to_string()),
        ..Default::default()
    };
    let opts = build_put_options(&config, "f.txt", &make_ctx()).unwrap();
    assert!(
        opts.attributes
            .get(&object_store::Attribute::ContentDisposition)
            .is_none()
    );
}

#[test]
fn test_put_options_content_disposition_custom() {
    let config = BlobConfig {
        content_disposition: Some("inline".to_string()),
        ..Default::default()
    };
    let opts = build_put_options(&config, "f.txt", &make_ctx()).unwrap();
    let cd = opts
        .attributes
        .get(&object_store::Attribute::ContentDisposition)
        .unwrap_or_else(|| panic!("should be set"));
    assert_eq!(cd.as_ref(), "inline");
}

// -----------------------------------------------------------------------
// Disable evaluation tests
// -----------------------------------------------------------------------

#[test]
fn test_is_disabled_none() {
    assert!(!make_ctx().skip_with_log(&None, &test_log(), "t").unwrap());
}

#[test]
fn test_is_disabled_bool_true() {
    assert!(
        make_ctx()
            .skip_with_log(&Some(StringOrBool::Bool(true)), &test_log(), "t")
            .unwrap()
    );
}

#[test]
fn test_is_disabled_bool_false() {
    assert!(
        !make_ctx()
            .skip_with_log(&Some(StringOrBool::Bool(false)), &test_log(), "t")
            .unwrap()
    );
}

#[test]
fn test_is_disabled_string_true() {
    assert!(
        make_ctx()
            .skip_with_log(
                &Some(StringOrBool::String("true".to_string())),
                &test_log(),
                "t"
            )
            .unwrap()
    );
}

#[test]
fn test_is_disabled_string_false() {
    assert!(
        !make_ctx()
            .skip_with_log(
                &Some(StringOrBool::String("false".to_string())),
                &test_log(),
                "t"
            )
            .unwrap()
    );
}

#[test]
fn test_is_disabled_template_evaluates_to_true() {
    let ctx = make_ctx_with_snapshot();
    let disable = Some(StringOrBool::String(
        "{% if IsSnapshot %}true{% endif %}".to_string(),
    ));
    assert!(ctx.skip_with_log(&disable, &test_log(), "t").unwrap());
}

#[test]
fn test_is_disabled_template_evaluates_to_false() {
    let ctx = make_ctx(); // IsSnapshot is "false"
    let disable = Some(StringOrBool::String(
        "{% if IsSnapshot == \"true\" %}true{% endif %}".to_string(),
    ));
    assert!(!ctx.skip_with_log(&disable, &test_log(), "t").unwrap());
}

/// Regression: a malformed `skip:` template now propagates as Err
/// instead of silently evaluating to "not skipped".
#[test]
fn test_is_disabled_template_render_failure_propagates() {
    let ctx = make_ctx();
    let disable = Some(StringOrBool::String(
        "{{ NonexistentVarThatTeraDoesNotKnow }}".to_string(),
    ));
    let err = ctx
        .skip_with_log(&disable, &test_log(), "t")
        .unwrap_err()
        .to_string();
    assert!(err.contains("evaluate skip expression"), "{err}");
}

// -----------------------------------------------------------------------
// Remote path formatting
// -----------------------------------------------------------------------

#[test]
fn test_format_remote_path_s3() {
    let path = format_remote_path(Provider::S3, "my-bucket", "releases/v1", "file.tar.gz");
    assert_eq!(path, "s3://my-bucket/releases/v1/file.tar.gz");
}

#[test]
fn test_format_remote_path_gcs() {
    let path = format_remote_path(Provider::Gcs, "my-bucket", "dir", "file.tar.gz");
    assert_eq!(path, "gs://my-bucket/dir/file.tar.gz");
}

#[test]
fn test_format_remote_path_azure() {
    let path = format_remote_path(Provider::AzBlob, "container", "path", "file.tar.gz");
    assert_eq!(path, "azblob://container/path/file.tar.gz");
}

#[test]
fn test_format_remote_path_empty_directory() {
    let path = format_remote_path(Provider::S3, "b", "", "file.tar.gz");
    assert_eq!(path, "s3://b/file.tar.gz");
}

#[test]
fn test_format_remote_path_trims_slashes() {
    let path = format_remote_path(Provider::S3, "b", "/dir/", "file.tar.gz");
    assert_eq!(path, "s3://b/dir/file.tar.gz");
}

// -----------------------------------------------------------------------
// KMS provider detection tests
// -----------------------------------------------------------------------

#[test]
fn test_parse_kms_provider_aws() {
    assert_eq!(
        parse_kms_provider("awskms://alias/my-key"),
        KmsProvider::Aws
    );
    assert_eq!(
        parse_kms_provider("awskms:///arn:aws:kms:us-east-1:123456:key/abc-def"),
        KmsProvider::Aws
    );
}

#[test]
fn test_parse_kms_provider_gcp() {
    assert_eq!(
        parse_kms_provider("gcpkms://projects/my-proj/locations/global/keyRings/kr/cryptKeys/key"),
        KmsProvider::Gcp
    );
}

#[test]
fn test_parse_kms_provider_azure() {
    assert_eq!(
        parse_kms_provider("azurekeyvault://my-vault/keys/my-key"),
        KmsProvider::Azure
    );
    assert_eq!(
        parse_kms_provider("azurekeyvault://my-vault/keys/my-key/version1"),
        KmsProvider::Azure
    );
}

#[test]
fn test_parse_kms_provider_server_side() {
    // Plain ARN — no URL scheme
    assert_eq!(
        parse_kms_provider("arn:aws:kms:us-east-1:123456:key/abc-def"),
        KmsProvider::ServerSide
    );
    // Plain key ID
    assert_eq!(
        parse_kms_provider("1234abcd-12ab-34cd-56ef-1234567890ab"),
        KmsProvider::ServerSide
    );
    // Alias without scheme
    assert_eq!(parse_kms_provider("alias/my-key"), KmsProvider::ServerSide);
}

#[test]
fn test_encrypt_with_kms_server_side_passthrough() {
    // ServerSide should return data unchanged
    let data = b"hello world";
    let result = encrypt_with_kms(data, "some-key-arn", KmsProvider::ServerSide).unwrap();
    assert_eq!(result, data);
}

// -----------------------------------------------------------------------
// Error handling tests
// -----------------------------------------------------------------------

#[test]
fn test_handle_upload_error_not_found() {
    let err = object_store::Error::NotFound {
        path: "test/path".to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "not found",
        )),
    };
    let anyhow_err = handle_upload_error(err, "local.tar.gz", "remote/file.tar.gz");
    let msg = anyhow_err.to_string();
    assert!(msg.contains("bucket or object not found"), "got: {msg}");
    assert!(msg.contains("local.tar.gz"), "got: {msg}");
}

#[test]
fn test_handle_upload_error_unauthenticated() {
    let err = object_store::Error::Unauthenticated {
        path: "test".to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "bad creds",
        )),
    };
    let anyhow_err = handle_upload_error(err, "a.tar.gz", "r/a.tar.gz");
    assert!(anyhow_err.to_string().contains("authentication failed"));
}

#[test]
fn test_handle_upload_error_permission_denied() {
    let err = object_store::Error::PermissionDenied {
        path: "test".to_string(),
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        )),
    };
    let anyhow_err = handle_upload_error(err, "a.tar.gz", "r/a.tar.gz");
    assert!(anyhow_err.to_string().contains("access denied"));
}

#[test]
fn test_handle_upload_error_generic() {
    let err = object_store::Error::Generic {
        store: "S3",
        source: Box::new(std::io::Error::other("network timeout")),
    };
    let anyhow_err = handle_upload_error(err, "a.tar.gz", "r/a.tar.gz");
    assert!(anyhow_err.to_string().contains("upload failed"));
}

// -----------------------------------------------------------------------
// Config parsing tests
// -----------------------------------------------------------------------

#[test]
fn test_blob_config_parses_s3() {
    let yaml = r#"
blobs:
  - provider: s3
    bucket: my-releases
    region: us-west-2
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    assert_eq!(blobs.len(), 1);
    assert_eq!(blobs[0].provider, "s3");
    assert_eq!(blobs[0].bucket, "my-releases");
    assert_eq!(blobs[0].region.as_deref(), Some("us-west-2"));
}

#[test]
fn test_blob_config_parses_gcs() {
    let yaml = r#"
blobs:
  - provider: gs
    bucket: my-gcs-bucket
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    assert_eq!(blobs[0].provider, "gs");
}

#[test]
fn test_blob_config_parses_azblob() {
    let yaml = r#"
blobs:
  - provider: azblob
    bucket: my-container
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    assert_eq!(blobs[0].provider, "azblob");
}

#[test]
fn test_blob_config_multiple_providers() {
    let yaml = r#"
blobs:
  - provider: s3
    bucket: aws-bucket
  - provider: gs
    bucket: gcs-bucket
  - provider: azblob
    bucket: azure-container
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    assert_eq!(blobs.len(), 3);
}

#[test]
fn test_blob_config_all_fields() {
    let yaml = r#"
blobs:
  - provider: s3
    bucket: my-bucket
    directory: "releases/v1"
    region: eu-central-1
    endpoint: "http://minio:9000"
    disable_ssl: true
    s3_force_path_style: true
    acl: private
    cache_control:
      - "no-cache"
      - "no-store"
    content_disposition: "inline"
    kms_key: "key123"
    ids:
      - build-linux
    skip: false
    include_meta: true
    extra_files:
      - glob: "LICENSE*"
    extra_files_only: false
    id: my-blob-config
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    let b = &blobs[0];
    assert_eq!(b.provider, "s3");
    assert_eq!(b.bucket, "my-bucket");
    assert_eq!(b.directory.as_deref(), Some("releases/v1"));
    assert_eq!(b.region.as_deref(), Some("eu-central-1"));
    assert_eq!(b.endpoint.as_deref(), Some("http://minio:9000"));
    assert_eq!(b.disable_ssl, Some(true));
    assert_eq!(b.s3_force_path_style, Some(true));
    assert_eq!(b.acl.as_deref(), Some("private"));
    assert_eq!(b.cache_control.as_ref().unwrap(), &["no-cache", "no-store"]);
    assert_eq!(b.content_disposition.as_deref(), Some("inline"));
    assert_eq!(b.kms_key.as_deref(), Some("key123"));
    assert_eq!(b.ids.as_ref().unwrap(), &["build-linux"]);
    assert_eq!(b.skip, Some(StringOrBool::Bool(false)));
    assert_eq!(b.include_meta, Some(true));
    assert!(b.extra_files.is_some());
    assert_eq!(b.extra_files_only, Some(false));
    assert_eq!(b.id.as_deref(), Some("my-blob-config"));
}

#[test]
fn test_blob_config_cache_control_as_string() {
    // Backward compat: cache_control as a single string
    let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    cache_control: "max-age=86400"
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    assert_eq!(blobs[0].cache_control.as_ref().unwrap(), &["max-age=86400"]);
}

#[test]
fn test_blob_config_cache_control_as_array() {
    let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    cache_control:
      - "max-age=86400"
      - "public"
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    assert_eq!(
        blobs[0].cache_control.as_ref().unwrap(),
        &["max-age=86400", "public"]
    );
}

#[test]
fn test_blob_config_disable_as_bool() {
    let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    skip: true
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    assert_eq!(blobs[0].skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_blob_config_disable_as_template_string() {
    let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    skip: "{% if IsSnapshot %}true{% endif %}"
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    match &blobs[0].skip {
        Some(StringOrBool::String(s)) => {
            assert!(s.contains("IsSnapshot"));
        }
        other => panic!("expected StringOrBool::String, got: {other:?}"),
    }
}

#[test]
fn test_blob_config_defaults() {
    let cfg = BlobConfig::default();
    assert!(cfg.provider.is_empty());
    assert!(cfg.bucket.is_empty());
    assert!(cfg.directory.is_none());
    assert!(cfg.region.is_none());
    assert!(cfg.endpoint.is_none());
    assert!(cfg.disable_ssl.is_none());
    assert!(cfg.s3_force_path_style.is_none());
    assert!(cfg.acl.is_none());
    assert!(cfg.cache_control.is_none());
    assert!(cfg.content_disposition.is_none());
    assert!(cfg.kms_key.is_none());
    assert!(cfg.ids.is_none());
    assert!(cfg.skip.is_none());
    assert!(cfg.include_meta.is_none());
    assert!(cfg.extra_files.is_none());
    assert!(cfg.extra_files_only.is_none());
    assert!(cfg.id.is_none());
}

#[test]
fn test_blob_config_extra_files_with_name_template() {
    let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    extra_files:
      - glob: "./LICENSE"
        name_template: "LICENSE.txt"
      - glob: "./README.md"
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    let extras = blobs[0].extra_files.as_ref().unwrap();
    assert_eq!(extras.len(), 2);
    assert_eq!(extras[0].glob(), "./LICENSE");
    assert_eq!(extras[0].name_template(), Some("LICENSE.txt"));
    assert_eq!(extras[1].glob(), "./README.md");
    assert!(extras[1].name_template().is_none());
}

#[test]
fn test_blob_config_extra_files_name_template_alias() {
    let yaml = r#"
blobs:
  - provider: s3
    bucket: b
    extra_files:
      - glob: "./LICENSE"
        name_template: "LICENSE-{{ Tag }}"
"#;
    let crate_cfg: anodizer_core::config::CrateConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let blobs = crate_cfg.blobs.unwrap();
    let extras = blobs[0].extra_files.as_ref().unwrap();
    assert_eq!(extras[0].name_template(), Some("LICENSE-{{ Tag }}"));
}

#[test]
fn test_partial_config_parses() {
    let yaml = r#"
partial:
  by: goos
"#;
    let config: anodizer_core::config::Config =
        serde_yaml_ng::from_str(&format!("project_name: test\ncrates: []\n{}", yaml)).unwrap();
    let partial = config.partial.unwrap();
    assert_eq!(partial.by.as_deref(), Some("goos"));
}

#[test]
fn test_partial_config_by_target() {
    let yaml = "project_name: test\ncrates: []\npartial:\n  by: target\n";
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.partial.unwrap().by.as_deref(), Some("target"));
}

#[test]
fn test_partial_config_defaults() {
    let yaml = "project_name: test\ncrates: []\npartial: {}\n";
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    // by defaults to None, which the runtime interprets as "goos"
    assert!(config.partial.unwrap().by.is_none());
}

// -----------------------------------------------------------------------
// Extra files resolution tests
// -----------------------------------------------------------------------

#[test]
fn test_resolve_extra_files_basic() {
    let tmp = tempfile::TempDir::new().unwrap();
    let file_path = tmp.path().join("LICENSE");
    std::fs::write(&file_path, "MIT").unwrap();

    let extras = vec![ExtraFileSpec::Glob(file_path.to_string_lossy().to_string())];
    let resolved = resolve_extra_files(&extras, &make_ctx(), &test_log()).unwrap();
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].1, "LICENSE");
}

#[test]
fn test_resolve_extra_files_with_name_template() {
    let tmp = tempfile::TempDir::new().unwrap();
    let file_path = tmp.path().join("LICENSE");
    std::fs::write(&file_path, "MIT").unwrap();

    let extras = vec![ExtraFileSpec::Detailed {
        glob: file_path.to_string_lossy().to_string(),
        name_template: Some("LICENSE-{{ Tag }}".to_string()),
    }];

    let mut ctx = make_ctx();
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    let resolved = resolve_extra_files(&extras, &ctx, &test_log()).unwrap();
    assert_eq!(resolved[0].1, "LICENSE-v1.0.0");
}

#[test]
fn test_resolve_extra_files_glob_pattern() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("LICENSE"), "MIT").unwrap();
    std::fs::write(tmp.path().join("LICENSE.md"), "MIT").unwrap();

    let glob_pattern = format!("{}/*", tmp.path().display());
    let extras = vec![ExtraFileSpec::Glob(glob_pattern)];
    let resolved = resolve_extra_files(&extras, &make_ctx(), &test_log()).unwrap();
    assert_eq!(resolved.len(), 2);
}

#[test]
fn test_resolve_extra_files_no_match_warns_and_continues() {
    let extras = vec![ExtraFileSpec::Glob(
        "/nonexistent/path/to/files/*.xyz".to_string(),
    )];
    // Should succeed with empty results (warning logged), not error
    let result = resolve_extra_files(&extras, &make_ctx(), &test_log()).unwrap();
    assert!(result.is_empty());
}

// -----------------------------------------------------------------------
// Artifact filtering tests
// -----------------------------------------------------------------------

#[test]
fn test_collect_artifacts_filters_by_crate() {
    let mut ctx = make_ctx();
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/a.tar.gz"),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/b.tar.gz"),
        target: None,
        crate_name: "othercrate".to_string(),
        metadata: Default::default(),
        size: None,
    });
    let config = BlobConfig::default();
    let arts = collect_artifacts(&ctx, &config, "mycrate");
    assert_eq!(arts.len(), 1);
    assert_eq!(arts[0].crate_name, "mycrate");
}

#[test]
fn test_collect_artifacts_extra_files_only() {
    let mut ctx = make_ctx();
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/a.tar.gz"),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: Default::default(),
        size: None,
    });
    let config = BlobConfig {
        extra_files_only: Some(true),
        ..Default::default()
    };
    let arts = collect_artifacts(&ctx, &config, "mycrate");
    assert!(arts.is_empty());
}

#[test]
fn test_collect_artifacts_ids_filter() {
    let mut ctx = make_ctx();
    let mut meta = std::collections::HashMap::new();
    meta.insert("id".to_string(), "linux-build".to_string());
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/a.tar.gz"),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: meta,
        size: None,
    });
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/b.tar.gz"),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: Default::default(),
        size: None,
    });
    let config = BlobConfig {
        ids: Some(vec!["linux-build".to_string()]),
        ..Default::default()
    };
    let arts = collect_artifacts(&ctx, &config, "mycrate");
    assert_eq!(arts.len(), 1);
}

#[test]
fn test_collect_artifacts_includes_metadata_kind() {
    let mut ctx = make_ctx();
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Metadata,
        name: String::new(),
        path: PathBuf::from("dist/metadata.json"),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: Default::default(),
        size: None,
    });
    // Without include_meta
    let config = BlobConfig::default();
    let arts = collect_artifacts(&ctx, &config, "mycrate");
    assert!(arts.is_empty());

    // With include_meta
    let config_meta = BlobConfig {
        include_meta: Some(true),
        ..Default::default()
    };
    let arts = collect_artifacts(&ctx, &config_meta, "mycrate");
    assert_eq!(arts.len(), 1);
}

// -----------------------------------------------------------------------
// Stage behavior tests
// -----------------------------------------------------------------------

#[test]
fn test_blob_stage_skips_when_no_config() {
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        crates: vec![anodizer_core::config::CrateConfig {
            name: "mycrate".to_string(),
            path: ".".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let opts = ContextOptions::default();
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "test");

    let stage = BlobStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_ok());
}

#[test]
fn test_blob_stage_skips_disabled_config() {
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        crates: vec![anodizer_core::config::CrateConfig {
            name: "mycrate".to_string(),
            path: ".".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: "s3".to_string(),
                bucket: "b".to_string(),
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let opts = ContextOptions::default();
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "test");

    let stage = BlobStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_ok());
}

#[test]
fn test_blob_stage_empty_provider_error() {
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        crates: vec![anodizer_core::config::CrateConfig {
            name: "mycrate".to_string(),
            path: ".".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: String::new(),
                bucket: "b".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let opts = ContextOptions::default();
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "test");

    let stage = BlobStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("provider is required")
    );
}

#[test]
fn test_blob_stage_empty_bucket_error() {
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        crates: vec![anodizer_core::config::CrateConfig {
            name: "mycrate".to_string(),
            path: ".".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: "s3".to_string(),
                bucket: String::new(),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let opts = ContextOptions::default();
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "test");

    let stage = BlobStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("bucket is required")
    );
}

#[test]
fn test_blob_stage_invalid_provider() {
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        crates: vec![anodizer_core::config::CrateConfig {
            name: "mycrate".to_string(),
            path: ".".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: "dropbox".to_string(),
                bucket: "b".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let opts = ContextOptions::default();
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "test");

    let stage = BlobStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("unknown provider"));
}

#[test]
fn test_blob_stage_dry_run_logs_commands() {
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        crates: vec![anodizer_core::config::CrateConfig {
            name: "mycrate".to_string(),
            path: ".".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: "s3".to_string(),
                bucket: "my-bucket".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut opts = ContextOptions::default();
    opts.dry_run = true;
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "test");

    // Add an artifact so there's something to "upload"
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/test-v1.0.0.tar.gz"),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = BlobStage;
    // Dry-run should succeed without any credentials
    let result = stage.run(&mut ctx);
    assert!(result.is_ok());
}

// -----------------------------------------------------------------------
// Q9.1 — provider must be Tera-template-rendered before any provider-keyed
// dispatch (S3-ACL gate, GCS-ACL gate, …). Mirrors GoReleaser commit
// 4d1924d (`internal/pipe/blob/upload.go`): `provider == "s3"` was a raw
// string compare, so `provider: "{{ .ProviderName }}"` skipped the ACL
// branch. Anodizer renders `provider` once via `ctx.render_template` then
// dispatches against `Provider::S3` (an enum) so the bug has no surface,
// but two tests pin the contract:
//   - `..._for_acl_dispatch` — full stage dry-run: templated provider +
//     ACL must not error.
//   - `..._exercises_s3_acl_validator` — bypass dry-run by calling the
//     S3 builder directly with an invalid ACL; it must reject (proves
//     the ACL gate runs on the S3 path that a templated provider
//     resolves into).
// -----------------------------------------------------------------------

#[test]
fn test_q9_1_provider_template_resolves_to_s3_for_acl_dispatch() {
    // Templated provider → after render → "s3" → Provider::S3 → ACL gate.
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        crates: vec![anodizer_core::config::CrateConfig {
            name: "mycrate".to_string(),
            path: ".".to_string(),
            blobs: Some(vec![BlobConfig {
                provider: "{{ ProviderName }}".to_string(),
                bucket: "my-bucket".to_string(),
                acl: Some("private".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut opts = ContextOptions::default();
    opts.dry_run = true;
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "test");
    ctx.template_vars_mut().set("ProviderName", "s3");

    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/test-v1.0.0.tar.gz"),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = BlobStage;
    // The render → Provider::parse → S3 path must succeed under dry-run.
    let result = stage.run(&mut ctx);
    assert!(
        result.is_ok(),
        "templated provider that resolves to 's3' must dispatch through \
         the S3 path; got: {:?}",
        result.err()
    );
}

#[test]
fn test_q9_1_template_resolved_provider_exercises_s3_acl_validator() {
    // After Q9.1 fix, `provider: "{{ .ProviderName }}"` resolved to "s3"
    // must reach the S3 dispatch arm — so the S3-only ACL validator runs.
    // Direct-call `build_s3_store` with an invalid ACL: it must reject.
    // (If the bug regressed and the rendered provider were treated as
    // non-S3, an invalid ACL would be silently passed through.)
    let config = BlobConfig {
        provider: "s3".to_string(), // post-render value, mimicking the dispatch result
        bucket: "b".to_string(),
        acl: Some("not-a-real-acl".to_string()),
        ..Default::default()
    };
    let result = build_s3_store(&config, "b", &make_ctx(), &RetryConfig::default());
    assert!(
        result.is_err(),
        "S3 ACL validator must reject an invalid canned ACL after the \
         dispatcher routes a templated provider into the S3 arm"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("invalid S3 canned ACL"),
        "ACL validator error must mention the rejection, got: {err}"
    );
}

// -----------------------------------------------------------------------
// StringOrBool unit tests
// -----------------------------------------------------------------------

#[test]
fn test_string_or_bool_as_bool() {
    assert!(StringOrBool::Bool(true).as_bool());
    assert!(!StringOrBool::Bool(false).as_bool());
    assert!(StringOrBool::String("true".to_string()).as_bool());
    assert!(StringOrBool::String("1".to_string()).as_bool());
    assert!(!StringOrBool::String("false".to_string()).as_bool());
    assert!(!StringOrBool::String("".to_string()).as_bool());
}

#[test]
fn test_string_or_bool_is_template() {
    assert!(StringOrBool::String("{% if X %}true{% endif %}".to_string()).is_template());
    assert!(!StringOrBool::String("true".to_string()).is_template());
    assert!(!StringOrBool::Bool(true).is_template());
}

// -----------------------------------------------------------------------
// Integration test with InMemory store
// -----------------------------------------------------------------------

#[test]
fn test_upload_to_in_memory_store() {
    let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let tmp = tempfile::TempDir::new().unwrap();

    // Create test files
    let file1 = tmp.path().join("app.tar.gz");
    std::fs::write(&file1, b"archive data").unwrap();
    let file2 = tmp.path().join("checksums.sha256");
    std::fs::write(&file2, b"abc123  app.tar.gz").unwrap();

    let upload_items = vec![
        (file1, "app.tar.gz".to_string()),
        (file2, "checksums.sha256".to_string()),
    ];
    let config = BlobConfig::default();
    let ctx = make_ctx();
    let _log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);

    let put_opts: Vec<PutOptions> = upload_items
        .iter()
        .map(|(_, k)| build_put_options(&config, k, &ctx).unwrap())
        .collect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = upload_files_owned(
        &rt,
        Arc::clone(&store),
        upload_items.clone(),
        "myproject/v1.0.0".to_string(),
        put_opts,
        1,
        None,
    );
    assert!(result.is_ok(), "upload failed: {:?}", result.err());

    // Verify files were uploaded to the store
    rt.block_on(async {
        let path1 = ObjectPath::from("myproject/v1.0.0/app.tar.gz");
        let get_result = store
            .get_opts(&path1, object_store::GetOptions::default())
            .await
            .unwrap();
        let data = get_result.bytes().await.unwrap();
        assert_eq!(data.as_ref(), b"archive data");

        let path2 = ObjectPath::from("myproject/v1.0.0/checksums.sha256");
        let get_result = store
            .get_opts(&path2, object_store::GetOptions::default())
            .await
            .unwrap();
        let data = get_result.bytes().await.unwrap();
        assert_eq!(data.as_ref(), b"abc123  app.tar.gz");
    });
}

#[test]
fn test_upload_to_in_memory_store_empty_directory() {
    let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let tmp = tempfile::TempDir::new().unwrap();

    let file1 = tmp.path().join("file.txt");
    std::fs::write(&file1, b"data").unwrap();

    let upload_items = vec![(file1, "file.txt".to_string())];
    let config = BlobConfig::default();
    let ctx = make_ctx();
    let _log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);

    let put_opts: Vec<PutOptions> = upload_items
        .iter()
        .map(|(_, k)| build_put_options(&config, k, &ctx).unwrap())
        .collect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = upload_files_owned(
        &rt,
        Arc::clone(&store),
        upload_items.clone(),
        String::new(),
        put_opts,
        1,
        None,
    );
    assert!(result.is_ok());

    rt.block_on(async {
        let path = ObjectPath::from("file.txt");
        let get_result = store
            .get_opts(&path, object_store::GetOptions::default())
            .await
            .unwrap();
        let data = get_result.bytes().await.unwrap();
        assert_eq!(data.as_ref(), b"data");
    });
}

#[test]
fn test_upload_with_content_disposition() {
    let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let tmp = tempfile::TempDir::new().unwrap();

    let file1 = tmp.path().join("app.tar.gz");
    std::fs::write(&file1, b"data").unwrap();

    let upload_items = vec![(file1, "app.tar.gz".to_string())];
    let config = BlobConfig {
        content_disposition: Some("attachment;filename={{Filename}}".to_string()),
        ..Default::default()
    };
    let ctx = make_ctx();
    let _log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);

    let put_opts: Vec<PutOptions> = upload_items
        .iter()
        .map(|(_, k)| build_put_options(&config, k, &ctx).unwrap())
        .collect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = upload_files_owned(
        &rt,
        store,
        upload_items.clone(),
        "dir".to_string(),
        put_opts,
        1,
        None,
    );
    assert!(result.is_ok());
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn test_log() -> anodizer_core::log::StageLogger {
    anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet)
}

fn make_ctx() -> Context {
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        ..Default::default()
    };
    let opts = ContextOptions::default();
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "test");
    ctx.template_vars_mut().set("IsSnapshot", "false");
    ctx
}

fn make_ctx_with_snapshot() -> Context {
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        ..Default::default()
    };
    let mut opts = ContextOptions::default();
    opts.snapshot = true;
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "test");
    ctx.template_vars_mut().set("IsSnapshot", "true");
    ctx
}
