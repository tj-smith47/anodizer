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
fn test_put_options_content_disposition_default_is_attachment() {
    // Regression: the blob default force-defaults
    //     ContentDisposition = "attachment;filename={{.Filename}}"
    // when the user did not configure one. Anodizer must mirror that (with
    // `{{ Filename }}` rendered to the artifact filename) so a copy-pasted
    // config produces downloadable artifacts instead of in-browser
    // previews. `"-"` remains the disable-sentinel for users who deliberately
    // want the bucket-default behaviour.
    let config = BlobConfig::default();
    let opts = build_put_options(&config, "myapp-v1.tar.gz", &make_ctx()).unwrap();
    let cd = opts
        .attributes
        .get(&object_store::Attribute::ContentDisposition)
        .unwrap_or_else(|| panic!("default content-disposition must be set"));
    assert_eq!(cd.as_ref(), "attachment;filename=myapp-v1.tar.gz");
}

#[test]
fn test_put_options_content_disposition_default_empty_string_treated_as_unset() {
    // An explicit empty string gets the same force-default treatment as
    // None — the only way to opt out is the `"-"` sentinel.
    let config = BlobConfig {
        content_disposition: Some(String::new()),
        ..Default::default()
    };
    let opts = build_put_options(&config, "checksums.txt", &make_ctx()).unwrap();
    let cd = opts
        .attributes
        .get(&object_store::Attribute::ContentDisposition)
        .unwrap_or_else(|| panic!("empty string must trigger GR default"));
    assert_eq!(cd.as_ref(), "attachment;filename=checksums.txt");
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
  by: os
"#;
    let config: anodizer_core::config::Config =
        serde_yaml_ng::from_str(&format!("project_name: test\ncrates: []\n{}", yaml)).unwrap();
    let partial = config.partial.unwrap();
    assert_eq!(partial.by.as_deref(), Some("os"));
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
    // by defaults to None, which the runtime interprets as "os"
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
        allow_empty: false,
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

#[test]
fn test_collect_artifacts_excludes_binary_sign_outputs() {
    // Binary-sign Signature/Certificate intermediates must never be uploaded
    // to blob storage; only legitimate archive-sign signatures should pass.
    let mut ctx = make_ctx();

    let mut binary_sign_meta = std::collections::HashMap::new();
    binary_sign_meta.insert("type".to_string(), "Signature".to_string());
    binary_sign_meta.insert("binary_sign".to_string(), "true".to_string());
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Signature,
        name: String::new(),
        path: PathBuf::from("dist/anodizer_linux_amd64"),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: binary_sign_meta,
        size: None,
    });

    let mut binary_sign_cert_meta = std::collections::HashMap::new();
    binary_sign_cert_meta.insert("type".to_string(), "Certificate".to_string());
    binary_sign_cert_meta.insert("binary_sign".to_string(), "true".to_string());
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Certificate,
        name: String::new(),
        path: PathBuf::from("dist/anodizer_linux_amd64.pem"),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: binary_sign_cert_meta,
        size: None,
    });

    let mut archive_sign_meta = std::collections::HashMap::new();
    archive_sign_meta.insert("type".to_string(), "Signature".to_string());
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: ArtifactKind::Signature,
        name: String::new(),
        path: PathBuf::from("dist/mycrate_1.0.0_linux_amd64.tar.gz.sig"),
        target: None,
        crate_name: "mycrate".to_string(),
        metadata: archive_sign_meta,
        size: None,
    });

    let config = BlobConfig::default();
    let arts = collect_artifacts(&ctx, &config, "mycrate");
    let names: Vec<String> = arts
        .iter()
        .map(|a| a.path.to_string_lossy().into_owned())
        .collect();

    assert!(
        !names.iter().any(|p| p.ends_with("anodizer_linux_amd64")),
        "binary-sign Signature must not appear in blob upload set; got {:?}",
        names
    );
    assert!(
        !names
            .iter()
            .any(|p| p.ends_with("anodizer_linux_amd64.pem")),
        "binary-sign Certificate must not appear in blob upload set; got {:?}",
        names
    );
    assert!(
        names
            .iter()
            .any(|p| p.ends_with("mycrate_1.0.0_linux_amd64.tar.gz.sig")),
        "archive-sign Signature must appear in blob upload set; got {:?}",
        names
    );
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
// dispatch (S3-ACL gate, GCS-ACL gate, …). `provider == "s3"` was a raw
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

#[test]
fn test_q9_1_templated_provider_routes_through_s3_acl_validator() {
    // Composed end-to-end test: this is the contract the prior two tests
    // pinned independently — render-then-dispatch happens together, and a
    // templated provider value resolving to "s3" plus an invalid ACL must
    // be rejected by the S3-arm validator. The earlier dry-run + direct
    // `build_s3_store` tests cover the two halves; this one rules out a
    // regression where rendering and dispatch grow apart (e.g. a future
    // refactor that bypasses Provider::parse for templated values).
    //
    // Drives the validation path via `validate_only`, the crate-private
    // hook added in `run.rs` for exactly this purpose: render the
    // templated provider, dispatch via `Provider::parse`, then `build_store`
    // — but stop short of the network upload.
    let config = BlobConfig {
        provider: "{{ ProviderName }}".to_string(),
        bucket: "my-bucket".to_string(),
        acl: Some("invalid-acl-value".to_string()),
        ..Default::default()
    };

    let mut ctx_opts = ContextOptions::default();
    // Crucially NOT dry_run: validate_only ignores the dry-run flag, so a
    // future change that wires this into a stage entry point can't quietly
    // short-circuit on dry_run and skip the validator.
    ctx_opts.dry_run = false;
    let mut ctx = Context::new(
        anodizer_core::config::Config {
            project_name: "test".to_string(),
            ..Default::default()
        },
        ctx_opts,
    );
    ctx.template_vars_mut().set("Tag", "v1.0.0");
    ctx.template_vars_mut().set("ProjectName", "test");
    ctx.template_vars_mut().set("ProviderName", "s3");

    let result = crate::run::validate_only(&config, &ctx);
    assert!(
        result.is_err(),
        "templated provider that renders to 's3' must reach the S3 ACL \
         validator and reject 'invalid-acl-value'; got Ok"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("invalid S3 canned ACL"),
        "validator error must name the S3 ACL rejection (proves the \
         template rendered, dispatched into S3, and reached the ACL gate); \
         got: {err}"
    );
    assert!(
        err.contains("invalid-acl-value"),
        "validator error should echo the offending ACL value, got: {err}"
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
    let log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);

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
        &log,
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
    let log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);

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
        &log,
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

/// Important #3 — `upload_files_owned` returns the list of keys that
/// successfully landed so `BlobPublisher::run` can record only landed
/// uploads in evidence (not the planned set). The two test files both
/// succeed; the returned vec carries both keys in deterministic order.
#[test]
fn upload_files_owned_returns_successful_keys() {
    let store: Arc<dyn ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let tmp = tempfile::TempDir::new().unwrap();
    let f1 = tmp.path().join("a.txt");
    std::fs::write(&f1, b"alpha").unwrap();
    let f2 = tmp.path().join("b.txt");
    std::fs::write(&f2, b"bravo").unwrap();

    let upload_items = vec![(f1, "alpha.txt".to_string()), (f2, "bravo.txt".to_string())];
    let config = BlobConfig::default();
    let ctx = make_ctx();
    let log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);
    let put_opts: Vec<PutOptions> = upload_items
        .iter()
        .map(|(_, k)| build_put_options(&config, k, &ctx).unwrap())
        .collect();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let keys = upload_files_owned(
        &rt,
        store,
        upload_items,
        "drop".to_string(),
        put_opts,
        1,
        None,
        &log,
    )
    .expect("upload should succeed");
    // Order is sorted (deterministic across runs) so evidence is stable.
    assert_eq!(keys, vec!["drop/alpha.txt", "drop/bravo.txt"]);
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
    let log = anodizer_core::log::StageLogger::new("test", anodizer_core::log::Verbosity::Quiet);

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
        &log,
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

// -----------------------------------------------------------------------
// Publish-report wire-up tests
//
// BlobStage runs as its own Stage (positioned BEFORE
// SnapcraftPublishStage in the pipeline) and must append a
// `PublisherResult` to `ctx.publish_report` so the submitter gate can
// see a required-blob failure via the same
// `any_failed(Assets, required_only=true)` check that already gates
// every other Submitter publisher.
// -----------------------------------------------------------------------

/// Build a `BlobTarget` for the `s3://my-bucket/<key>` shape used by the
/// `record_blob_result` test fixtures. `record_blob_result` takes
/// `&[BlobTarget]` instead of `&[String]` so the rollback DELETE path
/// has the structured (provider, bucket, key, region, endpoint) tuple
/// it needs to reconstruct the store.
fn mk_target(key: &str) -> crate::publisher::BlobTarget {
    crate::publisher::BlobTarget {
        provider: "s3".to_string(),
        bucket: "my-bucket".to_string(),
        key: key.to_string(),
        region: None,
        endpoint: None,
    }
}

#[test]
fn blob_stage_appends_succeeded_to_publish_report() {
    use crate::run::record_blob_result;
    use anodizer_core::{PublisherGroup, PublisherOutcome};

    let mut ctx = make_ctx();
    let uploaded = vec![mk_target("proj/v1/a.tar.gz"), mk_target("proj/v1/b.tar.gz")];
    record_blob_result(&mut ctx, &uploaded, &Ok(()), /* required = */ false);

    let report = ctx
        .publish_report()
        .expect("publish_report initialized by blob stage");
    assert_eq!(report.results.len(), 1);
    let r = &report.results[0];
    assert_eq!(r.name, "blob");
    assert_eq!(r.group, PublisherGroup::Assets);
    assert!(
        !r.required,
        "default `required = false` until BlobConfig.required opts in"
    );
    assert!(
        matches!(r.outcome, PublisherOutcome::Succeeded),
        "expected Succeeded, got {:?}",
        r.outcome
    );
    let evidence = r
        .evidence
        .as_ref()
        .expect("succeeded entry carries evidence");
    assert_eq!(
        evidence.primary_ref.as_deref(),
        Some("s3://my-bucket/proj/v1/a.tar.gz")
    );
    assert_eq!(evidence.artifact_paths.len(), 2);
}

#[test]
fn blob_stage_appends_failed_to_publish_report() {
    use crate::run::record_blob_result;
    use anodizer_core::PublisherOutcome;

    let mut ctx = make_ctx();
    // Mid-stream failure: one key landed before the upload errored. The
    // partial-success list is preserved on the helper input, but
    // failed entries record no evidence so a downstream rollback can't
    // mistakenly treat the failed publisher as having a clean
    // artifact_paths snapshot.
    let partial = vec![mk_target("proj/v1/a.tar.gz")];
    let err = anyhow::anyhow!("upload failed: 503 Service Unavailable");
    record_blob_result(&mut ctx, &partial, &Err(err), /* required = */ false);

    let report = ctx.publish_report().expect("publish_report initialized");
    assert_eq!(report.results.len(), 1);
    let r = &report.results[0];
    match &r.outcome {
        PublisherOutcome::Failed(msg) => {
            assert!(
                msg.contains("503"),
                "Failed message should preserve the error text, got {msg}"
            )
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(
        r.evidence.is_none(),
        "Failed entries must not carry evidence (downstream rollback safety)"
    );
}

#[test]
fn blob_stage_initializes_publish_report_when_none() {
    use crate::run::record_blob_result;

    // PublishStage was skipped (e.g. `--publish blob` subset run), so
    // `ctx.publish_report` is None. BlobStage must initialize the
    // report on first append so the SnapcraftPublishStage gate has a
    // report to consult.
    let mut ctx = make_ctx();
    assert!(ctx.publish_report().is_none(), "fixture invariant");

    record_blob_result(&mut ctx, &[], &Ok(()), /* required = */ false);

    let report = ctx
        .publish_report()
        .expect("BlobStage initializes the report when None");
    assert_eq!(report.results.len(), 1);
    assert_eq!(report.results[0].name, "blob");
}

#[test]
fn blob_stage_skips_via_gate_on_required_upstream_failure() {
    use anodizer_core::{PublisherGroup, PublisherOutcome, PublisherResult, SkipReason};

    // v0.8.0: BlobStage runs after the trait dispatch. A required upstream
    // failure (here a required Manager publisher) must close the gate and
    // skip the blob upload — uploading more reversible bytes to an
    // already-broken release just orphans assets. The gate fires at the top
    // of `run`, before the no-work check, so a pre-seeded report alone trips
    // it deterministically without staging real artifacts.
    let mut ctx = make_ctx();
    let mut report = anodizer_core::PublishReport::default();
    report.results.push(PublisherResult {
        name: "homebrew".to_string(),
        group: PublisherGroup::Manager,
        required: true,
        outcome: PublisherOutcome::Failed("tap push rejected".to_string()),
        evidence: None,
    });
    ctx.publish_report = Some(report);

    BlobStage.run(&mut ctx).expect("gate path returns Ok");

    let blob = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .find(|r| r.name == "blob")
        .expect("blob entry recorded");
    assert_eq!(
        blob.outcome,
        PublisherOutcome::Skipped(SkipReason::SubmitterGated),
        "a required upstream failure must gate the blob upload"
    );
    assert_eq!(blob.group, PublisherGroup::Assets);
}

#[test]
fn blob_stage_not_gated_on_optional_upstream_failure() {
    use anodizer_core::{PublisherGroup, PublisherOutcome, PublisherResult, SkipReason};

    // Continue-on-error preserved: an OPTIONAL upstream failure must NOT
    // gate blob. With no blob-configured crates the stage takes the no-work
    // path and records nothing — the tell that it passed the gate rather
    // than short-circuiting to Skipped(SubmitterGated).
    let mut ctx = make_ctx();
    let mut report = anodizer_core::PublishReport::default();
    report.results.push(PublisherResult {
        name: "cargo".to_string(),
        group: PublisherGroup::Submitter,
        required: false,
        outcome: PublisherOutcome::Failed("optional cargo boom".to_string()),
        evidence: None,
    });
    ctx.publish_report = Some(report);

    BlobStage.run(&mut ctx).expect("ungated path returns Ok");

    let gated = ctx
        .publish_report()
        .expect("report present")
        .results
        .iter()
        .any(|r| {
            r.name == "blob"
                && matches!(
                    r.outcome,
                    PublisherOutcome::Skipped(SkipReason::SubmitterGated)
                )
        });
    assert!(
        !gated,
        "an optional upstream failure must not gate the blob upload"
    );
}

#[test]
fn blob_stage_does_not_touch_publish_report_when_no_work() {
    // No blob configs → no jobs → no PublisherResult appended. The
    // submitter gate that follows BlobStage must NOT see a fabricated
    // "blob: Succeeded" entry just because the stage ran past an empty
    // crate set.
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
    stage.run(&mut ctx).expect("no-config run is Ok");
    assert!(
        ctx.publish_report().is_none(),
        "no work attempted → no report entry"
    );
}

// -----------------------------------------------------------------------
// `BlobConfig.required` wire-up
//
// `record_blob_result` now takes a `required: bool` parameter derived
// from any blob config opting in via `required: true`. The tests pin
// the three relevant shapes: default-off, opt-in-on, and aggregated
// (any-true-makes-the-stage-required). Aggregation matches the
// "any failed required publisher in Assets group fails the release"
// semantics in `PublishReport::any_failed`.
// -----------------------------------------------------------------------

#[test]
fn record_blob_result_required_false_by_default() {
    use crate::run::record_blob_result;

    let mut ctx = make_ctx();
    record_blob_result(
        &mut ctx,
        &[mk_target("k")],
        &Ok(()),
        /* required = */ false,
    );
    let report = ctx.publish_report().expect("report initialized");
    assert!(
        !report.results[0].required,
        "BlobConfig.required = None (default) → PublisherResult.required = false"
    );
}

#[test]
fn record_blob_result_required_true_when_set() {
    use crate::run::record_blob_result;

    let mut ctx = make_ctx();
    record_blob_result(
        &mut ctx,
        &[mk_target("k")],
        &Ok(()),
        /* required = */ true,
    );
    let report = ctx.publish_report().expect("report initialized");
    assert!(
        report.results[0].required,
        "required = true threads into PublisherResult.required"
    );
}

#[test]
fn derive_blob_required_aggregates_any_true_across_configs() {
    use crate::run::derive_blob_required;

    // Two blob configs on one crate; only the second opts in. The
    // stage-level aggregation must report `true` so a failed upload
    // anywhere in the stage trips the submitter gate.
    let crate_cfg = anodizer_core::config::CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        blobs: Some(vec![
            BlobConfig {
                provider: "s3".to_string(),
                bucket: "b1".to_string(),
                required: Some(false),
                ..Default::default()
            },
            BlobConfig {
                provider: "gcs".to_string(),
                bucket: "b2".to_string(),
                required: Some(true),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        crates: vec![crate_cfg],
        ..Default::default()
    };
    let ctx = Context::new(config, ContextOptions::default());
    assert!(
        derive_blob_required(&ctx),
        "any-true aggregation: one required-blob config makes the stage required"
    );
}

#[test]
fn derive_blob_required_false_when_no_config_opts_in() {
    use crate::run::derive_blob_required;

    let crate_cfg = anodizer_core::config::CrateConfig {
        name: "demo".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        blobs: Some(vec![BlobConfig {
            provider: "s3".to_string(),
            bucket: "b1".to_string(),
            // `required` omitted → defaults to None → false
            ..Default::default()
        }]),
        ..Default::default()
    };
    let config = anodizer_core::config::Config {
        project_name: "test".to_string(),
        crates: vec![crate_cfg],
        ..Default::default()
    };
    let ctx = Context::new(config, ContextOptions::default());
    assert!(
        !derive_blob_required(&ctx),
        "no config opts in → derived required = false"
    );
}

#[test]
fn record_blob_result_failed_required_blob_trips_assets_required_gate() {
    use crate::run::record_blob_result;

    // Integration-shaped unit test: a required blob fails →
    // `PublishReport::any_failed(Assets, required_only=true)` returns
    // true. This is exactly the predicate the SnapcraftPublishStage
    // submitter gate consults to skip-with-`Skipped(SubmitterGated)`.
    let mut ctx = make_ctx();
    let err = anyhow::anyhow!("upload failed: 503 Service Unavailable");
    record_blob_result(&mut ctx, &[], &Err(err), /* required = */ true);
    let report = ctx.publish_report().expect("report initialized");
    assert!(
        report.any_failed(
            anodizer_core::PublisherGroup::Assets,
            /* required_only = */ true
        ),
        "required-blob failure must trip the Assets-group required-only gate"
    );
    // And the non-required gate sees it too (sanity).
    assert!(report.any_failed(anodizer_core::PublisherGroup::Assets, false));
}
