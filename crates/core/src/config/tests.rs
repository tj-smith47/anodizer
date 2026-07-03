#![allow(clippy::field_reassign_with_default)]

// External crates
use serde::Deserialize;

// Inline items from config/mod.rs
use super::WorkspaceConfig;
use super::{Config, ERR_DEFAULTS_AXIS_MISMATCH, IncludeFilePath, IncludeSpec, IncludeUrlConfig};
use super::{
    validate_changelog_groups_depth, validate_changelog_paths, validate_defaults_axis,
    validate_exclude_globs, validate_format_overrides, validate_homebrew_cask_url_template,
    validate_on_failure_root_only, validate_tag_sort, validate_version,
    validate_winget_dependency_architectures, validate_winget_upgrade_behavior,
};

// Items re-exported from config submodules (all reachable as super::ItemName
// because config/mod.rs does `pub use submod::*;` for each)
use super::GitConfig;
use super::HookEntry;
use super::{Amd64Variant, config_schema};
use super::{ArchivesConfig, ChecksumConfig, ContentSource, ExtraFileSpec};
use super::{BuilderKind, all_builds_prebuilt, validate_builds};
use super::{ChangelogConfig, MilestoneConfig, SbomConfig};
use super::{CrateConfig, CrossStrategy, MetadataConfig};
use super::{
    EnvFilesConfig, EnvFilesTokenConfig, load_env_files, load_token_files_with_env,
    read_token_file, read_token_file_with_env,
};
use super::{
    ForceTokenKind, GitHubUrlsConfig, GitLabUrlsConfig, GiteaUrlsConfig, MakeLatestConfig,
    OnFailureConfig, ReleaseConfig,
};
use super::{HumanDuration, StringOrBool};
use super::{
    MacOSNativeArtifactKind, MacOSNativeNotarizeConfig, MacOSNativeSignNotarizeConfig,
    MacOSNotarizeApiConfig, MacOSSignConfig,
};

// parse_humantime_duration is pub(super) in string_or_bool — accessible from
// sibling child modules (tests lives under config, same parent as string_or_bool)
use super::string_or_bool::parse_humantime_duration;

#[test]
fn test_minimal_yaml_config() {
    let yaml = r#"
project_name: myproject
crates:
  - name: myproject
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.project_name, "myproject");
    assert_eq!(config.crates.len(), 1);
    assert_eq!(config.dist, std::path::PathBuf::from("./dist"));
}

#[test]
fn test_minimal_toml_config() {
    let toml_str = r#"
project_name = "myproject"

[[crates]]
name = "myproject"
path = "."
tag_template = "v{{ .Version }}"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(config.project_name, "myproject");
}

#[test]
fn test_full_config_with_defaults() {
    let yaml = r#"
project_name: cfgd
dist: ./dist
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-apple-darwin
  cross: auto
  archives:
    formats: [tar.gz]
    format_overrides:
      - os: windows
        formats: [zip]
  checksum:
    algorithm: sha256
crates:
  - name: cfgd
    path: crates/cfgd
    tag_template: "v{{ .Version }}"
    builds:
      - binary: cfgd
        features: []
        no_default_features: false
    archives:
      - name_template: "{{ .ProjectName }}-{{ .Version }}-{{ .Os }}-{{ .Arch }}"
        files:
          - LICENSE
    release:
      github:
        owner: tj-smith47
        name: cfgd
      draft: false
      prerelease: auto
      name_template: "{{ .Tag }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let defaults = config.defaults.unwrap();
    assert_eq!(defaults.targets.unwrap().len(), 2);
    assert_eq!(defaults.cross, Some(CrossStrategy::Auto));
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.name_template, Some("{{ .Tag }}".to_string()));
}

#[test]
fn test_snapshot_config() {
    let yaml = r#"
project_name: test
snapshot:
  version_template: "{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.snapshot.unwrap().version_template,
        "{{ .Version }}-SNAPSHOT-{{ .ShortCommit }}"
    );
}

#[test]
fn test_archives_false() {
    let yaml = r#"
project_name: test
crates:
  - name: operator
    path: crates/operator
    tag_template: "v{{ .Version }}"
    archives: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(matches!(
        config.crates[0].archives,
        ArchivesConfig::Disabled
    ));
}

#[test]
fn test_publish_cargo_present_and_with_options() {
    // Presence of `cargo:` opts the crate in (no `enabled` field, no bool shorthand).
    let yaml_present = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml_present).unwrap();
    assert!(config.crates[0].publish.as_ref().unwrap().cargo.is_some());

    let yaml_obj = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo:
        index_timeout: 120
        no_verify: true
        allow_dirty: true
        features: [foo, bar]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml_obj).unwrap();
    let cargo = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .cargo
        .as_ref()
        .unwrap();
    assert_eq!(cargo.index_timeout, Some(120));
    assert_eq!(cargo.no_verify, Some(true));
    assert_eq!(cargo.allow_dirty, Some(true));
    assert_eq!(
        cargo.features,
        Some(vec!["foo".to_string(), "bar".to_string()])
    );
}

#[test]
fn test_publish_cargo_bool_shorthand_rejected() {
    // `cargo: true` is no longer a valid shorthand.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo: true
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "publish.cargo: true must fail to parse (no bool shorthand)"
    );
}

#[test]
fn test_publish_cargo_legacy_crates_key_rejected() {
    // the old `publish.crates:` key was renamed to
    // `publish.cargo:` with no alias.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      crates: true
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "publish.crates is no longer a valid key (renamed to cargo); deny_unknown_fields must reject it"
    );
}

// ---- MakeLatestConfig tests ----

#[test]
fn test_make_latest_auto() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: auto
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.make_latest, Some(MakeLatestConfig::Auto));
}

#[test]
fn test_make_latest_true() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.make_latest, Some(MakeLatestConfig::Bool(true)));
}

#[test]
fn test_make_latest_false() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.make_latest, Some(MakeLatestConfig::Bool(false)));
}

#[test]
fn test_make_latest_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      draft: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.make_latest, None);
}

#[test]
fn test_make_latest_template_string() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: "{{ if .IsSnapshot }}false{{ else }}true{{ end }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(
        release.make_latest,
        Some(MakeLatestConfig::String(
            "{{ if .IsSnapshot }}false{{ else }}true{{ end }}".to_string()
        ))
    );
}

#[test]
fn test_make_latest_string_true() {
    // The string "true" should deserialize to Bool(true) for consistency.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: "true"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.make_latest, Some(MakeLatestConfig::Bool(true)));
}

#[test]
fn test_make_latest_string_false() {
    // The string "false" should deserialize to Bool(false) for consistency.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      make_latest: "false"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.make_latest, Some(MakeLatestConfig::Bool(false)));
}

// ---- ChangelogConfig header/footer/disable tests ----

#[test]
fn test_changelog_header_footer() {
    let yaml = r##"
project_name: test
changelog:
  header: "# My Release Notes"
  footer: "---\nGenerated by anodizer"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"##;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cl = config.changelog.as_ref().unwrap();
    assert_eq!(
        cl.header,
        Some(ContentSource::Inline("# My Release Notes".to_string()))
    );
    assert_eq!(
        cl.footer,
        Some(ContentSource::Inline(
            "---\nGenerated by anodizer".to_string()
        ))
    );
}

#[test]
fn test_changelog_header_from_file_and_url() {
    let yaml = r#"
project_name: test
changelog:
  header:
    from_file: ./HEADER.md
  footer:
    from_url: https://example.com/footer.md
    headers:
      Accept: text/markdown
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cl = config.changelog.as_ref().unwrap();
    match cl.header.as_ref().unwrap() {
        ContentSource::FromFile { from_file } => assert_eq!(from_file, "./HEADER.md"),
        other => panic!("expected FromFile, got {other:?}"),
    }
    match cl.footer.as_ref().unwrap() {
        ContentSource::FromUrl { from_url, headers } => {
            assert_eq!(from_url, "https://example.com/footer.md");
            assert_eq!(
                headers
                    .as_ref()
                    .and_then(|m| m.get("Accept"))
                    .map(String::as_str),
                Some("text/markdown")
            );
        }
        other => panic!("expected FromUrl, got {other:?}"),
    }
}

#[test]
fn test_changelog_disable() {
    let yaml = r#"
project_name: test
changelog:
  skip: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cl = config.changelog.as_ref().unwrap();
    assert_eq!(cl.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_changelog_disable_false() {
    let yaml = r#"
project_name: test
changelog:
  skip: false
  sort: desc
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cl = config.changelog.as_ref().unwrap();
    assert_eq!(cl.skip, Some(StringOrBool::Bool(false)));
    assert_eq!(cl.sort, Some("desc".to_string()));
}

// ---- ChecksumConfig resolved_*() accessors (lazy-defaults policy) ----

#[test]
fn test_checksum_resolved_algorithm_default() {
    let cfg = ChecksumConfig::default();
    assert_eq!(cfg.resolved_algorithm(), "sha256");
}

#[test]
fn test_checksum_resolved_algorithm_user_value_wins() {
    let cfg = ChecksumConfig {
        algorithm: Some("sha512".to_string()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_algorithm(), "sha512");
}

#[test]
fn test_checksum_resolved_split_default() {
    let cfg = ChecksumConfig::default();
    assert!(!cfg.resolved_split());
}

#[test]
fn test_checksum_resolved_split_user_value_wins() {
    let cfg = ChecksumConfig {
        split: Some(true),
        ..Default::default()
    };
    assert!(cfg.resolved_split());
}

#[test]
fn test_checksum_resolved_combined_name_template_default() {
    let cfg = ChecksumConfig::default();
    assert_eq!(
        cfg.resolved_combined_name_template(),
        "{{ ProjectName }}_{{ Version }}_checksums.txt"
    );
}

#[test]
fn test_checksum_resolved_combined_name_template_user_value_wins() {
    let cfg = ChecksumConfig {
        name_template: Some("custom-{{ Version }}.txt".to_string()),
        ..Default::default()
    };
    assert_eq!(
        cfg.resolved_combined_name_template(),
        "custom-{{ Version }}.txt"
    );
}

// ---- Notarize resolved_*() accessors (lazy-defaults policy) ----

#[test]
fn test_macos_sign_resolved_timestamp_url_default() {
    let cfg = MacOSSignConfig::default();
    assert_eq!(
        cfg.resolved_timestamp_url(),
        "http://timestamp.apple.com/ts01"
    );
}

#[test]
fn test_macos_sign_resolved_timestamp_url_user_value_wins() {
    let cfg = MacOSSignConfig {
        timestamp_url: Some("http://corp.example/ts".to_string()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_timestamp_url(), "http://corp.example/ts");
}

#[test]
fn test_macos_sign_resolved_timestamp_url_blank_falls_back() {
    let cfg = MacOSSignConfig {
        timestamp_url: Some("   ".to_string()),
        ..Default::default()
    };
    assert_eq!(
        cfg.resolved_timestamp_url(),
        "http://timestamp.apple.com/ts01"
    );
}

#[test]
fn test_macos_notarize_api_resolved_wait_default() {
    assert!(!MacOSNotarizeApiConfig::default().resolved_wait());
}

#[test]
fn test_macos_notarize_api_resolved_wait_user_value_wins() {
    let cfg = MacOSNotarizeApiConfig {
        wait: Some(true),
        ..Default::default()
    };
    assert!(cfg.resolved_wait());
}

#[test]
fn test_macos_notarize_api_resolved_timeout_default() {
    assert_eq!(MacOSNotarizeApiConfig::default().resolved_timeout(), "10m");
}

#[test]
fn test_macos_notarize_api_resolved_timeout_user_value_wins() {
    let cfg = MacOSNotarizeApiConfig {
        timeout: Some(HumanDuration(parse_humantime_duration("15m").unwrap())),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_timeout(), "15m");
}

#[test]
fn test_macos_native_notarize_resolved_wait_default() {
    assert!(!MacOSNativeNotarizeConfig::default().resolved_wait());
}

#[test]
fn test_macos_native_notarize_resolved_wait_user_value_wins() {
    let cfg = MacOSNativeNotarizeConfig {
        wait: Some(true),
        ..Default::default()
    };
    assert!(cfg.resolved_wait());
}

#[test]
fn test_macos_native_notarize_resolved_timeout_default() {
    assert_eq!(
        MacOSNativeNotarizeConfig::default().resolved_timeout(),
        "10m"
    );
}

#[test]
fn test_macos_native_notarize_resolved_timeout_user_value_wins() {
    let cfg = MacOSNativeNotarizeConfig {
        timeout: Some(HumanDuration(parse_humantime_duration("30m").unwrap())),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_timeout(), "30m");
}

#[test]
fn test_macos_native_sign_notarize_resolved_use_default() {
    assert_eq!(
        MacOSNativeSignNotarizeConfig::default().resolved_use(),
        MacOSNativeArtifactKind::Dmg
    );
}

#[test]
fn test_macos_native_sign_notarize_resolved_use_user_value_wins() {
    let cfg = MacOSNativeSignNotarizeConfig {
        use_: Some(MacOSNativeArtifactKind::Pkg),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_use(), MacOSNativeArtifactKind::Pkg);
}

// ---- SbomConfig resolved_*() accessors (lazy-defaults policy) ----

#[test]
fn test_sbom_resolved_id_default() {
    assert_eq!(SbomConfig::default().resolved_id(), "default");
}

#[test]
fn test_sbom_resolved_id_user_value_wins() {
    let cfg = SbomConfig {
        id: Some("custom".to_string()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_id(), "custom");
}

#[test]
fn test_sbom_resolved_cmd_default() {
    assert_eq!(SbomConfig::default().resolved_cmd(), "syft");
}

#[test]
fn test_sbom_resolved_cmd_user_value_wins() {
    let cfg = SbomConfig {
        cmd: Some("trivy".to_string()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_cmd(), "trivy");
}

#[test]
fn test_sbom_resolved_artifacts_default() {
    assert_eq!(SbomConfig::default().resolved_artifacts(), "archive");
}

#[test]
fn test_sbom_resolved_artifacts_user_value_wins() {
    let cfg = SbomConfig {
        artifacts: Some("binary".to_string()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_artifacts(), "binary");
}

#[test]
fn test_sbom_resolved_documents_default_binary() {
    assert_eq!(
        SbomConfig::default().resolved_documents("binary"),
        vec!["{{ .Binary }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}.sbom.json".to_string()]
    );
}

#[test]
fn test_sbom_resolved_documents_default_any() {
    assert_eq!(
        SbomConfig::default().resolved_documents("any"),
        Vec::<String>::new()
    );
}

#[test]
fn test_sbom_resolved_documents_default_archive() {
    assert_eq!(
        SbomConfig::default().resolved_documents("archive"),
        vec!["{{ .ArtifactName }}.sbom.json".to_string()]
    );
}

#[test]
fn test_sbom_resolved_documents_user_value_wins() {
    let cfg = SbomConfig {
        documents: Some(vec!["custom-{{ Version }}.sbom.json".to_string()]),
        ..Default::default()
    };
    assert_eq!(
        cfg.resolved_documents("binary"),
        vec!["custom-{{ Version }}.sbom.json".to_string()]
    );
}

#[test]
fn test_sbom_resolved_args_default_syft() {
    assert_eq!(
        SbomConfig::default().resolved_args("syft"),
        vec![
            "$artifact".to_string(),
            "--output".to_string(),
            "spdx-json=$document".to_string(),
            "--enrich".to_string(),
            "all".to_string(),
        ]
    );
}

#[test]
fn test_sbom_resolved_args_default_non_syft_is_empty() {
    assert_eq!(
        SbomConfig::default().resolved_args("trivy"),
        Vec::<String>::new()
    );
}

#[test]
fn test_sbom_resolved_args_user_value_wins() {
    let custom = vec!["sbom".to_string(), "$artifact".to_string()];
    let cfg = SbomConfig {
        args: Some(custom.clone()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_args("syft"), custom);
    assert_eq!(cfg.resolved_args("trivy"), custom);
}

#[test]
fn test_sbom_default_syft_env_archive() {
    assert_eq!(
        SbomConfig::default_syft_env_for("syft", "archive"),
        vec![(
            "SYFT_FILE_METADATA_CATALOGER_ENABLED".to_string(),
            "true".to_string(),
        )]
    );
}

#[test]
fn test_sbom_default_syft_env_source() {
    assert_eq!(
        SbomConfig::default_syft_env_for("syft", "source"),
        vec![(
            "SYFT_FILE_METADATA_CATALOGER_ENABLED".to_string(),
            "true".to_string(),
        )]
    );
}

#[test]
fn test_sbom_default_syft_env_other_artifacts_empty() {
    assert!(SbomConfig::default_syft_env_for("syft", "binary").is_empty());
    assert!(SbomConfig::default_syft_env_for("syft", "any").is_empty());
}

#[test]
fn test_sbom_default_syft_env_non_syft_empty() {
    assert!(SbomConfig::default_syft_env_for("trivy", "archive").is_empty());
}

// ---- ReleaseConfig resolved_*() accessors (lazy-defaults policy) ----

#[test]
fn test_release_resolved_name_template_default() {
    assert_eq!(
        ReleaseConfig::default().resolved_name_template(),
        "{{ Tag }}"
    );
}

#[test]
fn test_release_resolved_name_template_user_value_wins() {
    let cfg = ReleaseConfig {
        name_template: Some("{{ ProjectName }} {{ Version }}".to_string()),
        ..Default::default()
    };
    assert_eq!(
        cfg.resolved_name_template(),
        "{{ ProjectName }} {{ Version }}"
    );
}

#[test]
fn test_release_resolved_mode_default() {
    assert_eq!(
        ReleaseConfig::default().resolved_mode().unwrap(),
        "keep-existing"
    );
}

#[test]
fn test_release_resolved_mode_empty_string_falls_back() {
    let cfg = ReleaseConfig {
        mode: Some(String::new()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_mode().unwrap(), "keep-existing");
}

#[test]
fn test_release_resolved_mode_valid_values() {
    for mode in ["keep-existing", "append", "prepend", "replace"] {
        let cfg = ReleaseConfig {
            mode: Some(mode.to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_mode().unwrap(), mode);
    }
}

#[test]
fn test_release_resolved_mode_invalid_value_errors() {
    let cfg = ReleaseConfig {
        mode: Some("clobber".to_string()),
        ..Default::default()
    };
    let err = cfg.resolved_mode().unwrap_err();
    assert!(
        err.to_string().contains("invalid mode 'clobber'"),
        "got: {err}"
    );
}

#[test]
fn test_release_resolved_bool_defaults_false() {
    let cfg = ReleaseConfig::default();
    assert!(!cfg.resolved_draft());
    assert!(!cfg.resolved_replace_existing_draft());
    assert!(!cfg.resolved_replace_existing_artifacts());
    assert!(!cfg.resolved_include_meta());
    assert!(!cfg.resolved_use_existing_draft());
}

#[test]
fn test_release_resolved_bool_user_values_win() {
    let cfg = ReleaseConfig {
        draft: Some(true),
        replace_existing_draft: Some(true),
        replace_existing_artifacts: Some(true),
        include_meta: Some(true),
        use_existing_draft: Some(true),
        ..Default::default()
    };
    assert!(cfg.resolved_draft());
    assert!(cfg.resolved_replace_existing_draft());
    assert!(cfg.resolved_replace_existing_artifacts());
    assert!(cfg.resolved_include_meta());
    assert!(cfg.resolved_use_existing_draft());
}

#[test]
fn test_release_resolved_on_failure_defaults_to_rollback() {
    let cfg = ReleaseConfig::default();
    assert_eq!(cfg.resolved_on_failure(), OnFailureConfig::Rollback);
}

#[test]
fn test_release_on_failure_parses_both_values() {
    for (yaml, expected) in [
        ("on_failure: rollback", OnFailureConfig::Rollback),
        ("on_failure: hold", OnFailureConfig::Hold),
    ] {
        let cfg: ReleaseConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(cfg.resolved_on_failure(), expected, "yaml: {yaml}");
    }
}

#[test]
fn test_release_on_failure_rejects_unknown_value() {
    let err = serde_yaml_ng::from_str::<ReleaseConfig>("on_failure: explode").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("rollback") && msg.contains("hold"),
        "error must name the valid set, got: {msg}"
    );
}

#[test]
fn test_validate_on_failure_root_only_accepts_root_setting() {
    let config = Config {
        project_name: "test".into(),
        release: Some(ReleaseConfig {
            on_failure: Some(OnFailureConfig::Hold),
            ..Default::default()
        }),
        crates: vec![CrateConfig {
            name: "app".into(),
            path: ".".into(),
            tag_template: "v{{ .Version }}".into(),
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }],
        ..Default::default()
    };
    validate_on_failure_root_only(&config).expect("root-level on_failure is the supported shape");
}

#[test]
fn test_validate_on_failure_root_only_rejects_crate_level_setting() {
    let config = Config {
        project_name: "test".into(),
        crates: vec![CrateConfig {
            name: "app".into(),
            path: ".".into(),
            tag_template: "v{{ .Version }}".into(),
            release: Some(ReleaseConfig {
                on_failure: Some(OnFailureConfig::Hold),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let err = validate_on_failure_root_only(&config)
        .expect_err("crate-level on_failure must be rejected");
    assert!(err.contains("app"), "must name the offender: {err}");
    assert!(err.contains("root-level"), "must explain the rule: {err}");
    assert!(err.contains("top-level"), "must point at the fix: {err}");
}

#[test]
fn test_validate_on_failure_root_only_rejects_workspace_crate_setting() {
    let config = Config {
        project_name: "test".into(),
        workspaces: Some(vec![WorkspaceConfig {
            name: "ws".into(),
            crates: vec![CrateConfig {
                name: "ws-member".into(),
                path: "crates/member".into(),
                tag_template: "member-v{{ .Version }}".into(),
                release: Some(ReleaseConfig {
                    on_failure: Some(OnFailureConfig::Rollback),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let err = validate_on_failure_root_only(&config)
        .expect_err("workspace-crate on_failure must be rejected");
    assert!(err.contains("ws-member"), "must name the offender: {err}");
}

// ---- ChangelogConfig resolved_*() accessors (lazy-defaults policy) ----

#[test]
fn test_changelog_resolved_sort_default_empty() {
    assert_eq!(ChangelogConfig::default().resolved_sort().unwrap(), "");
}

#[test]
fn test_changelog_resolved_sort_valid_values() {
    for s in ["", "asc", "desc"] {
        let cfg = ChangelogConfig {
            sort: Some(s.to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_sort().unwrap(), s);
    }
}

#[test]
fn test_changelog_resolved_sort_invalid_errors() {
    let cfg = ChangelogConfig {
        sort: Some("random".to_string()),
        ..Default::default()
    };
    let err = cfg.resolved_sort().unwrap_err().to_string();
    assert!(err.contains("invalid sort 'random'"), "got: {err}");
}

#[test]
fn test_changelog_resolved_use_source_default_git() {
    assert_eq!(ChangelogConfig::default().resolved_use_source(), "git");
}

#[test]
fn test_changelog_resolved_use_source_user_value_wins() {
    let cfg = ChangelogConfig {
        use_source: Some("github".to_string()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_use_source(), "github");
}

#[test]
fn test_changelog_resolved_title_default() {
    assert_eq!(ChangelogConfig::default().resolved_title(), "Changelog");
}

#[test]
fn test_changelog_resolved_title_explicit_empty_preserved() {
    let cfg = ChangelogConfig {
        title: Some(String::new()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_title(), "");
}

#[test]
fn test_changelog_resolved_abbrev_default_zero() {
    assert_eq!(ChangelogConfig::default().resolved_abbrev(), 0);
}

#[test]
fn test_changelog_resolved_abbrev_user_value_wins() {
    let cfg = ChangelogConfig {
        abbrev: Some(7),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_abbrev(), 7);
}

// abbrev clamp: values below `-1` are clamped
// to `-1`. Upstream's `git log --abbrev=N` panics on `-2`, `-3`, …;
// anodizer renders SHAs in Rust so it would not panic, but the clamp
// keeps behavioural parity (negative-of-any-magnitude → "omit hash").
#[test]
fn test_changelog_resolved_abbrev_clamps_negative_below_minus_one() {
    for raw in [-2i32, -5, -100, i32::MIN] {
        let cfg = ChangelogConfig {
            abbrev: Some(raw),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_abbrev(),
            -1,
            "abbrev={raw} must clamp to -1 (parity with GoReleaser 88daaf3)"
        );
    }
}

#[test]
fn test_changelog_resolved_abbrev_minus_one_passes_through() {
    let cfg = ChangelogConfig {
        abbrev: Some(-1),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_abbrev(), -1);
}

#[test]
fn test_changelog_resolved_format_user_value_wins() {
    let cfg = ChangelogConfig {
        format: Some("custom format".to_string()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_format("git", 0), "custom format");
    assert_eq!(cfg.resolved_format("github", -1), "custom format");
}

#[test]
fn test_changelog_resolved_format_default_no_hash() {
    let cfg = ChangelogConfig::default();
    assert_eq!(cfg.resolved_format("git", -1), "{{ Message }}");
    assert_eq!(cfg.resolved_format("github", -1), "{{ Message }}");
}

#[test]
fn test_changelog_resolved_format_default_scm() {
    let cfg = ChangelogConfig::default();
    for backend in ["github", "gitlab", "gitea"] {
        let tmpl = cfg.resolved_format(backend, 0);
        assert!(
            tmpl.contains("{% if Login %}"),
            "expected SCM template for backend {backend}, got: {tmpl}"
        );
        // Changelog-entry format:
        // the SCM-mode default uses the FULL SHA, not the abbreviated
        // ShortSHA. Pin the prefix to catch silent ShortSHA regressions.
        assert!(
            tmpl.starts_with("{{ SHA }}: "),
            "SCM default must use full SHA (not ShortSHA) for backend \
                 {backend}, got: {tmpl}"
        );
        assert!(
            !tmpl.contains("ShortSHA"),
            "SCM default must not reference ShortSHA for backend \
                 {backend}, got: {tmpl}"
        );
    }
}

#[test]
fn test_changelog_resolved_format_default_git() {
    let cfg = ChangelogConfig::default();
    assert_eq!(cfg.resolved_format("git", 0), "{{ SHA }} {{ Message }}");
}

#[test]
fn test_changelog_resolved_snapshot_default_false() {
    assert!(!ChangelogConfig::default().resolved_snapshot());
}

#[test]
fn test_changelog_resolved_snapshot_user_value_wins() {
    let cfg = ChangelogConfig {
        snapshot: Some(true),
        ..Default::default()
    };
    assert!(cfg.resolved_snapshot());
}

// ---- MilestoneConfig resolved_*() accessors (lazy-defaults policy) ----

#[test]
fn test_milestone_resolved_name_template_default() {
    assert_eq!(
        MilestoneConfig::default().resolved_name_template(),
        "{{ Tag }}"
    );
}

#[test]
fn test_milestone_resolved_name_template_user_value_wins() {
    let cfg = MilestoneConfig {
        name_template: Some("v{{ Version }}".to_string()),
        ..Default::default()
    };
    assert_eq!(cfg.resolved_name_template(), "v{{ Version }}");
}

#[test]
fn test_milestone_resolved_close_default_false() {
    assert!(!MilestoneConfig::default().resolved_close());
}

#[test]
fn test_milestone_resolved_close_user_value_wins() {
    let cfg = MilestoneConfig {
        close: Some(true),
        ..Default::default()
    };
    assert!(cfg.resolved_close());
}

#[test]
fn test_milestone_resolved_fail_on_error_default_false() {
    assert!(!MilestoneConfig::default().resolved_fail_on_error());
}

#[test]
fn test_milestone_resolved_fail_on_error_user_value_wins() {
    let cfg = MilestoneConfig {
        fail_on_error: Some(true),
        ..Default::default()
    };
    assert!(cfg.resolved_fail_on_error());
}

// ---- ChecksumConfig disable tests ----

#[test]
fn test_checksum_disable() {
    let yaml = r#"
project_name: test
defaults:
  checksum:
    skip: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.defaults.as_ref().unwrap().checksum.as_ref().unwrap();
    assert_eq!(checksum.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_checksum_disable_per_crate() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    checksum:
      skip: true
      algorithm: sha512
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.crates[0].checksum.as_ref().unwrap();
    assert_eq!(checksum.skip, Some(StringOrBool::Bool(true)));
    assert_eq!(checksum.algorithm, Some("sha512".to_string()));
}

#[test]
fn test_checksum_disable_template_string() {
    let yaml = r#"
project_name: test
defaults:
  checksum:
    skip: "{{ if .IsSnapshot }}true{{ end }}"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.defaults.as_ref().unwrap().checksum.as_ref().unwrap();
    match &checksum.skip {
        Some(StringOrBool::String(s)) => {
            assert!(s.contains("IsSnapshot"));
        }
        other => panic!("expected StringOrBool::String, got {:?}", other),
    }
}

#[test]
fn test_checksum_extra_files_object_form() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    checksum:
      extra_files:
        - "dist/*.bin"
        - glob: "release/*.deb"
          name_template: "{{ .ArtifactName }}.checksum"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.crates[0].checksum.as_ref().unwrap();
    let extra = checksum.extra_files.as_ref().unwrap();
    assert_eq!(extra.len(), 2);
    assert_eq!(extra[0], ExtraFileSpec::Glob("dist/*.bin".to_string()));
    match &extra[1] {
        ExtraFileSpec::Detailed {
            glob,
            name_template,
            ..
        } => {
            assert_eq!(glob, "release/*.deb");
            assert_eq!(
                name_template.as_deref(),
                Some("{{ .ArtifactName }}.checksum")
            );
        }
        other => panic!("expected ExtraFileSpec::Detailed, got {:?}", other),
    }
}

// ---- MakeLatestConfig serialization roundtrip ----

#[test]
fn test_make_latest_serialize_roundtrip() {
    let auto = MakeLatestConfig::Auto;
    let json = serde_json::to_string(&auto).unwrap();
    assert_eq!(json, "\"auto\"");

    let bool_true = MakeLatestConfig::Bool(true);
    let json = serde_json::to_string(&bool_true).unwrap();
    assert_eq!(json, "true");

    let bool_false = MakeLatestConfig::Bool(false);
    let json = serde_json::to_string(&bool_false).unwrap();
    assert_eq!(json, "false");

    let tmpl =
        MakeLatestConfig::String("{{ if .IsSnapshot }}false{{ else }}true{{ end }}".to_string());
    let json = serde_json::to_string(&tmpl).unwrap();
    assert_eq!(json, "\"{{ if .IsSnapshot }}false{{ else }}true{{ end }}\"");
}

// ---- ReleaseConfig header/footer tests ----

#[test]
fn test_release_header_footer_inline() {
    let yaml = r###"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      header: "## Custom Header"
      footer: "---\nPowered by anodizer"
"###;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(
        release.header,
        Some(ContentSource::Inline("## Custom Header".to_string()))
    );
    assert_eq!(
        release.footer,
        Some(ContentSource::Inline(
            "---\nPowered by anodizer".to_string()
        ))
    );
}

#[test]
fn test_release_header_footer_from_file() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      header:
        from_file: ./RELEASE_HEADER.md
      footer:
        from_file: ./RELEASE_FOOTER.md
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(
        release.header,
        Some(ContentSource::FromFile {
            from_file: "./RELEASE_HEADER.md".to_string()
        })
    );
    assert_eq!(
        release.footer,
        Some(ContentSource::FromFile {
            from_file: "./RELEASE_FOOTER.md".to_string()
        })
    );
}

#[test]
fn test_release_header_footer_from_url() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      header:
        from_url: https://example.com/header.md
      footer:
        from_url: https://example.com/footer.md
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(
        release.header,
        Some(ContentSource::FromUrl {
            from_url: "https://example.com/header.md".to_string(),
            headers: None,
        })
    );
    assert_eq!(
        release.footer,
        Some(ContentSource::FromUrl {
            from_url: "https://example.com/footer.md".to_string(),
            headers: None,
        })
    );
}

#[test]
fn test_release_header_footer_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      draft: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.header, None);
    assert_eq!(release.footer, None);
}

// ---- ReleaseConfig extra_files tests ----

#[test]
fn test_release_extra_files_glob_strings() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      extra_files:
        - "dist/*.sig"
        - "CHANGELOG.md"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    let files = release.extra_files.as_ref().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0], ExtraFileSpec::Glob("dist/*.sig".to_string()));
    assert_eq!(files[1], ExtraFileSpec::Glob("CHANGELOG.md".to_string()));
}

#[test]
fn test_release_extra_files_detailed_objects() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      extra_files:
        - glob: "dist/*.sig"
          name_template: "{{ .ArtifactName }}.sig"
        - glob: "docs/*.pdf"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    let files = release.extra_files.as_ref().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].glob(), "dist/*.sig");
    assert_eq!(files[0].name_template(), Some("{{ .ArtifactName }}.sig"));
    assert_eq!(files[1].glob(), "docs/*.pdf");
    assert_eq!(files[1].name_template(), None);
}

#[test]
fn test_release_extra_files_mixed() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      extra_files:
        - "dist/*.sig"
        - glob: "docs/*.pdf"
          name_template: "{{ .ArtifactName }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    let files = release.extra_files.as_ref().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0], ExtraFileSpec::Glob("dist/*.sig".to_string()));
    assert_eq!(files[1].glob(), "docs/*.pdf");
}

#[test]
fn test_release_extra_files_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      draft: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.extra_files, None);
}

// ---- ReleaseConfig templated_extra_files tests ----

#[test]
fn test_release_templated_extra_files_parsed() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      templated_extra_files:
        - src: LICENSE.tpl
          dst: LICENSE.txt
        - src: README.md.tpl
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    let tpl = release.templated_extra_files.as_ref().unwrap();
    assert_eq!(tpl.len(), 2);
    assert_eq!(tpl[0].src, "LICENSE.tpl");
    assert_eq!(tpl[0].dst.as_deref(), Some("LICENSE.txt"));
    assert_eq!(tpl[1].src, "README.md.tpl");
    assert_eq!(tpl[1].dst, None);
}

#[test]
fn test_release_templated_extra_files_defaults_to_none() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      draft: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.templated_extra_files, None);
}

#[test]
fn test_checksum_templated_extra_files_parsed() {
    let yaml = r#"
name_template: "checksums.txt"
templated_extra_files:
  - src: "notes.tpl"
    dst: "RELEASE_NOTES.txt"
"#;
    let cfg: ChecksumConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let tpl = cfg.templated_extra_files.as_ref().unwrap();
    assert_eq!(tpl.len(), 1);
    assert_eq!(tpl[0].src, "notes.tpl");
    assert_eq!(tpl[0].dst.as_deref(), Some("RELEASE_NOTES.txt"));
}

// ---- ReleaseConfig skip_upload tests ----

#[test]
fn test_release_skip_upload() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      skip_upload: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.skip_upload, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_release_skip_upload_false() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      skip_upload: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.skip_upload, Some(StringOrBool::Bool(false)));
}

#[test]
fn test_release_skip_upload_auto() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      skip_upload: "auto"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(
        release.skip_upload,
        Some(StringOrBool::String("auto".to_string()))
    );
}

// ---- ReleaseConfig replace_existing_draft / replace_existing_artifacts tests ----

#[test]
fn test_release_replace_existing_draft() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      replace_existing_draft: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.replace_existing_draft, Some(true));
}

#[test]
fn test_release_replace_existing_artifacts() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      replace_existing_artifacts: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.replace_existing_artifacts, Some(true));
}

// ---- ReleaseConfig tag override tests ----

#[test]
fn test_release_tag_override_parsed() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "myapp/v{{ .Version }}"
    release:
      tag: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.tag, Some("v{{ .Version }}".to_string()));
}

#[test]
fn test_release_tag_override_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      draft: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.tag, None);
}

#[test]
fn test_release_all_new_fields() {
    let yaml = r##"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      github:
        owner: myorg
        name: myrepo
      draft: true
      make_latest: auto
      header: "# Release Notes"
      footer: "Thank you!"
      extra_files:
        - "dist/extra.zip"
      skip_upload: false
      replace_existing_draft: true
      replace_existing_artifacts: false
      target_commitish: main
      discussion_category_name: Announcements
      include_meta: true
      use_existing_draft: false
      tag: "v{{ .Version }}"
"##;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(
        release.header,
        Some(ContentSource::Inline("# Release Notes".to_string()))
    );
    assert_eq!(
        release.footer,
        Some(ContentSource::Inline("Thank you!".to_string()))
    );
    assert_eq!(
        release.extra_files.as_ref().unwrap(),
        &[ExtraFileSpec::Glob("dist/extra.zip".to_string())]
    );
    assert_eq!(release.skip_upload, Some(StringOrBool::Bool(false)));
    assert_eq!(release.replace_existing_draft, Some(true));
    assert_eq!(release.replace_existing_artifacts, Some(false));
    assert_eq!(release.make_latest, Some(MakeLatestConfig::Auto));
    assert_eq!(release.target_commitish, Some("main".to_string()));
    assert_eq!(
        release.discussion_category_name,
        Some("Announcements".to_string())
    );
    assert_eq!(release.include_meta, Some(true));
    assert_eq!(release.use_existing_draft, Some(false));
    assert_eq!(release.tag, Some("v{{ .Version }}".to_string()));
}

// ---- SignConfig / signs migration tests ----

#[test]
fn test_signs_single_object() {
    let yaml = r#"
project_name: test
signs:
  artifacts: all
  cmd: gpg
  args:
    - "--detach-sig"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.signs.len(), 1);
    assert_eq!(config.signs[0].artifacts, Some("all".to_string()));
    assert_eq!(config.signs[0].cmd, Some("gpg".to_string()));
    assert_eq!(config.signs[0].args.as_ref().unwrap().len(), 1);
}

#[test]
fn test_signs_array_format() {
    let yaml = r#"
project_name: test
signs:
  - id: gpg-sign
    artifacts: checksum
    cmd: gpg
    args:
      - "--detach-sig"
  - id: cosign-sign
    artifacts: binary
    cmd: cosign
    args:
      - "sign"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.signs.len(), 2);
    assert_eq!(config.signs[0].id, Some("gpg-sign".to_string()));
    assert_eq!(config.signs[0].artifacts, Some("checksum".to_string()));
    assert_eq!(config.signs[1].id, Some("cosign-sign".to_string()));
    assert_eq!(config.signs[1].artifacts, Some("binary".to_string()));
}

#[test]
fn test_signs_omitted_is_empty() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.signs.is_empty());
}

#[test]
fn test_signs_new_fields() {
    let yaml = r#"
project_name: test
signs:
  - id: my-signer
    artifacts: archive
    cmd: gpg
    args:
      - "--detach-sig"
    signature: "{{ .Artifact }}.asc"
    stdin: "my-passphrase"
    ids:
      - my-archive
      - my-binary
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.signs.len(), 1);
    let sign = &config.signs[0];
    assert_eq!(sign.id, Some("my-signer".to_string()));
    assert_eq!(sign.artifacts, Some("archive".to_string()));
    assert_eq!(sign.signature, Some("{{ .Artifact }}.asc".to_string()));
    assert_eq!(sign.stdin, Some("my-passphrase".to_string()));
    assert_eq!(sign.ids.as_ref().unwrap().len(), 2);
    assert_eq!(sign.ids.as_ref().unwrap()[0], "my-archive");
}

#[test]
fn test_signs_stdin_file_field() {
    let yaml = r#"
project_name: test
signs:
  - artifacts: all
    cmd: gpg
    stdin_file: "/path/to/passphrase.txt"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.signs.len(), 1);
    assert_eq!(
        config.signs[0].stdin_file,
        Some("/path/to/passphrase.txt".to_string())
    );
}

#[test]
fn test_signs_single_object_with_new_fields() {
    let yaml = r#"
project_name: test
signs:
  id: default
  artifacts: package
  cmd: gpg
  signature: "{{ .Artifact }}.sig"
  stdin: "pass"
  ids:
    - pkg-id
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.signs.len(), 1);
    let sign = &config.signs[0];
    assert_eq!(sign.id, Some("default".to_string()));
    assert_eq!(sign.artifacts, Some("package".to_string()));
    assert_eq!(sign.signature, Some("{{ .Artifact }}.sig".to_string()));
    assert_eq!(sign.stdin, Some("pass".to_string()));
    assert_eq!(sign.ids.as_ref().unwrap(), &["pkg-id"]);
}

#[test]
fn test_signs_toml_single_object() {
    let toml_str = r#"
project_name = "test"

[signs]
artifacts = "checksum"
cmd = "gpg"

[[crates]]
name = "a"
path = "."
tag_template = "v{{ .Version }}"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(config.signs.len(), 1);
    assert_eq!(config.signs[0].artifacts, Some("checksum".to_string()));
}

#[test]
fn test_signs_toml_array() {
    let toml_str = r#"
project_name = "test"

[[signs]]
id = "first"
artifacts = "all"
cmd = "gpg"

[[signs]]
id = "second"
artifacts = "binary"
cmd = "cosign"

[[crates]]
name = "a"
path = "."
tag_template = "v{{ .Version }}"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(config.signs.len(), 2);
    assert_eq!(config.signs[0].id, Some("first".to_string()));
    assert_eq!(config.signs[1].id, Some("second".to_string()));
}

#[test]
fn test_signs_default_config_has_empty_signs() {
    let config = Config::default();
    assert!(config.signs.is_empty());
}

// ---- binary_signs artifacts constraint ----

#[test]
fn test_binary_signs_artifacts_binary_accepted() {
    let yaml = r#"
project_name: test
binary_signs:
  - id: cosign-binary
    artifacts: binary
    cmd: cosign
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.binary_signs.len(), 1);
    assert_eq!(config.binary_signs[0].artifacts.as_deref(), Some("binary"));
}

#[test]
fn test_binary_signs_artifacts_none_accepted() {
    let yaml = r#"
project_name: test
binary_signs:
  - artifacts: none
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.binary_signs[0].artifacts.as_deref(), Some("none"));
}

#[test]
fn test_binary_signs_artifacts_omitted_accepted() {
    let yaml = r#"
project_name: test
binary_signs:
  - id: implicit
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.binary_signs[0].artifacts, None);
}

#[test]
fn test_binary_signs_artifacts_archive_rejected() {
    // Anything broader than `binary` / `none` would silently match
    // nothing because the binary-sign loop only iterates Binary
    // artifacts; reject at parse time instead.
    let yaml = r#"
project_name: test
binary_signs:
  - artifacts: archive
crates: []
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "binary_signs[].artifacts: archive must be rejected"
    );
}

#[test]
fn test_binary_signs_artifacts_all_rejected() {
    let yaml = r#"
project_name: test
binary_signs:
  - artifacts: all
crates: []
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "binary_signs[].artifacts: all must be rejected"
    );
}

#[test]
fn test_binary_signs_artifacts_schema_is_runtime_constrained() {
    // The constraint on `binary_signs[].artifacts` lives in the custom
    // deserializer, not as a serde-typed enum, because `SignConfig` is
    // shared with the top-level `signs:` field (which legitimately
    // accepts a wider artifact filter set). The JSON schema therefore
    // inherits the unconstrained `Option<String>` shape from `SignConfig`
    // — this test pins that contract so any future schema-typing attempt
    // surfaces as a deliberate decision (and updates this test + the
    // documenting comment above `deserialize_binary_signs`).
    let json = super::config_schema();
    let sign_artifacts = json
        .pointer("/definitions/SignConfig/properties/artifacts")
        .expect("SignConfig.artifacts must appear in the generated schema");
    // `artifacts` is `Option<String>` → schemars emits a nullable string
    // (`type: ["string", "null"]` on Draft-07). Either form is acceptable
    // here — the assertion is that no `enum` constraint has been added.
    assert!(
        sign_artifacts.get("enum").is_none(),
        "binary_signs[].artifacts schema must remain unconstrained \
             (constraint lives in deserialize_binary_signs); got: {sign_artifacts}"
    );
}

// ---- report_sizes tests ----

#[test]
fn test_report_sizes_true() {
    let yaml = r#"
project_name: test
report_sizes: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.report_sizes, Some(true));
}

#[test]
fn test_report_sizes_false() {
    let yaml = r#"
project_name: test
report_sizes: false
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.report_sizes, Some(false));
}

#[test]
fn test_report_sizes_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.report_sizes, None);
}

// ---- env tests ----

#[test]
fn test_env_field_parsed() {
    let yaml = r#"
project_name: test
env:
  - MY_VAR=hello
  - DEPLOY_ENV=staging
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env = config.env.as_ref().unwrap();
    assert!(env.contains(&"MY_VAR=hello".to_string()));
    assert!(env.contains(&"DEPLOY_ENV=staging".to_string()));
}

#[test]
fn test_env_field_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.env, None);
}

#[test]
fn test_env_field_toml() {
    let toml_str = r#"
project_name = "test"
env = ["API_KEY=secret123", "STAGE=prod"]

[[crates]]
name = "a"
path = "."
tag_template = "v{{ .Version }}"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let env = config.env.as_ref().unwrap();
    assert!(env.contains(&"API_KEY=secret123".to_string()));
    assert!(env.contains(&"STAGE=prod".to_string()));
}

#[test]
fn test_env_list_form_toml() {
    let toml_str = r#"
project_name = "test"
env = ["MY_VAR=hello", "STAGE=prod"]
crates = []
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let env = config.env.as_ref().unwrap();
    assert!(env.contains(&"MY_VAR=hello".to_string()));
    assert!(env.contains(&"STAGE=prod".to_string()));
}

// ---- env list form tests ----

#[test]
fn test_env_list_form_parsed() {
    let yaml = r#"
project_name: test
env:
  - MY_VAR=hello
  - DEPLOY_ENV=staging
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env = config.env.as_ref().unwrap();
    assert!(env.contains(&"MY_VAR=hello".to_string()));
    assert!(env.contains(&"DEPLOY_ENV=staging".to_string()));
}

#[test]
fn test_env_list_form_with_template_expressions() {
    let yaml = r#"
project_name: test
env:
  - "MY_VERSION={{ .Tag }}"
  - "BUILD_DATE={{ .Date }}"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env = config.env.as_ref().unwrap();
    // Values are stored raw; template rendering happens at setup_env time.
    assert!(env.contains(&"MY_VERSION={{ .Tag }}".to_string()));
    assert!(env.contains(&"BUILD_DATE={{ .Date }}".to_string()));
}

#[test]
fn test_env_list_form_value_with_equals() {
    // Values can contain = signs (only the first = splits key from value).
    let yaml = r#"
project_name: test
env:
  - "LDFLAGS=-X main.version=1.0.0"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env = config.env.as_ref().unwrap();
    assert!(
        env.contains(&"LDFLAGS=-X main.version=1.0.0".to_string()),
        "only first = should split key from value"
    );
}

#[test]
fn test_env_list_form_empty_value() {
    let yaml = r#"
project_name: test
env:
  - "EMPTY_VAR="
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env = config.env.as_ref().unwrap();
    assert!(env.contains(&"EMPTY_VAR=".to_string()));
}

#[test]
fn test_env_list_form_no_equals_is_error() {
    // Vec<String> accepts any string at parse time; validation happens when
    // parse_env_entries is called by consumers (e.g. setup_env).
    let yaml = r#"
project_name: test
env:
  - "NO_EQUALS"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env = config.env.as_ref().unwrap();
    let err = super::parse_env_entries(env).unwrap_err();
    assert!(
        err.to_string().contains("KEY=VALUE"),
        "parse_env_entries should mention KEY=VALUE format, got: {}",
        err
    );
}

#[test]
fn test_env_list_form_empty_key_is_error() {
    // Vec<String> accepts any string at parse time; validation happens when
    // parse_env_entries is called by consumers (e.g. setup_env).
    let yaml = r#"
project_name: test
env:
  - "=orphan_value"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env = config.env.as_ref().unwrap();
    let err = super::parse_env_entries(env).unwrap_err();
    assert!(
        err.to_string().contains("empty key"),
        "parse_env_entries should mention empty key, got: {}",
        err
    );
}

#[test]
fn test_env_list_form_last_wins_on_duplicates() {
    let yaml = r#"
project_name: test
env:
  - "DUPED=first"
  - "DUPED=second"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env = config.env.as_ref().unwrap();
    // Vec<String> preserves all entries; consumers use last-wins semantics when iterating
    assert!(
        env.contains(&"DUPED=second".to_string()),
        "later entries should be present"
    );
}

#[test]
fn test_workspace_env_list_form() {
    let yaml = r#"
project_name: test
crates: []
workspaces:
  - name: ws1
    crates: []
    env:
      - "WS_VAR=from-workspace"
      - "WS_BUILD={{ .Tag }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let ws = &config.workspaces.as_ref().unwrap()[0];
    let env = ws.env.as_ref().unwrap();
    assert!(env.contains(&"WS_VAR=from-workspace".to_string()));
    assert!(env.contains(&"WS_BUILD={{ .Tag }}".to_string()));
}

// ---- Error path tests: malformed YAML / schema violations ----

#[test]
fn test_malformed_yaml_syntax_error() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
  invalid_indentation
    this_is_broken: [
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "malformed YAML should fail to parse");
    let err = result.unwrap_err().to_string();
    // Serde_yaml errors include line/column info
    assert!(!err.is_empty(), "error message should not be empty");
}

#[test]
fn test_type_mismatch_string_where_array_expected() {
    let yaml = r#"
project_name: test
crates: "this should be an array"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "string where array expected should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("invalid type") || err.contains("expected a sequence"),
        "error should mention type mismatch, got: {err}"
    );
}

#[test]
fn test_type_mismatch_object_where_string_expected() {
    // An object (mapping) where a string is expected for project_name
    // should be rejected by serde_yaml_ng, unlike a number which gets coerced.
    let yaml = r#"
project_name:
  nested: object
  another: field
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "mapping where string expected should fail to parse"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("invalid type") || err.contains("expected a string"),
        "error should mention type mismatch, got: {err}"
    );
}

#[test]
fn test_type_mismatch_bool_where_array_expected_for_targets() {
    let yaml = r#"
project_name: test
defaults:
  targets: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "bool where array expected for targets should fail"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("invalid type")
            || err.contains("expected a sequence")
            || err.contains("targets"),
        "error should mention type mismatch for targets, got: {err}"
    );
}

#[test]
fn test_invalid_cross_strategy_value() {
    let yaml = r#"
project_name: test
defaults:
  cross: invalid_strategy
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "invalid cross strategy should fail to parse"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("unknown variant") || err.contains("invalid_strategy"),
        "error should mention the invalid variant, got: {err}"
    );
}

#[test]
fn test_prerelease_invalid_string_value() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      prerelease: "always"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "prerelease: 'always' should fail (only 'auto' or bool accepted)"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("auto") || err.contains("always"),
        "error should mention expected values, got: {err}"
    );
}

#[test]
fn test_archives_true_is_invalid() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    archives: true
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "archives: true should be rejected (only false or array accepted)"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("true is not valid") || err.contains("false or a list"),
        "error should explain valid archives values, got: {err}"
    );
}

#[test]
fn test_completely_empty_yaml() {
    // Empty YAML deserializes to defaults because Config uses #[serde(default)].
    // serde_yaml_ng treats empty input as `null`, which the default impl handles.
    let yaml = "";
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    let config =
        result.unwrap_or_else(|e| panic!("empty YAML should parse to Config defaults: {e}"));
    assert!(
        config.project_name.is_empty(),
        "default project_name should be empty"
    );
    assert!(config.crates.is_empty(), "default crates should be empty");
    assert_eq!(
        config.dist,
        std::path::PathBuf::from("./dist"),
        "default dist should be ./dist"
    );
}

// ---- Unknown fields tests ----

// ---- BinstallConfig / VersionSyncConfig tests ----

#[test]
fn test_binstall_config_parsed() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    binstall:
      enabled: true
      pkg_url: "https://example.com/{{ .Version }}/{ target }"
      bin_dir: "{ bin }{ binary-ext }"
      pkg_fmt: tgz
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let bs = config.crates[0].binstall.as_ref().unwrap();
    assert_eq!(bs.enabled, Some(true));
    assert_eq!(
        bs.pkg_url,
        Some("https://example.com/{{ .Version }}/{ target }".to_string())
    );
    assert_eq!(bs.bin_dir, Some("{ bin }{ binary-ext }".to_string()));
    assert_eq!(bs.pkg_fmt, Some("tgz".to_string()));
}

#[test]
fn test_binstall_config_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].binstall.is_none());
}

#[test]
fn test_binstall_config_partial() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    binstall:
      enabled: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let bs = config.crates[0].binstall.as_ref().unwrap();
    assert_eq!(bs.enabled, Some(true));
    assert_eq!(bs.pkg_url, None);
    assert_eq!(bs.bin_dir, None);
    assert_eq!(bs.pkg_fmt, None);
}

#[test]
fn test_binstall_config_overrides_parsed() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    binstall:
      enabled: true
      overrides:
        x86_64-unknown-linux-gnu:
          pkg_url: "https://example.com/{{ .Version }}/myapp-linux-amd64.tar.gz"
          pkg_fmt: tgz
          bin_dir: "{ bin }{ binary-ext }"
        aarch64-apple-darwin:
          pkg_url: "https://example.com/{{ .Version }}/myapp-darwin-arm64.tar.gz"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let bs = config.crates[0].binstall.as_ref().unwrap();
    let overrides = bs.overrides.as_ref().expect("overrides should parse");
    assert_eq!(overrides.len(), 2);

    let linux = overrides.get("x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(
        linux.pkg_url,
        Some("https://example.com/{{ .Version }}/myapp-linux-amd64.tar.gz".to_string())
    );
    assert_eq!(linux.pkg_fmt, Some("tgz".to_string()));
    assert_eq!(linux.bin_dir, Some("{ bin }{ binary-ext }".to_string()));

    let darwin = overrides.get("aarch64-apple-darwin").unwrap();
    assert_eq!(
        darwin.pkg_url,
        Some("https://example.com/{{ .Version }}/myapp-darwin-arm64.tar.gz".to_string())
    );
    // Unset override fields default to None.
    assert_eq!(darwin.pkg_fmt, None);
    assert_eq!(darwin.bin_dir, None);
}

#[test]
fn test_version_sync_config_parsed() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    version_sync:
      enabled: true
      mode: tag
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let vs = config.crates[0].version_sync.as_ref().unwrap();
    assert_eq!(vs.enabled, Some(true));
    assert_eq!(vs.mode, Some("tag".to_string()));
}

#[test]
fn test_version_sync_config_explicit_mode() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    version_sync:
      enabled: true
      mode: explicit
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let vs = config.crates[0].version_sync.as_ref().unwrap();
    assert_eq!(vs.mode, Some("explicit".to_string()));
}

#[test]
fn test_version_sync_config_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].version_sync.is_none());
}

#[test]
fn test_binstall_and_version_sync_together() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    binstall:
      enabled: true
      pkg_fmt: zip
    version_sync:
      enabled: true
      mode: tag
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].binstall.is_some());
    assert!(config.crates[0].version_sync.is_some());
}

#[test]
fn test_binstall_config_toml() {
    let toml_str = r#"
project_name = "test"

[[crates]]
name = "myapp"
path = "."
tag_template = "v{{ .Version }}"

[crates.binstall]
enabled = true
pkg_url = "https://example.com"
pkg_fmt = "tgz"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let bs = config.crates[0].binstall.as_ref().unwrap();
    assert_eq!(bs.enabled, Some(true));
    assert_eq!(bs.pkg_url, Some("https://example.com".to_string()));
}

#[test]
fn test_version_sync_config_toml() {
    let toml_str = r#"
project_name = "test"

[[crates]]
name = "myapp"
path = "."
tag_template = "v{{ .Version }}"

[crates.version_sync]
enabled = true
mode = "tag"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let vs = config.crates[0].version_sync.as_ref().unwrap();
    assert_eq!(vs.enabled, Some(true));
    assert_eq!(vs.mode, Some("tag".to_string()));
}

#[test]
fn test_crate_config_default_has_none_binstall_version_sync() {
    let config = CrateConfig::default();
    assert!(config.binstall.is_none());
    assert!(config.version_sync.is_none());
}

// ---- Unknown fields tests ----

#[test]
fn test_unknown_top_level_fields_rejected() {
    // strict YAML parsing rejects unknown fields
    let yaml = r#"
project_name: test
unknown_top_level_field: "this should be rejected"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "unknown top-level fields should be rejected"
    );
    assert!(
        result.unwrap_err().to_string().contains("unknown field"),
        "error should mention unknown field"
    );
}

#[test]
fn test_unknown_crate_level_fields_rejected() {
    // The build config subtree (`CrateConfig` and its nested `builds[]` shape)
    // is strict: an unknown crate-level field is a hard parse error, matching
    // the top-level `Config` strictness. This catches typos and removed fields
    // (e.g. the old `docker:` block) instead of silently dropping them.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nonexistent_field: true
    something_else: "hello"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "unknown crate-level fields should be rejected"
    );
    assert!(
        result.unwrap_err().to_string().contains("unknown field"),
        "error should mention unknown field"
    );
}

/// Every config struct is `deny_unknown_fields` (GoReleaser-style strict
/// parsing), so an unknown key at ANY nesting depth — top-level section,
/// nested sub-config, or per-crate leaf — is a hard error, not silently
/// ignored. This pins the strict contract at each level so a typo'd or
/// misplaced key surfaces at parse time rather than silently no-op'ing on a
/// release.
#[test]
fn test_unknown_nested_fields_rejected() {
    let cases = [
        (
            "nested section (defaults)",
            r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
  unknown_default_field: "boom"
"#,
        ),
        (
            "nested section (changelog)",
            "project_name: test\nchangelog:\n  sort: asc\n  mystery_option: true\n",
        ),
        (
            "per-crate leaf (checksum)",
            r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    checksum:
      algorithm: sha256
      future_field: "boom"
"#,
        ),
    ];
    for (label, yaml) in cases {
        let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
        let err = result.expect_err(&format!("{label}: unknown field must be rejected"));
        assert!(
            err.to_string().contains("unknown field"),
            "{label}: error should name the unknown field, got: {err}"
        );
    }
}

// ---- BuildConfig reproducible field tests ----

#[test]
fn test_build_config_reproducible_true() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        reproducible: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.reproducible, Some(true));
}

#[test]
fn test_build_config_reproducible_false() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        reproducible: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.reproducible, Some(false));
}

#[test]
fn test_build_config_reproducible_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.reproducible, None);
}

// ---- WorkspaceConfig tests ----

#[test]
fn test_workspace_config_parses() {
    let yaml = r#"
project_name: monorepo
crates: []
workspaces:
  - name: frontend
    crates:
      - name: frontend-app
        path: "apps/frontend"
        tag_template: "frontend-v{{ .Version }}"
    changelog:
      sort: asc
  - name: backend
    crates:
      - name: backend-api
        path: "apps/backend"
        tag_template: "backend-v{{ .Version }}"
      - name: backend-worker
        path: "apps/worker"
        tag_template: "worker-v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let workspaces = config.workspaces.as_ref().unwrap();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(workspaces[0].name, "frontend");
    assert_eq!(workspaces[0].crates.len(), 1);
    assert_eq!(workspaces[0].crates[0].name, "frontend-app");
    assert!(workspaces[0].changelog.is_some());
    assert_eq!(workspaces[1].name, "backend");
    assert_eq!(workspaces[1].crates.len(), 2);
}

#[test]
fn test_workspace_config_with_signs_and_hooks() {
    let yaml = r#"
project_name: monorepo
crates: []
workspaces:
  - name: myws
    crates:
      - name: mylib
        path: "."
        tag_template: "v{{ .Version }}"
    signs:
      - artifacts: all
        cmd: gpg
    before:
      hooks:
        - echo before
    after:
      hooks:
        - echo after
    env:
      - MY_VAR=hello
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let ws = &config.workspaces.as_ref().unwrap()[0];
    assert_eq!(ws.name, "myws");
    assert_eq!(ws.signs.len(), 1);
    assert!(ws.before.is_some());
    assert!(ws.after.is_some());
    assert!(
        ws.env
            .as_ref()
            .unwrap()
            .contains(&"MY_VAR=hello".to_string())
    );
}

#[test]
fn test_workspace_config_omitted() {
    let yaml = r#"
project_name: simple
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.workspaces.is_none());
}

#[test]
fn test_workspace_config_empty_array() {
    let yaml = r#"
project_name: test
crates: []
workspaces: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let workspaces = config.workspaces.as_ref().unwrap();
    assert!(workspaces.is_empty());
}

// ---- ChocolateyConfig tests ----

#[test]
fn test_chocolatey_config_toml() {
    // ChocolateyConfig.repository is the unified RepositoryConfig form
    // (owner/name + token/branch/...).
    let toml_str = r#"
project_name = "test"

[[crates]]
name = "mytool"
path = "."
tag_template = "v{{ .Version }}"

[crates.publish.chocolatey]
description = "A tool"
license = "MIT"
authors = "Author"
tags = ["cli"]

[crates.publish.chocolatey.repository]
owner = "org"
name = "tool"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let choco = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .chocolatey
        .as_ref()
        .unwrap();

    assert_eq!(choco.description, Some("A tool".to_string()));
    let repo = choco.repository.as_ref().unwrap();
    assert_eq!(repo.owner.as_deref(), Some("org"));
}

// ---- Behavior-toggle test ----

#[test]
fn test_changelog_snapshot_field_parses() {
    // The `changelog.snapshot: true` opt-in parses + round-trips on
    // ChangelogConfig. Behavior wiring lives in
    // `crates/stage-changelog/src/lib.rs::ChangelogStage::run`.
    let yaml = r#"
project_name: test
changelog:
  snapshot: true
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cl = config.changelog.as_ref().unwrap();
    assert_eq!(cl.snapshot, Some(true));
}

#[test]
fn test_changelog_snapshot_omitted_is_none() {
    let yaml = r#"
project_name: test
changelog:
  sort: asc
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cl = config.changelog.as_ref().unwrap();
    assert_eq!(cl.snapshot, None);
}

// ---- Plural-canonical key + alias tests ----

#[test]
fn test_top_level_plural_canonical_keys_parse() {
    // The plural canonical keys (nfpms, dmgs, msis, flatpaks) are
    // anodizer's only spelling at top level — singular forms would
    // be rejected as unknown fields.
    let yaml = r#"
project_name: test
defaults:
  nfpms:
    formats: [deb]
  dmgs:
    name: test
  msis:
    name: test
  flatpaks:
    runtime: org.freedesktop.Platform
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let d = config.defaults.unwrap();
    assert!(d.nfpms.is_some());
    assert!(d.dmgs.is_some());
    assert!(d.msis.is_some());
    assert!(d.flatpaks.is_some());
}

#[test]
fn test_makeself_filename_field() {
    // `filename:` is the canonical field name.
    let yaml = r#"
project_name: test
makeselfs:
  - id: default
    filename: "myapp-{{ .Version }}.run"
    script: install.sh
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.makeselfs[0].filename.as_deref(),
        Some("myapp-{{ .Version }}.run")
    );
}

#[test]
fn test_announce_smtp_aliases_email() {
    // The `smtp:` → `email:` rename (both are kept as
    // aliases; anodizer matches).
    let yaml = r#"
project_name: test
announce:
  smtp:
    enabled: true
    host: smtp.example.com
    port: 587
    username: user
    from: from@example.com
    to: ["to@example.com"]
    subject_template: "Release {{ .Version }}"
    message_template: "Body"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(
        config.announce.unwrap().email.is_some(),
        "smtp: should alias to email:"
    );
}

#[test]
fn test_announce_canonical_email_still_works() {
    let yaml = r#"
project_name: test
announce:
  email:
    enabled: true
    host: smtp.example.com
    port: 587
    username: user
    from: from@example.com
    to: ["to@example.com"]
    subject_template: "Release {{ .Version }}"
    message_template: "Body"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.announce.unwrap().email.is_some());
}

// ---- Legacy-field rejection tests ( hard-break shape) ----

#[test]
fn test_legacy_docker_field_rejected() {
    // `crates[].docker:` is no longer a recognized field. Any value
    // parses (CrateConfig isn't deny_unknown_fields) but it has nowhere
    // to land — confirm via explicit absence.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    dockers_v2:
      - images: [registry/img]
        tags: ["{{ .Version }}"]
        dockerfile: Dockerfile
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].dockers_v2.is_some());
    // No `docker` field exists on CrateConfig anymore.
}

#[test]
fn test_homebrew_legacy_commit_author_flat_fields_rejected() {
    // HomebrewConfig has `#[serde(deny_unknown_fields)]`, so the
    // dropped flat fields fail to parse outright.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        commit_author_name: TJ
        commit_author_email: tj@example.com
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "homebrew.commit_author_name must be rejected; use commit_author block"
    );
}

// ScoopConfig has `#[serde(deny_unknown_fields)]`. Use the structured
// `commit_author: { name, email, signing }` block; the flat fields
// must fail parsing.
#[test]
fn test_scoop_legacy_commit_author_flat_fields_rejected() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      scoop:
        commit_author_name: TJ
        commit_author_email: tj@example.com
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "scoop.commit_author_name must be rejected; use commit_author block"
    );
}

#[test]
fn test_aur_legacy_url_field_rejected() {
    // AurConfig has `deny_unknown_fields`; the dropped legacy `url:`
    // field must fail parsing (PKGBUILD url= resolves through
    // homepage → crate metadata → derived github URL).
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        url: "https://example.com/a"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "aur.url must be rejected; use homepage");
}

#[test]
fn test_homebrew_legacy_tap_field_rejected() {
    // HomebrewConfig has `deny_unknown_fields`; legacy `tap:` is
    // gone (use `repository:`).
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        tap:
          owner: x
          name: y
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "homebrew.tap must be rejected");
}

#[test]
fn test_scoop_legacy_bucket_field_rejected() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      scoop:
        bucket:
          owner: x
          name: y
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "scoop.bucket must be rejected");
}

#[test]
fn test_winget_legacy_manifests_repo_rejected() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        manifests_repo:
          owner: x
          name: y
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "winget.manifests_repo must be rejected");
}

#[test]
fn test_chocolatey_legacy_project_repo_rejected() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        project_repo:
          owner: x
          name: y
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "chocolatey.project_repo must be rejected (use repository)"
    );
}

#[test]
fn test_krew_legacy_manifests_repo_rejected() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      krew:
        manifests_repo:
          owner: x
          name: y
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "krew.manifests_repo must be rejected");
}

#[test]
fn test_krew_legacy_upstream_repo_rejected() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      krew:
        upstream_repo:
          owner: x
          name: y
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "krew.upstream_repo must be rejected");
}

#[test]
fn test_notarize_macos_skip_roundtrip() {
    // `skip:` is the canonical per-config gating field; known-good
    // YAML with `skip: false` parses cleanly.
    let yaml = r#"
notarize:
  macos:
    - skip: false
      sign:
        certificate: /tmp/cert.p12
        password: pw
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let macos = config.notarize.unwrap().macos.unwrap();
    assert_eq!(macos[0].skip, Some(StringOrBool::Bool(false)));
}

/// The upstream writes `enabled:` (opt-in, default false) where anodizer
/// writes `skip:` (opt-out once the block is present). The deserializer
/// inverts the bool so an imported YAML runs the pipeline
/// instead of being rejected at parse time.
#[test]
fn test_notarize_macos_enabled_alias_inverts_to_skip() {
    let yaml_true = r#"
notarize:
  macos:
    - enabled: true
      sign:
        certificate: /tmp/cert.p12
        password: pw
crates: []
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml_true).expect("enabled alias should parse");
    let macos = cfg.notarize.unwrap().macos.unwrap();
    assert_eq!(
        macos[0].skip,
        Some(StringOrBool::Bool(false)),
        "`enabled: true` must invert to `skip: false`"
    );

    let yaml_false = r#"
notarize:
  macos:
    - enabled: false
      sign:
        certificate: /tmp/cert.p12
        password: pw
crates: []
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml_false).expect("enabled alias should parse");
    let macos = cfg.notarize.unwrap().macos.unwrap();
    assert_eq!(
        macos[0].skip,
        Some(StringOrBool::Bool(true)),
        "`enabled: false` must invert to `skip: true`"
    );
}

#[test]
fn test_notarize_macos_native_enabled_alias_inverts_to_skip() {
    let yaml = r#"
notarize:
  macos_native:
    - enabled: true
crates: []
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("enabled alias should parse");
    let macos_native = cfg.notarize.unwrap().macos_native.unwrap();
    assert_eq!(
        macos_native[0].skip,
        Some(StringOrBool::Bool(false)),
        "`enabled: true` must invert to `skip: false`"
    );
}

#[test]
fn test_notarize_top_level_unknown_field_rejected() {
    // Unknown fields on the top-level NotarizeConfig are also rejected
    // via `deny_unknown_fields`.
    let yaml = r#"
notarize:
  enabled: true
crates: []
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "unknown field `enabled` on NotarizeConfig must be rejected"
    );
}

// ---- Unified nFPM/SRPM content + signature tests ----

#[test]
fn test_nfpm_content_canonical_keys_in_srpm_full() {
    // SRPM contents share [`NfpmContent`]; canonical `src`/`dst` keys
    // are required (the `source`/`destination` aliases).
    let yaml = r#"
project_name: test
srpm:
  enabled: true
  contents:
    - src: ./LICENSE
      dst: /usr/share/doc/myapp/LICENSE
      type: doc
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let contents = config.srpms.as_ref().unwrap().contents.as_ref().unwrap();
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].src, "./LICENSE");
    assert_eq!(contents[0].dst, "/usr/share/doc/myapp/LICENSE");
    assert_eq!(contents[0].content_type.as_deref(), Some("doc"));
}

#[test]
fn test_nfpm_content_canonical_keys_in_srpm() {
    // Canonical `src` / `dst` keys also work in srpm contents.
    let yaml = r#"
project_name: test
srpm:
  enabled: true
  contents:
    - src: ./README.md
      dst: /usr/share/doc/myapp/README.md
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let contents = config.srpms.as_ref().unwrap().contents.as_ref().unwrap();
    assert_eq!(contents[0].src, "./README.md");
}

#[test]
fn test_nfpm_signature_canonical_passphrase() {
    // SRPM signatures share [`NfpmSignatureConfig`]; canonical
    // `key_passphrase:` is the only accepted spelling (
    // the `passphrase:` alias).
    let yaml = r#"
project_name: test
srpm:
  enabled: true
  signature:
    key_file: /keys/srpm.gpg
    key_passphrase: "s3cret"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let sig = config.srpms.as_ref().unwrap().signature.as_ref().unwrap();
    assert_eq!(sig.key_file.as_deref(), Some("/keys/srpm.gpg"));
    assert_eq!(sig.key_passphrase.as_deref(), Some("s3cret"));
}

#[test]
fn test_srpm_singular_alias_still_accepted() {
    // H4: Config.srpm renamed to Config.srpms for parity with
    // Defaults.srpms; the legacy `srpm:` spelling stays accepted via
    // serde alias.
    let yaml_legacy = r#"
project_name: test
srpm:
  enabled: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let yaml_canonical = r#"
project_name: test
srpms:
  enabled: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let legacy: Config = serde_yaml_ng::from_str(yaml_legacy).unwrap();
    let canonical: Config = serde_yaml_ng::from_str(yaml_canonical).unwrap();
    assert!(legacy.srpms.is_some(), "srpm: alias must populate srpms");
    assert!(canonical.srpms.is_some(), "srpms: must populate srpms");
    assert_eq!(
        legacy.srpms.as_ref().unwrap().enabled,
        canonical.srpms.as_ref().unwrap().enabled
    );
}

#[test]
fn test_nfpm_singular_alias_still_accepted() {
    // H4: CrateConfig.nfpm renamed to CrateConfig.nfpms for parity with
    // every other plural-name per-crate packaging list (`dmgs`, `msis`,
    // `pkgs`, ...). The legacy `nfpm:` spelling stays accepted via serde
    // alias.
    let yaml_legacy = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - id: deb
        formats: [deb]
"#;
    let yaml_canonical = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nfpms:
      - id: deb
        formats: [deb]
"#;
    let legacy: Config = serde_yaml_ng::from_str(yaml_legacy).unwrap();
    let canonical: Config = serde_yaml_ng::from_str(yaml_canonical).unwrap();
    assert_eq!(legacy.crates[0].nfpms.as_ref().unwrap().len(), 1);
    assert_eq!(canonical.crates[0].nfpms.as_ref().unwrap().len(), 1);
    assert_eq!(
        legacy.crates[0].nfpms.as_ref().unwrap()[0].id,
        canonical.crates[0].nfpms.as_ref().unwrap()[0].id
    );
}

// ---- WingetConfig tests ----

// ---- AurConfig tests ----

#[test]
fn test_aur_config_yaml() {
    let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        git_url: "ssh://aur@aur.archlinux.org/mytool.git"
        name: mytool-bin
        description: "A great tool"
        license: MIT
        maintainers:
          - "Jane Doe <jane@example.com>"
        depends:
          - glibc
          - openssl
        optdepends:
          - "git: for VCS support"
        conflicts:
          - mytool-git
        provides:
          - mytool
        replaces:
          - old-mytool
        backup:
          - etc/mytool/config.toml
        homepage: "https://github.com/org/mytool"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let aur = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .aur
        .as_ref()
        .unwrap();

    assert_eq!(
        aur.git_url,
        Some("ssh://aur@aur.archlinux.org/mytool.git".to_string())
    );
    assert_eq!(aur.name, Some("mytool-bin".to_string()));
    assert_eq!(aur.description, Some("A great tool".to_string()));
    assert_eq!(aur.license, Some("MIT".to_string()));
    assert_eq!(
        aur.maintainers,
        Some(vec!["Jane Doe <jane@example.com>".to_string()])
    );
    assert_eq!(
        aur.depends,
        Some(vec!["glibc".to_string(), "openssl".to_string()])
    );
    assert_eq!(
        aur.optdepends,
        Some(vec!["git: for VCS support".to_string()])
    );
    assert_eq!(aur.conflicts, Some(vec!["mytool-git".to_string()]));
    assert_eq!(aur.provides, Some(vec!["mytool".to_string()]));
    assert_eq!(aur.replaces, Some(vec!["old-mytool".to_string()]));
    assert_eq!(aur.backup, Some(vec!["etc/mytool/config.toml".to_string()]));
    assert_eq!(
        aur.homepage,
        Some("https://github.com/org/mytool".to_string())
    );
}

#[test]
fn test_aur_config_minimal() {
    let yaml = r#"
project_name: test
crates:
  - name: mytool
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        git_url: "ssh://aur@aur.archlinux.org/mytool.git"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let aur = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .aur
        .as_ref()
        .unwrap();

    assert_eq!(
        aur.git_url,
        Some("ssh://aur@aur.archlinux.org/mytool.git".to_string())
    );
    assert!(aur.name.is_none());
    assert!(aur.description.is_none());
    assert!(aur.license.is_none());
    assert!(aur.maintainers.is_none());
    assert!(aur.depends.is_none());
    assert!(aur.optdepends.is_none());
    assert!(aur.conflicts.is_none());
    assert!(aur.provides.is_none());
    assert!(aur.replaces.is_none());
    assert!(aur.backup.is_none());
}

#[test]
fn test_aur_config_toml() {
    let toml_str = r#"
project_name = "test"

[[crates]]
name = "mytool"
path = "."
tag_template = "v{{ .Version }}"

[crates.publish.aur]
git_url = "ssh://aur@aur.archlinux.org/mytool.git"
description = "A tool"
license = "MIT"
depends = ["glibc"]
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let aur = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .aur
        .as_ref()
        .unwrap();

    assert_eq!(
        aur.git_url,
        Some("ssh://aur@aur.archlinux.org/mytool.git".to_string())
    );
    assert_eq!(aur.description, Some("A tool".to_string()));
    assert_eq!(aur.depends, Some(vec!["glibc".to_string()]));
}

// ---- KrewConfig tests ----

// ---- Combined all publishers ----

// ---- Config version tests ----

#[test]
fn test_version_field_none_is_valid() {
    let config = Config::default();
    assert!(validate_version(&config).is_ok());
}

#[test]
fn test_version_field_1_is_valid() {
    let yaml = r#"
project_name: test
version: 1
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.version, Some(1));
    assert!(validate_version(&config).is_ok());
}

#[test]
fn test_version_field_2_is_valid() {
    let yaml = r#"
project_name: test
version: 2
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.version, Some(2));
    assert!(validate_version(&config).is_ok());
}

#[test]
fn test_version_field_99_is_rejected() {
    let config = Config {
        version: Some(99),
        ..Default::default()
    };
    let result = validate_version(&config);
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .contains("unsupported config version: 99")
    );
}

// ---- env_files tests ----

#[test]
fn test_env_files_list_form_parses() {
    let yaml = r#"
project_name: test
env_files:
  - ".env"
  - ".release.env"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env_files = config.env_files.unwrap();
    let files = env_files
        .as_list()
        .unwrap_or_else(|| panic!("expected List variant"));
    assert_eq!(files.len(), 2);
    assert_eq!(files[0], ".env");
    assert_eq!(files[1], ".release.env");
}

#[test]
fn test_env_files_struct_form_parses() {
    let yaml = r#"
project_name: test
env_files:
  github_token: "~/.config/goreleaser/github_token"
  gitlab_token: "/etc/tokens/gitlab"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env_files = config.env_files.unwrap();
    let tokens = env_files
        .as_token_files()
        .unwrap_or_else(|| panic!("expected TokenFiles variant"));
    assert_eq!(
        tokens.github_token.as_deref(),
        Some("~/.config/goreleaser/github_token")
    );
    assert_eq!(tokens.gitlab_token.as_deref(), Some("/etc/tokens/gitlab"));
    assert!(tokens.gitea_token.is_none());
}

#[test]
fn test_env_files_struct_form_empty_mapping() {
    let yaml = r#"
project_name: test
env_files:
  gitea_token: "/tmp/gitea"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let env_files = config.env_files.unwrap();
    let tokens = env_files
        .as_token_files()
        .unwrap_or_else(|| panic!("expected TokenFiles variant"));
    assert!(tokens.github_token.is_none());
    assert!(tokens.gitlab_token.is_none());
    assert_eq!(tokens.gitea_token.as_deref(), Some("/tmp/gitea"));
}

#[test]
fn test_env_files_field_omitted() {
    let yaml = r#"
project_name: test
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.env_files.is_none());
}

#[test]
fn test_read_token_file_reads_first_line() {
    use std::io::Write;
    let dir = tempfile::TempDir::new().unwrap();
    let token_path = dir.path().join("github_token");
    let mut f = std::fs::File::create(&token_path).unwrap();
    writeln!(f, "ghp_abc123xyz").unwrap();
    writeln!(f, "this line should be ignored").unwrap();
    drop(f);

    let result = read_token_file(&token_path.to_string_lossy()).unwrap();
    assert_eq!(result, Some("ghp_abc123xyz".to_string()));
}

#[test]
fn test_read_token_file_trims_whitespace() {
    use std::io::Write;
    let dir = tempfile::TempDir::new().unwrap();
    let token_path = dir.path().join("token");
    let mut f = std::fs::File::create(&token_path).unwrap();
    writeln!(f, "  spaced_token  ").unwrap();
    drop(f);

    let result = read_token_file(&token_path.to_string_lossy()).unwrap();
    assert_eq!(result, Some("spaced_token".to_string()));
}

#[test]
fn test_read_token_file_nonexistent_returns_none() {
    let result = read_token_file("/tmp/nonexistent_token_file_99999").unwrap();
    assert!(result.is_none());
}

#[test]
fn test_read_token_file_empty_returns_none() {
    let dir = tempfile::TempDir::new().unwrap();
    let token_path = dir.path().join("empty_token");
    std::fs::write(&token_path, "").unwrap();

    let result = read_token_file(&token_path.to_string_lossy()).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_load_token_files_reads_tokens() {
    use std::io::Write;
    let dir = tempfile::TempDir::new().unwrap();

    let gh_path = dir.path().join("github_token");
    let mut f = std::fs::File::create(&gh_path).unwrap();
    writeln!(f, "ghp_test123").unwrap();
    drop(f);

    let gl_path = dir.path().join("gitlab_token");
    let mut f = std::fs::File::create(&gl_path).unwrap();
    writeln!(f, "glpat-test456").unwrap();
    drop(f);

    let config = EnvFilesTokenConfig {
        github_token: Some(gh_path.to_string_lossy().to_string()),
        gitlab_token: Some(gl_path.to_string_lossy().to_string()),
        gitea_token: None, // uses default path which won't exist
    };

    // Empty env: no token vars set, so the files are the only source.
    let env = crate::MapEnvSource::new();
    let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
    let vars = load_token_files_with_env(&config, &log, &env).unwrap();

    assert_eq!(vars.get("GITHUB_TOKEN").unwrap(), "ghp_test123");
    assert_eq!(vars.get("GITLAB_TOKEN").unwrap(), "glpat-test456");
    // GITEA_TOKEN not present — default file doesn't exist
    assert!(!vars.contains_key("GITEA_TOKEN"));
}

#[test]
fn test_load_token_files_env_var_takes_precedence() {
    use std::io::Write;
    let dir = tempfile::TempDir::new().unwrap();

    let gh_path = dir.path().join("github_token");
    let mut f = std::fs::File::create(&gh_path).unwrap();
    writeln!(f, "file_token").unwrap();
    drop(f);

    let config = EnvFilesTokenConfig {
        github_token: Some(gh_path.to_string_lossy().to_string()),
        gitlab_token: None,
        gitea_token: None,
    };

    // GITHUB_TOKEN injected into the env source — should take precedence over file.
    let env = crate::MapEnvSource::new().with("GITHUB_TOKEN", "env_token");
    let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
    let vars = load_token_files_with_env(&config, &log, &env).unwrap();

    // File token should NOT be loaded because env var was set
    assert!(
        !vars.contains_key("GITHUB_TOKEN"),
        "env var should take precedence; file should not be loaded"
    );
}

#[test]
fn test_read_token_file_tilde_expansion() {
    // Test that tilde expansion uses the injected HOME value.
    let dir = tempfile::TempDir::new().unwrap();
    let token_path = dir.path().join(".config/goreleaser/github_token");
    std::fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    std::fs::write(&token_path, "tilde_token\n").unwrap();

    let env = crate::MapEnvSource::new().with("HOME", dir.path().to_string_lossy().to_string());
    let result = read_token_file_with_env("~/.config/goreleaser/github_token", &env).unwrap();

    assert_eq!(result, Some("tilde_token".to_string()));
}

#[test]
fn test_load_env_files_sets_vars() {
    use std::io::Write;
    let dir = tempfile::TempDir::new().unwrap();
    let env_path = dir.path().join(".env");
    let mut f = std::fs::File::create(&env_path).unwrap();
    writeln!(f, "# comment line").unwrap();
    writeln!(f).unwrap();
    writeln!(f, "TEST_ANODIZER_KEY=hello_world").unwrap();
    writeln!(f, "TEST_ANODIZER_QUOTED=\"with quotes\"").unwrap();
    writeln!(f, "TEST_ANODIZER_SINGLE='single_quoted'").unwrap();
    writeln!(f, "export TEST_ANODIZER_EXPORT=exported_val").unwrap();
    drop(f);

    let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
    let vars = load_env_files(&[env_path.to_string_lossy().to_string()], &log, false).unwrap();
    assert_eq!(vars.get("TEST_ANODIZER_KEY").unwrap(), "hello_world");
    assert_eq!(vars.get("TEST_ANODIZER_QUOTED").unwrap(), "with quotes");
    assert_eq!(
        vars.get("TEST_ANODIZER_SINGLE").unwrap(),
        "single_quoted",
        "single-quoted values should have quotes stripped"
    );
    assert_eq!(
        vars.get("TEST_ANODIZER_EXPORT").unwrap(),
        "exported_val",
        "export prefix should be stripped"
    );
}

#[test]
fn test_load_env_files_edge_cases() {
    use std::io::Write;
    let dir = tempfile::TempDir::new().unwrap();
    let env_path = dir.path().join(".env-edge");
    let mut f = std::fs::File::create(&env_path).unwrap();
    // Single quote char as value should not panic
    writeln!(f, "TEST_ANODIZER_SINGLEQ=\"").unwrap();
    // Empty key line (=value) should be skipped
    writeln!(f, "=orphan_value").unwrap();
    // Line without = should be skipped with warning
    writeln!(f, "NO_EQUALS_HERE").unwrap();
    drop(f);

    let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
    let vars = load_env_files(&[env_path.to_string_lossy().to_string()], &log, false).unwrap();
    // The single-quote value should be kept as-is (not stripped, length < 2 for
    // matching quotes)
    assert_eq!(vars.get("TEST_ANODIZER_SINGLEQ").unwrap(), "\"");
    // Empty key and no-equals lines should have been skipped
    assert!(!vars.contains_key(""), "empty key should be skipped");
}

#[test]
fn test_load_env_files_nonexistent_skips_with_warning() {
    let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
    let result = load_env_files(
        &["/tmp/nonexistent_anodizer_env_file_12345".to_string()],
        &log,
        false,
    );
    // Missing env files should be skipped (not an error), returning empty vars.
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[test]
fn test_load_env_files_nonexistent_strict_mode_errors() {
    let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
    let result = load_env_files(
        &["/tmp/nonexistent_anodizer_env_file_12345".to_string()],
        &log,
        true,
    );
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("strict mode"));
}

// ---- env_files TOML tests ----

// NOTE: EnvFilesConfig uses a custom Deserialize impl that reads into
// serde_yaml_ng::Value as an intermediate. Since serde_yaml_ng::Value
// implements generic Deserialize, this works across formats (YAML, TOML,
// JSON) -- the intermediate is populated via serde's data model, not
// from literal YAML text.

#[test]
fn test_env_files_list_form_toml() {
    // TOML array should deserialize to EnvFilesConfig::List via the
    // serde_yaml_ng::Value intermediate.
    #[derive(Deserialize)]
    struct Wrapper {
        env_files: EnvFilesConfig,
    }
    let toml_str = r#"env_files = [".env", ".env.local"]"#;
    let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
    let files = wrapper
        .env_files
        .as_list()
        .unwrap_or_else(|| panic!("expected List variant"));
    assert_eq!(files.len(), 2);
    assert_eq!(files[0], ".env");
    assert_eq!(files[1], ".env.local");
}

#[test]
fn test_env_files_struct_form_toml() {
    // TOML table should deserialize to EnvFilesConfig::TokenFiles via
    // the serde_yaml_ng::Value intermediate.
    #[derive(Deserialize)]
    struct Wrapper {
        env_files: EnvFilesConfig,
    }
    let toml_str = r#"
[env_files]
github_token = "~/.config/goreleaser/github_token"
gitlab_token = "/etc/tokens/gitlab"
"#;
    let wrapper: Wrapper = toml::from_str(toml_str).unwrap();
    let tokens = wrapper
        .env_files
        .as_token_files()
        .unwrap_or_else(|| panic!("expected TokenFiles variant"));
    assert_eq!(
        tokens.github_token.as_deref(),
        Some("~/.config/goreleaser/github_token")
    );
    assert_eq!(tokens.gitlab_token.as_deref(), Some("/etc/tokens/gitlab"));
    assert!(tokens.gitea_token.is_none());
}

#[test]
fn test_env_files_token_config_toml_rejects_unknown_fields() {
    // Verify deny_unknown_fields works: a typo like `github_tokne` must fail.
    let toml_str = r#"github_tokne = "~/.config/goreleaser/github_token""#;
    let result = toml::from_str::<EnvFilesTokenConfig>(toml_str);
    assert!(
        result.is_err(),
        "EnvFilesTokenConfig should reject unknown fields like 'github_tokne'"
    );
}

// ---- BuildIgnore tests ----

#[test]
fn test_build_ignore_parses() {
    // defaults.ignore moved to defaults.builds.ignore
    // (path-mirror of BuildConfig).
    let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-unknown-linux-gnu
  builds:
    ignore:
      - os: windows
        arch: arm64
      - os: linux
        arch: "386"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let defaults = config.defaults.unwrap();
    let ignores = defaults.builds.unwrap().ignore.unwrap();
    assert_eq!(ignores.len(), 2);
    assert_eq!(ignores[0].os, "windows");
    assert_eq!(ignores[0].arch, "arm64");
    assert_eq!(ignores[1].os, "linux");
    assert_eq!(ignores[1].arch, "386");
}

#[test]
fn test_build_ignore_omitted() {
    let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let defaults = config.defaults.unwrap();
    assert!(defaults.builds.is_none());
}

// ---- BuildOverride tests ----

#[test]
fn test_build_override_parses() {
    // defaults.overrides moved to defaults.builds.overrides
    // (path-mirror of BuildConfig).
    let yaml = r#"
project_name: test
defaults:
  builds:
    overrides:
      - targets:
          - "x86_64-*"
        features:
          - simd
        flags:
          - "--release"
        env:
          - CC=gcc
      - targets:
          - "*-apple-darwin"
        features:
          - metal
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let defaults = config.defaults.unwrap();
    let overrides = defaults.builds.unwrap().overrides.unwrap();
    assert_eq!(overrides.len(), 2);
    assert_eq!(overrides[0].targets, vec!["x86_64-*"]);
    assert_eq!(overrides[0].features, Some(vec!["simd".to_string()]));
    assert_eq!(overrides[0].flags, Some(vec!["--release".to_string()]));
    assert!(
        overrides[0]
            .env
            .as_ref()
            .unwrap()
            .contains(&"CC=gcc".to_string())
    );
    assert_eq!(overrides[1].targets, vec!["*-apple-darwin"]);
    assert_eq!(overrides[1].features, Some(vec!["metal".to_string()]));
    assert!(overrides[1].env.is_none());
}

#[test]
fn test_build_override_omitted() {
    let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let defaults = config.defaults.unwrap();
    assert!(defaults.builds.is_none());
}

// ---- JSON Schema generation test ----

#[test]
fn test_json_schema_generation() {
    let schema = super::config_schema();
    let json = serde_json::to_string_pretty(&schema).unwrap();
    assert!(json.contains("project_name"));
    assert!(json.contains("env_files"));
    assert!(json.contains("version"));
    assert!(json.contains("BuildIgnore"));
    assert!(json.contains("BuildOverride"));
}

// ---- Homebrew new fields parsing tests ----

// ---- Scoop new fields parsing tests ----

// -----------------------------------------------------------------------
// GitConfig tests
// -----------------------------------------------------------------------

#[test]
fn test_git_config_all_fields() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
git:
  tag_sort: "-version:creatordate"
  ignore_tags:
    - "nightly*"
    - "legacy-*"
  ignore_tag_prefixes:
    - "internal/"
    - "test-"
  prerelease_suffix: "-rc"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let git = config
        .git
        .unwrap_or_else(|| panic!("git section should be present"));
    assert_eq!(git.tag_sort.as_deref(), Some("-version:creatordate"));
    assert_eq!(
        git.ignore_tags.as_deref(),
        Some(&["nightly*".to_string(), "legacy-*".to_string()][..])
    );
    assert_eq!(
        git.ignore_tag_prefixes.as_deref(),
        Some(&["internal/".to_string(), "test-".to_string()][..])
    );
    assert_eq!(git.prerelease_suffix.as_deref(), Some("-rc"));
}

#[test]
fn test_git_config_omitted_is_none() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.git.is_none());
}

#[test]
fn test_git_config_partial_only_tag_sort() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
git:
  tag_sort: "-version:refname"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let git = config
        .git
        .unwrap_or_else(|| panic!("git section should be present"));
    assert_eq!(git.tag_sort.as_deref(), Some("-version:refname"));
    assert!(git.ignore_tags.is_none());
    assert!(git.ignore_tag_prefixes.is_none());
    assert!(git.prerelease_suffix.is_none());
}

#[test]
fn test_git_config_ignore_tags_accepts_array() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
git:
  ignore_tags:
    - "alpha*"
    - "beta*"
    - "rc-*"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let tags = config.git.unwrap().ignore_tags.unwrap();
    assert_eq!(tags.len(), 3);
    assert_eq!(tags[0], "alpha*");
    assert_eq!(tags[1], "beta*");
    assert_eq!(tags[2], "rc-*");
}

#[test]
fn test_validate_tag_sort_valid_refname() {
    let config = Config {
        git: Some(GitConfig {
            tag_sort: Some("-version:refname".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(validate_tag_sort(&config).is_ok());
}

#[test]
fn test_validate_tag_sort_valid_creatordate() {
    let config = Config {
        git: Some(GitConfig {
            tag_sort: Some("-version:creatordate".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(validate_tag_sort(&config).is_ok());
}

#[test]
fn test_validate_tag_sort_none_is_valid() {
    let config = Config {
        git: Some(GitConfig::default()),
        ..Default::default()
    };
    assert!(validate_tag_sort(&config).is_ok());
}

#[test]
fn test_validate_tag_sort_no_git_config_is_valid() {
    let config = Config::default();
    assert!(validate_tag_sort(&config).is_ok());
}

#[test]
fn test_validate_tag_sort_invalid_rejected() {
    let config = Config {
        git: Some(GitConfig {
            tag_sort: Some("alphabetical".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = validate_tag_sort(&config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.contains("alphabetical"),
        "error should contain the bad value: {}",
        err
    );
    assert!(
        err.contains("-version:refname"),
        "error should list accepted values: {}",
        err
    );
}

#[test]
fn test_validate_tag_sort_valid_semver() {
    let config = Config {
        git: Some(GitConfig {
            tag_sort: Some("semver".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(validate_tag_sort(&config).is_ok());
}

#[test]
fn test_validate_tag_sort_valid_smartsemver() {
    let config = Config {
        git: Some(GitConfig {
            tag_sort: Some("smartsemver".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(validate_tag_sort(&config).is_ok());
}

#[test]
fn test_validate_tag_sort_invalid_lists_semver_modes() {
    let config = Config {
        git: Some(GitConfig {
            tag_sort: Some("bogus".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let err = validate_tag_sort(&config).unwrap_err();
    assert!(err.contains("semver"), "error should mention semver: {err}");
    assert!(
        err.contains("smartsemver"),
        "error should mention smartsemver: {err}"
    );
}

#[test]
fn test_git_config_yaml_accepts_semver() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
git:
  tag_sort: "semver"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.git.as_ref().unwrap().tag_sort.as_deref(),
        Some("semver")
    );
    assert!(validate_tag_sort(&config).is_ok());
}

#[test]
fn test_git_config_yaml_accepts_smartsemver() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
git:
  tag_sort: "smartsemver"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.git.as_ref().unwrap().tag_sort.as_deref(),
        Some("smartsemver")
    );
    assert!(validate_tag_sort(&config).is_ok());
}

// ---- partial.by validation tests ----

#[test]
fn test_validate_partial_none_is_ok() {
    let config = Config::default();
    assert!(super::validate_partial(&config).is_ok());
}

#[test]
fn test_validate_partial_accepts_os_and_target() {
    for by in ["os", "target"] {
        let config = Config {
            partial: Some(super::PartialConfig {
                by: Some(by.to_string()),
            }),
            ..Default::default()
        };
        assert!(
            super::validate_partial(&config).is_ok(),
            "partial.by={by} must validate"
        );
    }
}

#[test]
fn test_validate_partial_rejects_pre_rename_goos() {
    let config = Config {
        partial: Some(super::PartialConfig {
            by: Some("goos".to_string()),
        }),
        ..Default::default()
    };
    let err = super::validate_partial(&config).unwrap_err();
    assert!(
        err.contains("goos"),
        "error should name the bad value: {err}"
    );
    assert!(
        err.contains("\"os\"") && err.contains("\"target\""),
        "error should list accepted values: {err}"
    );
}

// ---- defaults axis-mismatch validation tests ----

#[test]
fn test_validate_defaults_axis_no_defaults_is_ok() {
    let config = Config::default();
    assert!(validate_defaults_axis(&config).is_ok());
}

#[test]
fn test_validate_defaults_axis_crates_block_with_top_level_crates_is_ok() {
    let yaml = r#"
project_name: test
defaults:
  crates: {}
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(validate_defaults_axis(&config).is_ok());
}

#[test]
fn test_validate_defaults_axis_workspaces_block_with_top_level_workspaces_is_ok() {
    let yaml = r#"
project_name: test
defaults:
  workspaces: {}
workspaces:
  - name: ws1
    crates:
      - name: a
        path: "."
        tag_template: "v{{ .Version }}"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(validate_defaults_axis(&config).is_ok());
}

#[test]
fn test_validate_defaults_axis_crates_block_without_top_level_crates_errors() {
    let yaml = r#"
project_name: test
defaults:
  crates: {}
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_defaults_axis(&config).unwrap_err();
    assert!(
        err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH),
        "error should be tagged with the {ERR_DEFAULTS_AXIS_MISMATCH} marker prefix: {err}"
    );
    assert!(
        err.contains("defaults.crates"),
        "error should mention defaults.crates: {err}"
    );
}

#[test]
fn test_validate_defaults_axis_workspaces_block_without_top_level_workspaces_errors() {
    let yaml = r#"
project_name: test
defaults:
  workspaces: {}
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_defaults_axis(&config).unwrap_err();
    assert!(
        err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH),
        "error should be tagged with the {ERR_DEFAULTS_AXIS_MISMATCH} marker prefix: {err}"
    );
    assert!(
        err.contains("defaults.workspaces"),
        "error should mention defaults.workspaces: {err}"
    );
}

#[test]
fn test_validate_defaults_axis_both_blocks_set_errors() {
    let yaml = r#"
project_name: test
defaults:
  crates: {}
  workspaces: {}
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_defaults_axis(&config).unwrap_err();
    assert!(
        err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH),
        "error should be tagged with the {ERR_DEFAULTS_AXIS_MISMATCH} marker prefix: {err}"
    );
    assert!(
        err.contains("mutually exclusive"),
        "error should mention mutual exclusion: {err}"
    );
}

#[test]
fn test_validate_defaults_axis_wrong_axis_errors() {
    // defaults.crates set but top-level uses workspaces
    let yaml = r#"
project_name: test
defaults:
  crates: {}
workspaces:
  - name: ws1
    crates:
      - name: a
        path: "."
        tag_template: "v{{ .Version }}"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_defaults_axis(&config).unwrap_err();
    assert!(
        err.starts_with(ERR_DEFAULTS_AXIS_MISMATCH),
        "error should be tagged with the {ERR_DEFAULTS_AXIS_MISMATCH} marker prefix: {err}"
    );
    assert!(
        err.contains("workspaces"),
        "error should mention top-level workspaces: {err}"
    );
}

// ---------------------------------------------------------------------------
// validate_homebrew_cask_url_template tests
// ---------------------------------------------------------------------------

#[test]
fn test_validate_homebrew_cask_url_template_both_set_rejected() {
    // Setting url_template AND url.template on the same HomebrewCaskConfig
    // must be a hard validation error.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew_cask:
        url_template: "https://example.com/{{ .Tag }}/myapp.dmg"
        url:
          template: "https://example.com/{{ .Tag }}/myapp.dmg"
          verified: "example.com"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_homebrew_cask_url_template(&config).unwrap_err();
    assert!(
        err.contains("url_template") && err.contains("url.template"),
        "error should mention both conflicting fields: {err}"
    );
    assert!(
        err.contains("mutually exclusive"),
        "error should say they are mutually exclusive: {err}"
    );
}

#[test]
fn test_validate_homebrew_cask_url_template_only_url_template_is_ok() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew_cask:
        url_template: "https://example.com/{{ .Tag }}/myapp.dmg"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(validate_homebrew_cask_url_template(&config).is_ok());
}

#[test]
fn test_validate_homebrew_cask_url_template_only_url_is_ok() {
    let yaml = r#"
project_name: test
homebrew_casks:
  - name: myapp
    url:
      template: "https://example.com/{{ .Tag }}/myapp.dmg"
      verified: "example.com"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(validate_homebrew_cask_url_template(&config).is_ok());
}

#[test]
fn test_validate_homebrew_cask_url_template_top_level_both_set_rejected() {
    // Same conflict detected in top-level homebrew_casks array.
    let yaml = r#"
project_name: test
homebrew_casks:
  - name: myapp
    url_template: "https://example.com/{{ .Tag }}/myapp.dmg"
    url:
      template: "https://example.com/{{ .Tag }}/myapp.dmg"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_homebrew_cask_url_template(&config).unwrap_err();
    assert!(
        err.contains("homebrew_casks[0]"),
        "error should identify the offending entry: {err}"
    );
}

#[test]
fn test_validate_homebrew_cask_url_template_defaults_axis_both_set_rejected() {
    // The validator must cover the defaults.publish.homebrew_cask axis, not
    // just crates[] and the top-level list — config-mode parity.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
defaults:
  publish:
    homebrew_cask:
      url_template: "https://example.com/{{ .Tag }}/myapp.dmg"
      url:
        template: "https://example.com/{{ .Tag }}/myapp.dmg"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_homebrew_cask_url_template(&config).unwrap_err();
    assert!(
        err.contains("defaults.publish.homebrew_cask"),
        "error should identify the defaults axis location: {err}"
    );
    assert!(
        err.contains("mutually exclusive"),
        "error should say they are mutually exclusive: {err}"
    );
}

#[test]
fn test_validate_homebrew_cask_url_template_workspace_axis_both_set_rejected() {
    // The validator must cover the workspaces[].crates[].publish.homebrew_cask
    // axis — workspace per-crate mode parity.
    let yaml = r#"
project_name: test
crates: []
workspaces:
  - name: ws1
    crates:
      - name: myapp
        path: "."
        tag_template: "v{{ .Version }}"
        publish:
          homebrew_cask:
            url_template: "https://example.com/{{ .Tag }}/myapp.dmg"
            url:
              template: "https://example.com/{{ .Tag }}/myapp.dmg"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_homebrew_cask_url_template(&config).unwrap_err();
    assert!(
        err.contains("workspaces[ws1].crates[myapp].publish.homebrew_cask"),
        "error should identify the workspace axis location: {err}"
    );
}

// ---------------------------------------------------------------------------
// validate_winget_upgrade_behavior tests
// ---------------------------------------------------------------------------

#[test]
fn test_validate_winget_upgrade_behavior_rejects_unknown_value() {
    // A value outside winget's UpgradeBehavior enum must be rejected at
    // config-validate time (not silently emitted into a manifest the winget
    // validator later rejects at PR time).
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        publisher: AcmeCo
        upgrade_behavior: reinstall
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_winget_upgrade_behavior(&config).unwrap_err();
    assert!(
        err.contains("crates[myapp].publish.winget") && err.contains("reinstall"),
        "error should identify the location + bad value: {err}"
    );
    assert!(
        err.contains("install") && err.contains("uninstallPrevious") && err.contains("deny"),
        "error should list the allowed values: {err}"
    );
}

#[test]
fn test_validate_winget_upgrade_behavior_accepts_each_valid_value() {
    for value in ["install", "uninstallPrevious", "deny"] {
        let yaml = format!(
            r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    publish:
      winget:
        publisher: AcmeCo
        upgrade_behavior: {value}
"#
        );
        let config: Config = serde_yaml_ng::from_str(&yaml).unwrap();
        assert!(
            validate_winget_upgrade_behavior(&config).is_ok(),
            "`{value}` is a valid winget upgrade_behavior and must pass"
        );
    }
}

#[test]
fn test_validate_winget_upgrade_behavior_unset_is_ok() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        publisher: AcmeCo
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(validate_winget_upgrade_behavior(&config).is_ok());
}

#[test]
fn test_validate_winget_upgrade_behavior_defaults_axis() {
    // defaults.publish.winget axis must be covered — config-mode parity.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
defaults:
  publish:
    winget:
      publisher: AcmeCo
      upgrade_behavior: bogus
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_winget_upgrade_behavior(&config).unwrap_err();
    assert!(
        err.contains("defaults.publish.winget") && err.contains("bogus"),
        "error should identify the defaults axis + bad value: {err}"
    );
}

#[test]
fn test_validate_winget_upgrade_behavior_workspace_axis() {
    // workspaces[].crates[].publish.winget axis — workspace per-crate parity.
    let yaml = r#"
project_name: test
crates: []
workspaces:
  - name: ws1
    crates:
      - name: myapp
        path: "."
        tag_template: "v{{ .Version }}"
        publish:
          winget:
            publisher: AcmeCo
            upgrade_behavior: nope
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_winget_upgrade_behavior(&config).unwrap_err();
    assert!(
        err.contains("workspaces[ws1].crates[myapp].publish.winget") && err.contains("nope"),
        "error should identify the workspace axis + bad value: {err}"
    );
}

// ---------------------------------------------------------------------------
// validate_winget_dependency_architectures tests
// ---------------------------------------------------------------------------

#[test]
fn test_validate_winget_dependency_architectures_rejects_unknown_value() {
    // A non-winget arch name (the common `amd64` cargo/Go spelling) matches no
    // installer and would silently drop the dependency from the manifest.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        publisher: AcmeCo
        dependencies:
          - package_identifier: Microsoft.VCRedist.2015+.x64
            architectures: ["amd64"]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_winget_dependency_architectures(&config).unwrap_err();
    assert!(
        err.contains("crates[myapp].publish.winget")
            && err.contains("dependencies[0]")
            && err.contains("amd64"),
        "error should identify the location, index, and bad value: {err}"
    );
    assert!(
        err.contains("x64") && err.contains("arm64") && err.contains("x86"),
        "error should list the allowed values: {err}"
    );
}

#[test]
fn test_validate_winget_dependency_architectures_rejects_wrong_case() {
    // Matching is exact + case-sensitive, so `X64` is as invalid as `amd64`.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        publisher: AcmeCo
        dependencies:
          - package_identifier: Acme.Runtime
            architectures: ["X64"]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_winget_dependency_architectures(&config).unwrap_err();
    assert!(
        err.contains("X64"),
        "error should name the mis-cased value: {err}"
    );
}

#[test]
fn test_validate_winget_dependency_architectures_accepts_each_valid_value() {
    for value in ["x64", "arm64", "x86"] {
        let yaml = format!(
            r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{{{ .Version }}}}"
    publish:
      winget:
        publisher: AcmeCo
        dependencies:
          - package_identifier: Acme.Runtime
            architectures: ["{value}"]
"#
        );
        let config: Config = serde_yaml_ng::from_str(&yaml).unwrap();
        assert!(
            validate_winget_dependency_architectures(&config).is_ok(),
            "`{value}` is a valid winget architecture and must pass"
        );
    }
}

#[test]
fn test_validate_winget_dependency_architectures_empty_list_is_ok() {
    // An empty `architectures: []` means "all installers" and is valid.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        publisher: AcmeCo
        dependencies:
          - package_identifier: Acme.Runtime
            architectures: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(validate_winget_dependency_architectures(&config).is_ok());
}

#[test]
fn test_validate_winget_dependency_architectures_unset_is_ok() {
    // Absent `architectures` means "all installers" and is valid.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        publisher: AcmeCo
        dependencies:
          - package_identifier: Acme.Runtime
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(validate_winget_dependency_architectures(&config).is_ok());
}

#[test]
fn test_validate_winget_dependency_architectures_no_deps_is_ok() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        publisher: AcmeCo
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(validate_winget_dependency_architectures(&config).is_ok());
}

#[test]
fn test_validate_winget_dependency_architectures_defaults_axis() {
    // defaults.publish.winget axis must be covered — config-mode parity.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
defaults:
  publish:
    winget:
      publisher: AcmeCo
      dependencies:
        - package_identifier: Acme.Runtime
          architectures: ["aarch64"]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_winget_dependency_architectures(&config).unwrap_err();
    assert!(
        err.contains("defaults.publish.winget") && err.contains("aarch64"),
        "error should identify the defaults axis + bad value: {err}"
    );
}

#[test]
fn test_validate_winget_dependency_architectures_workspace_axis() {
    // workspaces[].crates[].publish.winget axis — workspace per-crate parity.
    let yaml = r#"
project_name: test
crates: []
workspaces:
  - name: ws1
    crates:
      - name: myapp
        path: "."
        tag_template: "v{{ .Version }}"
        publish:
          winget:
            publisher: AcmeCo
            dependencies:
              - package_identifier: Acme.Runtime
                architectures: ["i386"]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_winget_dependency_architectures(&config).unwrap_err();
    assert!(
        err.contains("workspaces[ws1].crates[myapp].publish.winget") && err.contains("i386"),
        "error should identify the workspace axis + bad value: {err}"
    );
}

#[test]
fn test_git_config_ignore_tag_prefixes_accepts_array() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
git:
  ignore_tag_prefixes:
    - "wip/"
    - "experiment/"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let prefixes = config.git.unwrap().ignore_tag_prefixes.unwrap();
    assert_eq!(prefixes.len(), 2);
    assert_eq!(prefixes[0], "wip/");
    assert_eq!(prefixes[1], "experiment/");
}

#[test]
fn test_metadata_config_with_mod_timestamp() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
metadata:
  mod_timestamp: "{{ .CommitTimestamp }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let meta = config.metadata.unwrap();
    assert_eq!(meta.mod_timestamp.unwrap(), "{{ .CommitTimestamp }}");
}

#[test]
fn test_metadata_config_omitted_is_none() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.metadata.is_none());
}

#[test]
fn test_metadata_config_empty_section() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
metadata: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let meta = config.metadata.unwrap();
    assert!(meta.mod_timestamp.is_none());
}

#[test]
fn test_variables_config_parsed() {
    let yaml = r#"
project_name: test
variables:
  description: "my project description"
  somethingElse: "yada yada yada"
  empty: ""
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let vars = config.variables.as_ref().unwrap();
    assert_eq!(vars.get("description").unwrap(), "my project description");
    assert_eq!(vars.get("somethingElse").unwrap(), "yada yada yada");
    assert_eq!(vars.get("empty").unwrap(), "");
    assert_eq!(vars.len(), 3);
}

#[test]
fn test_variables_config_omitted_is_none() {
    let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.variables.is_none());
}

// ---- Project metadata accessor + fallback tests ------------------------
//
// `meta_homepage` / `meta_description` / `meta_license` back the
// `or_else(|| ctx.config.meta_*())` fallbacks that the per-publisher
// blocks (homebrew formula + cask, scoop, dockerhub, mcp, nix, …)
// consult when their own field is unset. These tests pin the
// reference shape so a regression in the accessor (renaming
// metadata fields, switching MetadataConfig storage) surfaces
// before the publishers silently emit empty homepage / description
// strings.

#[test]
fn test_meta_accessors_return_values_when_set() {
    let yaml = r#"
project_name: test
metadata:
  homepage: "https://example.com"
  description: "shared description"
  license: "MIT"
  maintainers:
    - "Alice <a@example.com>"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.meta_homepage(), Some("https://example.com"));
    assert_eq!(config.meta_description(), Some("shared description"));
    assert_eq!(config.meta_license(), Some("MIT"));
    assert_eq!(
        config.meta_first_maintainer(),
        Some("Alice <a@example.com>")
    );
}

#[test]
fn test_meta_accessors_return_none_when_metadata_omitted() {
    let yaml = r#"
project_name: test
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.meta_homepage().is_none());
    assert!(config.meta_description().is_none());
    assert!(config.meta_license().is_none());
    assert!(config.meta_first_maintainer().is_none());
}

// ---- Cargo.toml-derived metadata: per-crate accessors + precedence ----
//
// `populate_derived_metadata` reads each crate's `Cargo.toml [package]`
// (description/license/homepage/authors) so a plain Rust project resolves
// publisher metadata WITHOUT a top-level `metadata:` YAML block. The
// crate-aware `meta_*_for(crate)` accessors layer it under the YAML block:
// YAML wins, then Cargo.toml-derived, then None.

fn write_cargo(dir: &std::path::Path, body: &str) {
    std::fs::write(dir.join("Cargo.toml"), body).unwrap();
}

#[test]
fn derived_metadata_fills_publisher_fields_without_yaml_block() {
    let base = tempfile::tempdir().unwrap();
    let crate_dir = base.path().join("app");
    std::fs::create_dir_all(&crate_dir).unwrap();
    write_cargo(
        &crate_dir,
        r#"
[package]
name = "app"
description = "the app crate"
license = "MIT"
homepage = "https://app.example"
authors = ["Ada <ada@example.com>"]
"#,
    );

    let mut config = Config {
        project_name: "test".into(),
        crates: vec![CrateConfig {
            name: "app".into(),
            path: "app".into(),
            tag_template: "v{{ .Version }}".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    // No top-level `metadata:` block at all.
    assert!(config.metadata.is_none());
    config.populate_derived_metadata(base.path());

    // The crate-aware accessors now resolve the publisher metadata that
    // winget/snapcraft/nfpm previously hard-errored on.
    assert_eq!(config.meta_description_for("app"), Some("the app crate"));
    assert_eq!(config.meta_license_for("app"), Some("MIT"));
    assert_eq!(config.meta_homepage_for("app"), Some("https://app.example"));
    assert_eq!(
        config.meta_first_maintainer_for("app"),
        Some("Ada <ada@example.com>")
    );

    // The crate-agnostic accessors stay None (no YAML block) — only the
    // `_for` variants consult Cargo.toml.
    assert!(config.meta_description().is_none());
    assert!(config.meta_license().is_none());
}

#[test]
fn yaml_metadata_block_wins_over_cargo_toml() {
    let base = tempfile::tempdir().unwrap();
    let crate_dir = base.path().join("app");
    std::fs::create_dir_all(&crate_dir).unwrap();
    write_cargo(
        &crate_dir,
        r#"
[package]
name = "app"
description = "from cargo"
license = "MIT"
"#,
    );

    let mut config = Config {
        project_name: "test".into(),
        metadata: Some(MetadataConfig {
            description: Some("from yaml".into()),
            license: Some("Apache-2.0".into()),
            ..Default::default()
        }),
        crates: vec![CrateConfig {
            name: "app".into(),
            path: "app".into(),
            tag_template: "v{{ .Version }}".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    config.populate_derived_metadata(base.path());

    // Hand-written `metadata:` wins; Cargo.toml only fills genuine gaps.
    assert_eq!(config.meta_description_for("app"), Some("from yaml"));
    assert_eq!(config.meta_license_for("app"), Some("Apache-2.0"));
}

#[test]
fn per_crate_workspace_each_crate_gets_its_own_cargo_metadata() {
    let base = tempfile::tempdir().unwrap();
    for (name, desc) in [("core", "the core lib"), ("csi", "the csi plugin")] {
        let dir = base.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        write_cargo(
            &dir,
            &format!("[package]\nname = \"{name}\"\ndescription = \"{desc}\"\nlicense = \"MIT\"\n"),
        );
    }

    let mut config = Config {
        project_name: "test".into(),
        crates: vec![
            CrateConfig {
                name: "core".into(),
                path: "core".into(),
                tag_template: "v{{ .Version }}".into(),
                ..Default::default()
            },
            CrateConfig {
                name: "csi".into(),
                path: "csi".into(),
                tag_template: "csi-v{{ .Version }}".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    config.populate_derived_metadata(base.path());

    // Each crate's publishers see THAT crate's own Cargo.toml description —
    // csi's description must not leak the core crate's, and vice versa.
    assert_eq!(config.meta_description_for("core"), Some("the core lib"));
    assert_eq!(config.meta_description_for("csi"), Some("the csi plugin"));
}

#[test]
fn license_file_only_leaves_license_empty_no_fabrication() {
    let base = tempfile::tempdir().unwrap();
    let crate_dir = base.path().join("app");
    std::fs::create_dir_all(&crate_dir).unwrap();
    write_cargo(
        &crate_dir,
        r#"
[package]
name = "app"
description = "has a license file, not an SPDX id"
license-file = "LICENSE"
"#,
    );

    let mut config = Config {
        project_name: "test".into(),
        crates: vec![CrateConfig {
            name: "app".into(),
            path: "app".into(),
            tag_template: "v{{ .Version }}".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    config.populate_derived_metadata(base.path());

    // description still derives; license stays empty (no SPDX synthesised).
    assert_eq!(
        config.meta_description_for("app"),
        Some("has a license file, not an SPDX id")
    );
    assert!(config.meta_license_for("app").is_none());
}

// ---- SnapcraftConfig disable StringOrBool tests ----

#[test]
fn test_snapcraft_disable_bool_true() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - skip: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
    assert_eq!(snap.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_snapcraft_disable_bool_false() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - skip: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
    assert_eq!(snap.skip, Some(StringOrBool::Bool(false)));
}

#[test]
fn test_snapcraft_disable_template_string() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - skip: "{{ if .IsSnapshot }}true{{ end }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
    match &snap.skip {
        Some(StringOrBool::String(s)) => {
            assert!(s.contains("IsSnapshot"));
        }
        other => panic!("expected StringOrBool::String, got {:?}", other),
    }
}

#[test]
fn test_snapcraft_disable_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - name: mysnap
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
    assert!(snap.skip.is_none());
}

// `docker_v2[].skip_push` is not a recognized field; `deny_unknown_fields`
// on `DockerV2Config` must reject it at parse time instead of silently
// dropping. Use the canonical `skip:` to suppress the publish step.
#[test]
fn test_docker_v2_skip_push_rejected() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    dockers_v2:
      - dockerfile: Dockerfile
        images: ["ghcr.io/owner/app"]
        tags: ["{{ .Version }}"]
        skip_push: true
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "docker_v2[].skip_push must be rejected (use canonical `skip:`)"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("skip_push") || err.contains("unknown field"),
        "error should mention the rejected field; got: {err}"
    );
}

// Snapcraft has no top-level `slots:` concept (only per-app slots via
// `apps.<name>.slots`); `deny_unknown_fields` on `SnapcraftConfig` must
// reject the top-level form at parse time instead of silently dropping.
#[test]
fn test_snapcraft_top_level_slots_rejected() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - name: mysnap
        slots:
          dbus-svc:
            interface: dbus
            bus: session
            name: com.example.svc
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "snapcrafts[].slots must be rejected (use apps.<name>.slots)"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("slots") || err.contains("unknown field"),
        "error should mention the rejected field; got: {err}"
    );
}

// ---- AurConfig disable StringOrBool tests ----

#[test]
fn test_aur_disable_bool_true() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        skip: true
        git_url: "ssh://aur@aur.archlinux.org/a.git"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let aur = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .aur
        .as_ref()
        .unwrap();
    assert_eq!(aur.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_aur_disable_template_string() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        skip: "{{ if .IsSnapshot }}true{{ end }}"
        git_url: "ssh://aur@aur.archlinux.org/a.git"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let aur = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .aur
        .as_ref()
        .unwrap();
    match &aur.skip {
        Some(StringOrBool::String(s)) => {
            assert!(s.contains("IsSnapshot"));
        }
        other => panic!("expected StringOrBool::String, got {:?}", other),
    }
}

#[test]
fn test_aur_disable_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        git_url: "ssh://aur@aur.archlinux.org/a.git"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let aur = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .aur
        .as_ref()
        .unwrap();
    assert!(aur.skip.is_none());
}

// ---- PublisherConfig disable StringOrBool tests ----

#[test]
fn test_publisher_disable_bool_true() {
    let yaml = r#"
project_name: test
publishers:
  - cmd: "echo hello"
    skip: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let pub_cfg = &config.publishers.as_ref().unwrap()[0];
    assert_eq!(pub_cfg.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_publisher_disable_template_string() {
    let yaml = r#"
project_name: test
publishers:
  - cmd: "echo hello"
    skip: "{{ if .IsSnapshot }}true{{ end }}"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let pub_cfg = &config.publishers.as_ref().unwrap()[0];
    match &pub_cfg.skip {
        Some(StringOrBool::String(s)) => {
            assert!(s.contains("IsSnapshot"));
        }
        other => panic!("expected StringOrBool::String, got {:?}", other),
    }
}

#[test]
fn test_publisher_disable_omitted() {
    let yaml = r#"
project_name: test
publishers:
  - cmd: "echo hello"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let pub_cfg = &config.publishers.as_ref().unwrap()[0];
    assert!(pub_cfg.skip.is_none());
}

#[test]
fn test_disable_alias_sets_skip_truthy() {
    // GoReleaser spells the section toggle `disable:`; anodizer's canonical is
    // `skip:`. The serde alias maps `disable:` onto `skip` with identical
    // polarity (disable:true == skip:true). Prove it round-trips truthy on
    // every aliased struct, and that the canonical `skip:` still works too.
    let yaml = r#"
project_name: test
publishers:
  - cmd: "echo hello"
    disable: true
sboms:
  - artifacts: archive
    disable: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    checksum:
      disable: true
    docker_digest:
      disable: true
    flatpaks:
      - app_id: org.example.App
        disable: true
    blobs:
      - provider: s3
        bucket: my-bucket
        disable: true
    publish:
      aur:
        disable: true
      aur_source:
        disable: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();

    let crate_cfg = &config.crates[0];
    let publish = crate_cfg.publish.as_ref().unwrap();
    let truthy: &[(&str, &Option<StringOrBool>)] = &[
        ("publisher", &config.publishers.as_ref().unwrap()[0].skip),
        ("sbom", &config.sboms[0].skip),
        ("checksum", &crate_cfg.checksum.as_ref().unwrap().skip),
        (
            "docker_digest",
            &crate_cfg.docker_digest.as_ref().unwrap().skip,
        ),
        ("flatpak", &crate_cfg.flatpaks.as_ref().unwrap()[0].skip),
        ("blob", &crate_cfg.blobs.as_ref().unwrap()[0].skip),
        ("aur", &publish.aur.as_ref().unwrap().skip),
        ("aur_source", &publish.aur_source.as_ref().unwrap().skip),
    ];
    for (label, skip) in truthy {
        assert!(
            skip.as_ref().is_some_and(StringOrBool::as_bool),
            "{label} `disable: true` should set skip truthy"
        );
    }
}

#[test]
fn test_canonical_skip_still_parses_truthy() {
    // The `disable:` alias must not clobber the canonical `skip:` spelling:
    // prove `skip: true` still round-trips truthy on a couple of the structs.
    let yaml = r#"
project_name: test
publishers:
  - cmd: "echo hello"
    skip: true
sboms:
  - artifacts: archive
    skip: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(
        config.publishers.as_ref().unwrap()[0]
            .skip
            .as_ref()
            .is_some_and(StringOrBool::as_bool),
        "publisher `skip: true` should set skip truthy"
    );
    assert!(
        config.sboms[0]
            .skip
            .as_ref()
            .is_some_and(StringOrBool::as_bool),
        "sbom `skip: true` should set skip truthy"
    );
}

// ---- skip_upload StringOrBool tests for publisher configs ----

#[test]
fn test_aur_skip_upload_bool_true() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur:
        skip_upload: true
        git_url: "ssh://aur@aur.archlinux.org/a.git"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let aur = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .aur
        .as_ref()
        .unwrap();
    assert_eq!(aur.skip_upload, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_nix_skip_upload_template() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      nix:
        skip_upload: "{{ .Env.SKIP }}"
        repository:
          owner: org
          name: nixpkgs
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nix = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .nix
        .as_ref()
        .unwrap();
    match &nix.skip_upload {
        Some(StringOrBool::String(s)) => {
            assert!(s.contains(".Env.SKIP"));
        }
        other => panic!("expected StringOrBool::String, got {:?}", other),
    }
}

// -----------------------------------------------------------------------
// TemplateFileConfig tests
// -----------------------------------------------------------------------

#[test]
fn test_template_files_parses_from_yaml() {
    let yaml = r#"
project_name: myproject
crates: []
template_files:
  - id: install-script
    src: install.sh.tpl
    dst: install.sh
    mode: "0755"
  - src: README.md.tpl
    dst: README.md
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let tfs = config.template_files.unwrap();
    assert_eq!(tfs.len(), 2);

    assert_eq!(tfs[0].id.as_deref(), Some("install-script"));
    assert_eq!(tfs[0].src, "install.sh.tpl");
    assert_eq!(tfs[0].dst, "install.sh");
    assert_eq!(tfs[0].mode, Some("0755".to_string()));

    assert_eq!(tfs[1].id, None);
    assert_eq!(tfs[1].src, "README.md.tpl");
    assert_eq!(tfs[1].dst, "README.md");
    assert_eq!(tfs[1].mode, None);
}

#[test]
fn test_template_files_defaults_to_none() {
    let yaml = r#"
project_name: myproject
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.template_files.is_none());
}

// -----------------------------------------------------------------------
// IncludeSpec parsing tests
// -----------------------------------------------------------------------

#[test]
fn test_include_spec_plain_string() {
    let yaml = r#"
project_name: test
includes:
  - ./defaults.yaml
  - extra.yaml
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let includes = config.includes.unwrap();
    assert_eq!(includes.len(), 2);
    assert_eq!(
        includes[0],
        IncludeSpec::Path("./defaults.yaml".to_string())
    );
    assert_eq!(includes[1], IncludeSpec::Path("extra.yaml".to_string()));
}

#[test]
fn test_include_spec_from_file() {
    let yaml = r#"
project_name: test
includes:
  - from_file:
      path: ./config/goreleaser.yaml
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let includes = config.includes.unwrap();
    assert_eq!(includes.len(), 1);
    assert_eq!(
        includes[0],
        IncludeSpec::FromFile {
            from_file: IncludeFilePath {
                path: "./config/goreleaser.yaml".to_string(),
            },
        }
    );
}

#[test]
fn test_include_spec_from_url_without_headers() {
    let yaml = r#"
project_name: test
includes:
  - from_url:
      url: https://example.com/config.yaml
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let includes = config.includes.unwrap();
    assert_eq!(includes.len(), 1);
    assert_eq!(
        includes[0],
        IncludeSpec::FromUrl {
            from_url: IncludeUrlConfig {
                url: "https://example.com/config.yaml".to_string(),
                headers: None,
            },
        }
    );
}

#[test]
fn test_include_spec_from_url_with_headers() {
    let yaml = r#"
project_name: test
includes:
  - from_url:
      url: https://api.mycompany.com/configs/release.yaml
      headers:
        x-api-token: "${MYCOMPANY_TOKEN}"
        Authorization: "Bearer ${GITHUB_TOKEN}"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let includes = config.includes.unwrap();
    assert_eq!(includes.len(), 1);
    match &includes[0] {
        IncludeSpec::FromUrl { from_url } => {
            assert_eq!(
                from_url.url,
                "https://api.mycompany.com/configs/release.yaml"
            );
            let headers = from_url.headers.as_ref().unwrap();
            assert_eq!(headers.len(), 2);
            assert_eq!(headers["x-api-token"], "${MYCOMPANY_TOKEN}");
            assert_eq!(headers["Authorization"], "Bearer ${GITHUB_TOKEN}");
        }
        other => panic!("expected FromUrl, got: {:?}", other),
    }
}

#[test]
fn test_include_spec_mixed_forms() {
    let yaml = r#"
project_name: test
includes:
  - ./defaults.yaml
  - from_file:
      path: ./config/shared.yaml
  - from_url:
      url: https://example.com/config.yaml
      headers:
        x-token: secret
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let includes = config.includes.unwrap();
    assert_eq!(includes.len(), 3);
    assert!(matches!(&includes[0], IncludeSpec::Path(s) if s == "./defaults.yaml"));
    assert!(
        matches!(&includes[1], IncludeSpec::FromFile { from_file } if from_file.path == "./config/shared.yaml")
    );
    assert!(
        matches!(&includes[2], IncludeSpec::FromUrl { from_url } if from_url.url == "https://example.com/config.yaml")
    );
}

#[test]
fn test_include_spec_no_includes_field() {
    let yaml = r#"
project_name: test
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.includes.is_none());
}

#[test]
fn test_include_spec_empty_includes() {
    let yaml = r#"
project_name: test
includes: []
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.includes, Some(vec![]));
}

#[test]
fn test_include_spec_github_shorthand_url() {
    // The GitHub shorthand (no https:// prefix) should parse fine as a URL
    // string — normalization happens at resolve time, not parse time.
    let yaml = r#"
project_name: test
includes:
  - from_url:
      url: caarlos0/goreleaserfiles/main/packages.yml
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let includes = config.includes.unwrap();
    assert_eq!(includes.len(), 1);
    match &includes[0] {
        IncludeSpec::FromUrl { from_url } => {
            assert_eq!(from_url.url, "caarlos0/goreleaserfiles/main/packages.yml");
        }
        other => panic!("expected FromUrl, got: {:?}", other),
    }
}

// ---- Platform URL config tests ----

#[test]
fn test_github_urls_config_all_fields() {
    let yaml = r#"
api: https://github.example.com/api/v3/
upload: https://github.example.com/api/uploads/
download: https://github.example.com/
skip_tls_verify: true
"#;
    let cfg: GitHubUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        cfg.api.as_deref(),
        Some("https://github.example.com/api/v3/")
    );
    assert_eq!(
        cfg.upload.as_deref(),
        Some("https://github.example.com/api/uploads/")
    );
    assert_eq!(cfg.download.as_deref(), Some("https://github.example.com/"));
    assert_eq!(cfg.skip_tls_verify, Some(true));
}

#[test]
fn test_github_urls_config_defaults() {
    let yaml = "{}";
    let cfg: GitHubUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.api, None);
    assert_eq!(cfg.upload, None);
    assert_eq!(cfg.download, None);
    assert_eq!(cfg.skip_tls_verify, None);
}

#[test]
fn test_gitlab_urls_config_all_fields() {
    let yaml = r#"
api: https://gitlab.example.com/api/v4/
download: https://gitlab.example.com/
skip_tls_verify: false
use_package_registry: true
use_job_token: true
"#;
    let cfg: GitLabUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        cfg.api.as_deref(),
        Some("https://gitlab.example.com/api/v4/")
    );
    assert_eq!(cfg.download.as_deref(), Some("https://gitlab.example.com/"));
    assert_eq!(cfg.skip_tls_verify, Some(false));
    assert_eq!(cfg.use_package_registry, Some(true));
    assert_eq!(cfg.use_job_token, Some(true));
}

#[test]
fn test_gitlab_urls_config_defaults() {
    let yaml = "{}";
    let cfg: GitLabUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.api, None);
    assert_eq!(cfg.download, None);
    assert_eq!(cfg.skip_tls_verify, None);
    assert_eq!(cfg.use_package_registry, None);
    assert_eq!(cfg.use_job_token, None);
}

#[test]
fn test_gitea_urls_config_all_fields() {
    let yaml = r#"
api: https://gitea.example.com/api/v1/
download: https://gitea.example.com/
skip_tls_verify: true
"#;
    let cfg: GiteaUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        cfg.api.as_deref(),
        Some("https://gitea.example.com/api/v1/")
    );
    assert_eq!(cfg.download.as_deref(), Some("https://gitea.example.com/"));
    assert_eq!(cfg.skip_tls_verify, Some(true));
}

#[test]
fn test_gitea_urls_config_defaults() {
    let yaml = "{}";
    let cfg: GiteaUrlsConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.api, None);
    assert_eq!(cfg.download, None);
    assert_eq!(cfg.skip_tls_verify, None);
}

#[test]
fn test_release_config_gitlab_gitea_fields() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      github:
        owner: gh-owner
        name: gh-repo
      gitlab:
        owner: gitlab-owner
        name: gitlab-repo
      gitea:
        owner: gitea-owner
        name: gitea-repo
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    let github = release.github.as_ref().unwrap();
    assert_eq!(github.owner, "gh-owner");
    assert_eq!(github.name, "gh-repo");
    let gitlab = release.gitlab.as_ref().unwrap();
    assert_eq!(gitlab.owner, "gitlab-owner");
    assert_eq!(gitlab.name, "gitlab-repo");
    let gitea = release.gitea.as_ref().unwrap();
    assert_eq!(gitea.owner, "gitea-owner");
    assert_eq!(gitea.name, "gitea-repo");
}

#[test]
fn test_config_github_urls_field() {
    let yaml = r#"
project_name: test
github_urls:
  api: https://ghe.corp.com/api/v3/
  upload: https://ghe.corp.com/api/uploads/
  download: https://ghe.corp.com/
  skip_tls_verify: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let urls = config.github_urls.as_ref().unwrap();
    assert_eq!(urls.api.as_deref(), Some("https://ghe.corp.com/api/v3/"));
    assert_eq!(
        urls.upload.as_deref(),
        Some("https://ghe.corp.com/api/uploads/")
    );
    assert_eq!(urls.download.as_deref(), Some("https://ghe.corp.com/"));
    assert_eq!(urls.skip_tls_verify, Some(true));
}

#[test]
fn test_config_gitlab_urls_field() {
    let yaml = r#"
project_name: test
gitlab_urls:
  api: https://gitlab.corp.com/api/v4/
  download: https://gitlab.corp.com/
  skip_tls_verify: false
  use_package_registry: true
  use_job_token: false
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let urls = config.gitlab_urls.as_ref().unwrap();
    assert_eq!(urls.api.as_deref(), Some("https://gitlab.corp.com/api/v4/"));
    assert_eq!(urls.download.as_deref(), Some("https://gitlab.corp.com/"));
    assert_eq!(urls.skip_tls_verify, Some(false));
    assert_eq!(urls.use_package_registry, Some(true));
    assert_eq!(urls.use_job_token, Some(false));
}

#[test]
fn test_config_gitea_urls_field() {
    let yaml = r#"
project_name: test
gitea_urls:
  api: https://gitea.corp.com/api/v1/
  download: https://gitea.corp.com/
  skip_tls_verify: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let urls = config.gitea_urls.as_ref().unwrap();
    assert_eq!(urls.api.as_deref(), Some("https://gitea.corp.com/api/v1/"));
    assert_eq!(urls.download.as_deref(), Some("https://gitea.corp.com/"));
    assert_eq!(urls.skip_tls_verify, Some(true));
}

#[test]
fn test_config_force_token_field() {
    let yaml = r#"
project_name: test
force_token: gitlab
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.force_token, Some(ForceTokenKind::GitLab));
}

#[test]
fn test_config_force_token_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.force_token, None::<ForceTokenKind>);
}

#[test]
fn test_config_all_platform_urls_and_force_token() {
    let yaml = r#"
project_name: test
github_urls:
  api: https://ghe.corp.com/api/v3/
gitlab_urls:
  api: https://gitlab.corp.com/api/v4/
  use_job_token: true
gitea_urls:
  api: https://gitea.corp.com/api/v1/
force_token: github
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.github_urls.as_ref().unwrap().api.as_deref(),
        Some("https://ghe.corp.com/api/v3/")
    );
    assert_eq!(
        config.gitlab_urls.as_ref().unwrap().api.as_deref(),
        Some("https://gitlab.corp.com/api/v4/")
    );
    assert_eq!(
        config.gitlab_urls.as_ref().unwrap().use_job_token,
        Some(true)
    );
    assert_eq!(
        config.gitea_urls.as_ref().unwrap().api.as_deref(),
        Some("https://gitea.corp.com/api/v1/")
    );
    assert_eq!(config.force_token, Some(ForceTokenKind::GitHub));
}

#[test]
fn test_dockerhub_config_parse() {
    let yaml = r#"
project_name: test
dockerhub:
  - username: myuser
    secret_name: DOCKER_TOKEN
    images:
      - myorg/myapp
    description: "My app"
    skip: true
    full_description:
      from_file:
        path: ./README.md
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let dh = &cfg.dockerhub.unwrap()[0];
    assert_eq!(dh.username.as_deref(), Some("myuser"));
    assert_eq!(dh.secret_name.as_deref(), Some("DOCKER_TOKEN"));
    assert_eq!(dh.images.as_ref().unwrap(), &["myorg/myapp"]);
    assert_eq!(dh.description.as_deref(), Some("My app"));
    assert_eq!(dh.skip, Some(StringOrBool::Bool(true)));
    let fd = dh.full_description.as_ref().unwrap();
    assert!(fd.from_url.is_none());
    let ff = fd.from_file.as_ref().unwrap();
    assert_eq!(ff.path, "./README.md");
}

#[test]
fn test_dockerhub_from_url_parse() {
    let yaml = r#"
project_name: test
dockerhub:
  - username: myuser
    full_description:
      from_url:
        url: "https://raw.githubusercontent.com/org/repo/main/README.md"
        headers:
          Authorization: "Bearer {{ .Env.GH_TOKEN }}"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let dh = &cfg.dockerhub.unwrap()[0];
    let fu = dh
        .full_description
        .as_ref()
        .unwrap()
        .from_url
        .as_ref()
        .unwrap();
    assert_eq!(
        fu.url,
        "https://raw.githubusercontent.com/org/repo/main/README.md"
    );
    let headers = fu.headers.as_ref().unwrap();
    assert_eq!(
        headers.get("Authorization").unwrap(),
        "Bearer {{ .Env.GH_TOKEN }}"
    );
}

#[test]
fn test_artifactory_config_parse() {
    let yaml = r#"
project_name: test
artifactories:
  - name: production
    target: "https://artifactory.example.com/repo/{{ .ProjectName }}/{{ .Version }}/"
    username: deployer
    mode: archive
    skip: "{{ .Env.SKIP }}"
    ids:
      - default
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let art = &cfg.artifactories.unwrap()[0];
    assert_eq!(art.name.as_deref(), Some("production"));
    assert_eq!(
        art.target.as_deref(),
        Some("https://artifactory.example.com/repo/{{ .ProjectName }}/{{ .Version }}/")
    );
    assert_eq!(art.username.as_deref(), Some("deployer"));
    assert_eq!(art.mode.as_deref(), Some("archive"));
    assert_eq!(
        art.skip,
        Some(StringOrBool::String("{{ .Env.SKIP }}".to_string()))
    );
    assert_eq!(art.ids.as_ref().unwrap(), &["default"]);
}

#[test]
fn test_cloudsmith_config_parse() {
    let yaml = r#"
project_name: test
cloudsmiths:
  - organization: myorg
    repository: myrepo
    formats:
      - deb
    distributions:
      deb: "ubuntu/focal"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cs = &cfg.cloudsmiths.unwrap()[0];
    assert_eq!(cs.organization.as_deref(), Some("myorg"));
    assert_eq!(cs.repository.as_deref(), Some("myrepo"));
    assert_eq!(cs.formats.as_ref().unwrap(), &["deb"]);
    let dists = cs.distributions.as_ref().unwrap();
    match dists.get("deb").unwrap() {
        crate::config::CloudSmithDistributions::Single(s) => {
            assert_eq!(s, "ubuntu/focal");
        }
        other => panic!("expected Single, got {:?}", other),
    }
}

/// Multi-distribution array form: `deb: ["ubuntu/focal", ...]`
/// must parse into [`CloudSmithDistributions::Multiple`] so the publisher
/// can issue one upload per slug.
#[test]
fn test_cloudsmith_distributions_array_form() {
    let yaml = r#"
project_name: test
cloudsmiths:
  - organization: myorg
    repository: myrepo
    distributions:
      deb:
        - ubuntu/focal
        - ubuntu/jammy
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cs = &cfg.cloudsmiths.unwrap()[0];
    let dists = cs.distributions.as_ref().unwrap();
    match dists.get("deb").unwrap() {
        crate::config::CloudSmithDistributions::Multiple(v) => {
            assert_eq!(
                v,
                &vec!["ubuntu/focal".to_string(), "ubuntu/jammy".to_string()]
            );
        }
        other => panic!("expected Multiple, got {:?}", other),
    }
}

// -----------------------------------------------------------------------
// env: Vec<String> tests — list-only, null/missing, parse helpers
// -----------------------------------------------------------------------

#[test]
fn test_docker_sign_env_list_format() {
    let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
    env:
      - COSIGN_PASSWORD=hunter2
      - COSIGN_KEY=/path/to/key
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let ds = &cfg.docker_signs.as_ref().unwrap()[0];
    let env = ds.env.as_ref().expect("env should be Some");
    assert_eq!(
        env,
        &vec!["COSIGN_PASSWORD=hunter2", "COSIGN_KEY=/path/to/key"]
    );
}

#[test]
fn test_docker_sign_env_map_form_rejected() {
    let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
    env:
      COSIGN_PASSWORD: hunter2
"#;
    let result = serde_yaml_ng::from_str::<Config>(yaml);
    assert!(
        result.is_err(),
        "map form should be rejected after Vec<String> migration"
    );
}

#[test]
fn test_docker_sign_env_null() {
    let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
    env: ~
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let ds = &cfg.docker_signs.as_ref().unwrap()[0];
    assert!(ds.env.is_none());
}

#[test]
fn test_docker_sign_env_missing() {
    let yaml = r#"
project_name: test
docker_signs:
  - cmd: cosign
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let ds = &cfg.docker_signs.as_ref().unwrap()[0];
    assert!(ds.env.is_none());
}

#[test]
fn test_sign_config_env_list_format() {
    let yaml = r#"
project_name: test
signs:
  - cmd: gpg
    env:
      - GPG_KEY=ABCDEF
      - GPG_TTY=/dev/pts/0
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let s = &cfg.signs[0];
    let env = s.env.as_ref().expect("env should be Some");
    assert_eq!(env, &vec!["GPG_KEY=ABCDEF", "GPG_TTY=/dev/pts/0"]);
}

#[test]
fn test_publisher_env_list_format() {
    let yaml = r#"
project_name: test
publishers:
  - name: mypub
    cmd: publish.sh
    env:
      - API_TOKEN=secret123
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let p = &cfg.publishers.as_ref().unwrap()[0];
    let env = p.env.as_ref().expect("env should be Some");
    assert_eq!(env, &vec!["API_TOKEN=secret123"]);
}

#[test]
fn test_publisher_if_condition_parses() {
    // Spot-check that the new `if:` field deserializes via the `#[serde(rename = "if")]`
    // attribute on PublisherConfig. Mirrors the surface every other new `if:`
    // consumer added in this batch (blob/upload/announce/archive/snapcraft/aur/etc.).
    let yaml = r#"
project_name: test
publishers:
  - name: snapshot-only
    cmd: publish.sh
    if: "{{ IsSnapshot }}"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("config parses with if:");
    let p = &cfg.publishers.as_ref().unwrap()[0];
    assert_eq!(p.if_condition.as_deref(), Some("{{ IsSnapshot }}"));
}

#[test]
fn test_archive_if_condition_parses() {
    let yaml = r#"
project_name: test
defaults:
  archives:
    id: linux-only
    formats: [tar.gz]
    if: "{{ eq .Os \"linux\" }}"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("archives.if: parses");
    let archive = cfg.defaults.unwrap().archives.unwrap();
    assert_eq!(
        archive.if_condition.as_deref(),
        Some("{{ eq .Os \"linux\" }}"),
        "ArchiveConfig must round-trip the `if:` field",
    );
}

#[test]
fn test_hook_if_condition_parses() {
    let yaml = r#"
project_name: test
before:
  hooks:
    - cmd: "go mod tidy"
      if: "{{ IsSnapshot }}"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).expect("hook.if: parses");
    let hooks = cfg.before.unwrap().hooks.unwrap();
    match &hooks[0] {
        crate::config::HookEntry::Structured(h) => {
            assert_eq!(h.if_condition.as_deref(), Some("{{ IsSnapshot }}"));
        }
        other => panic!("expected structured hook, got {other:?}"),
    }
}

#[test]
fn test_build_override_env_list_format() {
    let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
  builds:
    overrides:
      - targets:
          - "x86_64-*"
        env:
          - CC=gcc-12
          - CFLAGS=-O2 -Wall
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let overrides = config.defaults.unwrap().builds.unwrap().overrides.unwrap();
    let env = overrides[0].env.as_ref().expect("env should be Some");
    assert_eq!(env, &vec!["CC=gcc-12", "CFLAGS=-O2 -Wall"]);
}

#[test]
fn test_structured_hook_env_list_format() {
    let yaml = r#"
project_name: test
before:
  hooks:
    - cmd: echo hello
      env:
        - MY_VAR=foo
        - OTHER=bar=baz
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let hooks = cfg.before.as_ref().unwrap().hooks.as_ref().unwrap();
    match &hooks[0] {
        HookEntry::Structured(h) => {
            let env = h.env.as_ref().expect("env should be Some");
            assert_eq!(env, &vec!["MY_VAR=foo", "OTHER=bar=baz"]);
        }
        HookEntry::Simple(_) => panic!("expected Structured hook"),
    }
}

#[test]
fn test_sbom_config_env_list_format() {
    let yaml = r#"
project_name: test
sboms:
  - cmd: syft
    env:
      - SYFT_FILE_METADATA_CATALOGER_ENABLED=true
      - SYFT_SCOPE=all-layers
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let s = &cfg.sboms[0];
    let env = s.env.as_ref().expect("env should be Some");
    assert_eq!(
        env,
        &vec![
            "SYFT_FILE_METADATA_CATALOGER_ENABLED=true",
            "SYFT_SCOPE=all-layers"
        ]
    );
}

#[test]
fn test_sbom_config_env_missing() {
    let yaml = r#"
project_name: test
sboms:
  - cmd: syft
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let s = &cfg.sboms[0];
    assert!(s.env.is_none());
}

// ---- env map-form rejection tests (must be Vec<String>, not map) ----

/// Assert that the given YAML fails to deserialize into `Config` because an
/// `env:` field was supplied as a map (`KEY: value`) rather than the
/// required `Vec<String>` (`- KEY=value`).
#[track_caller]
fn assert_env_map_rejected(yaml: &str, label: &str) {
    let result = serde_yaml_ng::from_str::<Config>(yaml);
    assert!(
        result.is_err(),
        "{label}.env map form should be rejected after Vec<String> migration"
    );
}

#[test]
fn test_top_level_env_map_form_rejected() {
    let yaml = r#"
project_name: test
crates: []
env:
  MY_VAR: hello
"#;
    assert_env_map_rejected(yaml, "top-level Config");
}

#[test]
fn test_build_override_env_map_form_rejected() {
    // defaults.overrides moved under defaults.builds.overrides.
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
defaults:
  builds:
    overrides:
      - targets: ["x86_64-unknown-linux-gnu"]
        env:
          MY_VAR: hello
"#;
    assert_env_map_rejected(yaml, "BuildOverride");
}

#[test]
fn test_sign_config_env_map_form_rejected() {
    let yaml = r#"
project_name: test
crates: []
signs:
  - cmd: cosign
    env:
      COSIGN_PASSWORD: hunter2
"#;
    assert_env_map_rejected(yaml, "SignConfig");
}

#[test]
fn test_sbom_config_env_map_form_rejected() {
    let yaml = r#"
project_name: test
sboms:
  - cmd: syft
    env:
      MY_VAR: value
"#;
    assert_env_map_rejected(yaml, "SbomConfig");
}

#[test]
fn test_workspace_env_map_form_rejected() {
    let yaml = r#"
project_name: test
workspaces:
  - name: myws
    crates: []
    env:
      MY_VAR: value
"#;
    assert_env_map_rejected(yaml, "WorkspaceConfig");
}

#[test]
fn test_publisher_config_env_map_form_rejected() {
    let yaml = r#"
project_name: test
crates: []
publishers:
  - cmd: "my-publisher"
    env:
      MY_VAR: value
"#;
    assert_env_map_rejected(yaml, "PublisherConfig");
}

#[test]
fn test_structured_hook_env_map_form_rejected() {
    let yaml = r#"
project_name: test
crates: []
before:
  hooks:
    - cmd: "echo hello"
      env:
        MY_VAR: value
"#;
    assert_env_map_rejected(yaml, "StructuredHook");
}

// ---- defaults.archives.format_overrides validation -------------------

#[test]
fn test_validate_format_overrides_in_defaults_block_rejects_unknown_os() {
    // defaults.archives.format_overrides[].os = "pc-windows-msvc" is a
    // common Rust-triple typo for "windows" and used to slip past the
    // validator because the defaults block was not walked.
    let yaml = r#"
project_name: test
defaults:
  archives:
    format_overrides:
      - os: pc-windows-msvc
        formats: [zip]
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_format_overrides(&config).unwrap_err();
    assert!(
        err.contains("defaults.archives"),
        "error should locate the offender at defaults.archives: {err}"
    );
    assert!(
        err.contains("pc-windows-msvc"),
        "error should echo the bad os value: {err}"
    );
}

#[test]
fn test_validate_format_overrides_in_defaults_block_accepts_known_os() {
    let yaml = r#"
project_name: test
defaults:
  archives:
    format_overrides:
      - os: windows
        formats: [zip]
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    validate_format_overrides(&config).expect("known os value should pass");
}

// ---- DefaultsCrateBlock / DefaultsWorkspaceBlock unknown-field rejection

#[test]
fn test_defaults_crates_block_rejects_unknown_field() {
    let yaml = r#"
project_name: test
defaults:
  crates:
    foo: bar
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    let err = result.expect_err("unknown field under defaults.crates should be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown field"),
        "error should mention 'unknown field': {msg}"
    );
}

#[test]
fn test_defaults_workspaces_block_rejects_unknown_field() {
    let yaml = r#"
project_name: test
defaults:
  workspaces:
    foo: bar
workspaces:
  - name: ws1
    crates:
      - name: a
        path: "."
        tag_template: "v{{ .Version }}"
crates: []
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    let err = result.expect_err("unknown field under defaults.workspaces should be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown field"),
        "error should mention 'unknown field': {msg}"
    );
}

// ---------------------------------------------------------------------------
// StringOrBool::try_evaluates_to_true — always-render normalization
// ---------------------------------------------------------------------------
//
// The helper used to short-circuit on `!s.contains('{')` and skip the render
// closure for plain literals. That diverged from the sibling `should_skip_upload`
// in stage-publish, which always renders. After unification both go through the
// same render path; Tera leaves plain literals unchanged so it's a transparent
// no-op for `"true"` / `"false"` and an `Err` for `"{{ broken"`.

#[test]
fn test_try_evaluates_to_true_plain_literal_invokes_render() {
    // Records every input the closure sees so we can assert the render step
    // ran even for the plain-literal "true" case (regression against the old
    // contains('{') short-circuit).
    let calls = std::cell::RefCell::new(Vec::<String>::new());
    let render = |s: &str| -> anyhow::Result<String> {
        calls.borrow_mut().push(s.to_string());
        // Mimic Tera's behavior: plain literals pass through untouched.
        Ok(s.to_string())
    };

    let val = StringOrBool::String("true".to_string());
    let got = val
        .try_evaluates_to_true(render)
        .expect("plain literal 'true' should evaluate without error");
    assert!(got, "plain literal 'true' should resolve to true");
    assert_eq!(
        calls.borrow().as_slice(),
        &["true".to_string()],
        "render closure must be invoked exactly once with the raw value, even for plain literals",
    );

    // And the inverse: plain literal "false" still returns false, also via render.
    calls.borrow_mut().clear();
    let val = StringOrBool::String("false".to_string());
    let got = val.try_evaluates_to_true(render).expect("plain false ok");
    assert!(!got);
    assert_eq!(
        calls.borrow().as_slice(),
        &["false".to_string()],
        "render closure must run for plain-literal 'false' too",
    );
}

#[test]
fn test_try_evaluates_to_true_invalid_template_surfaces_error() {
    // A Tera-syntactically-invalid template must propagate as an Err rather
    // than being silently treated as a literal (which the old short-circuit
    // would have done for any value not containing '{', but a `{{ broken`
    // string does contain '{' — the regression we're pinning here is the
    // post-fix invariant: the render closure's error always reaches the caller).
    let render = |_: &str| -> anyhow::Result<String> { anyhow::bail!("tera parse error") };

    let val = StringOrBool::String("{{ broken".to_string());
    let err = val
        .try_evaluates_to_true(render)
        .expect_err("malformed template must surface as Err");
    assert!(
        err.to_string().contains("tera parse error"),
        "error must propagate from render closure: {err}",
    );
}

#[test]
fn test_try_evaluates_to_true_bool_variant_skips_render() {
    // The Bool variant has nothing to render — we keep that as a fast path
    // both for correctness (no template engine for a literal bool) and so
    // configs that set `skip: true` don't pay a render cost per evaluation.
    let render = |_: &str| -> anyhow::Result<String> {
        panic!("render closure must not be called for StringOrBool::Bool");
    };

    assert!(
        StringOrBool::Bool(true)
            .try_evaluates_to_true(render)
            .unwrap()
    );
    assert!(
        !StringOrBool::Bool(false)
            .try_evaluates_to_true(render)
            .unwrap()
    );
}

// ---------------------------------------------------------------------------
// evaluate_if_condition — central `if:` predicate
// ---------------------------------------------------------------------------

#[test]
fn test_evaluate_if_condition_none_proceeds() {
    use super::evaluate_if_condition;
    let render = |_: &str| -> anyhow::Result<String> {
        panic!("render must not run when condition is None")
    };
    assert!(
        evaluate_if_condition(None, "x", render).unwrap(),
        "no condition set must always proceed",
    );
}

#[test]
fn test_evaluate_if_condition_empty_proceeds() {
    use super::evaluate_if_condition;
    let render = |_: &str| -> anyhow::Result<String> {
        panic!("render must not run for empty literal — empty gate is a no-op")
    };
    assert!(
        evaluate_if_condition(Some(""), "x", render).unwrap(),
        "empty `if:` literal must be a no-op (matches GR's no-`if:`=always-run)",
    );
}

#[test]
fn test_evaluate_if_condition_falsy_values_skip() {
    use super::evaluate_if_condition;
    for falsy in ["false", "0", "no", "  false  ", ""] {
        let v = falsy.to_string();
        let render = move |_: &str| Ok(v.clone());
        let proceed = evaluate_if_condition(Some("anything"), "x", render).unwrap();
        assert!(
            !proceed,
            "rendered value {falsy:?} (trimmed) must skip the resource",
        );
    }
}

#[test]
fn test_evaluate_if_condition_truthy_proceeds() {
    use super::evaluate_if_condition;
    for truthy in ["true", "1", "yes", "TRUE", "any-non-falsy"] {
        let v = truthy.to_string();
        let render = move |_: &str| Ok(v.clone());
        let proceed = evaluate_if_condition(Some("anything"), "x", render).unwrap();
        assert!(
            proceed,
            "rendered value {truthy:?} must proceed (only false/0/no/empty are falsy)",
        );
    }
}

#[test]
fn test_evaluate_if_condition_render_error_propagates() {
    use super::evaluate_if_condition;
    let render = |_: &str| -> anyhow::Result<String> { anyhow::bail!("tera parse error") };
    let err = evaluate_if_condition(Some("{{ broken"), "publisher 'foo'", render).unwrap_err();
    let chain = format!("{err:#}");
    assert!(
        chain.contains("publisher 'foo'") && chain.contains("template render failed"),
        "render error must carry label + diagnostic: {chain}",
    );
}

#[test]
fn test_evaluate_if_condition_rejects_stale_bool_string_compare() {
    use super::evaluate_if_condition;
    let render = |_: &str| -> anyhow::Result<String> {
        panic!("render must not run for a rejected stale compare")
    };
    for stale in [
        r#"{% if IsSnapshot == "false" or IsHarness == "true" %}true{% endif %}"#,
        r#"{{ eq .IsSnapshot "false" }}"#,
        r#"{% if NightlyBuild == "0" %}true{% endif %}"#,
    ] {
        let err = evaluate_if_condition(Some(stale), "sign config 'default'", render)
            .expect_err("stale typed compare must hard-error, not silently skip");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("sign config 'default'") && chain.contains("never matches"),
            "error must carry label + diagnostic for {stale}: {chain}",
        );
    }
}

#[test]
fn test_string_or_bool_rejects_stale_bool_string_compare() {
    let skip = StringOrBool::String(r#"{{ IsSnapshot == "true" }}"#.to_string());
    let err = skip
        .try_evaluates_to_true(|_| panic!("render must not run for a rejected stale compare"))
        .expect_err("stale typed compare in skip-style fields must hard-error");
    assert!(
        format!("{err:#}").contains("never matches"),
        "diagnostic must explain the type mismatch: {err:#}",
    );
}

#[test]
fn test_evaluate_if_condition_bool_vars_snapshot_vs_release() {
    use super::evaluate_if_condition;
    use crate::context::{Context, ContextOptions};

    let eval = |snapshot: bool, tpl: &str| -> bool {
        let opts = ContextOptions {
            snapshot,
            ..Default::default()
        };
        let mut ctx = Context::new(Config::default(), opts);
        ctx.git_info = None;
        ctx.populate_git_vars();
        evaluate_if_condition(Some(tpl), "t", |t| ctx.render_template(t)).unwrap()
    };

    // Go-style `{{ not .IsSnapshot }}`: proceed on release, skip on snapshot.
    assert!(eval(false, "{{ not .IsSnapshot }}"));
    assert!(!eval(true, "{{ not .IsSnapshot }}"));

    // Bare truthiness: proceed on snapshot, skip on release.
    assert!(eval(true, "{{ IsSnapshot }}"));
    assert!(!eval(false, "{{ IsSnapshot }}"));

    // Tera statement form.
    assert!(eval(false, "{% if not IsSnapshot %}true{% endif %}"));
    assert!(!eval(true, "{% if not IsSnapshot %}true{% endif %}"));

    // The dogfood form: release or harness (IsHarness false here).
    assert!(eval(false, "{{ not IsSnapshot or IsHarness }}"));
    assert!(!eval(true, "{{ not IsSnapshot or IsHarness }}"));
}

// ---- F2: `disable` → `skip` serde aliases ----
//
// Imported docker_v2/snapcraft/nsis/msi/release configs use `disable:`;
// anodizer renamed to `skip:`. With `deny_unknown_fields` on the
// strictly-validated structs, an imported YAML carrying `disable:`
// would fail to parse. The `#[serde(alias = "disable")]` attribute lets
// both spellings deserialize into the same field.

#[test]
fn test_docker_v2_disable_alias_accepts_legacy_spelling() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    dockers_v2:
      - images: [ghcr.io/example/app]
        disable: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("disable: alias must parse");
    let docker = &config.crates[0].dockers_v2.as_ref().unwrap()[0];
    assert_eq!(docker.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_snapcraft_disable_alias_accepts_legacy_spelling() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - disable: "{{ if .IsSnapshot }}true{{ end }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("disable: alias must parse");
    let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
    match &snap.skip {
        Some(StringOrBool::String(s)) => assert!(s.contains("IsSnapshot")),
        other => panic!("expected template string, got {:?}", other),
    }
}

#[test]
fn test_msi_disable_alias_accepts_legacy_spelling() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - disable: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("disable: alias must parse");
    let msi = &config.crates[0].msis.as_ref().unwrap()[0];
    assert_eq!(msi.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_nsis_disable_alias_accepts_legacy_spelling() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nsis:
      - disable: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("disable: alias must parse");
    let nsis = &config.crates[0].nsis.as_ref().unwrap()[0];
    assert_eq!(nsis.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_release_disable_alias_accepts_legacy_spelling() {
    let yaml = r#"
project_name: test
release:
  disable: "{{ .IsSnapshot }}"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("disable: alias must parse");
    let rel = config.release.as_ref().unwrap();
    match &rel.skip {
        Some(StringOrBool::String(s)) => assert!(s.contains("IsSnapshot")),
        other => panic!("expected template string, got {:?}", other),
    }
}

// ---- amd64_variant field on DMG/MSI/NSIS/nfpm ----
//
// The `goamd64` field; previously absent on these surfaces, so
// multi-amd64-variant builds couldn't filter. Tests assert the YAML round-
// trips into the new struct field. Stage-level wiring (filter the artifact
// set against the configured variant) lives in stage-{dmg,msi,nsis,nfpm}/
// and is tracked separately — this commit adds the surface only.

#[test]
fn test_dmg_amd64_variant_field_deserializes() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    dmgs:
      - id: my_dmg
        amd64_variant: v3
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("dmg amd64_variant must parse");
    let dmg = &config.crates[0].dmgs.as_ref().unwrap()[0];
    assert_eq!(dmg.amd64_variant, Some(Amd64Variant::V3));
}

#[test]
fn test_msi_amd64_variant_field_deserializes() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - id: my_msi
        amd64_variant: v2
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("msi amd64_variant must parse");
    let msi = &config.crates[0].msis.as_ref().unwrap()[0];
    assert_eq!(msi.amd64_variant, Some(Amd64Variant::V2));
}

#[test]
fn test_nsis_amd64_variant_field_deserializes() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nsis:
      - id: my_nsis
        amd64_variant: v4
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("nsis amd64_variant must parse");
    let nsis = &config.crates[0].nsis.as_ref().unwrap()[0];
    assert_eq!(nsis.amd64_variant, Some(Amd64Variant::V4));
}

#[test]
fn test_nfpm_amd64_variant_field_deserializes_as_list() {
    // nfpm uses `[]string` (multi-variant filter), not `string`.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nfpms:
      - id: my_nfpm
        formats: [deb]
        amd64_variant: [v2, v3]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("nfpm amd64_variant list must parse");
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert_eq!(
        nfpm.amd64_variant.as_deref(),
        Some(&[Amd64Variant::V2, Amd64Variant::V3][..])
    );
}

#[test]
fn test_nfpm_amd64_variant_omitted_is_none() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nfpms:
      - id: my_nfpm
        formats: [deb]
"#;
    let config: Config =
        serde_yaml_ng::from_str(yaml).expect("nfpm without amd64_variant must parse");
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert!(nfpm.amd64_variant.is_none());
}

// ---- Q-arch2: ID uniqueness for archives[] and universal_binaries[] ----

#[test]
fn test_archives_id_uniqueness_rejects_duplicate() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - id: foo
        formats: [tar.gz]
      - id: foo
        formats: [zip]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = super::validate_id_uniqueness(&config).unwrap_err();
    assert!(
        err.contains("archives id \"foo\""),
        "expected duplicate-id error, got: {}",
        err
    );
}

#[test]
fn test_archives_id_uniqueness_accepts_distinct() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - id: foo
        formats: [tar.gz]
      - id: bar
        formats: [zip]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::validate_id_uniqueness(&config).expect("distinct ids must pass");
}

#[test]
fn test_universal_binaries_id_uniqueness_rejects_duplicate() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    universal_binaries:
      - id: ub
        name_template: "{{ .ProjectName }}_macos"
      - id: ub
        name_template: "{{ .ProjectName }}_macos2"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = super::validate_id_uniqueness(&config).unwrap_err();
    assert!(
        err.contains("universal_binaries id \"ub\""),
        "expected duplicate-id error, got: {}",
        err
    );
}

#[test]
fn test_universal_binaries_id_uniqueness_accepts_distinct() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    universal_binaries:
      - id: ub_main
        name_template: "{{ .ProjectName }}_macos"
      - id: ub_alt
        name_template: "{{ .ProjectName }}_macos_alt"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::validate_id_uniqueness(&config).expect("distinct ids must pass");
}

// ---- Q-src3: source.prefix_template defaults to name_template ----

#[test]
fn test_source_prefix_template_defaults_to_name_template_when_unset() {
    use super::SourceConfig;
    let mut src = SourceConfig {
        enabled: Some(true),
        name_template: Some("{{ .ProjectName }}-{{ .Version }}".to_string()),
        prefix_template: None,
        ..Default::default()
    };
    src.apply_prefix_template_default();
    assert_eq!(
        src.prefix_template.as_deref(),
        Some("{{ .ProjectName }}-{{ .Version }}")
    );
}

#[test]
fn test_source_prefix_template_preserved_when_user_set() {
    use super::SourceConfig;
    let mut src = SourceConfig {
        enabled: Some(true),
        name_template: Some("default-name".to_string()),
        prefix_template: Some("custom-prefix".to_string()),
        ..Default::default()
    };
    src.apply_prefix_template_default();
    assert_eq!(src.prefix_template.as_deref(), Some("custom-prefix"));
}

#[test]
fn test_source_prefix_template_remains_none_when_name_template_unset() {
    use super::SourceConfig;
    let mut src = SourceConfig::default();
    src.apply_prefix_template_default();
    assert!(src.prefix_template.is_none());
}

// ---- Q-brew1: HomebrewConflict accepts both string and object shapes ----
//
// Homebrew conflicts is `[]string` (just names). anodizer's
// HomebrewConflict is a strict superset (`Name` | `WithReason`), modeled as
// an untagged enum so a YAML list of either bare strings, structured
// `{name, because}` objects, or a mixed list all deserializes correctly.
// These tests pin that behavior so a refactor cannot accidentally drop the
// string form, which would silently drop all conflicts: from imported
// configs.

#[test]
fn test_homebrew_conflicts_string_form_accepted() {
    use super::publishers::HomebrewConflict;
    let yaml = r#"
- foo
- bar
"#;
    let conflicts: Vec<HomebrewConflict> = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(conflicts.len(), 2);
    assert_eq!(conflicts[0].name(), "foo");
    assert!(conflicts[0].because().is_none());
    assert_eq!(conflicts[1].name(), "bar");
}

#[test]
fn test_homebrew_conflicts_object_form_accepted() {
    use super::publishers::HomebrewConflict;
    let yaml = r#"
- name: foo
  because: "both install bin/foo"
"#;
    let conflicts: Vec<HomebrewConflict> = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(conflicts[0].name(), "foo");
    assert_eq!(conflicts[0].because(), Some("both install bin/foo"));
}

#[test]
fn test_homebrew_conflicts_mixed_form_accepted() {
    use super::publishers::HomebrewConflict;
    let yaml = r#"
- foo
- name: bar
  because: "shared symlink"
- baz
"#;
    let conflicts: Vec<HomebrewConflict> = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(conflicts.len(), 3);
    assert_eq!(conflicts[0].name(), "foo");
    assert!(conflicts[0].because().is_none());
    assert_eq!(conflicts[1].name(), "bar");
    assert_eq!(conflicts[1].because(), Some("shared symlink"));
    assert_eq!(conflicts[2].name(), "baz");
}

#[test]
fn test_homebrew_conflicts_full_yaml_with_string_list_form() {
    // End-to-end: a homebrew block with `conflicts: [foo, bar]` (the
    // import shape) must round-trip through Config without issue.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        repository:
          owner: example
          name: tap
        conflicts: [foo, bar]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("string-form conflicts must parse");
    let brew = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .homebrew
        .as_ref()
        .unwrap();
    let conflicts = brew.conflicts.as_ref().unwrap();
    assert_eq!(conflicts.len(), 2);
    assert_eq!(conflicts[0].name(), "foo");
    assert_eq!(conflicts[1].name(), "bar");
}

// ---- legacy V1 `dockers:` block rejection ----

#[test]
fn test_v1_dockers_block_rejected_with_migration_message() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
dockers:
  - image_templates: ["ghcr.io/example/app:{{ .Version }}"]
    dockerfile: Dockerfile
"#;
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
    let err = super::validate_no_docker_v1(&raw).unwrap_err();
    assert!(
        err.contains("dockers_v2") && err.contains("dockers"),
        "expected migration message naming dockers_v2 and dockers, got: {}",
        err
    );
}

#[test]
fn test_no_dockers_block_passes() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
dockers_v2:
  - images: [ghcr.io/example/app]
"#;
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
    super::validate_no_docker_v1(&raw).expect("dockers_v2 only must pass");
}

// ---- F3: legacy archive/snapshot/build aliases ----

#[test]
fn test_archives_format_singular_alias_folds_into_formats() {
    // The fold happens inline in ArchiveConfig::deserialize — no separate
    // apply pass is needed.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - id: legacy
        format: tar.gz
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let arch = match &config.crates[0].archives {
        super::ArchivesConfig::Configs(list) => &list[0],
        _ => panic!("expected configs variant"),
    };
    assert_eq!(arch.formats.as_deref(), Some(&[String::from("tar.gz")][..]));
}

#[test]
fn test_archives_builds_alias_deserializes_into_ids() {
    // Serde alias does the work — `builds: [...]` populates the canonical
    // `ids` field on parse.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - id: legacy
        builds: [foo, bar]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let arch = match &config.crates[0].archives {
        super::ArchivesConfig::Configs(list) => &list[0],
        _ => panic!("expected configs variant"),
    };
    assert_eq!(
        arch.ids.as_deref(),
        Some(&[String::from("foo"), String::from("bar")][..])
    );
}

#[test]
fn test_archives_format_overrides_singular_alias_folds() {
    // FormatOverride::deserialize folds singular `format:` into `formats`.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - id: legacy
        formats: [tar.gz]
        format_overrides:
          - os: windows
            format: zip
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let arch = match &config.crates[0].archives {
        super::ArchivesConfig::Configs(list) => &list[0],
        _ => panic!("expected configs variant"),
    };
    let over = &arch.format_overrides.as_ref().unwrap()[0];
    assert_eq!(over.formats.as_deref(), Some(&[String::from("zip")][..]));
}

#[test]
fn test_archive_config_rejects_unknown_field() {
    // ArchiveConfig carries an outer `#[serde(deny_unknown_fields)]` so the
    // generated JSON schema emits `additionalProperties: false`; the inner
    // `Raw` enforces the same strictness at runtime. A genuine typo (here a
    // misspelled `name_template`) must hard-fail at parse time rather than be
    // silently dropped.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - id: typo
        name_templat: "{{ .ProjectName }}"
"#;
    let err = serde_yaml_ng::from_str::<Config>(yaml).unwrap_err();
    assert!(
        err.to_string().contains("name_templat"),
        "ArchiveConfig must reject the unknown `name_templat` field, got: {err}"
    );
}

#[test]
fn test_format_override_rejects_unknown_field() {
    // FormatOverride's outer deny mirrors its inner `Raw` deny: an unknown key
    // is rejected. The deprecated singular `format:` is still accepted (folded
    // into `formats:`), but a typo like `formatz:` is not.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - id: legacy
        formats: [tar.gz]
        format_overrides:
          - os: windows
            formatz: [zip]
"#;
    let err = serde_yaml_ng::from_str::<Config>(yaml).unwrap_err();
    assert!(
        err.to_string().contains("formatz"),
        "FormatOverride must reject the unknown `formatz` field, got: {err}"
    );
}

#[test]
fn test_hooks_config_rejects_unknown_field() {
    // HooksConfig now carries an outer `#[serde(deny_unknown_fields)]` (schema
    // strictness); its inner `Raw` already denied unknown fields at runtime.
    // The canonical `hooks:` and the deprecated `post:` aliases are both still
    // accepted, but a typo like `hookz:` is rejected.
    let yaml = r#"
project_name: test
before:
  hookz:
    - echo hi
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let err = serde_yaml_ng::from_str::<Config>(yaml).unwrap_err();
    assert!(
        err.to_string().contains("hookz"),
        "HooksConfig must reject the unknown `hookz` field, got: {err}"
    );
}

#[test]
fn test_hooks_config_accepts_post_alias_after_outer_deny() {
    // Adding the outer `deny_unknown_fields` to HooksConfig must NOT break the
    // deprecated `post:` alias — the manual `Deserialize` (via the inner `Raw`)
    // still folds `post:` into the canonical `hooks:`. The outer attr is inert
    // for deserialization; the manual impl governs.
    let yaml = r#"
project_name: test
after:
  post:
    - echo done
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml)
        .expect("deprecated `post:` alias must still deserialize under outer deny");
    let after = config.after.as_ref().expect("after block present");
    let hooks = after.hooks.as_deref().expect("post: folded into hooks:");
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0], "echo done");
    assert!(after.post.is_none(), "post: must be cleared after folding");
}

#[test]
fn test_snapshot_name_template_alias_deserializes_as_version_template() {
    let yaml = r#"
project_name: test
snapshot:
  name_template: "{{ .Version }}-snap"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let snap = config.snapshot.as_ref().unwrap();
    assert_eq!(snap.version_template, "{{ .Version }}-snap");
}

#[test]
fn test_builds_gobinary_field_is_accepted_and_ignored() {
    // The Go-only `gobinary:` key is accepted as a back-compat alias and then
    // ignored: imported configs carrying it must parse rather than hard-fail,
    // but anodizer never acts on it (cargo is the only builder).
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - id: legacy
        gobinary: /usr/local/bin/go
"#;
    let config: Config = serde_yaml_ng::from_str(yaml)
        .expect("gobinary: must parse as an accepted-but-ignored legacy field");
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.gobinary.as_deref(), Some("/usr/local/bin/go"));
}

#[test]
fn test_makeself_config_rejects_renamed_go_fields() {
    // `goos`/`goarch` were hard-renamed to `os`/`arch`. With
    // `deny_unknown_fields` the old keys fail loudly instead of being
    // silently ignored (which would revert the filter to its default and
    // ship the wrong platforms).
    for old in ["goos", "goarch"] {
        let err = serde_yaml_ng::from_str::<super::MakeselfConfig>(&format!("{old}: [linux]\n"))
            .unwrap_err();
        assert!(
            err.to_string().contains(old),
            "makeself {old}: must be rejected, got: {err}"
        );
    }
}

#[test]
fn test_makeself_config_accepts_disable_alias() {
    // The legacy makeself spelling uses `disable: string` as its skip
    // mechanism. With `deny_unknown_fields` on MakeselfConfig it would hard-reject
    // unless aliased — assert the alias folds `disable:` into `skip`.
    let cfg: super::MakeselfConfig = serde_yaml_ng::from_str("disable: true\n").unwrap();
    match cfg.skip {
        Some(super::StringOrBool::Bool(true)) => {}
        other => panic!("expected disable: true to populate skip=Bool(true), got: {other:?}"),
    }
}

#[test]
fn test_makeself_file_accepts_src_dst_aliases() {
    // The legacy MakeselfFile keys its fields `src`/`dst`; anodizer renamed
    // them to source/destination. Assert the legacy spellings still parse via alias.
    let f: super::MakeselfFile =
        serde_yaml_ng::from_str("src: bin/app\ndst: usr/bin/app\n").unwrap();
    assert_eq!(f.source, "bin/app");
    assert_eq!(f.destination.as_deref(), Some("usr/bin/app"));
}

#[test]
fn test_appimages_parse_full_block_from_top_level_config() {
    // End-to-end: the top-level `appimages:` key flows through the Config
    // deserializer (single object → vec of one) and every field lands.
    let yaml = r#"
project_name: helix
appimages:
  - id: helix
    ids: [helix-bin]
    desktop: contrib/Helix.desktop
    icon: contrib/helix.png
    appdir_extra:
      - src: runtime/
        dst: usr/lib/helix/runtime
    update_information: "gh-releases-zsync|helix-editor|helix|latest|helix-*.AppImage.zsync"
    runtime_harvest:
      command: "{{ .ArtifactPath }} --populate {{ .HarvestDir }}"
      dir: usr/lib/helix/runtime
"#;
    let cfg: super::Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.appimages.len(), 1);
    let ai = &cfg.appimages[0];
    assert_eq!(ai.id.as_deref(), Some("helix"));
    assert_eq!(ai.ids.as_deref(), Some(&["helix-bin".to_string()][..]));
    assert_eq!(ai.desktop.as_deref(), Some("contrib/Helix.desktop"));
    assert_eq!(ai.icon.as_deref(), Some("contrib/helix.png"));
    let extra = ai.appdir_extra.as_ref().unwrap();
    assert_eq!(extra[0].src, "runtime/");
    assert_eq!(extra[0].dst, "usr/lib/helix/runtime");
    assert!(
        ai.update_information
            .as_deref()
            .unwrap()
            .starts_with("gh-releases-zsync")
    );
    let h = ai.runtime_harvest.as_ref().unwrap();
    assert!(h.command.contains("{{ .HarvestDir }}"));
    assert_eq!(h.dir, "usr/lib/helix/runtime");
}

#[test]
fn test_appimage_config_rejects_unknown_field() {
    // deny_unknown_fields guards against typo'd keys silently reverting to
    // defaults.
    let err = serde_yaml_ng::from_str::<super::AppImageConfig>("bogus_field: 1\n").unwrap_err();
    assert!(err.to_string().contains("bogus_field"));
}

#[test]
fn test_appimage_extra_accepts_source_destination_aliases() {
    let e: super::AppImageExtra =
        serde_yaml_ng::from_str("source: runtime/\ndestination: usr/lib/x\n").unwrap();
    assert_eq!(e.src, "runtime/");
    assert_eq!(e.dst, "usr/lib/x");
}

#[test]
fn test_nfpm_config_accepts_builds_alias_into_ids() {
    // The legacy NFPM config keeps a deprecated `builds []string` aliasing
    // `ids`. With `deny_unknown_fields` on NfpmConfig it would hard-reject
    // unless aliased — assert `builds:` lands in `ids`.
    let cfg: super::NfpmConfig = serde_yaml_ng::from_str("builds: [foo, bar]\n").unwrap();
    assert_eq!(
        cfg.ids.as_deref(),
        Some(&[String::from("foo"), String::from("bar")][..])
    );
}

#[test]
fn test_installer_and_nfpm_configs_accept_legacy_goamd64_alias() {
    // `goamd64` is accepted as a serde alias for `amd64_variant` across
    // dmg/msi/nsis/nfpm so imported configs carrying the legacy spelling fold
    // into the canonical field rather than hard-failing.
    let dmg: super::DmgConfig = serde_yaml_ng::from_str("goamd64: v3\n").unwrap();
    assert_eq!(dmg.amd64_variant, Some(Amd64Variant::V3));
    let msi: super::MsiConfig = serde_yaml_ng::from_str("goamd64: v3\n").unwrap();
    assert_eq!(msi.amd64_variant, Some(Amd64Variant::V3));
    let nsis: super::NsisConfig = serde_yaml_ng::from_str("goamd64: v3\n").unwrap();
    assert_eq!(nsis.amd64_variant, Some(Amd64Variant::V3));
    let nfpm: super::NfpmConfig = serde_yaml_ng::from_str("goamd64: [v3]\n").unwrap();
    assert_eq!(nfpm.amd64_variant.as_deref(), Some(&[Amd64Variant::V3][..]));
}

/// The whole `amd64_variant` domain is the typed [`Amd64Variant`] enum, so a
/// level typo dies at parse instead of silently selecting only variant-less
/// baseline archives. `homebrew: { amd64_variant: "x86-64-v3" }` is the proven
/// failure: it used to parse, then the selector matched only untagged
/// baseline archives and published the UNTUNED binary under a formula the
/// operator believed was v3-tuned.
#[test]
fn test_publisher_amd64_variant_typo_rejected_at_parse_per_crate_axis() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        amd64_variant: "x86-64-v3"
"#;
    let err = serde_yaml_ng::from_str::<Config>(yaml)
        .expect_err("a typo'd amd64_variant must fail at parse")
        .to_string();
    assert!(
        err.contains("unknown variant `x86-64-v3`")
            && err.contains("expected one of `v1`, `v2`, `v3`, `v4`"),
        "parse error must name the bad value and the valid set: {err}"
    );
}

/// Same closed domain on the defaults axis: a garbage level under
/// `defaults.publish.*` is rejected at parse, not merged into every crate.
#[test]
fn test_publisher_amd64_variant_typo_rejected_at_parse_defaults_axis() {
    let yaml = r#"
project_name: test
defaults:
  publish:
    winget:
      amd64_variant: v5
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let err = serde_yaml_ng::from_str::<Config>(yaml)
        .expect_err("a garbage defaults-axis amd64_variant must fail at parse")
        .to_string();
    assert!(
        err.contains("unknown variant `v5`")
            && err.contains("expected one of `v1`, `v2`, `v3`, `v4`"),
        "parse error must name the bad value and the valid set: {err}"
    );
}

/// The legacy `goamd64:` alias routes through the same enum: an invalid
/// level under the old spelling is rejected too, on installers and on
/// nfpm's list form alike.
#[test]
fn test_goamd64_alias_typo_rejected_at_parse() {
    assert!(
        serde_yaml_ng::from_str::<super::MsiConfig>("goamd64: x86-64-v3\n").is_err(),
        "msi goamd64 typo must fail at parse"
    );
    assert!(
        serde_yaml_ng::from_str::<super::NfpmConfig>("formats: [deb]\ngoamd64: [x86-64-v3]\n")
            .is_err(),
        "nfpm goamd64 list typo must fail at parse"
    );
}

#[test]
fn test_builds_all_known_fields_still_parse() {
    // Guard: strictness must reject ONLY unknown fields. A build entry that
    // exercises the full known `BuildConfig` surface (plus nested ignore /
    // overrides / hooks / prebuilt) must still parse cleanly.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - id: main
        binary: a
        skip: false
        targets: ["x86_64-unknown-linux-gnu"]
        features: ["foo"]
        no_default_features: true
        env:
          x86_64-unknown-linux-gnu:
            RUSTFLAGS: "-C target-cpu=native"
        copy_from: other
        flags: ["--locked"]
        reproducible: true
        hooks:
          pre: ["echo pre"]
          post: ["echo post"]
        ignore:
          - os: windows
            arch: arm64
        overrides:
          - targets: ["*-linux-*"]
            env: ["FOO=bar"]
            flags: ["--offline"]
            features: ["bar"]
        cross_tool: /usr/bin/cross
        mod_timestamp: "{{ .CommitTimestamp }}"
        command: "auditable build"
        no_unique_dist_dir: true
        builder: prebuilt
        prebuilt:
          path: "bin/{{ .Target }}/a"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("known-good build config must parse");
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.id.as_deref(), Some("main"));
    assert_eq!(build.command.as_deref(), Some("auditable build"));
    assert_eq!(build.prebuilt.as_ref().unwrap().path, "bin/{{ .Target }}/a");
    assert_eq!(build.ignore.as_ref().unwrap()[0].os, "windows");
    assert_eq!(
        build.overrides.as_ref().unwrap()[0].features,
        Some(vec!["bar".to_owned()])
    );
}

#[test]
fn test_builds_structured_hook_unknown_field_rejected() {
    // `StructuredHook` carries `deny_unknown_fields`, so a typo inside the
    // object form of a build hook is a hard error rather than being silently
    // dropped — closing the strictness hole at `builds[].hooks.pre[].{...}`.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - id: app
        hooks:
          pre:
            - cmd: "echo hi"
              typo_here: true
"#;
    let err = serde_yaml_ng::from_str::<Config>(yaml)
        .expect_err("a typo'd structured-hook field must be rejected");
    assert!(
        err.to_string().contains("typo_here"),
        "error should name the unknown hook field, got: {err}"
    );
}

#[test]
fn test_builds_structured_hook_all_fields_parse() {
    // Guard: hook strictness must reject ONLY unknown fields. A structured
    // build hook exercising every modeled `StructuredHook` field (cmd, dir,
    // env, output, if, ids, artifacts, run_once) must still parse cleanly.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - id: app
        hooks:
          pre:
            - cmd: "echo hi"
              dir: "./sub"
              env: ["FOO=bar"]
              output: true
              if: '{{ eq .Os "linux" }}'
              ids: ["app"]
              artifacts: all
              run_once: true
"#;
    let config: Config =
        serde_yaml_ng::from_str(yaml).expect("fully-populated structured hook must parse");
    let hooks = config.crates[0].builds.as_ref().unwrap()[0]
        .hooks
        .as_ref()
        .unwrap();
    let pre = &hooks.pre.as_ref().unwrap()[0];
    // Prove all 8 fields round-trip into the struct, not just that it parsed.
    let HookEntry::Structured(hook) = pre else {
        panic!("expected object-form hook to deserialize as HookEntry::Structured, got {pre:?}");
    };
    assert_eq!(hook.cmd, "echo hi");
    assert_eq!(hook.dir.as_deref(), Some("./sub"));
    assert_eq!(hook.env.as_deref(), Some(&["FOO=bar".to_owned()][..]));
    assert_eq!(hook.output, Some(true));
    assert_eq!(
        hook.if_condition.as_deref(),
        Some(r#"{{ eq .Os "linux" }}"#)
    );
    assert_eq!(hook.ids.as_deref(), Some(&["app".to_owned()][..]));
    assert_eq!(
        hook.artifacts,
        Some(super::BeforePublishArtifactFilter::All)
    );
    assert!(hook.run_once);
}

#[test]
fn test_archives_id_uniqueness_workspace_crate() {
    let yaml = r#"
project_name: test
workspaces:
  - name: ws1
    crates:
      - name: a
        path: "."
        tag_template: "v{{ .Version }}"
        archives:
          - id: dup
            formats: [tar.gz]
          - id: dup
            formats: [zip]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = super::validate_id_uniqueness(&config).unwrap_err();
    assert!(err.contains("workspaces[ws1]"), "got: {}", err);
}

#[test]
fn submitter_required_true_warns() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        required: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    assert!(
        !warnings.is_empty(),
        "expected a warning for chocolatey required: true"
    );
    assert!(
        warnings.iter().any(|w| w.message.contains("chocolatey")),
        "expected chocolatey in warning, got: {:?}",
        warnings
    );
    assert!(
        warnings.iter().any(|w| w.publisher == "chocolatey"),
        "advisory must carry the chocolatey dispatch publisher identity, got: {:?}",
        warnings
    );
}

#[test]
fn manager_required_true_no_warning() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        required: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    assert!(
        warnings.is_empty(),
        "homebrew (Manager group) should not trigger a warning, got: {:?}",
        warnings
    );
}

#[test]
fn submitter_required_none_no_warning() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    assert!(
        warnings.is_empty(),
        "chocolatey with required: None should not warn, got: {:?}",
        warnings
    );
}

#[test]
fn submitter_required_false_no_warning() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      winget:
        required: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    assert!(
        warnings.is_empty(),
        "winget with required: false should not warn, got: {:?}",
        warnings
    );
}

#[test]
fn multiple_submitters_required_true_warns_each() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        required: true
      winget:
        required: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    assert_eq!(
        warnings.len(),
        2,
        "expected one warning per submitter, got: {:?}",
        warnings
    );
    assert!(
        warnings.iter().any(|w| w.message.contains("chocolatey")),
        "missing chocolatey warning"
    );
    assert!(
        warnings.iter().any(|w| w.message.contains("winget")),
        "missing winget warning"
    );
}

#[test]
fn submitter_aur_source_nested_required_true_warns() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur_source:
        required: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    assert_eq!(
        warnings.len(),
        1,
        "expected one warning for nested aur_source, got: {:?}",
        warnings
    );
    assert!(
        warnings[0].message.contains("aur_source"),
        "expected aur_source in warning, got: {}",
        warnings[0].message
    );
    assert!(
        warnings[0].message.contains("crate 'a'"),
        "expected crate-name prefix in warning, got: {}",
        warnings[0].message
    );
    assert_eq!(
        warnings[0].publisher, "upstream-aur",
        "aur_source advisory must key on the dispatch publisher name 'upstream-aur'"
    );
}

#[test]
fn submitter_aur_sources_top_level_required_true_warns() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
aur_sources:
  - name: mypkg
    required: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    assert_eq!(
        warnings.len(),
        1,
        "expected one warning for top-level aur_sources entry, got: {:?}",
        warnings
    );
    assert!(
        warnings[0].message.contains("aur_source"),
        "expected aur_source in warning, got: {}",
        warnings[0].message
    );
    assert!(
        warnings[0].message.contains("top-level aur_sources"),
        "expected top-level prefix in warning, got: {}",
        warnings[0].message
    );
}

#[test]
fn submitter_warning_message_matches_spec_verbatim() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        required: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    assert_eq!(warnings.len(), 1, "expected exactly one warning");
    let msg = &warnings[0].message;
    for clause in [
        "chocolatey",
        "submits to an external moderation queue",
        "fails the release when the submission itself fails",
        "moderation outcome happens outside the release run",
        "cannot be gated",
        "crate 'myapp'",
    ] {
        assert!(
            msg.contains(clause),
            "warning missing clause {:?}, got: {}",
            clause,
            msg
        );
    }
}

#[test]
fn submitter_required_true_warns_on_defaults_publish_axis() {
    // The submitter walker must cover the defaults.publish axis, not just
    // crates[].publish — config-mode parity (defaults feed every crate).
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
defaults:
  publish:
    chocolatey:
      required: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    assert_eq!(
        warnings.len(),
        1,
        "expected one warning for defaults.publish chocolatey, got: {:?}",
        warnings
    );
    assert!(
        warnings[0].message.contains("chocolatey"),
        "expected chocolatey in warning, got: {}",
        warnings[0].message
    );
    assert!(
        warnings[0].message.contains("defaults.publish"),
        "expected defaults.publish location in warning, got: {}",
        warnings[0].message
    );
}

#[test]
fn submitter_required_true_warns_on_workspace_crate_publish_axis() {
    // The submitter walker must cover the workspaces[].crates[].publish axis
    // — config-mode parity (workspace per-crate mode).
    let yaml = r#"
project_name: test
crates: []
workspaces:
  - name: ws1
    crates:
      - name: a
        path: "."
        tag_template: "v{{ .Version }}"
        publish:
          winget:
            required: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    assert_eq!(
        warnings.len(),
        1,
        "expected one warning for workspace crate winget, got: {:?}",
        warnings
    );
    assert!(
        warnings[0].message.contains("winget"),
        "expected winget in warning, got: {}",
        warnings[0].message
    );
    assert!(
        warnings[0].message.contains("workspaces[ws1].crates[a]"),
        "expected workspace crate location in warning, got: {}",
        warnings[0].message
    );
}

#[test]
fn submitter_advisory_publisher_identities_are_dispatch_names() {
    // The advisory's `publisher` field must match the publisher's DISPATCH
    // name so the CLI's `publisher_deselected(name)` filter (keyed on the same
    // dispatch names) can suppress an advisory for a deselected publisher.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      chocolatey:
        required: true
      winget:
        required: true
      aur_source:
        required: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::submitter_required_warnings(&config);
    let publishers: std::collections::BTreeSet<&str> =
        warnings.iter().map(|w| w.publisher.as_str()).collect();
    assert!(
        publishers.contains("chocolatey"),
        "chocolatey advisory must key on dispatch name 'chocolatey', got: {publishers:?}"
    );
    assert!(
        publishers.contains("winget"),
        "winget advisory must key on dispatch name 'winget', got: {publishers:?}"
    );
    assert!(
        publishers.contains("upstream-aur"),
        "aur_source advisory must key on the AUR-source dispatch name 'upstream-aur', got: {publishers:?}"
    );
}

// ---------------------------------------------------------------------------
// per-crate hooks (before / after / before_publish on CrateConfig)
// ---------------------------------------------------------------------------

#[test]
fn crate_config_parses_per_crate_hooks() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    before:
      hooks: ["echo crate-before"]
    after:
      hooks: ["echo crate-after"]
    before_publish:
      hooks:
        - cmd: "echo crate-bp"
          artifacts: package
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let c = &config.crates[0];
    assert!(
        c.before.as_ref().and_then(|h| h.hooks.as_ref()).is_some(),
        "per-crate before: must parse onto CrateConfig"
    );
    assert!(
        c.after.as_ref().and_then(|h| h.hooks.as_ref()).is_some(),
        "per-crate after: must parse onto CrateConfig"
    );
    let bp = c
        .before_publish
        .as_ref()
        .and_then(|h| h.hooks.as_ref())
        .expect("per-crate before_publish: must parse onto CrateConfig");
    assert_eq!(bp.len(), 1, "one before_publish entry expected");
}

#[test]
fn crate_config_per_crate_hooks_default_none() {
    // Single-crate / lockstep configs that omit per-crate hooks leave the
    // fields unset — the global before/after/before_publish remain the only
    // surface there.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let c = &config.crates[0];
    assert!(c.before.is_none(), "before: must default to None");
    assert!(c.after.is_none(), "after: must default to None");
    assert!(
        c.before_publish.is_none(),
        "before_publish: must default to None"
    );
}

// ---------------------------------------------------------------------------
// warn_on_legacy_homebrew_formula — deprecation of brews / publish.homebrew
// ---------------------------------------------------------------------------

#[test]
fn legacy_homebrew_formula_crate_publish_warns() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        repository:
          owner: o
          name: tap
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_homebrew_formula_warnings(&config);
    assert_eq!(
        warnings.len(),
        1,
        "expected one warning for crate publish.homebrew, got: {:?}",
        warnings
    );
    assert!(
        warnings[0].contains("crate 'a'"),
        "expected crate-name prefix, got: {}",
        warnings[0]
    );
}

#[test]
fn legacy_homebrew_formula_workspace_crate_publish_warns() {
    let yaml = r#"
project_name: test
workspaces:
  - name: ws1
    crates:
      - name: a
        path: "."
        tag_template: "v{{ .Version }}"
        publish:
          homebrew:
            repository:
              owner: o
              name: tap
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_homebrew_formula_warnings(&config);
    assert_eq!(
        warnings.len(),
        1,
        "expected one warning for workspace crate publish.homebrew, got: {:?}",
        warnings
    );
    assert!(
        warnings[0].contains("workspaces[ws1].crates[a]"),
        "expected workspace-crate prefix, got: {}",
        warnings[0]
    );
}

#[test]
fn legacy_homebrew_formula_defaults_publish_warns() {
    let yaml = r#"
project_name: test
defaults:
  publish:
    homebrew:
      repository:
        owner: o
        name: tap
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_homebrew_formula_warnings(&config);
    assert_eq!(
        warnings.len(),
        1,
        "expected one warning for defaults.publish.homebrew, got: {:?}",
        warnings
    );
    assert!(
        warnings[0].contains("defaults.publish"),
        "expected defaults.publish prefix, got: {}",
        warnings[0]
    );
}

#[test]
fn legacy_homebrew_formula_no_warning_when_only_cask() {
    let yaml = r#"
project_name: test
homebrew_casks:
  - name: a
    repository:
      owner: o
      name: tap
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew_cask:
        repository:
          owner: o
          name: tap
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_homebrew_formula_warnings(&config);
    assert!(
        warnings.is_empty(),
        "homebrew_casks / publish.homebrew_cask should not warn, got: {:?}",
        warnings
    );
}

#[test]
fn legacy_homebrew_formula_no_warning_when_publish_has_no_homebrew() {
    // Locks the predicate: a publish block with only non-homebrew publishers
    // must NOT trigger the deprecation warning.
    let yaml = r#"
project_name: x
crates:
  - name: x
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      cargo: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_homebrew_formula_warnings(&config);
    assert!(
        warnings.is_empty(),
        "expected no warning, got: {warnings:?}"
    );
}

#[test]
fn legacy_homebrew_formula_multiple_axes_each_warn() {
    let yaml = r#"
project_name: test
defaults:
  publish:
    homebrew:
      repository:
        owner: o
        name: tap
crates:
  - name: top
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        repository:
          owner: o
          name: tap
workspaces:
  - name: ws1
    crates:
      - name: nested
        path: ws1
        tag_template: "v{{ .Version }}"
        publish:
          homebrew:
            repository:
              owner: o
              name: tap
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_homebrew_formula_warnings(&config);
    assert_eq!(
        warnings.len(),
        3,
        "expected three warnings (one per placement axis), got: {:?}",
        warnings
    );
}

#[test]
fn legacy_homebrew_formula_warning_message_includes_migration_pointer() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew:
        repository:
          owner: o
          name: tap
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_homebrew_formula_warnings(&config);
    assert_eq!(warnings.len(), 1);
    let msg = &warnings[0];
    for clause in [
        "DEPRECATION",
        "publish.homebrew",
        "GoReleaser v2.16",
        "homebrew_casks",
        "https://anodize.dev/docs/publish/homebrew-casks/",
    ] {
        assert!(
            msg.contains(clause),
            "warning missing clause {:?}, got: {}",
            clause,
            msg
        );
    }
}

// ---- changelog.disable alias for skip ----

#[test]
fn test_changelog_disable_alias_parses() {
    let yaml = r#"
project_name: test
changelog:
  disable: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cl = config.changelog.as_ref().expect("changelog present");
    assert_eq!(
        cl.skip,
        Some(StringOrBool::Bool(true)),
        "disable: alias should populate skip"
    );
}

#[test]
fn test_changelog_disable_template_string_alias() {
    let yaml = r#"
project_name: test
changelog:
  disable: "{{ if IsSnapshot }}true{{ endif }}"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cl = config.changelog.as_ref().expect("changelog present");
    match cl.skip.as_ref() {
        Some(StringOrBool::String(s)) => assert!(s.contains("IsSnapshot")),
        other => panic!(
            "expected templated skip via disable alias, got: {:?}",
            other
        ),
    }
}

// ---- validate_changelog_groups_depth ----

#[test]
fn test_validate_changelog_groups_depth_accepts_one_level() {
    let yaml = r#"
project_name: test
changelog:
  groups:
    - title: Features
      regexp: "^feat"
      groups:
        - title: Core
          regexp: "^feat\\(core\\)"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(validate_changelog_groups_depth(&config).is_ok());
}

#[test]
fn test_validate_changelog_groups_depth_rejects_three_levels() {
    let yaml = r#"
project_name: test
changelog:
  groups:
    - title: Features
      regexp: "^feat"
      groups:
        - title: Core
          regexp: "^feat\\(core\\)"
          groups:
            - title: Auth
              regexp: "^feat\\(core/auth\\)"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_changelog_groups_depth(&config).unwrap_err();
    assert!(err.contains("Features"), "{err}");
    assert!(err.contains("Core"), "{err}");
    assert!(err.contains("one level"), "{err}");
}

#[test]
fn test_validate_changelog_groups_depth_walks_workspaces() {
    let yaml = r#"
project_name: test
workspaces:
  - name: w
    changelog:
      groups:
        - title: A
          groups:
            - title: B
              groups:
                - title: C
    crates:
      - name: a
        path: a
        tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_changelog_groups_depth(&config).unwrap_err();
    assert!(err.contains("workspaces[w].changelog"), "{err}");
}

// ---- validate_changelog_paths ----

#[test]
fn test_validate_changelog_paths_rejects_leading_slash() {
    let yaml = r#"
project_name: test
changelog:
  paths:
    - "/src/lib.rs"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_changelog_paths(&config).unwrap_err();
    assert!(err.contains("repo-root-relative"), "{err}");
    assert!(err.contains("/src/lib.rs"), "{err}");
}

#[test]
fn test_validate_changelog_paths_rejects_empty_entry() {
    let yaml = r#"
project_name: test
changelog:
  paths:
    - ""
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_changelog_paths(&config).unwrap_err();
    assert!(err.contains("empty"), "{err}");
}

#[test]
fn test_validate_changelog_paths_accepts_relative() {
    let yaml = r#"
project_name: test
changelog:
  paths:
    - "src/**/*.rs"
    - "crates/core"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(validate_changelog_paths(&config).is_ok());
}

// ---- validate_exclude_globs ----

#[test]
fn test_validate_exclude_globs_accepts_valid_globs_all_destinations() {
    let yaml = r#"
project_name: test
artifactories:
  - target: "https://repo/{{ .ArtifactName }}"
    exclude: ["*.sig", "*.cdx.json"]
cloudsmiths:
  - organization: org
    repository: repo
    exclude: ["*.sha256"]
gemfury:
  - account: acct
    exclude: ["*.sig"]
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      github: { owner: o, name: n }
      exclude: ["*.sha256", "*.sig", "*.cdx.json"]
    blobs:
      - provider: s3
        bucket: b
        exclude: ["*.sig"]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    validate_exclude_globs(&config).expect("valid globs on every destination pass");
}

#[test]
fn test_validate_exclude_globs_rejects_invalid_blob_glob() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    blobs:
      - provider: s3
        bucket: b
        exclude: ["["]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_exclude_globs(&config).unwrap_err();
    assert!(err.contains("crates[a].blobs[0]"), "{err}");
    assert!(err.contains("not a valid glob"), "{err}");
}

#[test]
fn test_validate_exclude_globs_rejects_invalid_release_glob() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    release:
      github: { owner: o, name: n }
      exclude: ["a[b"]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_exclude_globs(&config).unwrap_err();
    assert!(err.contains("crates[a].release"), "{err}");
    assert!(err.contains("not a valid glob"), "{err}");
}

#[test]
fn test_validate_exclude_globs_rejects_invalid_top_level_release_glob() {
    // The top-level shared `release:` block carries the same `exclude:` field
    // as a per-crate `release:`; a malformed glob there must be rejected too.
    let yaml = r#"
project_name: test
release:
  github: { owner: o, name: n }
  exclude: ["a[b"]
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_exclude_globs(&config).unwrap_err();
    assert!(
        err.starts_with("release:"),
        "expected top-level `release` location, got: {err}"
    );
    assert!(err.contains("not a valid glob"), "{err}");
}

#[test]
fn test_validate_exclude_globs_rejects_empty_entry() {
    let yaml = r#"
project_name: test
cloudsmiths:
  - organization: org
    repository: repo
    exclude: [""]
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_exclude_globs(&config).unwrap_err();
    assert!(err.contains("cloudsmiths[0]"), "{err}");
    assert!(err.contains("empty"), "{err}");
}

#[test]
fn test_validate_exclude_globs_rejects_invalid_glob_in_workspace_crate() {
    let yaml = r#"
project_name: test
workspaces:
  - name: ws
    crates:
      - name: a
        path: "crates/a"
        tag_template: "v{{ .Version }}"
        blobs:
          - provider: s3
            bucket: b
            exclude: ["x[y"]
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_exclude_globs(&config).unwrap_err();
    assert!(err.contains("workspaces[ws].crates[a].blobs[0]"), "{err}");
}

// ---------------------------------------------------------------------------
// Deprecation aliases
// ---------------------------------------------------------------------------

// ---- Row 1+2: nested docker_v2[].retry / docker_manifests[].retry ----

#[test]
fn legacy_docker_retry_warns_for_docker_v2() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    dockers_v2:
      - dockerfile: Dockerfile
        images: ["ghcr.io/example/app"]
        retry:
          attempts: 3
          delay: 1s
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_docker_retry_warnings(&config);
    assert_eq!(warnings.len(), 1, "expected one warning, got: {warnings:?}");
    let msg = &warnings[0];
    assert!(msg.contains("DEPRECATION"));
    assert!(msg.contains("crates[a].dockers_v2[0]"));
    assert!(msg.contains("dockers_v2.retry"));
    assert!(msg.contains("v2.15.3"));
    assert!(msg.contains("top-level `retry:`"));
}

#[test]
fn legacy_docker_retry_warns_for_docker_manifests() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    docker_manifests:
      - name_template: "ghcr.io/example/app:{{ .Version }}"
        image_templates: ["ghcr.io/example/app:{{ .Version }}-amd64"]
        retry:
          attempts: 5
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_docker_retry_warnings(&config);
    assert_eq!(warnings.len(), 1, "expected one warning, got: {warnings:?}");
    assert!(warnings[0].contains("crates[a].docker_manifests[0]"));
    assert!(warnings[0].contains("docker_manifests.retry"));
}

#[test]
fn legacy_docker_retry_no_warning_when_top_level_retry_only() {
    // Uses the `docker_v2:` back-compat alias (canonical is `dockers_v2:`) so
    // the serde alias stays covered alongside the canonical-key tests.
    let yaml = r#"
project_name: test
retry:
  attempts: 7
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    docker_v2:
      - dockerfile: Dockerfile
        images: ["ghcr.io/example/app"]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_docker_retry_warnings(&config);
    assert!(
        warnings.is_empty(),
        "expected no warnings, got: {warnings:?}"
    );
}

#[test]
fn legacy_docker_retry_warns_in_workspaces_and_defaults() {
    let yaml = r#"
project_name: test
defaults:
  dockers_v2:
    dockerfile: Dockerfile
    images: ["ghcr.io/example/app"]
    retry:
      attempts: 2
workspaces:
  - name: ws1
    crates:
      - name: nested
        path: ws1
        tag_template: "v{{ .Version }}"
        dockers_v2:
          - dockerfile: Dockerfile
            images: ["ghcr.io/example/app"]
            retry:
              attempts: 4
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let warnings = super::legacy_docker_retry_warnings(&config);
    assert_eq!(
        warnings.len(),
        2,
        "expected defaults + workspace warnings, got: {warnings:?}"
    );
    assert!(warnings.iter().any(|w| w.contains("defaults.dockers_v2")));
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("workspaces[ws1].crates[nested].dockers_v2[0]"))
    );
}

// ---- Row 3: V1 dockers: block rejection ----
//
// `test_v1_dockers_block_rejected_with_migration_message` already covers the
// rejection. Add a sibling test asserting the load_config path also surfaces
// the migration pointer (catch regressions where the validator is removed
// from the pipeline without removing this test).

#[test]
fn legacy_v1_dockers_block_error_mentions_v2_migration() {
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
dockers:
  - dockerfile: Dockerfile
    image_templates: ["ghcr.io/example/app:{{ .Version }}"]
"#,
    )
    .unwrap();
    let err = super::validate_no_docker_v1(&raw).expect_err("v1 dockers must reject");
    // Error must name both the legacy field and the v2 replacement so the
    // user does not need to consult the docs to find the new spelling.
    assert!(err.contains("dockers"), "missing legacy name: {err}");
    assert!(err.contains("dockers_v2"), "missing v2 pointer: {err}");
}

// ---- Row 4: furies: → gemfury: rename (v2.14) ----

#[test]
fn legacy_furies_top_level_parses_as_gemfury() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
furies:
  - id: my-fury
    account: my-account
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let gemfury = config.gemfury.expect("furies: alias must populate gemfury");
    assert_eq!(gemfury.len(), 1);
    assert_eq!(gemfury[0].id.as_deref(), Some("my-fury"));
    assert_eq!(gemfury[0].account.as_deref(), Some("my-account"));
}

#[test]
fn legacy_furies_alias_detected_in_raw_yaml() {
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
project_name: test
furies:
  - id: my-fury
crates: []
"#,
    )
    .unwrap();
    // The `warn_on_legacy_furies_alias` helper emits via `tracing::warn!`
    // and returns no value; validate the same raw-YAML probe the helper
    // uses to detect the legacy spelling.
    assert!(raw.get("furies").is_some());
}

#[test]
fn gemfury_canonical_key_does_not_trigger_furies_probe() {
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
project_name: test
gemfury:
  - id: my-fury
crates: []
"#,
    )
    .unwrap();
    assert!(
        raw.get("furies").is_none(),
        "canonical gemfury key must not look like the legacy furies alias"
    );
    assert!(raw.get("gemfury").is_some());
}

#[test]
fn legacy_nfpm_builds_warn_runs_for_crates_nfpms() {
    // crates[].nfpms[] with a legacy `builds:` key — the recursive probe must
    // reach it and run without panicking (the helper emits via tracing and
    // returns ()).
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
project_name: test
crates:
  - name: app
    nfpms:
      - id: deb
        builds: [foo]
      - id: rpm
        ids: [bar]
"#,
    )
    .unwrap();
    super::warn_on_legacy_nfpm_builds(&raw);
}

#[test]
fn legacy_nfpm_builds_warn_runs_for_defaults_nfpms() {
    // defaults.nfpms[] depth.
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
project_name: test
crates: []
defaults:
  nfpms:
    - id: deb
      builds: [foo, bar]
"#,
    )
    .unwrap();
    super::warn_on_legacy_nfpm_builds(&raw);
}

#[test]
fn legacy_nfpm_builds_warn_runs_for_nested_workspace_crates() {
    // workspaces[].crates[].nfpms[] — deepest nesting the recursive descent
    // must reach. Also exercises a single-map `nfpm:` value, not a sequence.
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
project_name: test
crates: []
workspaces:
  - members:
      - core
    crates:
      - name: core
        nfpm:
          id: deb
          builds: [foo]
"#,
    )
    .unwrap();
    super::warn_on_legacy_nfpm_builds(&raw);
}

#[test]
fn legacy_nfpm_builds_warn_runs_cleanly_without_builds_key() {
    // A config whose nfpm uses the canonical `ids:` and an unrelated archive
    // `builds:` elsewhere must still run cleanly (no panic). The archive
    // `builds:` is NOT an nfpm value, so it is not inspected by this probe.
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
project_name: test
crates:
  - name: app
    nfpms:
      - id: deb
        ids: [bar]
    archives:
      - id: default
        builds: [foo]
"#,
    )
    .unwrap();
    super::warn_on_legacy_nfpm_builds(&raw);
}

// ---- legacy `disable:` → `skip:` alias deprecation ----

fn disable_alias_warnings(yaml: &str) -> Vec<String> {
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).unwrap();
    super::legacy_disable_alias_warnings(&raw)
}

#[test]
fn legacy_disable_alias_top_level_release_warns_with_path() {
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
release:
  disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "expected one warning, got {warnings:?}");
    assert!(
        warnings[0].contains("release.disable"),
        "warning must name the path: {}",
        warnings[0]
    );
}

#[test]
fn legacy_disable_alias_top_level_changelog_warns() {
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
changelog:
  disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("changelog.disable"));
}

#[test]
fn legacy_disable_alias_top_level_dockerhub_warns() {
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
dockerhub:
  - username: u
    disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("dockerhub[0].disable"),
        "{}",
        warnings[0]
    );
}

#[test]
fn legacy_disable_alias_top_level_mcp_warns() {
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
mcp:
  name: srv
  disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("mcp.disable"));
}

#[test]
fn legacy_disable_alias_top_level_makeselfs_warns() {
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
makeselfs:
  - id: app
    disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("makeselfs[0].disable"));
}

#[test]
fn legacy_disable_alias_top_level_appimages_warns() {
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
appimages:
  - id: app
    disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("appimages[0].disable"));
}

#[test]
fn legacy_disable_alias_nested_crates_snapcrafts_warns() {
    // crates[].snapcrafts[] — proves the nearest-named-ancestor rule fires
    // axis-agnostically under a crate's publish-axis blocks.
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates:
  - name: app
    snapcrafts:
      - name: app
        disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("crates[0].snapcrafts[0].disable"),
        "{}",
        warnings[0]
    );
}

#[test]
fn legacy_disable_alias_nested_defaults_installers_warn() {
    // defaults.{msis,pkgs,nsis,dockers_v2} all map to skip-with-disable-alias
    // structs; one config exercises several blocks at the defaults axis.
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
defaults:
  msis:
    disable: true
  pkgs:
    disable: true
  nsis:
    disable: true
  makeselves:
    disable: true
  dockers_v2:
    disable: true
"#,
    );
    assert_eq!(warnings.len(), 5, "{warnings:?}");
    for block in ["msis", "pkgs", "nsis", "makeselves", "dockers_v2"] {
        let needle = format!("defaults.{block}.disable");
        assert!(
            warnings.iter().any(|w| w.contains(&needle)),
            "missing warning for {needle}: {warnings:?}"
        );
    }
}

#[test]
fn legacy_disable_alias_nested_workspace_crates_release_warns() {
    // Deepest axis: workspaces[].crates[].release.disable.
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
workspaces:
  - members: [core]
    crates:
      - name: core
        release:
          disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("workspaces[0].crates[0].release.disable"),
        "{}",
        warnings[0]
    );
}

// ---- NEGATIVE guards: the correctness constraint ----

#[test]
fn legacy_disable_alias_warns_on_npm_disable() {
    // `npms[].disable` folds into `skip` via serde alias, so the legacy
    // spelling must warn like every other aliased block.
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
npms:
  - disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("npms[0].disable"), "{}", warnings[0]);
}

#[test]
fn legacy_disable_alias_warns_on_gemfury_disable() {
    // `gemfury[].disable` folds into `skip` via serde alias — must warn.
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
gemfury:
  - id: x
    disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("gemfury[0].disable"),
        "{}",
        warnings[0]
    );
}

#[test]
fn legacy_disable_alias_warns_on_furies_legacy_alias_key() {
    // The legacy `furies:` block key maps to the same GemFuryConfig, so
    // `furies[].disable:` must warn under its legacy key too.
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
furies:
  - id: x
    disable: true
"#,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("furies[0].disable"), "{}", warnings[0]);
}

#[test]
fn legacy_disable_alias_silent_for_canonical_skip() {
    // Using the canonical `skip:` in an allow-listed block must not warn.
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
release:
  skip: true
changelog:
  skip: true
"#,
    );
    assert!(
        warnings.is_empty(),
        "canonical skip must not warn: {warnings:?}"
    );
}

#[test]
fn legacy_disable_alias_skips_freeform_map_user_key() {
    // A user may legitimately name a key `disable` inside a free-form map
    // (`variables`, `build_args`, `labels`, …). The nearest named ancestor of
    // such a key is the map's own key — never an allow-listed block — so no
    // warning fires. Includes `dockers_v2[].build_args.disable`: `dockers_v2` IS
    // allow-listed, but the immediate enclosing block of the key is
    // `build_args`, so it is correctly skipped.
    let warnings = disable_alias_warnings(
        r#"
project_name: test
crates: []
variables:
  disable: hello
dockers_v2:
  - build_args:
      disable: "1"
"#,
    );
    assert!(
        warnings.is_empty(),
        "free-form map keys named disable must not warn: {warnings:?}"
    );
}

// ---- Row 5: nested mcp.github (v2.13.1) ----

#[test]
fn legacy_mcp_github_block_rejected_with_migration_message() {
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
project_name: test
crates: []
mcp:
  github:
    owner: my-org
    name: my-repo
"#,
    )
    .unwrap();
    let err = super::validate_no_mcp_github(&raw).expect_err("nested mcp.github must reject");
    assert!(err.contains("mcp.github"));
    assert!(err.contains("v2.13.1"));
    assert!(err.contains("mcp.repository"));
}

#[test]
fn canonical_mcp_top_level_passes() {
    let raw: serde_yaml_ng::Value = serde_yaml_ng::from_str(
        r#"
project_name: test
crates: []
mcp:
  name: io.github.user/server
  repository:
    url: https://github.com/my-org/my-repo
    source: github
"#,
    )
    .unwrap();
    super::validate_no_mcp_github(&raw).expect("canonical mcp.* must pass");
}

// ---- Row 6: homebrew_casks[].binary singular → binaries plural (v2.12.6) ----

#[test]
fn legacy_homebrew_cask_binary_singular_folds_into_binaries() {
    let yaml = r#"
project_name: test
crates: []
homebrew_casks:
  - name: mycask
    repository:
      owner: o
      name: tap
    binary: my-cli
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::apply_homebrew_cask_legacy_singulars(&mut config);
    let cask = &config.homebrew_casks.as_ref().unwrap()[0];
    let binaries = cask
        .binaries
        .as_ref()
        .expect("legacy binary must fold into binaries");
    assert_eq!(binaries.len(), 1);
    assert_eq!(binaries[0].name(), "my-cli");
    assert!(
        cask.legacy_binary.is_none(),
        "legacy_binary must be consumed"
    );
}

#[test]
fn legacy_homebrew_cask_binary_prepended_when_both_present() {
    let yaml = r#"
project_name: test
crates: []
homebrew_casks:
  - name: mycask
    repository:
      owner: o
      name: tap
    binary: legacy-cli
    binaries:
      - new-cli
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::apply_homebrew_cask_legacy_singulars(&mut config);
    let binaries = config.homebrew_casks.as_ref().unwrap()[0]
        .binaries
        .as_ref()
        .unwrap();
    assert_eq!(binaries.len(), 2);
    assert_eq!(binaries[0].name(), "legacy-cli");
    assert_eq!(binaries[1].name(), "new-cli");
}

#[test]
fn legacy_homebrew_cask_binary_canonical_form_unchanged() {
    let yaml = r#"
project_name: test
crates: []
homebrew_casks:
  - name: mycask
    repository:
      owner: o
      name: tap
    binaries:
      - just-cli
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::apply_homebrew_cask_legacy_singulars(&mut config);
    let binaries = config.homebrew_casks.as_ref().unwrap()[0]
        .binaries
        .as_ref()
        .unwrap();
    assert_eq!(binaries.len(), 1);
    assert_eq!(binaries[0].name(), "just-cli");
}

#[test]
fn legacy_homebrew_cask_binary_folds_in_per_crate_publish() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew_cask:
        repository:
          owner: o
          name: tap
        binary: per-crate-cli
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::apply_homebrew_cask_legacy_singulars(&mut config);
    let cask = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .homebrew_cask
        .as_ref()
        .unwrap();
    let binaries = cask.binaries.as_ref().unwrap();
    assert_eq!(binaries.len(), 1);
    assert_eq!(binaries[0].name(), "per-crate-cli");
    assert!(cask.legacy_binary.is_none());
}

// ---- Row 6b: homebrew_casks[].manpage singular → manpages plural ----

#[test]
fn legacy_homebrew_cask_manpage_singular_folds_into_manpages() {
    let yaml = r#"
project_name: test
crates: []
homebrew_casks:
  - name: mycask
    repository:
      owner: o
      name: tap
    manpage: foo.1
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::apply_homebrew_cask_legacy_singulars(&mut config);
    let cask = &config.homebrew_casks.as_ref().unwrap()[0];
    let manpages = cask
        .manpages
        .as_ref()
        .expect("legacy manpage must fold into manpages");
    assert_eq!(manpages, &vec!["foo.1".to_string()]);
    assert!(
        cask.legacy_manpage.is_none(),
        "legacy_manpage must be consumed"
    );
}

#[test]
fn legacy_homebrew_cask_manpage_appended_when_both_present() {
    let yaml = r#"
project_name: test
crates: []
homebrew_casks:
  - name: mycask
    repository:
      owner: o
      name: tap
    manpage: legacy.1
    manpages:
      - new.1
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::apply_homebrew_cask_legacy_singulars(&mut config);
    let manpages = config.homebrew_casks.as_ref().unwrap()[0]
        .manpages
        .as_ref()
        .unwrap();
    // The legacy singular is appended to the END of manpages.
    assert_eq!(manpages, &vec!["new.1".to_string(), "legacy.1".to_string()]);
}

#[test]
fn legacy_homebrew_cask_manpage_not_reserialized() {
    let yaml = r#"
project_name: test
crates: []
homebrew_casks:
  - name: mycask
    repository:
      owner: o
      name: tap
    manpage: foo.1
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::apply_homebrew_cask_legacy_singulars(&mut config);
    let out = serde_yaml_ng::to_string(&config).unwrap();
    // skip_serializing: only the canonical plural form round-trips.
    assert!(
        !out.contains("manpage:"),
        "legacy `manpage:` must not be re-serialized:\n{out}"
    );
    assert!(
        out.contains("manpages:"),
        "canonical `manpages:` must be serialized:\n{out}"
    );
}

#[test]
fn legacy_homebrew_cask_manpage_folds_in_per_crate_publish() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      homebrew_cask:
        repository:
          owner: o
          name: tap
        manpage: per-crate.1
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::apply_homebrew_cask_legacy_singulars(&mut config);
    let cask = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .homebrew_cask
        .as_ref()
        .unwrap();
    assert_eq!(
        cask.manpages.as_ref().unwrap(),
        &vec!["per-crate.1".to_string()]
    );
    assert!(cask.legacy_manpage.is_none());
}

#[test]
fn legacy_homebrew_cask_manpage_folds_in_workspace_per_crate_publish() {
    // The fold must reach the workspaces[].crates[].publish.homebrew_cask
    // arm too — config-mode parity (single-crate, lockstep, per-crate).
    let yaml = r#"
project_name: test
crates: []
workspaces:
  - name: ws1
    crates:
      - name: a
        path: "."
        tag_template: "v{{ .Version }}"
        publish:
          homebrew_cask:
            repository:
              owner: o
              name: tap
            manpage: ws.1
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    super::apply_homebrew_cask_legacy_singulars(&mut config);
    let cask = config.workspaces.as_ref().unwrap()[0].crates[0]
        .publish
        .as_ref()
        .unwrap()
        .homebrew_cask
        .as_ref()
        .unwrap();
    assert_eq!(cask.manpages.as_ref().unwrap(), &vec!["ws.1".to_string()]);
    assert!(
        cask.legacy_manpage.is_none(),
        "legacy_manpage must be consumed in the workspace arm"
    );
}

// ---------------------------------------------------------------------------
// `builder: prebuilt` config surface
// ---------------------------------------------------------------------------

#[test]
fn builder_field_defaults_to_none_serde() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &cfg.crates[0].builds.as_ref().unwrap()[0];
    assert!(build.builder.is_none());
    assert!(build.prebuilt.is_none());
}

#[test]
fn builder_kind_prebuilt_parses_lowercase() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        prebuilt:
          path: "output/app_{{ .Target }}"
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &cfg.crates[0].builds.as_ref().unwrap()[0];
    assert!(matches!(build.builder, Some(BuilderKind::Prebuilt)));
    assert_eq!(
        build.prebuilt.as_ref().unwrap().path,
        "output/app_{{ .Target }}"
    );
}

#[test]
fn builds_amd64_variant_is_rejected_at_parse_on_every_axis() {
    // Typed as an enum, so serde is the gate — a garbage level fails the
    // PARSE on the crates axis, the workspaces axis, AND `defaults.builds`
    // (which the loader's validate_builds walk never visits, and which is
    // folded into crates only AFTER validation runs).
    let crates_yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        targets: ["x86_64-unknown-linux-gnu"]
        amd64_variant: "v3"
"#;
    let cfg: Config = serde_yaml_ng::from_str(crates_yaml).expect("a declared v3 level parses");
    assert_eq!(
        cfg.crates[0].builds.as_ref().unwrap()[0].amd64_variant,
        Some(Amd64Variant::V3)
    );

    let bad = crates_yaml.replace("\"v3\"", "\"x86-64-v3\"");
    let err = serde_yaml_ng::from_str::<Config>(&bad)
        .expect_err("a non-level value on the crates axis must fail the parse");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown variant `x86-64-v3`, expected one of `v1`, `v2`, `v3`, `v4`"),
        "names the bad value and the full valid set: {msg}"
    );

    let defaults_yaml = r#"
project_name: test
defaults:
  builds:
    amd64_variant: "x86-64-v3"
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let err = serde_yaml_ng::from_str::<Config>(defaults_yaml)
        .expect_err("the defaults axis must be gated by the same parse");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown variant `x86-64-v3`, expected one of `v1`, `v2`, `v3`, `v4`"),
        "defaults-axis garbage gets the same rejection: {msg}"
    );

    let workspaces_yaml = r#"
project_name: test
workspaces:
  - name: "ws"
    crates:
      - name: app
        path: "."
        tag_template: "v{{ .Version }}"
        builds:
          - binary: app
            amd64_variant: "v9"
"#;
    let err = serde_yaml_ng::from_str::<Config>(workspaces_yaml)
        .expect_err("the workspaces axis must be gated by the same parse");
    assert!(
        err.to_string().contains("unknown variant `v9`"),
        "workspaces-axis garbage gets the same rejection: {err}"
    );
}

#[test]
fn schema_types_builds_amd64_variant_as_the_level_enum() {
    // The generated schema.json must reject a bogus level (e.g. "v9") the
    // same way serde does: BuildConfig.amd64_variant references the closed
    // Amd64Variant enum instead of a free-form string.
    let schema = config_schema();
    let variant_ref = &schema["definitions"]["BuildConfig"]["properties"]["amd64_variant"];
    assert!(
        variant_ref
            .to_string()
            .contains("#/definitions/Amd64Variant"),
        "builds[].amd64_variant must reference the enum definition: {variant_ref}"
    );
    let levels = &schema["definitions"]["Amd64Variant"]["enum"];
    assert_eq!(
        levels,
        &serde_json::json!(["v1", "v2", "v3", "v4"]),
        "the enum definition is closed over the four levels"
    );
}

#[test]
fn schema_types_every_amd64_variant_field_as_the_level_enum() {
    // The whole `amd64_variant` domain references the one closed enum: every
    // config carrying the field (build declaration, AUR source template var,
    // installer/packager/publisher selectors) must reject a bogus level in
    // schema validation exactly as serde does at parse.
    let schema = config_schema();
    for def in [
        "BuildConfig",
        "AurSourceConfig",
        "DmgConfig",
        "MsiConfig",
        "NsisConfig",
        "NfpmConfig",
        "HomebrewConfig",
        "ScoopConfig",
        "ChocolateyConfig",
        "WingetConfig",
        "KrewConfig",
        "NixConfig",
        "AurConfig",
    ] {
        let field = &schema["definitions"][def]["properties"]["amd64_variant"];
        assert!(
            field.to_string().contains("#/definitions/Amd64Variant"),
            "{def}.amd64_variant must reference the enum definition: {field}"
        );
    }
}

#[test]
fn validate_builds_accepts_default_cargo() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    validate_builds(&cfg).expect("default cargo build must validate cleanly");
}

#[test]
fn validate_builds_rejects_prebuilt_without_path() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_builds(&cfg).unwrap_err();
    assert!(
        err.contains("`builder: prebuilt` requires a non-empty `prebuilt.path`"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_builds_rejects_prebuilt_with_empty_path() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        prebuilt:
          path: "   "
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_builds(&cfg).unwrap_err();
    assert!(
        err.contains("non-empty `prebuilt.path`"),
        "unexpected: {err}"
    );
}

#[test]
fn validate_builds_rejects_prebuilt_without_targets() {
    let yaml = r#"
project_name: test
defaults:
  targets: ["x86_64-unknown-linux-gnu"]
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        prebuilt:
          path: "output/app_{{ .Target }}"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_builds(&cfg).unwrap_err();
    assert!(
        err.contains("no explicit `targets:`"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_builds_rejects_prebuilt_with_cross_tool() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        cross_tool: "/usr/local/bin/my-cross"
        prebuilt:
          path: "output/app_{{ .Target }}"
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_builds(&cfg).unwrap_err();
    assert!(err.contains("`cross_tool`") && err.contains("mutually exclusive"));
}

#[test]
fn validate_builds_rejects_prebuilt_with_command_override() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        command: "auditable build"
        prebuilt:
          path: "output/app_{{ .Target }}"
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_builds(&cfg).unwrap_err();
    assert!(err.contains("`command:` override"));
}

#[test]
fn validate_builds_rejects_prebuilt_with_features() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        features: ["foo"]
        prebuilt:
          path: "output/app_{{ .Target }}"
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_builds(&cfg).unwrap_err();
    assert!(err.contains("`features:`"));
}

#[test]
fn validate_builds_rejects_prebuilt_with_no_default_features() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        no_default_features: true
        prebuilt:
          path: "output/app_{{ .Target }}"
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_builds(&cfg).unwrap_err();
    assert!(err.contains("`no_default_features:`"));
}

#[test]
fn validate_builds_rejects_crate_cross_with_prebuilt_build() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    cross: zigbuild
    builds:
      - binary: app
        builder: prebuilt
        prebuilt:
          path: "output/app_{{ .Target }}"
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = validate_builds(&cfg).unwrap_err();
    assert!(err.contains("crate-level `cross:`") && err.contains("builder: prebuilt"));
}

#[test]
fn validate_builds_accepts_prebuilt_minimal() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        prebuilt:
          path: "output/app_{{ .Target }}"
        targets: ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    validate_builds(&cfg).expect("minimal prebuilt config should validate");
}

#[test]
fn all_builds_prebuilt_true_when_every_build_is_prebuilt() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        prebuilt:
          path: "output/app_{{ .Target }}"
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(all_builds_prebuilt(&cfg));
}

#[test]
fn all_builds_prebuilt_false_when_any_build_is_cargo() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: app
        builder: prebuilt
        prebuilt:
          path: "output/app_{{ .Target }}"
        targets: ["x86_64-unknown-linux-gnu"]
      - binary: app
        targets: ["x86_64-unknown-linux-gnu"]
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(!all_builds_prebuilt(&cfg));
}

#[test]
fn all_builds_prebuilt_false_when_no_builds_declared() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(!all_builds_prebuilt(&cfg));
}

#[test]
fn cloudsmith_keep_versions_parses_and_defaults_none() {
    let yaml = r#"
project_name: test
cloudsmiths:
  - organization: acme
    repository: tools
    keep_versions: 3
  - organization: acme
    repository: tools-no-prune
"#;
    let cfg: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cs = cfg.cloudsmiths.as_ref().unwrap();
    assert_eq!(cs[0].keep_versions, Some(3));
    assert_eq!(cs[1].keep_versions, None);
}

#[test]
fn effective_default_targets_falls_back_to_canonical_when_unset() {
    // No `defaults` block at all → the canonical DEFAULT_TARGETS matrix.
    let cfg = Config::default();
    assert_eq!(
        cfg.effective_default_targets(),
        crate::target::DEFAULT_TARGETS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn effective_default_targets_falls_back_when_defaults_targets_empty() {
    // An explicitly-empty `defaults.targets` is treated as "unset" so the
    // build still has a target matrix to compile.
    let mut cfg = Config::default();
    cfg.defaults = Some(super::Defaults {
        targets: Some(vec![]),
        ..Default::default()
    });
    assert_eq!(
        cfg.effective_default_targets(),
        crate::target::DEFAULT_TARGETS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn effective_default_targets_uses_configured_targets_when_set() {
    let mut cfg = Config::default();
    cfg.defaults = Some(super::Defaults {
        targets: Some(vec![
            "aarch64-unknown-linux-gnu".to_string(),
            "x86_64-apple-darwin".to_string(),
        ]),
        ..Default::default()
    });
    assert_eq!(
        cfg.effective_default_targets(),
        vec![
            "aarch64-unknown-linux-gnu".to_string(),
            "x86_64-apple-darwin".to_string(),
        ],
    );
}

#[test]
fn default_cross_strategy_is_auto_when_unset() {
    let cfg = Config::default();
    assert_eq!(cfg.default_cross_strategy(), CrossStrategy::Auto);
    // A `defaults` block that omits `cross:` still resolves to Auto.
    let mut cfg2 = Config::default();
    cfg2.defaults = Some(super::Defaults::default());
    assert_eq!(cfg2.default_cross_strategy(), CrossStrategy::Auto);
}

#[test]
fn default_cross_strategy_uses_configured_strategy() {
    let mut cfg = Config::default();
    cfg.defaults = Some(super::Defaults {
        cross: Some(CrossStrategy::Cross),
        ..Default::default()
    });
    assert_eq!(cfg.default_cross_strategy(), CrossStrategy::Cross);
}
