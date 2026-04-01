use anodize_core::config::Config;
use anodize_core::context::Context;
pub use anodize_core::hooks::run_hooks;
use anodize_core::log::StageLogger;
use anodize_core::stage::Stage;
use anyhow::{Context as _, Result, bail};
use colored::Colorize;
use std::path::{Path, PathBuf};

/// Find config file. If `config_override` is provided, use that path directly;
/// otherwise search the current directory for well-known config file names.
pub fn find_config(config_override: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = config_override {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        bail!("config file not found: {}", path.display());
    }
    let candidates = [
        ".anodize.yaml",
        ".anodize.yml",
        ".anodize.toml",
        "anodize.yaml",
        "anodize.yml",
        "anodize.toml",
    ];
    for name in &candidates {
        let path = PathBuf::from(name);
        if path.exists() {
            return Ok(path);
        }
    }
    // Fallback: if Cargo.toml exists, use a default config instead of erroring.
    if Path::new("Cargo.toml").exists() {
        eprintln!("WARNING: no anodize config found; using defaults from Cargo.toml");
        return Ok(PathBuf::from("Cargo.toml"));
    }
    bail!(
        "no anodize config file found (tried: {}). Run `anodize init` to generate one.",
        candidates.join(", ")
    )
}

/// Deep-merge `overlay` into `base`. Mappings are merged recursively,
/// sequences are concatenated, and scalars/other values are replaced.
fn merge_yaml(base: &mut serde_yaml_ng::Value, overlay: &serde_yaml_ng::Value) {
    match (base, overlay) {
        (serde_yaml_ng::Value::Mapping(base_map), serde_yaml_ng::Value::Mapping(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(key) {
                    Some(existing) => merge_yaml(existing, value),
                    None => {
                        base_map.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (serde_yaml_ng::Value::Sequence(base_seq), serde_yaml_ng::Value::Sequence(overlay_seq)) => {
            base_seq.extend(overlay_seq.iter().cloned());
        }
        (base_val, overlay_val) => {
            *base_val = overlay_val.clone();
        }
    }
}

/// Load config from a file, auto-detecting format by extension.
///
/// For YAML files, processes `includes` by deep-merging included files together as
/// defaults, then merging the base (local) config on top. This means the base config
/// always takes priority over values from included files — includes provide defaults,
/// not overrides.
pub fn load_config(path: &Path) -> Result<Config> {
    // Special case: Cargo.toml fallback returns a default Config. The
    // find_config function returns "Cargo.toml" when no anodize config file
    // exists but a Cargo.toml is present in the working directory.
    if path.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml") {
        return Ok(Config::default());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let config = match ext {
        "yaml" | "yml" => load_yaml_config_with_includes(path, &content)?,
        "toml" => load_toml_config_with_includes(path, &content)?,
        _ => bail!("unsupported config format: {}", ext),
    };

    // Validate config schema version
    anodize_core::config::validate_version(&config).map_err(|e| anyhow::anyhow!("{}", e))?;
    // Validate git.tag_sort if present
    anodize_core::config::validate_tag_sort(&config).map_err(|e| anyhow::anyhow!("{}", e))?;

    Ok(config)
}

/// Load a YAML config, processing `includes` by deep-merging included files
/// as defaults and then merging the base (local) config on top.
fn load_yaml_config_with_includes(path: &Path, content: &str) -> Result<Config> {
    let base: serde_yaml_ng::Value = serde_yaml_ng::from_str(content)
        .with_context(|| format!("failed to parse YAML config: {}", path.display()))?;

    // Extract include paths before merging
    let include_paths: Vec<String> = base
        .get("includes")
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    // Accumulate all included files into a merged defaults value.
    // The base config is then merged on top so its values always win.
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut merged = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
    for include in &include_paths {
        // Reject absolute paths in includes to prevent unexpected file reads.
        if Path::new(include).is_absolute() {
            bail!(
                "includes: absolute paths are not allowed (got '{}' in {})",
                include,
                path.display()
            );
        }
        let include_path = base_dir.join(include);
        let include_content = std::fs::read_to_string(&include_path).with_context(|| {
            format!(
                "failed to read include file '{}' (referenced from {})",
                include_path.display(),
                path.display()
            )
        })?;
        let overlay = load_include_as_yaml(&include_path, &include_content)?;
        merge_yaml(&mut merged, &overlay);
    }
    // Merge base config on top of the accumulated defaults (base wins).
    merge_yaml(&mut merged, &base);

    serde_yaml_ng::from_value(merged)
        .with_context(|| format!("failed to deserialize config: {}", path.display()))
}

/// Load a TOML config, processing `includes` using the same merge strategy
/// as YAML: included files provide defaults, the base config wins.
fn load_toml_config_with_includes(path: &Path, content: &str) -> Result<Config> {
    // Parse the base TOML to a generic toml::Value first to extract includes.
    let base_toml: toml::Value = toml::from_str(content)
        .with_context(|| format!("failed to parse TOML config: {}", path.display()))?;

    let include_paths: Vec<String> = base_toml
        .get("includes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    if include_paths.is_empty() {
        // No includes — fast path: deserialize directly from TOML.
        return toml::from_str(content)
            .with_context(|| format!("failed to deserialize TOML config: {}", path.display()));
    }

    // Convert the base TOML to a YAML Value so we can use the existing
    // deep-merge logic. Round-trip through serde_json::Value as an
    // intermediate format that both serde_yaml_ng and toml support.
    let base_json = serde_json::to_value(&base_toml)
        .with_context(|| "failed to convert TOML config to JSON for merging")?;
    let base_yaml: serde_yaml_ng::Value = serde_yaml_ng::to_value(&base_json)
        .with_context(|| "failed to convert TOML config to YAML for merging")?;

    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut merged = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
    for include in &include_paths {
        if Path::new(include).is_absolute() {
            bail!(
                "includes: absolute paths are not allowed (got '{}' in {})",
                include,
                path.display()
            );
        }
        let include_path = base_dir.join(include);
        let include_content = std::fs::read_to_string(&include_path).with_context(|| {
            format!(
                "failed to read include file '{}' (referenced from {})",
                include_path.display(),
                path.display()
            )
        })?;
        let overlay = load_include_as_yaml(&include_path, &include_content)?;
        merge_yaml(&mut merged, &overlay);
    }
    // Merge base config on top of the accumulated defaults (base wins).
    merge_yaml(&mut merged, &base_yaml);

    serde_yaml_ng::from_value(merged)
        .with_context(|| format!("failed to deserialize config: {}", path.display()))
}

/// Parse an include file as a serde_yaml_ng::Value, auto-detecting format
/// by extension (YAML or TOML).
fn load_include_as_yaml(
    include_path: &Path,
    include_content: &str,
) -> Result<serde_yaml_ng::Value> {
    let ext = include_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "toml" => {
            let toml_val: toml::Value = toml::from_str(include_content).with_context(|| {
                format!("failed to parse include file: {}", include_path.display())
            })?;
            let json_val = serde_json::to_value(&toml_val).with_context(|| {
                format!(
                    "failed to convert TOML include to JSON: {}",
                    include_path.display()
                )
            })?;
            serde_yaml_ng::to_value(&json_val).with_context(|| {
                format!(
                    "failed to convert TOML include to YAML: {}",
                    include_path.display()
                )
            })
        }
        _ => {
            // Default: parse as YAML (works for .yaml, .yml, and extensionless)
            serde_yaml_ng::from_str(include_content).with_context(|| {
                format!("failed to parse include file: {}", include_path.display())
            })
        }
    }
}

// run_hooks is re-exported from anodize_core::hooks

pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self { stages: vec![] }
    }

    pub fn add(&mut self, stage: Box<dyn Stage>) {
        self.stages.push(stage);
    }

    pub fn run(&self, ctx: &mut Context, log: &StageLogger) -> Result<()> {
        // Validate --skip values against known stage names.
        const KNOWN_STAGES: &[&str] = &[
            "build",
            "upx",
            "archive",
            "nfpm",
            "snapcraft",
            "snapcraft-publish",
            "dmg",
            "msi",
            "pkg",
            "nsis",
            "appbundle",
            "flatpak",
            "notarize",
            "source",
            "changelog",
            "checksum",
            "sign",
            "release",
            "publish",
            "docker",
            "blob",
            "announce",
        ];
        for skip in &ctx.options.skip_stages {
            if !KNOWN_STAGES.contains(&skip.as_str()) {
                eprintln!(
                    "WARNING: unknown skip stage '{}'; valid stages: {}",
                    skip,
                    KNOWN_STAGES.join(", ")
                );
            }
        }

        for stage in &self.stages {
            let name = stage.name();
            if ctx.should_skip(name) {
                log.status(&format!("{} {}", name.bold(), "skipped".yellow()));
                continue;
            }
            log.status(&format!("\u{2022} {}...", name.bold()));
            match stage.run(ctx) {
                Ok(()) => {
                    log.status(&format!("{} {}", "\u{2713}".green().bold(), name.bold()));
                    // After the changelog stage completes, populate the ReleaseNotes
                    // template variable so subsequent stages can reference it.
                    if name == "changelog" {
                        ctx.populate_release_notes_var();
                    }
                }
                Err(e) => {
                    log.status(&format!(
                        "{} {} \u{2014} {}",
                        "\u{2717}".red().bold(),
                        name.bold(),
                        e
                    ));
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

/// Build the full release pipeline with all stages in order
pub fn build_release_pipeline() -> Pipeline {
    use anodize_stage_announce::AnnounceStage;
    use anodize_stage_archive::ArchiveStage;
    use anodize_stage_blob::BlobStage;
    use anodize_stage_build::BuildStage;
    use anodize_stage_changelog::ChangelogStage;
    use anodize_stage_checksum::ChecksumStage;
    use anodize_stage_dmg::DmgStage;
    use anodize_stage_docker::DockerStage;
    use anodize_stage_msi::MsiStage;
    use anodize_stage_nfpm::NfpmStage;
    use anodize_stage_nsis::NsisStage;
    use anodize_stage_pkg::PkgStage;
    use anodize_stage_publish::PublishStage;
    use anodize_stage_release::ReleaseStage;
    use anodize_stage_sign::SignStage;
    use anodize_stage_snapcraft::{SnapcraftPublishStage, SnapcraftStage};
    use anodize_stage_source::SourceStage;
    use anodize_stage_upx::UpxStage;
    use anodize_stage_appbundle::AppBundleStage;
    use anodize_stage_flatpak::FlatpakStage;
    use anodize_stage_notarize::NotarizeStage;

    let mut p = Pipeline::new();
    p.add(Box::new(BuildStage));
    p.add(Box::new(UpxStage));
    // Changelog runs before archive so release notes are available for archive naming.
    p.add(Box::new(ChangelogStage));
    p.add(Box::new(ArchiveStage));
    p.add(Box::new(NfpmStage));
    p.add(Box::new(SnapcraftStage));
    // AppBundle must run before DMG so signed bundles can be packaged into disk images.
    p.add(Box::new(AppBundleStage));
    p.add(Box::new(DmgStage));
    p.add(Box::new(MsiStage));
    p.add(Box::new(PkgStage));
    p.add(Box::new(NsisStage));
    p.add(Box::new(FlatpakStage));
    // Notarize runs after AppBundle, DMG, and PKG stages but before checksum/sign.
    p.add(Box::new(NotarizeStage));
    p.add(Box::new(SourceStage));
    p.add(Box::new(ChecksumStage));
    p.add(Box::new(SignStage));
    p.add(Box::new(ReleaseStage));
    p.add(Box::new(PublishStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(DockerStage));
    p.add(Box::new(BlobStage));
    p.add(Box::new(AnnounceStage));
    p
}

/// Build a pipeline that only runs the build stage (for --split mode).
pub fn build_split_pipeline() -> Pipeline {
    use anodize_stage_build::BuildStage;
    use anodize_stage_upx::UpxStage;

    let mut p = Pipeline::new();
    p.add(Box::new(BuildStage));
    p.add(Box::new(UpxStage));
    p
}

/// Build a publish-only pipeline: release, publish, snapcraft-publish, blob stages.
pub fn build_publish_pipeline() -> Pipeline {
    use anodize_stage_blob::BlobStage;
    use anodize_stage_publish::PublishStage;
    use anodize_stage_release::ReleaseStage;
    use anodize_stage_snapcraft::SnapcraftPublishStage;

    let mut p = Pipeline::new();
    p.add(Box::new(ReleaseStage));
    p.add(Box::new(PublishStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(BlobStage));
    p
}

/// Build an announce-only pipeline.
pub fn build_announce_pipeline() -> Pipeline {
    use anodize_stage_announce::AnnounceStage;

    let mut p = Pipeline::new();
    p.add(Box::new(AnnounceStage));
    p
}

/// Build a pipeline for --merge mode: all post-build stages.
pub fn build_merge_pipeline() -> Pipeline {
    use anodize_stage_announce::AnnounceStage;
    use anodize_stage_archive::ArchiveStage;
    use anodize_stage_blob::BlobStage;
    use anodize_stage_changelog::ChangelogStage;
    use anodize_stage_checksum::ChecksumStage;
    use anodize_stage_dmg::DmgStage;
    use anodize_stage_docker::DockerStage;
    use anodize_stage_msi::MsiStage;
    use anodize_stage_nfpm::NfpmStage;
    use anodize_stage_nsis::NsisStage;
    use anodize_stage_pkg::PkgStage;
    use anodize_stage_publish::PublishStage;
    use anodize_stage_release::ReleaseStage;
    use anodize_stage_sign::SignStage;
    use anodize_stage_snapcraft::{SnapcraftPublishStage, SnapcraftStage};
    use anodize_stage_source::SourceStage;
    use anodize_stage_appbundle::AppBundleStage;
    use anodize_stage_flatpak::FlatpakStage;
    use anodize_stage_notarize::NotarizeStage;

    let mut p = Pipeline::new();
    // Changelog runs before archive so release notes are available for archive naming.
    p.add(Box::new(ChangelogStage));
    p.add(Box::new(ArchiveStage));
    p.add(Box::new(NfpmStage));
    p.add(Box::new(SnapcraftStage));
    // AppBundle must run before DMG so signed bundles can be packaged into disk images.
    p.add(Box::new(AppBundleStage));
    p.add(Box::new(DmgStage));
    p.add(Box::new(MsiStage));
    p.add(Box::new(PkgStage));
    p.add(Box::new(NsisStage));
    p.add(Box::new(FlatpakStage));
    // Notarize runs after AppBundle, DMG, and PKG stages but before checksum/sign.
    p.add(Box::new(NotarizeStage));
    p.add(Box::new(SourceStage));
    p.add(Box::new(ChecksumStage));
    p.add(Box::new(SignStage));
    p.add(Box::new(ReleaseStage));
    p.add(Box::new(PublishStage));
    p.add(Box::new(SnapcraftPublishStage));
    p.add(Box::new(DockerStage));
    p.add(Box::new(BlobStage));
    p.add(Box::new(AnnounceStage));
    p
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_find_config_with_override_existing() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("custom-config.yaml");
        fs::write(&cfg_path, "project_name: test\ncrates: []\n").unwrap();

        let result = find_config(Some(cfg_path.as_path()));
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert_eq!(result.unwrap(), cfg_path);
    }

    #[test]
    fn test_find_config_with_override_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("does-not-exist.yaml");

        let result = find_config(Some(cfg_path.as_path()));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("config file not found"),
            "unexpected error message: {}",
            msg
        );
    }

    #[test]
    fn test_find_config_override_with_subdirectory_path() {
        let tmp = TempDir::new().unwrap();
        let subdir = tmp.path().join("nested").join("dir");
        fs::create_dir_all(&subdir).unwrap();
        let cfg_path = subdir.join("my-release.toml");
        fs::write(&cfg_path, "project_name = \"test\"\ncrates = []\n").unwrap();

        let result = find_config(Some(cfg_path.as_path()));
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert_eq!(result.unwrap(), cfg_path);
    }

    // -----------------------------------------------------------------------
    // merge_yaml tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_yaml_mappings_recursive() {
        let mut base: serde_yaml_ng::Value = serde_yaml_ng::from_str("a: 1\nb: 2").unwrap();
        let overlay: serde_yaml_ng::Value = serde_yaml_ng::from_str("b: 99\nc: 3").unwrap();
        merge_yaml(&mut base, &overlay);
        assert_eq!(base["a"], serde_yaml_ng::Value::Number(1.into()));
        assert_eq!(base["b"], serde_yaml_ng::Value::Number(99.into()));
        assert_eq!(base["c"], serde_yaml_ng::Value::Number(3.into()));
    }

    #[test]
    fn test_merge_yaml_nested_mappings() {
        let mut base: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("outer:\n  x: 1\n  y: 2").unwrap();
        let overlay: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("outer:\n  y: 99\n  z: 3").unwrap();
        merge_yaml(&mut base, &overlay);
        assert_eq!(base["outer"]["x"], serde_yaml_ng::Value::Number(1.into()));
        assert_eq!(base["outer"]["y"], serde_yaml_ng::Value::Number(99.into()));
        assert_eq!(base["outer"]["z"], serde_yaml_ng::Value::Number(3.into()));
    }

    #[test]
    fn test_merge_yaml_sequences_concatenate() {
        let mut base: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("items:\n  - a\n  - b").unwrap();
        let overlay: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("items:\n  - c\n  - d").unwrap();
        merge_yaml(&mut base, &overlay);
        let items = base["items"].as_sequence().unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].as_str().unwrap(), "a");
        assert_eq!(items[1].as_str().unwrap(), "b");
        assert_eq!(items[2].as_str().unwrap(), "c");
        assert_eq!(items[3].as_str().unwrap(), "d");
    }

    #[test]
    fn test_merge_yaml_scalar_override() {
        let mut base: serde_yaml_ng::Value = serde_yaml_ng::from_str("name: base").unwrap();
        let overlay: serde_yaml_ng::Value = serde_yaml_ng::from_str("name: overlay").unwrap();
        merge_yaml(&mut base, &overlay);
        assert_eq!(base["name"].as_str().unwrap(), "overlay");
    }

    // -----------------------------------------------------------------------
    // load_config with includes tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_config_includes_field_parses() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(
            &cfg_path,
            "project_name: myproject\nincludes:\n  - extra.yaml\ncrates: []\n",
        )
        .unwrap();
        let extra_path = tmp.path().join("extra.yaml");
        fs::write(&extra_path, "report_sizes: true\n").unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "myproject");
        assert_eq!(config.includes, Some(vec!["extra.yaml".to_string()]));
        assert_eq!(config.report_sizes, Some(true));
    }

    #[test]
    fn test_load_config_includes_merges_base_and_include() {
        let tmp = TempDir::new().unwrap();

        // Include file defines a dist override
        let include_path = tmp.path().join("overrides.yaml");
        fs::write(&include_path, "dist: /custom/dist\n").unwrap();

        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(
            &cfg_path,
            "project_name: merged\nincludes:\n  - overrides.yaml\ncrates: []\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "merged");
        assert_eq!(config.dist, std::path::PathBuf::from("/custom/dist"));
    }

    #[test]
    fn test_load_config_includes_sequences_concatenated() {
        let tmp = TempDir::new().unwrap();

        let include_path = tmp.path().join("more-crates.yaml");
        fs::write(
            &include_path,
            "crates:\n  - name: extra-crate\n    path: crates/extra\n",
        )
        .unwrap();

        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(
            &cfg_path,
            "project_name: seq-test\nincludes:\n  - more-crates.yaml\ncrates:\n  - name: base-crate\n    path: crates/base\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.crates.len(), 2);
        // Includes are accumulated as defaults first; base is merged on top,
        // so base sequences are appended after include sequences.
        assert_eq!(config.crates[0].name, "extra-crate");
        assert_eq!(config.crates[1].name, "base-crate");
    }

    #[test]
    fn test_load_config_base_wins_over_include_for_scalar() {
        let tmp = TempDir::new().unwrap();

        // Include file defines a dist that should be treated as a default.
        let include_path = tmp.path().join("defaults.yaml");
        fs::write(&include_path, "dist: /from-include\n").unwrap();

        // Base config also defines dist — it should win.
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(
            &cfg_path,
            "project_name: priority-test\nincludes:\n  - defaults.yaml\ndist: /from-base\ncrates: []\n",
        )
        .unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(
            config.dist,
            std::path::PathBuf::from("/from-base"),
            "base config should override include for scalar values"
        );
    }

    #[test]
    fn test_load_config_missing_include_file_returns_error() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(
            &cfg_path,
            "project_name: test\nincludes:\n  - nonexistent.yaml\ncrates: []\n",
        )
        .unwrap();

        let result = load_config(&cfg_path);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("nonexistent.yaml") || msg.contains("include"),
            "unexpected error message: {}",
            msg
        );
    }

    #[test]
    fn test_load_config_no_includes_works_as_before() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(&cfg_path, "project_name: simple\ncrates: []\n").unwrap();

        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.project_name, "simple");
        assert!(config.includes.is_none());
    }

    // ---- Version validation in load_config ----

    #[test]
    fn test_load_config_version_1_accepted() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(&cfg_path, "project_name: test\nversion: 1\ncrates: []\n").unwrap();
        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.version, Some(1));
    }

    #[test]
    fn test_load_config_version_2_accepted() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(&cfg_path, "project_name: test\nversion: 2\ncrates: []\n").unwrap();
        let config = load_config(&cfg_path).unwrap();
        assert_eq!(config.version, Some(2));
    }

    #[test]
    fn test_load_config_version_99_rejected() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(&cfg_path, "project_name: test\nversion: 99\ncrates: []\n").unwrap();
        let result = load_config(&cfg_path);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unsupported config version"),
            "error should mention unsupported version: {}",
            msg
        );
    }

    #[test]
    fn test_load_config_env_files_list_form() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(
            &cfg_path,
            "project_name: test\nenv_files:\n  - .env\n  - .release.env\ncrates: []\n",
        )
        .unwrap();
        let config = load_config(&cfg_path).unwrap();
        let env_files = config.env_files.unwrap();
        let files = env_files.as_list().expect("expected List variant");
        assert_eq!(files, &[".env", ".release.env"]);
    }

    #[test]
    fn test_load_config_env_files_struct_form() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(
            &cfg_path,
            "project_name: test\nenv_files:\n  github_token: /tmp/gh_token\n  gitlab_token: /tmp/gl_token\ncrates: []\n",
        )
        .unwrap();
        let config = load_config(&cfg_path).unwrap();
        let env_files = config.env_files.unwrap();
        let tokens = env_files.as_token_files().expect("expected TokenFiles variant");
        assert_eq!(tokens.github_token.as_deref(), Some("/tmp/gh_token"));
        assert_eq!(tokens.gitlab_token.as_deref(), Some("/tmp/gl_token"));
        assert!(tokens.gitea_token.is_none());
    }

    #[test]
    fn test_load_config_with_ignore_and_overrides() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("anodize.yaml");
        fs::write(
            &cfg_path,
            r#"
project_name: test
defaults:
  targets:
    - x86_64-unknown-linux-gnu
  ignore:
    - os: windows
      arch: arm64
  overrides:
    - targets: ["x86_64-*"]
      features: [simd]
crates: []
"#,
        )
        .unwrap();
        let config = load_config(&cfg_path).unwrap();
        let defaults = config.defaults.unwrap();
        assert_eq!(defaults.ignore.unwrap().len(), 1);
        assert_eq!(defaults.overrides.unwrap().len(), 1);
    }
}
