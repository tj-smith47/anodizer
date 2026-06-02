//! Config parsing depth tests — every field, every variation.
//!
//! Tests extracted from config.rs to keep file sizes manageable.
//! All tests use the public API (serde_yaml_ng::from_str / toml::from_str).

use std::path::PathBuf;

use anodizer_core::config::*;

// ====================================================================
// Config parsing depth — every field, every variation
// ====================================================================

// ---- project_name tests ----

#[test]
fn test_parse_project_name_valid() {
    let yaml = "project_name: my-cool-project\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.project_name, "my-cool-project");
}

#[test]
fn test_parse_project_name_empty_string() {
    let yaml = "project_name: \"\"\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.project_name, "");
}

#[test]
fn test_parse_project_name_default_omitted() {
    let yaml = "crates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.project_name, "");
}

#[test]
fn test_parse_project_name_special_characters() {
    let yaml = "project_name: \"my project @v2.0 (beta)\"\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.project_name, "my project @v2.0 (beta)");
}

#[test]
fn test_parse_project_name_unicode() {
    let yaml = "project_name: \"projet-\u{00e9}t\u{00e9}\"\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.project_name, "projet-\u{00e9}t\u{00e9}");
}

#[test]
fn test_parse_project_name_number_coerced() {
    // YAML coerces bare numbers to strings for serde string fields
    let yaml = "project_name: 12345\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.project_name, "12345");
}

// ---- dist tests ----

#[test]
fn test_parse_dist_valid() {
    let yaml = "project_name: test\ndist: ./output\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.dist, PathBuf::from("./output"));
}

#[test]
fn test_parse_dist_default_omitted() {
    let yaml = "project_name: test\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.dist, PathBuf::from("./dist"));
}

#[test]
fn test_parse_dist_custom_absolute_path() {
    let yaml = "project_name: test\ndist: /tmp/my-release\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.dist, PathBuf::from("/tmp/my-release"));
}

#[test]
fn test_parse_dist_path_with_spaces() {
    let yaml = "project_name: test\ndist: \"./my dist folder\"\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.dist, PathBuf::from("./my dist folder"));
}

#[test]
fn test_parse_dist_empty_string() {
    let yaml = "project_name: test\ndist: \"\"\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.dist, PathBuf::from(""));
}

// ---- defaults.targets tests ----

#[test]
fn test_parse_defaults_targets_valid() {
    let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-apple-darwin
    - x86_64-pc-windows-msvc
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let targets = config.defaults.unwrap().targets.unwrap();
    assert_eq!(targets.len(), 3);
    assert_eq!(targets[0], "x86_64-unknown-linux-gnu");
    assert_eq!(targets[1], "aarch64-apple-darwin");
    assert_eq!(targets[2], "x86_64-pc-windows-msvc");
}

#[test]
fn test_parse_defaults_targets_empty_array() {
    let yaml = r#"
project_name: test
defaults:
  targets: []
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let targets = config.defaults.unwrap().targets.unwrap();
    assert!(targets.is_empty());
}

#[test]
fn test_parse_defaults_targets_omitted() {
    let yaml = r#"
project_name: test
defaults:
  cross: auto
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.defaults.unwrap().targets, None);
}

#[test]
fn test_parse_defaults_targets_invalid_type_string() {
    let yaml = r#"
project_name: test
defaults:
  targets: "not-an-array"
crates: []
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn test_parse_defaults_targets_single_target() {
    let yaml = r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-musl
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let targets = config.defaults.unwrap().targets.unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0], "x86_64-unknown-linux-musl");
}

#[test]
fn test_parse_defaults_targets_arbitrary_strings_accepted() {
    // Config parsing accepts any string; validation is separate
    let yaml = r#"
project_name: test
defaults:
  targets:
    - "completely-invalid-triple"
    - ""
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let targets = config.defaults.unwrap().targets.unwrap();
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0], "completely-invalid-triple");
    assert_eq!(targets[1], "");
}

// ---- defaults.cross tests ----

#[test]
fn test_parse_defaults_cross_auto() {
    let yaml = "project_name: test\ndefaults:\n  cross: auto\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.defaults.unwrap().cross, Some(CrossStrategy::Auto));
}

#[test]
fn test_parse_defaults_cross_zigbuild() {
    let yaml = "project_name: test\ndefaults:\n  cross: zigbuild\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.defaults.unwrap().cross,
        Some(CrossStrategy::Zigbuild)
    );
}

#[test]
fn test_parse_defaults_cross_cross() {
    let yaml = "project_name: test\ndefaults:\n  cross: cross\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.defaults.unwrap().cross, Some(CrossStrategy::Cross));
}

#[test]
fn test_parse_defaults_cross_cargo() {
    let yaml = "project_name: test\ndefaults:\n  cross: cargo\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.defaults.unwrap().cross, Some(CrossStrategy::Cargo));
}

#[test]
fn test_parse_defaults_cross_omitted() {
    let yaml = "project_name: test\ndefaults:\n  targets: []\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.defaults.unwrap().cross, None);
}

#[test]
fn test_parse_defaults_cross_invalid_value() {
    let yaml = "project_name: test\ndefaults:\n  cross: docker\ncrates: []";
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("unknown variant") || err.contains("docker"));
}

#[test]
fn test_parse_defaults_cross_case_sensitive() {
    // CrossStrategy uses rename_all = "lowercase" so "Auto" should fail
    let yaml = "project_name: test\ndefaults:\n  cross: Auto\ncrates: []";
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

// ---- defaults.builds.flags tests ----
// (Per-build settings live under defaults.builds.* rather than flat on
// defaults — this mirrors BuildConfig's shape.)

#[test]
fn test_parse_defaults_flags_valid() {
    let yaml = r#"
project_name: test
defaults:
  builds:
    flags:
      - "--release"
      - "--locked"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.defaults.unwrap().builds.unwrap().flags,
        Some(vec!["--release".to_string(), "--locked".to_string()])
    );
}

#[test]
fn test_parse_defaults_flags_omitted() {
    let yaml = "project_name: test\ndefaults:\n  cross: auto\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.defaults.unwrap().builds.is_none());
}

#[test]
fn test_parse_defaults_flags_empty_list() {
    // Explicit `flags: []` is the canonical way to override a default to a
    // debug build; the legacy `flags: ""` string form is rejected in
    // favour of typed lists.
    let yaml =
        "project_name: test\ndefaults:\n  builds:\n    binary: \"\"\n    flags: []\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.defaults.unwrap().builds.unwrap().flags,
        Some(Vec::<String>::new())
    );
}

// ---- defaults.archives tests ----

#[test]
fn test_parse_defaults_archives_format() {
    let yaml = r#"
project_name: test
defaults:
  archives:
    formats: [zip]
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let archives = config.defaults.unwrap().archives.unwrap();
    assert_eq!(archives.formats.as_deref(), Some(&["zip".to_string()][..]));
}

#[test]
fn test_parse_defaults_archives_format_overrides() {
    let yaml = r#"
project_name: test
defaults:
  archives:
    formats: [tar.gz]
    format_overrides:
      - os: windows
        formats: [zip]
      - os: darwin
        formats: [tar.xz]
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let archives = config.defaults.unwrap().archives.unwrap();
    let overrides = archives.format_overrides.unwrap();
    assert_eq!(overrides.len(), 2);
    assert_eq!(overrides[0].os, "windows");
    assert_eq!(
        overrides[0].formats.as_deref(),
        Some(&["zip".to_string()][..])
    );
    assert_eq!(overrides[1].os, "darwin");
    assert_eq!(
        overrides[1].formats.as_deref(),
        Some(&["tar.xz".to_string()][..])
    );
}

#[test]
fn test_parse_defaults_archives_omitted() {
    let yaml = "project_name: test\ndefaults:\n  cross: auto\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.defaults.unwrap().archives.is_none());
}

/// `archives[].ids` accepts the canonical key (the `builds:` alias).
#[test]
fn test_parse_archives_ids_canonical() {
    use anodizer_core::config::ArchivesConfig;
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ .Version }}"
    archives:
      - formats: [tar.gz]
        ids: [myid, otherid]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let archives = &config.crates[0].archives;
    match archives {
        ArchivesConfig::Configs(v) => {
            assert_eq!(v.len(), 1);
            assert_eq!(
                v[0].ids,
                Some(vec!["myid".to_string(), "otherid".to_string()])
            );
        }
        _ => panic!("expected Configs variant, got {archives:?}"),
    }
}

// ---- defaults.checksum tests ----

#[test]
fn test_parse_defaults_checksum_algorithm_sha256() {
    let yaml = r#"
project_name: test
defaults:
  checksum:
    algorithm: sha256
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.defaults.unwrap().checksum.unwrap();
    assert_eq!(checksum.algorithm, Some("sha256".to_string()));
}

#[test]
fn test_parse_defaults_checksum_algorithm_sha512() {
    let yaml = r#"
project_name: test
defaults:
  checksum:
    algorithm: sha512
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.defaults.unwrap().checksum.unwrap();
    assert_eq!(checksum.algorithm, Some("sha512".to_string()));
}

#[test]
fn test_parse_defaults_checksum_algorithm_blake2b() {
    let yaml = r#"
project_name: test
defaults:
  checksum:
    algorithm: blake2b
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.defaults.unwrap().checksum.unwrap();
    assert_eq!(checksum.algorithm, Some("blake2b".to_string()));
}

#[test]
fn test_parse_defaults_checksum_all_fields() {
    let yaml = r#"
project_name: test
defaults:
  checksum:
    name_template: "checksums-{{ version }}.txt"
    algorithm: sha256
    skip: false
    extra_files:
      - "dist/extra.sig"
    ids:
      - my-archive
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.defaults.unwrap().checksum.unwrap();
    assert_eq!(
        checksum.name_template,
        Some("checksums-{{ version }}.txt".to_string())
    );
    assert_eq!(checksum.algorithm, Some("sha256".to_string()));
    assert_eq!(
        checksum.skip,
        Some(anodizer_core::config::StringOrBool::Bool(false))
    );
    assert_eq!(checksum.extra_files.as_ref().unwrap().len(), 1);
    assert_eq!(checksum.ids.as_ref().unwrap(), &["my-archive"]);
}

#[test]
fn test_parse_defaults_checksum_omitted() {
    let yaml = "project_name: test\ndefaults:\n  cross: auto\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.defaults.unwrap().checksum.is_none());
}

// ---- crates[].depends_on tests ----

#[test]
fn test_parse_crate_depends_on_valid() {
    let yaml = r#"
project_name: test
crates:
  - name: lib-a
    path: crates/lib-a
    tag_template: "v{{ version }}"
  - name: app-b
    path: crates/app-b
    tag_template: "v{{ version }}"
    depends_on:
      - lib-a
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let deps = config.crates[1].depends_on.as_ref().unwrap();
    assert_eq!(deps, &["lib-a"]);
}

#[test]
fn test_parse_crate_depends_on_multiple() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    depends_on:
      - core
      - utils
      - macros
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let deps = config.crates[0].depends_on.as_ref().unwrap();
    assert_eq!(deps.len(), 3);
    assert_eq!(deps[0], "core");
    assert_eq!(deps[1], "utils");
    assert_eq!(deps[2], "macros");
}

#[test]
fn test_parse_crate_depends_on_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: standalone
    path: "."
    tag_template: "v{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates[0].depends_on, None);
}

#[test]
fn test_parse_crate_depends_on_empty_array() {
    let yaml = r#"
project_name: test
crates:
  - name: standalone
    path: "."
    tag_template: "v{{ version }}"
    depends_on: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let deps = config.crates[0].depends_on.as_ref().unwrap();
    assert!(deps.is_empty());
}

#[test]
fn test_parse_crate_depends_on_invalid_type() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    depends_on: "not-an-array"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

// ---- crates[].tag_template tests ----

#[test]
fn test_parse_crate_tag_template_tera_syntax() {
    let yaml = r#"
project_name: test
crates:
  - name: my-crate
    path: "."
    tag_template: "{{ crate_name }}/v{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.crates[0].tag_template,
        "{{ crate_name }}/v{{ version }}"
    );
}

#[test]
fn test_parse_crate_tag_template_go_style() {
    let yaml = r#"
project_name: test
crates:
  - name: my-crate
    path: "."
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates[0].tag_template, "v{{ .Version }}");
}

#[test]
fn test_parse_crate_tag_template_empty() {
    let yaml = r#"
project_name: test
crates:
  - name: my-crate
    path: "."
    tag_template: ""
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates[0].tag_template, "");
}

#[test]
fn test_parse_crate_tag_template_default_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: my-crate
    path: "."
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    // Default is empty string per CrateConfig::default()
    assert_eq!(config.crates[0].tag_template, "");
}

#[test]
fn test_parse_crate_tag_template_with_prefix() {
    let yaml = r#"
project_name: test
crates:
  - name: my-crate
    path: "."
    tag_template: "my-crate/v{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates[0].tag_template, "my-crate/v{{ version }}");
}

// ---- builds[].copy_from tests ----

#[test]
fn test_parse_build_copy_from_valid() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
        copy_from: other-crate
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.copy_from, Some("other-crate".to_string()));
}

#[test]
fn test_parse_build_copy_from_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.copy_from, None);
}

#[test]
fn test_parse_build_copy_from_empty_string() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
        copy_from: ""
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.copy_from, Some(String::new()));
}

// ---- builds[].env tests ----

#[test]
fn test_parse_build_env_per_target() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
        env:
          x86_64-unknown-linux-gnu:
            CC: gcc
            CFLAGS: "-O2"
          aarch64-unknown-linux-gnu:
            CC: aarch64-linux-gnu-gcc
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    let env = build.env.as_ref().unwrap();
    assert_eq!(env.len(), 2);
    let linux_env = env.get("x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(linux_env.get("CC").unwrap(), "gcc");
    assert_eq!(linux_env.get("CFLAGS").unwrap(), "-O2");
    let arm_env = env.get("aarch64-unknown-linux-gnu").unwrap();
    assert_eq!(arm_env.get("CC").unwrap(), "aarch64-linux-gnu-gcc");
}

#[test]
fn test_parse_build_env_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.env, None);
}

#[test]
fn test_parse_build_env_empty_map() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
        env: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    let env = build.env.as_ref().unwrap();
    assert!(env.is_empty());
}

// ---- builds[].flags tests ----

#[test]
fn test_parse_build_flags_valid() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
        flags:
          - "--release"
          - "--locked"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(
        build.flags,
        Some(vec!["--release".to_string(), "--locked".to_string()])
    );
}

#[test]
fn test_parse_build_flags_string_form_rejected() {
    // The legacy `flags: "--release"` string form is gone — `flags` is
    // strictly a typed list now so quoted shell args round-trip as discrete
    // argv tokens (no shell-splitting at template-render time).
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
        flags: "--release --locked"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "string-form flags must be rejected (use a list)"
    );
}

#[test]
fn test_parse_build_flags_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.flags, None);
}

// ---- builds[].features / no_default_features tests ----

#[test]
fn test_parse_build_features_valid() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
        features:
          - tls
          - json
        no_default_features: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.features.as_ref().unwrap(), &["tls", "json"]);
    assert_eq!(build.no_default_features, Some(true));
}

#[test]
fn test_parse_build_features_empty_array() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
        features: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert!(build.features.as_ref().unwrap().is_empty());
}

#[test]
fn test_parse_build_no_default_features_false() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
        no_default_features: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.no_default_features, Some(false));
}

// ---- builds[] multiple builds tests ----

#[test]
fn test_parse_multiple_builds() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app-cli
        features:
          - cli
      - binary: app-server
        features:
          - server
        targets:
          - x86_64-unknown-linux-gnu
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let builds = config.crates[0].builds.as_ref().unwrap();
    assert_eq!(builds.len(), 2);
    assert_eq!(builds[0].binary.as_deref(), Some("app-cli"));
    assert_eq!(builds[1].binary.as_deref(), Some("app-server"));
    assert_eq!(
        builds[1].targets.as_ref().unwrap(),
        &["x86_64-unknown-linux-gnu"]
    );
}

#[test]
fn test_parse_builds_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].builds.is_none());
}

// ---- archive.binaries tests ----

#[test]
fn test_parse_archive_binaries_valid() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - binaries:
          - app-cli
          - app-server
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        let binaries = configs[0].binaries.as_ref().unwrap();
        assert_eq!(binaries, &["app-cli", "app-server"]);
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_binaries_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [tar.gz]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(configs[0].binaries, None);
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_binaries_empty_array() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - binaries: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert!(configs[0].binaries.as_ref().unwrap().is_empty());
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

// ---- archive.formats tests ----

#[test]
fn test_parse_archive_formats_tar_gz() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [tar.gz]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(
            configs[0].formats.as_deref(),
            Some(&["tar.gz".to_string()][..])
        );
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_formats_zip() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [zip]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(
            configs[0].formats.as_deref(),
            Some(&["zip".to_string()][..])
        );
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_formats_tar_xz() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [tar.xz]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(
            configs[0].formats.as_deref(),
            Some(&["tar.xz".to_string()][..])
        );
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_formats_invalid_accepted_at_parse_time() {
    // Config parsing accepts any string; validation happens later
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [not-a-real-format]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(
            configs[0].formats.as_deref(),
            Some(&["not-a-real-format".to_string()][..])
        );
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

// ---- archive.format_overrides tests ----

#[test]
fn test_parse_archive_format_overrides_multiple() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [tar.gz]
        format_overrides:
          - os: windows
            formats: [zip]
          - os: darwin
            formats: [tar.xz]
          - os: linux
            formats: [tar.zst]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        let overrides = configs[0].format_overrides.as_ref().unwrap();
        assert_eq!(overrides.len(), 3);
        assert_eq!(overrides[0].os, "windows");
        assert_eq!(
            overrides[0].formats.as_deref(),
            Some(&["zip".to_string()][..])
        );
        assert_eq!(overrides[2].os, "linux");
        assert_eq!(
            overrides[2].formats.as_deref(),
            Some(&["tar.zst".to_string()][..])
        );
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_format_overrides_unknown_os_accepted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - format_overrides:
          - os: freebsd
            formats: [tar.gz]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        let overrides = configs[0].format_overrides.as_ref().unwrap();
        assert_eq!(overrides[0].os, "freebsd");
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_format_overrides_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [tar.gz]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert!(configs[0].format_overrides.is_none());
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

// ---- archive.files tests ----

#[test]
fn test_parse_archive_files_glob_patterns() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - files:
          - LICENSE*
          - "README.md"
          - "docs/**/*.md"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        let files = configs[0].files.as_ref().unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0], "LICENSE*");
        assert_eq!(files[2], "docs/**/*.md");
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_files_empty_array() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - files: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert!(configs[0].files.as_ref().unwrap().is_empty());
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_files_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [tar.gz]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(configs[0].files, None);
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

// ---- archive.wrap_in_directory tests ----

#[test]
fn test_parse_archive_wrap_in_directory() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - wrap_in_directory: "my-app-{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(
            configs[0].wrap_in_directory,
            Some(WrapInDirectory::Name("my-app-{{ version }}".to_string()))
        );
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_wrap_in_directory_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [tar.gz]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(configs[0].wrap_in_directory, None);
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

// ---- archives disabled vs configs vs default tests ----

#[test]
fn test_parse_archives_as_array() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [tar.gz]
        name_template: "{{ project_name }}-{{ version }}"
      - formats: [zip]
        name_template: "{{ project_name }}-{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(configs.len(), 2);
        assert_eq!(
            configs[0].formats.as_deref(),
            Some(&["tar.gz".to_string()][..])
        );
        assert_eq!(
            configs[1].formats.as_deref(),
            Some(&["zip".to_string()][..])
        );
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archives_omitted_is_empty_configs() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert!(configs.is_empty());
    } else {
        panic!("expected ArchivesConfig::Configs (empty)");
    }
}

#[test]
fn test_parse_archives_null_is_empty_configs() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives: null
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert!(configs.is_empty());
    } else {
        panic!("expected ArchivesConfig::Configs for null");
    }
}

// ---- checksum per-crate all algorithms ----

#[test]
fn test_parse_checksum_algorithm_strings() {
    for algo in &["sha256", "sha384", "sha512", "blake2b", "md5", "crc32"] {
        let yaml = format!(
            r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{{{ version }}}}"
    checksum:
      algorithm: {algo}
"#
        );
        let config: Config = serde_yaml_ng::from_str(&yaml).unwrap();
        let checksum = config.crates[0].checksum.as_ref().unwrap();
        assert_eq!(checksum.algorithm, Some(algo.to_string()));
    }
}

#[test]
fn test_parse_checksum_name_template() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    checksum:
      name_template: "{{ project_name }}-checksums.txt"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.crates[0].checksum.as_ref().unwrap();
    assert_eq!(
        checksum.name_template,
        Some("{{ project_name }}-checksums.txt".to_string())
    );
}

#[test]
fn test_parse_checksum_extra_files() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    checksum:
      extra_files:
        - "dist/*.sig"
        - "dist/*.asc"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.crates[0].checksum.as_ref().unwrap();
    let files = checksum.extra_files.as_ref().unwrap();
    assert_eq!(files.len(), 2);
}

#[test]
fn test_parse_checksum_ids() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    checksum:
      ids:
        - my-archive
        - my-binary
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.crates[0].checksum.as_ref().unwrap();
    assert_eq!(checksum.ids.as_ref().unwrap(), &["my-archive", "my-binary"]);
}

#[test]
fn test_parse_checksum_disable_with_other_fields() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    checksum:
      skip: true
      algorithm: sha512
      name_template: "ignored.txt"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.crates[0].checksum.as_ref().unwrap();
    assert_eq!(
        checksum.skip,
        Some(anodizer_core::config::StringOrBool::Bool(true))
    );
    // Other fields are still parsed even when disabled
    assert_eq!(checksum.algorithm, Some("sha512".to_string()));
}

// ---- nfpm tests ----

#[test]
fn test_parse_nfpm_basic() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - package_name: my-app
        formats:
          - deb
          - rpm
        vendor: "My Company"
        homepage: "https://example.com"
        maintainer: "dev@example.com"
        description: "A test application"
        license: MIT
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert_eq!(nfpm.package_name, Some("my-app".to_string()));
    assert_eq!(nfpm.formats, vec!["deb", "rpm"]);
    assert_eq!(nfpm.vendor, Some("My Company".to_string()));
    assert_eq!(nfpm.homepage, Some("https://example.com".to_string()));
    assert_eq!(nfpm.maintainer, Some("dev@example.com".to_string()));
    assert_eq!(nfpm.description, Some("A test application".to_string()));
    assert_eq!(nfpm.license, Some("MIT".to_string()));
}

#[test]
fn test_parse_nfpm_file_name_template() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
        file_name_template: "{{ package_name }}_{{ version }}_{{ arch }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert_eq!(
        nfpm.file_name_template,
        Some("{{ package_name }}_{{ version }}_{{ arch }}".to_string())
    );
}

#[test]
fn test_parse_nfpm_file_name_template_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert_eq!(nfpm.file_name_template, None);
}

#[test]
fn test_parse_nfpm_overrides() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
          - rpm
        overrides:
          deb:
            depends:
              - libc6
          rpm:
            depends:
              - glibc
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    let overrides = nfpm.overrides.as_ref().unwrap();
    assert!(overrides.contains_key("deb"));
    assert!(overrides.contains_key("rpm"));
}

#[test]
fn test_parse_nfpm_overrides_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert_eq!(nfpm.overrides, None);
}

#[test]
fn test_parse_nfpm_formats_multiple() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
          - rpm
          - apk
          - archlinux
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert_eq!(nfpm.formats.len(), 4);
    assert_eq!(nfpm.formats[3], "archlinux");
}

#[test]
fn test_parse_nfpm_formats_empty() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert!(nfpm.formats.is_empty());
}

#[test]
fn test_parse_nfpm_contents() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
        contents:
          - src: "./app"
            dst: "/usr/bin/app"
          - src: "./config.toml"
            dst: "/etc/app/config.toml"
            type: config
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    let contents = nfpm.contents.as_ref().unwrap();
    assert_eq!(contents.len(), 2);
    assert_eq!(contents[0].src, "./app");
    assert_eq!(contents[0].dst, "/usr/bin/app");
    assert_eq!(contents[0].content_type, None);
    assert_eq!(contents[1].content_type, Some("config".to_string()));
}

#[test]
fn test_parse_nfpm_scripts() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
        scripts:
          preinstall: ./scripts/preinstall.sh
          postinstall: ./scripts/postinstall.sh
          preremove: ./scripts/preremove.sh
          postremove: ./scripts/postremove.sh
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    let scripts = nfpm.scripts.as_ref().unwrap();
    assert_eq!(
        scripts.preinstall,
        Some("./scripts/preinstall.sh".to_string())
    );
    assert_eq!(
        scripts.postinstall,
        Some("./scripts/postinstall.sh".to_string())
    );
    assert_eq!(
        scripts.preremove,
        Some("./scripts/preremove.sh".to_string())
    );
    assert_eq!(
        scripts.postremove,
        Some("./scripts/postremove.sh".to_string())
    );
}

#[test]
fn test_parse_nfpm_dependencies() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
        dependencies:
          deb:
            - libc6
            - libssl3
          rpm:
            - glibc
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    let deps = nfpm.dependencies.as_ref().unwrap();
    assert_eq!(deps.get("deb").unwrap().len(), 2);
    assert_eq!(deps.get("rpm").unwrap().len(), 1);
}

#[test]
fn test_parse_nfpm_recommends_suggests_conflicts() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
        recommends:
          - bash-completion
        suggests:
          - zsh
        conflicts:
          - old-app
        replaces:
          - old-app
        provides:
          - app-service
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert_eq!(nfpm.recommends.as_ref().unwrap(), &["bash-completion"]);
    assert_eq!(nfpm.suggests.as_ref().unwrap(), &["zsh"]);
    assert_eq!(nfpm.conflicts.as_ref().unwrap(), &["old-app"]);
    assert_eq!(nfpm.replaces.as_ref().unwrap(), &["old-app"]);
    assert_eq!(nfpm.provides.as_ref().unwrap(), &["app-service"]);
}

#[test]
fn test_parse_nfpm_bindir() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
        bindir: /usr/local/bin
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert_eq!(nfpm.bindir, Some("/usr/local/bin".to_string()));
}

#[test]
fn test_parse_nfpm_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].nfpms.is_none());
}

// ---- publish.homebrew tests ----

#[test]
fn test_parse_publish_homebrew_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    publish:
      cargo: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(
        config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .homebrew
            .is_none()
    );
}

// ---- publish.scoop tests ----

#[test]
fn test_parse_publish_scoop_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    publish: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].publish.as_ref().unwrap().scoop.is_none());
}

// ---- publish.cargo edge cases ----

#[test]
fn test_parse_publish_cargo_skip_true() {
    // Opt-out via the peer-publisher `skip` field.
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    publish:
      cargo:
        skip: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cargo = config.crates[0]
        .publish
        .as_ref()
        .unwrap()
        .cargo
        .as_ref()
        .unwrap();
    assert_eq!(cargo.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_parse_publish_cargo_empty_object_opts_in() {
    // Presence of `cargo: {}` is the canonical opt-in (no `enabled` field).
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    publish:
      cargo: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let publish = config.crates[0].publish.as_ref().unwrap();
    assert!(publish.cargo.is_some());
    let cargo = publish.cargo.as_ref().unwrap();
    assert_eq!(cargo.index_timeout, None);
    assert_eq!(cargo.skip, None);
}

#[test]
fn test_parse_publish_cargo_omitted_means_disabled() {
    // Omitting `cargo:` means "do not publish to crates.io".
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    publish: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let publish = config.crates[0].publish.as_ref().unwrap();
    assert!(publish.cargo.is_none());
}

#[test]
fn test_parse_publish_omitted_entirely() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].publish.is_none());
}

#[test]
fn test_parse_publish_cargo_bool_form_rejected() {
    // The bool shorthand `cargo: true` was removed now.
    // The only valid forms are `cargo: {}` (opt-in) or `cargo: { skip: true }` (opt-out).
    let yaml = r#"
project_name: test
crates:
  - name: foo
    publish:
      cargo: true
"#;
    let result = serde_yaml_ng::from_str::<Config>(yaml);
    assert!(
        result.is_err(),
        "publish.cargo: true must be rejected (bool shorthand removed)"
    );
}

#[test]
fn test_parse_publish_crates_key_rejected_after_rename() {
    // The `crates:` publish key was renamed to `cargo:` now.
    // Using the old name must fail with an unknown-field error.
    let yaml = r#"
project_name: test
crates:
  - name: foo
    publish:
      crates: { skip: false }
"#;
    let result = serde_yaml_ng::from_str::<Config>(yaml);
    assert!(
        result.is_err(),
        "publish.crates is renamed to publish.cargo"
    );
}

// ---- docker tests ----

// ---- publishers[] tests ----

#[test]
fn test_parse_publishers_valid() {
    let yaml = r#"
project_name: test
publishers:
  - name: s3-upload
    cmd: aws
    args:
      - s3
      - cp
    ids:
      - my-archive
    artifact_types:
      - archive
      - checksum
    env:
      - AWS_REGION=us-east-1
      - S3_BUCKET=my-releases
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let pub_cfg = &config.publishers.as_ref().unwrap()[0];
    assert_eq!(pub_cfg.name, Some("s3-upload".to_string()));
    assert_eq!(pub_cfg.cmd, "aws");
    assert_eq!(pub_cfg.args.as_ref().unwrap(), &["s3", "cp"]);
    assert_eq!(pub_cfg.ids.as_ref().unwrap(), &["my-archive"]);
    assert_eq!(
        pub_cfg.artifact_types.as_ref().unwrap(),
        &["archive", "checksum"]
    );
    let env = pub_cfg.env.as_ref().unwrap();
    assert!(env.contains(&"AWS_REGION=us-east-1".to_string()));
}

#[test]
fn test_parse_publishers_minimal() {
    let yaml = r#"
project_name: test
publishers:
  - cmd: publish.sh
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let pub_cfg = &config.publishers.as_ref().unwrap()[0];
    assert_eq!(pub_cfg.name, None);
    assert_eq!(pub_cfg.cmd, "publish.sh");
    assert_eq!(pub_cfg.args, None);
    assert_eq!(pub_cfg.ids, None);
    assert_eq!(pub_cfg.artifact_types, None);
    assert_eq!(pub_cfg.env, None);
}

#[test]
fn test_parse_publishers_multiple() {
    let yaml = r#"
project_name: test
publishers:
  - name: s3
    cmd: aws
  - name: gcs
    cmd: gsutil
  - name: custom
    cmd: ./scripts/publish.sh
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.publishers.as_ref().unwrap().len(), 3);
}

#[test]
fn test_parse_publishers_omitted() {
    let yaml = "project_name: test\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.publishers.is_none());
}

#[test]
fn test_parse_publishers_empty_array() {
    let yaml = "project_name: test\npublishers: []\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let publishers = config.publishers.as_ref().unwrap();
    assert!(publishers.is_empty());
}

// ---- before/after hooks tests ----

#[test]
fn test_parse_hooks_before() {
    let yaml = r#"
project_name: test
before:
  hooks:
    - "go mod tidy"
    - "make generate"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let before = config.before.as_ref().unwrap();
    let hooks = before.hooks.as_ref().unwrap();
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0], "go mod tidy");
    assert_eq!(hooks[1], "make generate");
}

#[test]
fn test_parse_hooks_after() {
    // Legacy `after.post:` is folded into `after.hooks:` at parse time
    // (back-compat alias for the legacy `after.hooks:`).
    let yaml = r#"
project_name: test
after:
  post:
    - "echo done"
    - "./scripts/post-release.sh"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let after = config.after.as_ref().unwrap();
    let hooks = after
        .hooks
        .as_ref()
        .expect("after.post should fold into after.hooks");
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0], "echo done");
    assert!(after.post.is_none(), "after.post should be empty post-fold");
}

#[test]
fn test_parse_hooks_after_canonical_hooks_spelling() {
    // Canonical `after.hooks:`.
    let yaml = r#"
project_name: test
after:
  hooks:
    - "echo done"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let after = config.after.as_ref().unwrap();
    let hooks = after.hooks.as_ref().unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0], "echo done");
}

#[test]
fn test_parse_hooks_both_before_and_after() {
    let yaml = r#"
project_name: test
before:
  hooks:
    - "pre-step"
after:
  post:
    - "post-step"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config
            .before
            .as_ref()
            .unwrap()
            .hooks
            .as_ref()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        config.after.as_ref().unwrap().hooks.as_ref().unwrap().len(),
        1,
        "after.post folds into after.hooks"
    );
}

#[test]
fn test_parse_hooks_omitted() {
    let yaml = "project_name: test\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.before.is_none());
    assert!(config.after.is_none());
}

#[test]
fn test_parse_hooks_empty_hooks_list() {
    let yaml = r#"
project_name: test
before:
  hooks: []
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(
        config
            .before
            .as_ref()
            .unwrap()
            .hooks
            .as_ref()
            .unwrap()
            .is_empty()
    );
}

// ---- release.name_template tests ----

#[test]
fn test_parse_release_name_template_valid() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    release:
      name_template: "Release {{ tag }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.name_template, Some("Release {{ tag }}".to_string()));
}

#[test]
fn test_parse_release_name_template_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    release:
      draft: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.name_template, None);
}

// ---- release.prerelease tests ----

#[test]
fn test_parse_release_prerelease_auto() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    release:
      prerelease: auto
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.prerelease, Some(PrereleaseConfig::Auto));
}

#[test]
fn test_parse_release_prerelease_true() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    release:
      prerelease: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.prerelease, Some(PrereleaseConfig::Bool(true)));
}

#[test]
fn test_parse_release_prerelease_false() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    release:
      prerelease: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.prerelease, Some(PrereleaseConfig::Bool(false)));
}

#[test]
fn test_parse_release_prerelease_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    release:
      draft: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.prerelease, None);
}

// ---- PrereleaseConfig serialization roundtrip ----

#[test]
fn test_prerelease_serialize_roundtrip() {
    let auto = PrereleaseConfig::Auto;
    let json = serde_json::to_string(&auto).unwrap();
    assert_eq!(json, "\"auto\"");

    let bool_true = PrereleaseConfig::Bool(true);
    let json = serde_json::to_string(&bool_true).unwrap();
    assert_eq!(json, "true");

    let bool_false = PrereleaseConfig::Bool(false);
    let json = serde_json::to_string(&bool_false).unwrap();
    assert_eq!(json, "false");
}

// ---- release.github tests ----

#[test]
fn test_parse_release_github_config() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    release:
      github:
        owner: my-org
        name: my-repo
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let github = config.crates[0]
        .release
        .as_ref()
        .unwrap()
        .github
        .as_ref()
        .unwrap();
    assert_eq!(github.owner, "my-org");
    assert_eq!(github.name, "my-repo");
}

#[test]
fn test_parse_release_github_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    release:
      draft: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.crates[0].release.as_ref().unwrap().github.is_none());
}

// ---- sign.signature tests ----

#[test]
fn test_parse_sign_signature_custom_template() {
    let yaml = r#"
project_name: test
signs:
  - artifacts: all
    cmd: gpg
    signature: "{{ .Artifact }}.custom.sig"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.signs[0].signature,
        Some("{{ .Artifact }}.custom.sig".to_string())
    );
}

#[test]
fn test_parse_sign_signature_omitted() {
    let yaml = r#"
project_name: test
signs:
  - artifacts: all
    cmd: gpg
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.signs[0].signature, None);
}

#[test]
fn test_parse_sign_signature_empty_string() {
    let yaml = r#"
project_name: test
signs:
  - artifacts: all
    cmd: gpg
    signature: ""
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.signs[0].signature, Some(String::new()));
}

// ---- docker_signs tests ----

#[test]
fn test_parse_docker_signs_valid() {
    let yaml = r#"
project_name: test
docker_signs:
  - artifacts: all
    cmd: cosign
    args:
      - sign
      - --yes
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let ds = &config.docker_signs.as_ref().unwrap()[0];
    assert_eq!(ds.artifacts, Some("all".to_string()));
    assert_eq!(ds.cmd, Some("cosign".to_string()));
    assert_eq!(ds.args.as_ref().unwrap(), &["sign", "--yes"]);
}

#[test]
fn test_parse_docker_signs_omitted() {
    let yaml = "project_name: test\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.docker_signs.is_none());
}

#[test]
fn test_parse_docker_signs_empty_array() {
    let yaml = "project_name: test\ndocker_signs: []\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.docker_signs.as_ref().unwrap().is_empty());
}

// ---- snapshot tests ----

#[test]
fn test_parse_snapshot_valid() {
    let yaml = r#"
project_name: test
snapshot:
  version_template: "{{ version }}-next"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.snapshot.as_ref().unwrap().version_template,
        "{{ version }}-next"
    );
}

#[test]
fn test_parse_snapshot_omitted() {
    let yaml = "project_name: test\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.snapshot.is_none());
}

// ---- announce tests ----

#[test]
fn test_parse_announce_discord() {
    let yaml = r#"
project_name: test
announce:
  discord:
    enabled: true
    webhook_url: "https://discord.com/api/webhooks/123/abc"
    message_template: "New release: {{ version }}"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let discord = config.announce.as_ref().unwrap().discord.as_ref().unwrap();
    assert_eq!(
        discord.enabled,
        Some(anodizer_core::config::StringOrBool::Bool(true))
    );
    assert_eq!(
        discord.webhook_url,
        Some("https://discord.com/api/webhooks/123/abc".to_string())
    );
    assert_eq!(
        discord.message_template,
        Some("New release: {{ version }}".to_string())
    );
}

#[test]
fn test_parse_announce_slack() {
    let yaml = r#"
project_name: test
announce:
  slack:
    enabled: true
    webhook_url: "https://hooks.slack.com/services/T00/B00/XXX"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let slack = config.announce.as_ref().unwrap().slack.as_ref().unwrap();
    assert_eq!(
        slack.enabled,
        Some(anodizer_core::config::StringOrBool::Bool(true))
    );
}

#[test]
fn test_parse_announce_webhook() {
    let yaml = r#"
project_name: test
announce:
  webhook:
    enabled: true
    endpoint_url: "https://api.example.com/webhook"
    content_type: application/json
    headers:
      Authorization: "Bearer token123"
    message_template: '{"version": "{{ version }}"}'
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let webhook = config.announce.as_ref().unwrap().webhook.as_ref().unwrap();
    assert_eq!(
        webhook.enabled,
        Some(anodizer_core::config::StringOrBool::Bool(true))
    );
    assert_eq!(
        webhook.endpoint_url,
        Some("https://api.example.com/webhook".to_string())
    );
    assert_eq!(webhook.content_type, Some("application/json".to_string()));
    let headers = webhook.headers.as_ref().unwrap();
    assert_eq!(headers.get("Authorization").unwrap(), "Bearer token123");
}

#[test]
fn test_parse_announce_omitted() {
    let yaml = "project_name: test\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.announce.is_none());
}

#[test]
fn test_parse_announce_empty() {
    let yaml = "project_name: test\nannounce: {}\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let announce = config.announce.as_ref().unwrap();
    assert!(announce.discord.is_none());
    assert!(announce.slack.is_none());
    assert!(announce.webhook.is_none());
}

// ---- changelog tests (additional edge cases) ----

#[test]
fn test_parse_changelog_sort_asc() {
    let yaml = r#"
project_name: test
changelog:
  sort: asc
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.changelog.as_ref().unwrap().sort,
        Some("asc".to_string())
    );
}

#[test]
fn test_parse_changelog_sort_desc() {
    let yaml = r#"
project_name: test
changelog:
  sort: desc
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.changelog.as_ref().unwrap().sort,
        Some("desc".to_string())
    );
}

#[test]
fn test_parse_changelog_filters() {
    let yaml = r#"
project_name: test
changelog:
  filters:
    exclude:
      - "^docs:"
      - "^chore:"
    include:
      - "^feat:"
      - "^fix:"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let filters = config.changelog.as_ref().unwrap().filters.as_ref().unwrap();
    assert_eq!(filters.exclude.as_ref().unwrap().len(), 2);
    assert_eq!(filters.include.as_ref().unwrap().len(), 2);
    assert_eq!(filters.exclude.as_ref().unwrap()[0], "^docs:");
}

#[test]
fn test_parse_changelog_groups() {
    let yaml = r#"
project_name: test
changelog:
  groups:
    - title: Features
      regexp: "^feat"
      order: 0
    - title: Bug Fixes
      regexp: "^fix"
      order: 1
    - title: Other
      order: 999
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let groups = config.changelog.as_ref().unwrap().groups.as_ref().unwrap();
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].title, "Features");
    assert_eq!(groups[0].regexp, Some("^feat".to_string()));
    assert_eq!(groups[0].order, Some(0));
    assert_eq!(groups[2].regexp, None);
}

#[test]
fn test_parse_changelog_use_source() {
    let yaml = r#"
project_name: test
changelog:
  use: github-native
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        config.changelog.as_ref().unwrap().use_source,
        Some("github-native".to_string())
    );
}

#[test]
fn test_parse_changelog_abbrev() {
    let yaml = r#"
project_name: test
changelog:
  abbrev: 12
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.changelog.as_ref().unwrap().abbrev, Some(12));
}

#[test]
fn test_parse_changelog_disable_with_groups() {
    let yaml = r#"
project_name: test
changelog:
  skip: true
  groups:
    - title: Features
      regexp: "^feat"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cl = config.changelog.as_ref().unwrap();
    assert_eq!(
        cl.skip,
        Some(anodizer_core::config::StringOrBool::Bool(true))
    );
    // Groups are still parsed even when disabled
    assert_eq!(cl.groups.as_ref().unwrap().len(), 1);
}

#[test]
fn test_parse_changelog_omitted() {
    let yaml = "project_name: test\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.changelog.is_none());
}

// ---- defaults section entirely omitted/empty ----

#[test]
fn test_parse_defaults_omitted() {
    let yaml = "project_name: test\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.defaults.is_none());
}

#[test]
fn test_parse_defaults_empty_object() {
    let yaml = "project_name: test\ndefaults: {}\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let defaults = config.defaults.as_ref().unwrap();
    assert!(defaults.targets.is_none());
    assert!(defaults.cross.is_none());
    assert!(defaults.builds.is_none());
    assert!(defaults.archives.is_none());
    assert!(defaults.checksum.is_none());
}

// ---- crate-level cross strategy ----

#[test]
fn test_parse_crate_cross_override() {
    let yaml = r#"
project_name: test
defaults:
  cross: auto
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    cross: zigbuild
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.defaults.unwrap().cross, Some(CrossStrategy::Auto));
    assert_eq!(config.crates[0].cross, Some(CrossStrategy::Zigbuild));
}

#[test]
fn test_parse_crate_cross_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates[0].cross, None);
}

// ---- multiple crates ----

#[test]
fn test_parse_multiple_crates() {
    let yaml = r#"
project_name: test
crates:
  - name: core
    path: crates/core
    tag_template: "core/v{{ version }}"
  - name: cli
    path: crates/cli
    tag_template: "cli/v{{ version }}"
    depends_on:
      - core
  - name: server
    path: crates/server
    tag_template: "server/v{{ version }}"
    depends_on:
      - core
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates.len(), 3);
    assert_eq!(config.crates[0].name, "core");
    assert_eq!(config.crates[1].name, "cli");
    assert_eq!(config.crates[2].name, "server");
    assert_eq!(config.crates[1].depends_on.as_ref().unwrap(), &["core"]);
}

// ---- TOML format tests ----

#[test]
fn test_parse_toml_full_defaults() {
    // flags live under defaults.builds.flags (path-mirror BuildConfig).
    let toml_str = r#"
project_name = "test"
dist = "./output"

[defaults]
targets = ["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"]
cross = "auto"

[defaults.builds]
binary = ""
flags = ["--release"]

[defaults.archives]
formats = ["tar.gz"]
[[defaults.archives.format_overrides]]
os = "windows"
formats = ["zip"]
[defaults.checksum]
algorithm = "sha256"

[[crates]]
name = "app"
path = "."
tag_template = "v{{ .Version }}"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(config.dist, PathBuf::from("./output"));
    let defaults = config.defaults.unwrap();
    assert_eq!(defaults.targets.as_ref().unwrap().len(), 2);
    assert_eq!(defaults.cross, Some(CrossStrategy::Auto));
    assert_eq!(
        defaults.builds.as_ref().unwrap().flags,
        Some(vec!["--release".to_string()])
    );
    let archives = defaults.archives.unwrap();
    assert_eq!(
        archives.formats.as_deref(),
        Some(&["tar.gz".to_string()][..])
    );
    assert_eq!(archives.format_overrides.as_ref().unwrap().len(), 1);
    assert_eq!(
        defaults.checksum.unwrap().algorithm,
        Some("sha256".to_string())
    );
}

#[test]
fn test_parse_toml_nfpm() {
    let toml_str = r#"
project_name = "test"

[[crates]]
name = "app"
path = "."
tag_template = "v{{ .Version }}"

[[crates.nfpms]]
package_name = "my-app"
formats = ["deb", "rpm"]
vendor = "ACME"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let nfpm = &config.crates[0].nfpms.as_ref().unwrap()[0];
    assert_eq!(nfpm.package_name, Some("my-app".to_string()));
    assert_eq!(nfpm.formats, vec!["deb", "rpm"]);
}

#[test]
fn test_parse_toml_publishers() {
    let toml_str = r#"
project_name = "test"

[[publishers]]
name = "upload"
cmd = "publish.sh"
args = ["--verbose"]
env = ["TOKEN=abc123"]

[[crates]]
name = "app"
path = "."
tag_template = "v{{ .Version }}"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    let pub_cfg = &config.publishers.as_ref().unwrap()[0];
    assert_eq!(pub_cfg.name, Some("upload".to_string()));
    assert!(
        pub_cfg
            .env
            .as_ref()
            .unwrap()
            .contains(&"TOKEN=abc123".to_string())
    );
}

#[test]
fn test_parse_toml_hooks() {
    let toml_str = r#"
project_name = "test"

[before]
hooks = ["cargo fmt", "cargo clippy"]

[after]
post = ["echo done"]

[[crates]]
name = "app"
path = "."
tag_template = "v{{ .Version }}"
"#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert_eq!(
        config
            .before
            .as_ref()
            .unwrap()
            .hooks
            .as_ref()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        config.after.as_ref().unwrap().hooks.as_ref().unwrap().len(),
        1,
        "TOML after.post folds into after.hooks"
    );
}

// ---- Type mismatch / invalid type tests ----

#[test]
fn test_parse_invalid_type_dist_array() {
    let yaml = "project_name: test\ndist:\n  - a\n  - b\ncrates: []";
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn test_parse_invalid_type_crates_string() {
    let yaml = "project_name: test\ncrates: not-an-array";
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn test_parse_invalid_type_builds_string() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds: "not an array"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn test_parse_invalid_type_docker_v2_string() {
    // `docker_v2:` is the only docker surface; a string-vs-list type
    // mismatch must still be rejected.
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    docker_v2: "not an array"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn test_parse_invalid_type_nfpm_string() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm: "not an array"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn test_parse_invalid_type_publishers_string() {
    let yaml = "project_name: test\npublishers: not-an-array\ncrates: []";
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn test_parse_invalid_type_env_array() {
    // Vec<String> accepts any string at parse time; entries without `=` are
    // caught by parse_env_entries when the env list is consumed.
    let yaml = "project_name: test\nenv:\n  - item\ncrates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let err = anodizer_core::config::parse_env_entries(config.env.as_ref().unwrap()).unwrap_err();
    assert!(
        err.to_string().contains("KEY=VALUE"),
        "parse_env_entries should reject entries without =, got: {}",
        err
    );
}

#[test]
fn test_parse_invalid_type_report_sizes_string() {
    let yaml = "project_name: test\nreport_sizes: maybe\ncrates: []";
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn test_parse_invalid_type_changelog_abbrev_string() {
    let yaml = r#"
project_name: test
changelog:
  abbrev: "not-a-number"
crates: []
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn test_parse_checksum_disable_string_is_valid() {
    // checksum.skip now accepts StringOrBool, so string values are valid
    let yaml = r#"
project_name: test
defaults:
  checksum:
    skip: "yes"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let checksum = config.defaults.unwrap().checksum.unwrap();
    assert_eq!(
        checksum.skip,
        Some(anodizer_core::config::StringOrBool::String(
            "yes".to_string()
        ))
    );
}

// ---- Interaction tests (disable + other fields, etc.) ----

#[test]
fn test_parse_changelog_disable_true_with_all_fields() {
    let yaml = r#"
project_name: test
changelog:
  skip: true
  sort: asc
  header: "header"
  footer: "footer"
  abbrev: 10
  use: github-native
  filters:
    exclude:
      - "^chore:"
  groups:
    - title: Features
      regexp: "^feat"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let cl = config.changelog.as_ref().unwrap();
    assert_eq!(
        cl.skip,
        Some(anodizer_core::config::StringOrBool::Bool(true))
    );
    assert_eq!(cl.sort, Some("asc".to_string()));
    assert_eq!(
        cl.header,
        Some(anodizer_core::config::ContentSource::Inline(
            "header".to_string()
        ))
    );
    assert_eq!(cl.abbrev, Some(10));
    assert!(cl.filters.is_some());
    assert!(cl.groups.is_some());
}

#[test]
fn test_parse_release_draft_with_skip_upload() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    release:
      draft: true
      skip_upload: true
      replace_existing_draft: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let release = config.crates[0].release.as_ref().unwrap();
    assert_eq!(release.draft, Some(true));
    assert_eq!(
        release.skip_upload,
        Some(anodizer_core::config::StringOrBool::Bool(true))
    );
    assert_eq!(release.replace_existing_draft, Some(true));
}

#[test]
fn test_parse_archives_disabled_with_release() {
    // A crate may disable archives but still have release config
    let yaml = r#"
project_name: test
crates:
  - name: operator
    path: "."
    tag_template: "v{{ version }}"
    archives: false
    release:
      draft: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(matches!(
        config.crates[0].archives,
        ArchivesConfig::Disabled
    ));
    assert!(config.crates[0].release.is_some());
}

#[test]
fn test_parse_build_env_with_features_and_flags() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    builds:
      - binary: app
        flags:
          - "--release"
        features:
          - tls
        no_default_features: true
        env:
          x86_64-unknown-linux-gnu:
            OPENSSL_DIR: /usr/local
        copy_from: shared-build
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.flags, Some(vec!["--release".to_string()]));
    assert_eq!(build.features.as_ref().unwrap(), &["tls"]);
    assert_eq!(build.no_default_features, Some(true));
    assert!(build.env.is_some());
    assert_eq!(build.copy_from, Some("shared-build".to_string()));
}

// ---- Comprehensive full config test ----

// ---- NfpmContent file_info tests ----

#[test]
fn test_parse_nfpm_content_file_info() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    nfpm:
      - formats:
          - deb
        contents:
          - src: "./app"
            dst: "/usr/bin/app"
            file_info:
              owner: root
              group: root
              mode: 0o755
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let content = &config.crates[0].nfpms.as_ref().unwrap()[0]
        .contents
        .as_ref()
        .unwrap()[0];
    let fi = content.file_info.as_ref().unwrap();
    assert_eq!(fi.owner, Some("root".to_string()));
    assert_eq!(fi.group, Some("root".to_string()));
    assert_eq!(fi.mode, Some(anodizer_core::config::StringOrU32(0o755)));
}

// ---- Default struct values tests ----

#[test]
fn test_config_default_struct() {
    let config = Config::default();
    assert_eq!(config.project_name, "");
    assert_eq!(config.dist, PathBuf::from("./dist"));
    assert!(config.defaults.is_none());
    assert!(config.before.is_none());
    assert!(config.after.is_none());
    assert!(config.crates.is_empty());
    assert!(config.changelog.is_none());
    assert!(config.signs.is_empty());
    assert!(config.docker_signs.is_none());
    assert!(config.snapshot.is_none());
    assert!(config.announce.is_none());
    assert_eq!(config.report_sizes, None);
    assert_eq!(config.env, None);
    assert!(config.publishers.is_none());
}

#[test]
fn test_crate_config_default_struct() {
    let config = CrateConfig::default();
    assert_eq!(config.name, "");
    assert_eq!(config.path, "");
    assert_eq!(config.tag_template, "");
    assert!(config.depends_on.is_none());
    assert!(config.builds.is_none());
    assert!(config.cross.is_none());
    assert!(matches!(config.archives, ArchivesConfig::Configs(ref v) if v.is_empty()));
    assert!(config.checksum.is_none());
    assert!(config.release.is_none());
    assert!(config.publish.is_none());
    assert!(config.nfpms.is_none());
}

#[test]
fn test_build_config_default_struct() {
    let config = BuildConfig::default();
    assert_eq!(config.binary, None);
    assert!(config.targets.is_none());
    assert!(config.features.is_none());
    assert!(config.no_default_features.is_none());
    assert!(config.env.is_none());
    assert!(config.copy_from.is_none());
    assert!(config.flags.is_none());
}

#[test]
fn test_archive_config_default_struct() {
    let config = ArchiveConfig::default();
    assert!(config.name_template.is_none());
    assert!(config.formats.is_none());
    assert!(config.format_overrides.is_none());
    assert!(config.files.is_none());
    assert!(config.binaries.is_none());
    assert!(config.wrap_in_directory.is_none());
}

#[test]
fn test_checksum_config_default_struct() {
    let config = ChecksumConfig::default();
    assert!(config.name_template.is_none());
    assert!(config.algorithm.is_none());
    assert!(config.skip.is_none());
    assert!(config.extra_files.is_none());
    assert!(config.ids.is_none());
}

#[test]
fn test_release_config_default_struct() {
    let config = ReleaseConfig::default();
    assert!(config.github.is_none());
    assert!(config.draft.is_none());
    assert!(config.prerelease.is_none());
    assert!(config.make_latest.is_none());
    assert!(config.name_template.is_none());
    assert!(config.header.is_none());
    assert!(config.footer.is_none());
    assert!(config.extra_files.is_none());
    assert!(config.skip_upload.is_none());
    assert!(config.replace_existing_draft.is_none());
    assert!(config.replace_existing_artifacts.is_none());
}

#[test]
fn test_nfpm_config_default_struct() {
    let config = NfpmConfig::default();
    assert!(config.package_name.is_none());
    assert!(config.formats.is_empty());
    assert!(config.vendor.is_none());
    assert!(config.homepage.is_none());
    assert!(config.maintainer.is_none());
    assert!(config.description.is_none());
    assert!(config.license.is_none());
    assert!(config.bindir.is_none());
    assert!(config.contents.is_none());
    assert!(config.dependencies.is_none());
    assert!(config.overrides.is_none());
    assert!(config.file_name_template.is_none());
    assert!(config.scripts.is_none());
    assert!(config.recommends.is_none());
    assert!(config.suggests.is_none());
    assert!(config.conflicts.is_none());
    assert!(config.replaces.is_none());
    assert!(config.provides.is_none());
}

#[test]
fn test_publisher_config_default_struct() {
    let config = PublisherConfig::default();
    assert!(config.name.is_none());
    assert_eq!(config.cmd, "");
    assert!(config.args.is_none());
    assert!(config.ids.is_none());
    assert!(config.artifact_types.is_none());
    assert!(config.env.is_none());
}

#[test]
fn test_sign_config_default_struct() {
    let config = SignConfig::default();
    assert!(config.id.is_none());
    assert!(config.artifacts.is_none());
    assert!(config.cmd.is_none());
    assert!(config.args.is_none());
    assert!(config.signature.is_none());
    assert!(config.stdin.is_none());
    assert!(config.stdin_file.is_none());
    assert!(config.ids.is_none());
}

#[test]
fn test_hooks_config_default_struct() {
    let config = HooksConfig::default();
    assert!(config.hooks.is_none());
    assert!(config.post.is_none());
}

#[test]
fn test_announce_config_default_struct() {
    let config = AnnounceConfig::default();
    assert!(config.discord.is_none());
    assert!(config.slack.is_none());
    assert!(config.webhook.is_none());
}

// ---- archive.name_template tests ----

#[test]
fn test_parse_archive_name_template_valid() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - name_template: "{{ project_name }}-{{ version }}-{{ os }}-{{ arch }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(
            configs[0].name_template,
            Some("{{ project_name }}-{{ version }}-{{ os }}-{{ arch }}".to_string())
        );
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

#[test]
fn test_parse_archive_name_template_omitted() {
    let yaml = r#"
project_name: test
crates:
  - name: app
    path: "."
    tag_template: "v{{ version }}"
    archives:
      - formats: [tar.gz]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    if let ArchivesConfig::Configs(configs) = &config.crates[0].archives {
        assert_eq!(configs[0].name_template, None);
    } else {
        panic!("expected ArchivesConfig::Configs");
    }
}

// ---- Publish config — combined homebrew+scoop+cargo ----

// ---- Announce provider disabled ----

#[test]
fn test_parse_announce_provider_disabled() {
    let yaml = r#"
project_name: test
announce:
  discord:
    enabled: false
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let discord = config.announce.as_ref().unwrap().discord.as_ref().unwrap();
    assert_eq!(
        discord.enabled,
        Some(anodizer_core::config::StringOrBool::Bool(false))
    );
}

// ---- DockerSignConfig all fields ----

#[test]
fn test_parse_docker_sign_all_fields() {
    let yaml = r#"
project_name: test
docker_signs:
  - artifacts: all
    cmd: cosign
    args:
      - sign
      - --key
      - cosign.key
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let ds = &config.docker_signs.as_ref().unwrap()[0];
    assert_eq!(ds.artifacts, Some("all".to_string()));
    assert_eq!(ds.cmd, Some("cosign".to_string()));
    assert_eq!(ds.args.as_ref().unwrap().len(), 3);
}

// ---- CargoPublishConfig default ----

#[test]
fn test_cargo_publish_config_default() {
    // Default-constructed config has every flag unset; presence in the
    // parent `publish.cargo:` is what opts the crate in.
    let cfg = CargoPublishConfig::default();
    assert_eq!(cfg.index_timeout, None);
    assert_eq!(cfg.no_verify, None);
    assert_eq!(cfg.allow_dirty, None);
    assert_eq!(cfg.skip, None);
    assert_eq!(cfg.features, None);
}

// ---- Webhook headers empty map ----

#[test]
fn test_parse_webhook_no_headers() {
    let yaml = r#"
project_name: test
announce:
  webhook:
    enabled: true
    endpoint_url: "https://example.com"
crates: []
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let webhook = config.announce.as_ref().unwrap().webhook.as_ref().unwrap();
    assert!(webhook.headers.is_none());
}

// ====================================================================
// Error path completeness — config error tests
// ====================================================================

// ---- Duplicate crate names ----

#[test]
fn test_parse_duplicate_crate_names_accepted_by_serde() {
    // serde/YAML does not reject duplicate list entries; both crates parse fine
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
  - name: myapp
    path: "./other"
    tag_template: "v{{ .Version }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates.len(), 2);
    assert_eq!(config.crates[0].name, "myapp");
    assert_eq!(config.crates[1].name, "myapp");
}

// ---- Invalid tag_template ----

#[test]
fn test_invalid_tag_template_syntax_error() {
    // A tag_template with bad Tera syntax should parse fine as a string,
    // but fail when rendered.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "{{ unclosed"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates[0].tag_template, "{{ unclosed");
    // Config parses but rendering would fail
}

#[test]
fn test_empty_tag_template() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: ""
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates[0].tag_template, "");
}

// ---- depends_on edge cases (no validation at parse time; resolved later) ----

#[test]
fn test_depends_on_nonexistent_crate_parses() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    depends_on:
      - nonexistent-crate
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let deps = config.crates[0].depends_on.as_ref().unwrap();
    assert_eq!(deps, &["nonexistent-crate"]);
}

#[test]
fn test_circular_depends_on_parses() {
    // Circular dependencies parse fine in YAML (no validation at parse time).
    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    depends_on: [b]
  - name: b
    path: "."
    tag_template: "v{{ .Version }}"
    depends_on: [a]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.crates.len(), 2);
    assert_eq!(config.crates[0].depends_on.as_ref().unwrap(), &["b"]);
    assert_eq!(config.crates[1].depends_on.as_ref().unwrap(), &["a"]);
}

#[test]
fn test_self_referencing_depends_on() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    depends_on: [myapp]
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let deps = config.crates[0].depends_on.as_ref().unwrap();
    assert_eq!(deps, &["myapp"]);
}

// ---- Invalid YAML / wrong types ----

#[test]
fn test_invalid_yaml_syntax_produces_parse_error() {
    let yaml = "project_name: test\ncrates:\n  - name: [invalid yaml structure";
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "invalid YAML should produce a parse error");
}

#[test]
fn test_wrong_type_for_crates_field() {
    let yaml = "project_name: test\ncrates: not_a_list";
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "crates should be a list, not a string");
}

#[test]
fn test_unknown_field_in_crate_config_rejected() {
    // CrateConfig uses #[serde(default, deny_unknown_fields)], so an unknown
    // crate-level field is a hard parse error — surfacing typos and removed
    // fields instead of silently dropping them.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    unknown_field: some_value
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

// ---- Archive disabled form ----

#[test]
fn test_archives_disabled_bool_parses() {
    // `archives: false` parses to ArchivesConfig::Disabled — short-circuits
    // archive emission entirely for the crate.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    archives: false
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(matches!(
        config.crates[0].archives,
        ArchivesConfig::Disabled
    ));
}

// ---- Missing required fields ----

#[test]
fn test_crate_missing_name_defaults_to_empty() {
    // CrateConfig uses #[serde(default)], so a missing `name` defaults to "".
    let yaml = r#"
project_name: test
crates:
  - path: "."
    tag_template: "v{{ .Version }}"
"#;
    let result: Result<Config, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_ok(),
        "missing name should parse due to #[serde(default)]"
    );
    let config = result.unwrap();
    assert_eq!(config.crates[0].name, "");
}

// ---- UniversalBinaryConfig default ----

#[test]
fn test_universal_binary_config_default() {
    let ub = UniversalBinaryConfig::default();
    assert!(ub.name_template.is_none());
    assert!(ub.replace.is_none());
    assert!(ub.ids.is_none());
}

// ---- monorepo config ----

#[test]
fn test_parse_monorepo_both_fields() {
    let yaml = r#"
crates: []
monorepo:
  tag_prefix: "subproject1/"
  dir: subproj1
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let mono = config.monorepo.expect("monorepo should be Some");
    assert_eq!(mono.tag_prefix.as_deref(), Some("subproject1/"));
    assert_eq!(mono.dir.as_deref(), Some("subproj1"));
}

#[test]
fn test_parse_monorepo_only_tag_prefix() {
    let yaml = r#"
crates: []
monorepo:
  tag_prefix: "myapp/"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let mono = config.monorepo.expect("monorepo should be Some");
    assert_eq!(mono.tag_prefix.as_deref(), Some("myapp/"));
    assert!(mono.dir.is_none());
}

#[test]
fn test_parse_monorepo_only_dir() {
    let yaml = r#"
crates: []
monorepo:
  dir: packages/backend
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let mono = config.monorepo.expect("monorepo should be Some");
    assert!(mono.tag_prefix.is_none());
    assert_eq!(mono.dir.as_deref(), Some("packages/backend"));
}

#[test]
fn test_parse_monorepo_absent_defaults_to_none() {
    let yaml = "crates: []";
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.monorepo.is_none());
}

#[test]
fn test_parse_monorepo_empty_map_defaults_to_empty() {
    let yaml = r#"
crates: []
monorepo: {}
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let mono = config
        .monorepo
        .expect("monorepo should be Some with defaults");
    assert!(mono.tag_prefix.is_none());
    assert!(mono.dir.is_none());
}

// ---- Comprehensive happy-path config (post-WAVE-5 shape) ----

#[test]
fn test_parse_comprehensive_config() {
    // End-to-end happy-path test that exercises every top-level surface:
    // typed fields (BuildConfig.flags as Vec<String>, ChangelogConfig.
    // {header,footer} as ContentSource), the unified RepositoryConfig form
    // (replacing legacy {tap,bucket,...} variants), structured
    // commit_author, and the current hard-break shapes.
    let yaml = r###"
project_name: comprehensive-test
dist: ./custom-dist
env:
  - GLOBAL_VAR=value
report_sizes: true
before:
  hooks:
    - "cargo fmt --check"
after:
  post:
    - "echo release complete"
defaults:
  targets:
    - x86_64-unknown-linux-gnu
    - aarch64-apple-darwin
  cross: zigbuild
  builds:
    flags:
      - "--release"
      - "--locked"
  archives:
    formats: [tar.gz]
    format_overrides:
      - os: windows
        formats: [zip]
  checksum:
    algorithm: sha256
    name_template: "checksums.txt"
changelog:
  sort: desc
  abbrev: 8
  header: "## Changelog"
  footer: "---"
  filters:
    exclude:
      - "^docs:"
  groups:
    - title: Features
      regexp: "^feat"
      order: 0
snapshot:
  version_template: "{{ version }}-SNAPSHOT"
signs:
  - id: gpg
    artifacts: all
    cmd: gpg
    args:
      - "--detach-sig"
    signature: "{{ .Artifact }}.sig"
publishers:
  - name: custom
    cmd: upload.sh
    env:
      - TOKEN=secret
crates:
  - name: lib
    path: crates/lib
    tag_template: "lib/v{{ version }}"
  - name: app
    path: crates/app
    tag_template: "app/v{{ version }}"
    depends_on:
      - lib
    cross: cargo
    builds:
      - binary: app
        features:
          - full
    archives:
      - formats: [tar.gz]
        files:
          - LICENSE
          - README.md
        binaries:
          - app
        name_template: "app-{{ version }}-{{ os }}-{{ arch }}"
    checksum:
      algorithm: sha512
    release:
      github:
        owner: org
        name: repo
      draft: false
      prerelease: auto
      make_latest: auto
      name_template: "App {{ tag }}"
      header: "## Release"
      footer: "---"
    publish:
      cargo:
        index_timeout: 60
      homebrew:
        repository:
          owner: org
          name: homebrew-tap
        commit_author:
          name: bot
          email: bot@example.com
        directory: Formula
        description: "App tool"
        license: MIT
      scoop:
        repository:
          owner: org
          name: scoop-bucket
        commit_author:
          name: bot
          email: bot@example.com
    docker_v2:
      - dockerfile: Dockerfile
        images:
          - "ghcr.io/org/app"
        tags:
          - "{{ version }}"
        platforms:
          - linux/amd64
          - linux/arm64
        labels:
          version: "{{ version }}"
    nfpm:
      - package_name: app
        formats:
          - deb
          - rpm
        vendor: Org
        description: "A cool app"
        file_name_template: "app_{{ version }}_{{ arch }}"
        overrides:
          deb:
            depends:
              - libc6
        bindir: /usr/bin
        contents:
          - src: "./app"
            dst: "/usr/bin/app"
        scripts:
          postinstall: ./scripts/post.sh
"###;
    let config: Config = serde_yaml_ng::from_str(yaml).expect("comprehensive yaml must parse");

    // Top-level
    assert_eq!(config.project_name, "comprehensive-test");
    assert_eq!(config.dist, PathBuf::from("./custom-dist"));
    assert_eq!(config.report_sizes, Some(true));
    assert!(config.env.is_some());
    assert!(config.before.is_some());
    assert!(config.after.is_some());

    // Defaults
    let defaults = config.defaults.as_ref().unwrap();
    assert_eq!(defaults.targets.as_ref().unwrap().len(), 2);
    assert_eq!(defaults.cross, Some(CrossStrategy::Zigbuild));
    let build_flags = defaults
        .builds
        .as_ref()
        .unwrap()
        .flags
        .as_ref()
        .expect("BuildConfig.flags must parse as Vec<String>");
    assert_eq!(
        build_flags.as_slice(),
        &["--release".to_string(), "--locked".to_string()]
    );

    // Changelog ( : ContentSource for header/footer)
    let cl = config.changelog.as_ref().unwrap();
    assert_eq!(cl.sort, Some("desc".to_string()));
    assert_eq!(cl.abbrev, Some(8));
    match cl.header.as_ref().expect("changelog.header") {
        ContentSource::Inline(s) => assert_eq!(s, "## Changelog"),
        other => panic!("expected ContentSource::Inline, got {other:?}"),
    }
    match cl.footer.as_ref().expect("changelog.footer") {
        ContentSource::Inline(s) => assert_eq!(s, "---"),
        other => panic!("expected ContentSource::Inline, got {other:?}"),
    }

    // Snapshot
    assert!(config.snapshot.is_some());

    // Signs
    assert_eq!(config.signs.len(), 1);

    // Publishers
    assert_eq!(config.publishers.as_ref().unwrap().len(), 1);

    // Crates
    assert_eq!(config.crates.len(), 2);
    let app = &config.crates[1];
    assert_eq!(app.name, "app");
    assert_eq!(app.depends_on.as_ref().unwrap(), &["lib"]);
    assert_eq!(app.cross, Some(CrossStrategy::Cargo));

    // App builds
    let builds = app.builds.as_ref().unwrap();
    assert_eq!(builds[0].binary.as_deref(), Some("app"));

    // App archives
    if let ArchivesConfig::Configs(configs) = &app.archives {
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].binaries.as_ref().unwrap(), &["app"]);
    } else {
        panic!("expected ArchivesConfig::Configs");
    }

    // App release
    let release = app.release.as_ref().unwrap();
    assert_eq!(release.prerelease, Some(PrereleaseConfig::Auto));
    assert_eq!(release.make_latest, Some(MakeLatestConfig::Auto));

    // App publish — uses the unified `repository:` form ( )
    let publish = app.publish.as_ref().unwrap();
    let hb = publish.homebrew.as_ref().expect("homebrew publisher");
    let repo = hb.repository.as_ref().expect("homebrew.repository");
    assert_eq!(repo.owner.as_deref(), Some("org"));
    assert_eq!(repo.name.as_deref(), Some("homebrew-tap"));
    let scoop = publish.scoop.as_ref().expect("scoop publisher");
    let scoop_repo = scoop.repository.as_ref().expect("scoop.repository");
    assert_eq!(scoop_repo.name.as_deref(), Some("scoop-bucket"));
    assert_eq!(publish.cargo.as_ref().unwrap().index_timeout, Some(60));

    // App docker_v2 ( : legacy `docker:` field dropped)
    let docker = &app.docker_v2.as_ref().unwrap()[0];
    assert_eq!(docker.platforms.as_ref().unwrap().len(), 2);
    assert_eq!(docker.images, vec!["ghcr.io/org/app".to_string()]);

    // App nfpm
    let nfpm = &app.nfpms.as_ref().unwrap()[0];
    assert_eq!(nfpm.formats, vec!["deb", "rpm"]);
    assert!(nfpm.overrides.is_some());
    assert!(nfpm.scripts.is_some());
}
