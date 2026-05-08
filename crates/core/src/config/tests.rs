#![allow(clippy::field_reassign_with_default)]

// External crates
use serde::Deserialize;

// Inline items from config/mod.rs
use super::{Config, ERR_DEFAULTS_AXIS_MISMATCH, IncludeFilePath, IncludeSpec, IncludeUrlConfig};
use super::{
    validate_defaults_axis, validate_format_overrides, validate_homebrew_cask_url_template,
    validate_tag_sort, validate_version,
};

// Items re-exported from config submodules (all reachable as super::ItemName
// because config/mod.rs does `pub use submod::*;` for each)
use super::GitConfig;
use super::HookEntry;
use super::{ArchivesConfig, ChecksumConfig, ContentSource, ExtraFileSpec};
use super::{ChangelogConfig, MilestoneConfig, SbomConfig};
use super::{CrateConfig, CrossStrategy};
use super::{
    EnvFilesConfig, EnvFilesTokenConfig, load_env_files, load_token_files, read_token_file,
};
use super::{
    ForceTokenKind, GitHubUrlsConfig, GitLabUrlsConfig, GiteaUrlsConfig, MakeLatestConfig,
    ReleaseConfig,
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
  flags: --release
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
    // Presence of `cargo:` opts the crate in (DEC-6 / ITEM-3 — no
    // `enabled` field, no bool shorthand).
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
    // ITEM-3 hard-break: `cargo: true` is no longer a valid shorthand.
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
    // ITEM-3 hard-break: the old `publish.crates:` key was renamed to
    // `publish.cargo:` with no alias (DEC-5).
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

// ---- ChecksumConfig resolved_*() accessors (Session C lazy-defaults policy) ----

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

// ---- Notarize resolved_*() accessors (Session C lazy-defaults policy) ----

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

// ---- SbomConfig resolved_*() accessors (Session C lazy-defaults policy) ----

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

// ---- ReleaseConfig resolved_*() accessors (Session C lazy-defaults policy) ----

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

// ---- ChangelogConfig resolved_*() accessors (Session C lazy-defaults policy) ----

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

// Q15.1 — abbrev clamp. Mirrors GoReleaser commit 88daaf3
// (internal/pipe/changelog/changelog.go): values below `-1` are clamped
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
        // Match GoReleaser (internal/pipe/changelog/changelog.go:54-61):
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

// ---- MilestoneConfig resolved_*() accessors (Session C lazy-defaults policy) ----

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

// ---- binary_signs artifacts constraint (SCH-27) ----

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
    let schema = schemars::schema_for!(Config);
    let json = serde_json::to_value(&schema).expect("schema must serialize");
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

// ---- env list form tests (GoReleaser parity) ----

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

// ---- Error path tests (Task 3B) ----

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
fn test_unknown_crate_level_fields_ignored() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nonexistent_field: true
    something_else: "hello"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates[0].name, "a");
}

#[test]
fn test_unknown_nested_fields_ignored() {
    let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
  unknown_default_field: "ignored"
changelog:
  sort: asc
  mystery_option: true
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    checksum:
      algorithm: sha256
      future_field: "ignored"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config
            .defaults
            .as_ref()
            .unwrap()
            .targets
            .as_ref()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        config.changelog.as_ref().unwrap().sort,
        Some("asc".to_string())
    );
    assert_eq!(
        config.crates[0].checksum.as_ref().unwrap().algorithm,
        Some("sha256".to_string())
    );
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

// ---- WAVE 5.7 behavior-toggle test (SCH-26) ----

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
    // SCH-11 (DEC-5 hard-break): `filename:` is the canonical field name.
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
    // Mirrors GR's own `smtp:` → `email:` rename (GR keeps both as
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

// ---- Legacy-field rejection tests (post-DEC-5 hard-break shape) ----

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
    docker_v2:
      - images: [registry/img]
        tags: ["{{ .Version }}"]
        dockerfile: Dockerfile
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].docker_v2.is_some());
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
    // `skip:` is the canonical per-config gating field (DEC-6); known-good
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

#[test]
fn test_notarize_macos_legacy_enabled_rejected() {
    // `deny_unknown_fields` must reject legacy `enabled: true` on
    // MacOSSignNotarizeConfig — without it, the field would silently
    // drop and produce a confusing no-op pipeline run.
    let yaml = r#"
notarize:
  macos:
    - enabled: true
      sign:
        certificate: /tmp/cert.p12
        password: pw
crates: []
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "legacy `enabled:` on MacOSSignNotarizeConfig must be rejected by deny_unknown_fields"
    );
}

#[test]
fn test_notarize_macos_native_legacy_enabled_rejected() {
    // Same `deny_unknown_fields` check for MacOSNativeSignNotarizeConfig.
    let yaml = r#"
notarize:
  macos_native:
    - enabled: true
crates: []
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "legacy `enabled:` on MacOSNativeSignNotarizeConfig must be rejected by deny_unknown_fields"
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
    // are required (DEC-5 dropped the `source`/`destination` aliases).
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
    // `key_passphrase:` is the only accepted spelling (DEC-5 dropped
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
#[serial_test::serial]
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

    // Temporarily unset any existing tokens to avoid interference
    let orig_gh = std::env::var("GITHUB_TOKEN").ok();
    let orig_gl = std::env::var("GITLAB_TOKEN").ok();
    let orig_gt = std::env::var("GITEA_TOKEN").ok();
    // SAFETY: test runs serially
    unsafe {
        std::env::remove_var("GITHUB_TOKEN");
        std::env::remove_var("GITLAB_TOKEN");
        std::env::remove_var("GITEA_TOKEN");
    }

    let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
    let vars = load_token_files(&config, &log).unwrap();

    // Restore original env
    unsafe {
        if let Some(v) = orig_gh {
            std::env::set_var("GITHUB_TOKEN", v);
        }
        if let Some(v) = orig_gl {
            std::env::set_var("GITLAB_TOKEN", v);
        }
        if let Some(v) = orig_gt {
            std::env::set_var("GITEA_TOKEN", v);
        }
    }

    assert_eq!(vars.get("GITHUB_TOKEN").unwrap(), "ghp_test123");
    assert_eq!(vars.get("GITLAB_TOKEN").unwrap(), "glpat-test456");
    // GITEA_TOKEN not present — default file doesn't exist
    assert!(!vars.contains_key("GITEA_TOKEN"));
}

#[test]
#[serial_test::serial]
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

    // Set GITHUB_TOKEN env var — should take precedence over file
    let orig = std::env::var("GITHUB_TOKEN").ok();
    // SAFETY: test runs serially
    unsafe {
        std::env::set_var("GITHUB_TOKEN", "env_token");
    }

    let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
    let vars = load_token_files(&config, &log).unwrap();

    // Restore
    unsafe {
        match orig {
            Some(v) => std::env::set_var("GITHUB_TOKEN", v),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
    }

    // File token should NOT be loaded because env var was set
    assert!(
        !vars.contains_key("GITHUB_TOKEN"),
        "env var should take precedence; file should not be loaded"
    );
}

#[test]
fn test_read_token_file_tilde_expansion() {
    // Test that tilde expansion uses HOME env var
    let dir = tempfile::TempDir::new().unwrap();
    let token_path = dir.path().join(".config/goreleaser/github_token");
    std::fs::create_dir_all(token_path.parent().unwrap()).unwrap();
    std::fs::write(&token_path, "tilde_token\n").unwrap();

    let orig_home = std::env::var("HOME").ok();
    // SAFETY: test runs serially
    unsafe {
        std::env::set_var("HOME", dir.path());
    }

    let result = read_token_file("~/.config/goreleaser/github_token").unwrap();

    unsafe {
        match orig_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

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
    // After WAVE 2, defaults.ignore moved to defaults.builds.ignore
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
    // After WAVE 2, defaults.overrides moved to defaults.builds.overrides
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
    let schema = schemars::schema_for!(Config);
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

// ---- defaults axis-mismatch validation tests (DEC-4) ----

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
// dropping. Use the canonical `skip:` (DEC-6) to suppress the publish step.
#[test]
fn test_docker_v2_skip_push_rejected() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    docker_v2:
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
    assert_eq!(dists.get("deb").unwrap(), "ubuntu/focal");
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
    // After WAVE 2, defaults.overrides moved under defaults.builds.overrides.
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

// ---- F2: `disable` → `skip` serde aliases (GR import compat) ----
//
// GoReleaser's docker_v2/snapcraft/nsis/msi/release configs use `disable:`;
// anodizer renamed to `skip:` per DEC-6. With `deny_unknown_fields` on the
// strictly-validated structs, an imported GR YAML carrying `disable:`
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
    docker_v2:
      - images: [ghcr.io/example/app]
        disable: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("disable: alias must parse");
    let docker = &config.crates[0].docker_v2.as_ref().unwrap()[0];
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

// ---- M8: goamd64 field on DMG/MSI/NSIS/nfpm ----
//
// Mirrors GR's `Goamd64` field; previously absent on these surfaces, so
// multi-amd64-variant builds couldn't filter. Tests assert the YAML round-
// trips into the new struct field. Stage-level wiring (filter the artifact
// set against the configured variant) lives in stage-{dmg,msi,nsis,nfpm}/
// and is tracked separately — this commit adds the surface only.

#[test]
fn test_dmg_goamd64_field_deserializes() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    dmgs:
      - id: my_dmg
        goamd64: v3
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("dmg goamd64 must parse");
    let dmg = &config.crates[0].dmgs.as_ref().unwrap()[0];
    assert_eq!(dmg.goamd64.as_deref(), Some("v3"));
}

#[test]
fn test_msi_goamd64_field_deserializes() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - id: my_msi
        goamd64: v2
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("msi goamd64 must parse");
    let msi = &config.crates[0].msis.as_ref().unwrap()[0];
    assert_eq!(msi.goamd64.as_deref(), Some("v2"));
}

#[test]
fn test_nsis_goamd64_field_deserializes() {
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nsis:
      - id: my_nsis
        goamd64: v4
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("nsis goamd64 must parse");
    let nsis = &config.crates[0].nsis.as_ref().unwrap()[0];
    assert_eq!(nsis.goamd64.as_deref(), Some("v4"));
}

#[test]
fn test_nfpm_goamd64_field_deserializes_as_list() {
    // GR nfpm uses `[]string` (multi-variant filter), not `string`.
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    nfpms:
      - id: my_nfpm
        formats: [deb]
        goamd64: [v2, v3]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("nfpm goamd64 list must parse");
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert_eq!(
        nfpm.goamd64.as_deref(),
        Some(&[String::from("v2"), String::from("v3")][..])
    );
}

#[test]
fn test_nfpm_goamd64_omitted_is_none() {
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
    let config: Config = serde_yaml_ng::from_str(yaml).expect("nfpm without goamd64 must parse");
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert!(nfpm.goamd64.is_none());
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
