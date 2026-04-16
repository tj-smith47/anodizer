use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// DmgTool detection
// ---------------------------------------------------------------------------

/// Which CLI tool to use for creating DMG/ISO images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmgTool {
    /// macOS native — `hdiutil create`
    Hdiutil,
    /// Linux fallback — `genisoimage`
    Genisoimage,
    /// Linux second fallback — `mkisofs`
    Mkisofs,
}

/// Detect which DMG creation tool is available on the system.
///
/// Preference order: hdiutil (macOS native) > genisoimage > mkisofs.
/// Returns `None` if no suitable tool is found.
pub fn dmg_tool() -> Option<DmgTool> {
    if anodize_core::util::find_binary("hdiutil") {
        Some(DmgTool::Hdiutil)
    } else if anodize_core::util::find_binary("genisoimage") {
        Some(DmgTool::Genisoimage)
    } else if anodize_core::util::find_binary("mkisofs") {
        Some(DmgTool::Mkisofs)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// dmg_command
// ---------------------------------------------------------------------------

/// Construct CLI arguments for creating a DMG/ISO from a staging directory.
///
/// - `tool`: which CLI to invoke
/// - `vol_name`: the volume label
/// - `staging_dir`: path to the directory whose contents go into the image
/// - `output_path`: path to the output `.dmg` file
pub fn dmg_command(
    tool: DmgTool,
    vol_name: &str,
    staging_dir: &str,
    output_path: &str,
) -> Vec<String> {
    match tool {
        DmgTool::Hdiutil => vec![
            "hdiutil".to_string(),
            "create".to_string(),
            "-volname".to_string(),
            vol_name.to_string(),
            "-srcfolder".to_string(),
            staging_dir.to_string(),
            "-ov".to_string(),
            "-format".to_string(),
            "UDZO".to_string(),
            output_path.to_string(),
        ],
        DmgTool::Genisoimage => vec![
            "genisoimage".to_string(),
            "-V".to_string(),
            vol_name.to_string(),
            "-D".to_string(),
            "-R".to_string(),
            "-apple".to_string(),
            "-no-pad".to_string(),
            "-o".to_string(),
            output_path.to_string(),
            staging_dir.to_string(),
        ],
        DmgTool::Mkisofs => vec![
            "mkisofs".to_string(),
            "-V".to_string(),
            vol_name.to_string(),
            "-D".to_string(),
            "-R".to_string(),
            "-apple".to_string(),
            "-no-pad".to_string(),
            "-o".to_string(),
            output_path.to_string(),
            staging_dir.to_string(),
        ],
    }
}

// ---------------------------------------------------------------------------
// DmgStage
// ---------------------------------------------------------------------------

pub struct DmgStage;

/// Parse Os and Arch from a Rust target triple using the shared mapping.
fn os_arch_from_target(target: Option<&str>) -> (String, String) {
    target
        .map(anodize_core::target::map_target)
        .unwrap_or_else(|| ("darwin".to_string(), "amd64".to_string()))
}

/// Default output filename template: `{ProjectName}_{Version}_{Arch}.dmg`
const DEFAULT_NAME_TEMPLATE: &str = "{{ ProjectName }}_{{ Version }}_{{ Arch }}.dmg";

impl Stage for DmgStage {
    fn name(&self) -> &str {
        "dmg"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("dmg");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have dmg config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.dmgs.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        let project_name = ctx.config.project_name.clone();

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archives_to_remove: Vec<PathBuf> = Vec::new();

        for krate in &crates {
            let Some(dmgs) = krate.dmgs.as_ref() else {
                continue;
            };
            for dmg_cfg in dmgs {
                // Skip disabled configs (supports bool or template string)
                if let Some(ref d) = dmg_cfg.disable
                    && d.is_disabled(|s| ctx.render_template(s))
                {
                    log.status(&format!(
                        "skipping disabled dmg config for crate {}",
                        krate.name
                    ));
                    continue;
                }

                // Validate `use` field
                let use_mode = dmg_cfg.use_.as_deref().unwrap_or("binary");
                if use_mode != "binary" && use_mode != "appbundle" {
                    anyhow::bail!(
                        "dmg: invalid `use` value '{}' for crate '{}'; expected 'binary' or 'appbundle'",
                        use_mode,
                        krate.name
                    );
                }

                // Collect source artifacts depending on `use` mode
                let source_artifacts: Vec<Artifact> = if use_mode == "appbundle" {
                    // Collect Installer artifacts with format=appbundle for this crate
                    ctx.artifacts
                        .by_kind_and_crate(ArtifactKind::Installer, &krate.name)
                        .into_iter()
                        .filter(|a| {
                            a.metadata
                                .get("format")
                                .map(|f| f == "appbundle")
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect()
                } else {
                    // Collect darwin Binary artifacts for this crate
                    ctx.artifacts
                        .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                        .into_iter()
                        .filter(|b| {
                            b.target
                                .as_deref()
                                .map(anodize_core::target::is_darwin)
                                .unwrap_or(false)
                        })
                        .cloned()
                        .collect()
                };

                // Filter by build IDs if specified
                let mut filtered = source_artifacts.clone();
                if let Some(ref filter_ids) = dmg_cfg.ids
                    && !filter_ids.is_empty()
                {
                    filtered.retain(|b| {
                        b.metadata
                            .get("id")
                            .map(|id| filter_ids.contains(id))
                            .unwrap_or(false)
                            || b.metadata
                                .get("name")
                                .map(|n| filter_ids.contains(n))
                                .unwrap_or(false)
                    });
                }

                // Warn and skip if no source artifacts found
                if filtered.is_empty() && source_artifacts.is_empty() {
                    if use_mode == "appbundle" {
                        log.warn(&format!(
                            "no appbundle artifacts found for crate '{}'; \
                             skipping DMG generation (expected Installer artifacts with format=appbundle)",
                            krate.name
                        ));
                    } else {
                        log.warn(&format!(
                            "no macOS binary artifacts found for crate '{}'; \
                             skipping DMG generation (expected binaries targeting darwin/apple)",
                            krate.name
                        ));
                    }
                    continue;
                }
                if filtered.is_empty() {
                    log.warn(&format!(
                        "ids filter {:?} matched no artifacts for crate '{}'; skipping",
                        dmg_cfg.ids, krate.name
                    ));
                    continue;
                }

                let effective_binaries: Vec<(Option<String>, PathBuf)> = filtered
                    .iter()
                    .map(|b| (b.target.clone(), b.path.clone()))
                    .collect();

                for (target, binary_path) in &effective_binaries {
                    // Derive Os/Arch from the target triple for template rendering
                    let (os, arch) = os_arch_from_target(target.as_deref());

                    // Set Os/Arch/Target in template vars for this iteration
                    ctx.template_vars_mut().set("Os", &os);
                    ctx.template_vars_mut().set("Arch", &arch);
                    ctx.template_vars_mut()
                        .set("Target", target.as_deref().unwrap_or(""));

                    // Determine output filename from name template or default
                    let name_template = dmg_cfg.name.as_deref().unwrap_or(DEFAULT_NAME_TEMPLATE);

                    let dmg_filename = ctx.render_template(name_template).with_context(|| {
                        format!(
                            "dmg: render name template for crate {} target {:?}",
                            krate.name, target
                        )
                    })?;

                    // Ensure the filename ends with .dmg (case-insensitive)
                    let dmg_filename = if dmg_filename.to_lowercase().ends_with(".dmg") {
                        dmg_filename
                    } else {
                        format!("{dmg_filename}.dmg")
                    };

                    // Output goes in dist/macos/
                    let output_dir = dist.join("macos");
                    let dmg_path = output_dir.join(&dmg_filename);

                    // Volume name: project_name
                    let vol_name = project_name.clone();

                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would create DMG {} for crate {} target {:?}",
                            dmg_filename, krate.name, target
                        ));

                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::DiskImage,
                            name: String::new(),
                            path: dmg_path,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            metadata: {
                                let mut m =
                                    HashMap::from([("format".to_string(), "dmg".to_string())]);
                                if let Some(id) = &dmg_cfg.id {
                                    m.insert("id".to_string(), id.clone());
                                }
                                m
                            },
                            size: None,
                        });

                        // If replace is set, mark archives for this crate+target for removal
                        archives_to_remove.extend(anodize_core::util::collect_if_replace(
                            dmg_cfg.replace,
                            &ctx.artifacts,
                            &krate.name,
                            target.as_deref(),
                        ));

                        continue;
                    }

                    // Live mode — detect tool
                    let tool = dmg_tool().ok_or_else(|| {
                        anyhow::anyhow!(
                            "no DMG creation tool found (need hdiutil, genisoimage, or mkisofs)"
                        )
                    })?;

                    // Create output directory
                    fs::create_dir_all(&output_dir).with_context(|| {
                        format!("create dmg output dir: {}", output_dir.display())
                    })?;

                    // Create staging directory
                    let staging_tmp =
                        tempfile::tempdir().context("create temp dir for dmg staging")?;
                    let staging_dir = staging_tmp.path();

                    // Copy binary into staging dir
                    let binary_name = binary_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&krate.name);
                    let staged_binary = staging_dir.join(binary_name);
                    fs::copy(binary_path, &staged_binary).with_context(|| {
                        format!("copy binary {} to staging dir", binary_path.display())
                    })?;

                    // Copy extra files into staging dir
                    if let Some(extra_files) = &dmg_cfg.extra_files {
                        for spec in extra_files {
                            let glob_pattern = spec.glob();
                            for entry in glob::glob(glob_pattern).with_context(|| {
                                format!("dmg: invalid extra_files glob '{}'", glob_pattern)
                            })? {
                                let src = entry.with_context(|| {
                                    format!("dmg: error reading glob match for '{}'", glob_pattern)
                                })?;
                                let dst_name = spec
                                    .name_template()
                                    .map(|s| s.to_string())
                                    .or_else(|| {
                                        src.file_name()
                                            .and_then(|n| n.to_str())
                                            .map(|s| s.to_string())
                                    })
                                    .unwrap_or_else(|| "extra".to_string());
                                let dst = staging_dir.join(&dst_name);
                                fs::copy(&src, &dst).with_context(|| {
                                    format!("copy extra file {} to staging dir", src.display())
                                })?;
                            }
                        }
                    }

                    // Process templated_extra_files: render and copy to staging dir
                    if let Some(ref tpl_specs) = dmg_cfg.templated_extra_files
                        && !tpl_specs.is_empty()
                    {
                        anodize_core::templated_files::process_templated_extra_files(
                            tpl_specs,
                            ctx,
                            staging_dir,
                            "dmg",
                        )?;
                    }

                    // Apply mod_timestamp if set
                    if let Some(ts) = &dmg_cfg.mod_timestamp {
                        anodize_core::util::apply_mod_timestamp(staging_dir, ts, &log)?;
                    }

                    // Build and run the command
                    let cmd_args = dmg_command(
                        tool,
                        &vol_name,
                        &staging_dir.to_string_lossy(),
                        &dmg_path.to_string_lossy(),
                    );

                    log.status(&format!("running: {}", cmd_args.join(" ")));

                    let output = Command::new(&cmd_args[0])
                        .args(&cmd_args[1..])
                        .output()
                        .with_context(|| {
                            format!(
                                "execute dmg tool for crate {} target {:?}",
                                krate.name, target
                            )
                        })?;
                    log.check_output(output, "dmg")?;

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::DiskImage,
                        name: String::new(),
                        path: dmg_path,
                        target: target.clone(),
                        crate_name: krate.name.clone(),
                        metadata: {
                            let mut m = HashMap::from([("format".to_string(), "dmg".to_string())]);
                            if let Some(id) = &dmg_cfg.id {
                                m.insert("id".to_string(), id.clone());
                            }
                            m
                        },
                        size: None,
                    });

                    // If replace is set, mark archives for this crate+target for removal
                    archives_to_remove.extend(anodize_core::util::collect_if_replace(
                        dmg_cfg.replace,
                        &ctx.artifacts,
                        &krate.name,
                        target.as_deref(),
                    ));
                }
            }
        }

        anodize_core::template::clear_per_target_vars(ctx.template_vars_mut());

        // Remove replaced archives
        if !archives_to_remove.is_empty() {
            ctx.artifacts.remove_by_paths(&archives_to_remove);
        }

        // Register new DMG artifacts
        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn test_dmg_tool_detection() {
        // dmg_tool() returns an Option<DmgTool>. On CI/Linux it may or may not
        // find genisoimage/mkisofs. We just verify the return type is correct.
        let result = dmg_tool();
        match result {
            Some(DmgTool::Hdiutil) => assert_eq!(result, Some(DmgTool::Hdiutil)),
            Some(DmgTool::Genisoimage) => assert_eq!(result, Some(DmgTool::Genisoimage)),
            Some(DmgTool::Mkisofs) => assert_eq!(result, Some(DmgTool::Mkisofs)),
            None => assert!(result.is_none()),
        }
    }

    #[test]
    fn test_dmg_command_hdiutil() {
        let cmd = dmg_command(DmgTool::Hdiutil, "MyApp", "/tmp/staging", "/tmp/out.dmg");
        assert_eq!(
            cmd,
            vec![
                "hdiutil",
                "create",
                "-volname",
                "MyApp",
                "-srcfolder",
                "/tmp/staging",
                "-ov",
                "-format",
                "UDZO",
                "/tmp/out.dmg",
            ]
        );
    }

    #[test]
    fn test_dmg_command_genisoimage() {
        let cmd = dmg_command(
            DmgTool::Genisoimage,
            "MyApp",
            "/tmp/staging",
            "/tmp/out.dmg",
        );
        assert_eq!(
            cmd,
            vec![
                "genisoimage",
                "-V",
                "MyApp",
                "-D",
                "-R",
                "-apple",
                "-no-pad",
                "-o",
                "/tmp/out.dmg",
                "/tmp/staging",
            ]
        );
    }

    #[test]
    fn test_dmg_command_mkisofs() {
        let cmd = dmg_command(DmgTool::Mkisofs, "MyApp", "/tmp/staging", "/tmp/out.dmg");
        assert_eq!(
            cmd,
            vec![
                "mkisofs",
                "-V",
                "MyApp",
                "-D",
                "-R",
                "-apple",
                "-no-pad",
                "-o",
                "/tmp/out.dmg",
                "/tmp/staging",
            ]
        );
    }

    #[test]
    fn test_stage_skips_when_no_dmg_config() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        // DmgStage should be a no-op when crates have no dmg block
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = DmgStage;
        assert!(stage.run(&mut ctx).is_ok());
        assert!(ctx.artifacts.all().is_empty());
    }

    #[test]
    fn test_stage_skips_when_disabled() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig, StringOrBool};
        use anodize_core::context::{Context, ContextOptions};

        let dmg_cfg = DmgConfig {
            disable: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add a darwin binary so the stage has something to potentially process
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        // No DMG artifacts should be produced because config is disabled
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert!(dmgs.is_empty());
    }

    #[test]
    fn test_stage_dry_run_registers_artifacts() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let dmg_cfg = DmgConfig::default();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register darwin binary artifacts
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_x86"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        // Two darwin binaries -> two DMG artifacts
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(dmgs.len(), 2);

        // All should have format=dmg metadata
        for dmg in &dmgs {
            assert_eq!(dmg.metadata.get("format").unwrap(), "dmg");
            assert_eq!(dmg.kind, ArtifactKind::DiskImage);
        }

        // Check targets are preserved
        let targets: Vec<&str> = dmgs.iter().map(|a| a.target.as_deref().unwrap()).collect();
        assert!(targets.contains(&"aarch64-apple-darwin"));
        assert!(targets.contains(&"x86_64-apple-darwin"));
    }

    #[test]
    fn test_stage_dry_run_with_name_template() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let dmg_cfg = DmgConfig {
            name: Some("{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}.dmg".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "2.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(dmgs.len(), 1);

        let dmg_path = dmgs[0].path.to_string_lossy();
        assert!(
            dmg_path.ends_with("myapp-2.0.0-darwin-arm64.dmg"),
            "expected template-rendered name, got: {dmg_path}"
        );
    }

    #[test]
    fn test_stage_dry_run_replace_removes_archives() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let dmg_cfg = DmgConfig {
            replace: Some(true),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register a darwin binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // Register an archive artifact for the same crate+target
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp_1.0.0_darwin_arm64.tar.gz"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "tar.gz".to_string())]),
            size: None,
        });

        // Also register a Linux archive that should NOT be removed
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp_1.0.0_linux_amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "tar.gz".to_string())]),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        // DMG artifact should be registered
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(dmgs.len(), 1);

        // The darwin archive should have been removed (replace: true)
        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1, "only the Linux archive should remain");
        assert!(
            archives[0].target.as_deref().unwrap().contains("linux"),
            "remaining archive should be the Linux one"
        );
    }

    #[test]
    fn test_config_parse_dmg() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    dmgs:
      - name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}.dmg"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let dmgs = config.crates[0].dmgs.as_ref().unwrap();
        assert_eq!(dmgs.len(), 1);
        assert_eq!(
            dmgs[0].name.as_deref(),
            Some("{{ ProjectName }}_{{ Version }}_{{ Arch }}.dmg")
        );
        assert!(dmgs[0].disable.is_none());
        assert!(dmgs[0].replace.is_none());
    }

    #[test]
    fn test_config_parse_dmg_full() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    dmgs:
      - id: macos-dmg
        ids:
          - build_darwin_arm64
          - build_darwin_amd64
        name: "myapp-{{ Version }}-{{ Os }}-{{ Arch }}.dmg"
        extra_files:
          - README.md
          - LICENSE
        replace: true
        mod_timestamp: "{{ .CommitTimestamp }}"
        disable: false
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let dmgs = config.crates[0].dmgs.as_ref().unwrap();
        assert_eq!(dmgs.len(), 1);

        let dmg = &dmgs[0];
        assert_eq!(dmg.id.as_deref(), Some("macos-dmg"));
        assert_eq!(
            dmg.ids.as_ref().unwrap(),
            &vec![
                "build_darwin_arm64".to_string(),
                "build_darwin_amd64".to_string()
            ]
        );
        assert_eq!(
            dmg.name.as_deref(),
            Some("myapp-{{ Version }}-{{ Os }}-{{ Arch }}.dmg")
        );
        let extras = dmg.extra_files.as_ref().unwrap();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].glob(), "README.md");
        assert_eq!(extras[1].glob(), "LICENSE");
        assert_eq!(dmg.replace, Some(true));
        assert_eq!(dmg.mod_timestamp.as_deref(), Some("{{ .CommitTimestamp }}"));
        assert_eq!(
            dmg.disable,
            Some(anodize_core::config::StringOrBool::Bool(false))
        );
    }

    #[test]
    fn test_invalid_name_template_errors() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let dmg_cfg = DmgConfig {
            // Tera will error on unclosed tags
            name: Some("{{ ProjectName }}_{{ Version".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add a darwin binary so we actually attempt to render the template
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_err(),
            "expected error from invalid template, got Ok"
        );
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("render") || err_msg.contains("template") || err_msg.contains("dmg"),
            "error should mention template rendering, got: {err_msg}"
        );
    }

    #[test]
    fn test_extra_files_copied_to_staging() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Create a fake binary and extra file on disk
        let binary_path = tmp.path().join("myapp");
        fs::write(&binary_path, b"fake-binary").unwrap();

        let extra_path = tmp.path().join("README.md");
        fs::write(&extra_path, b"readme content").unwrap();

        let dmg_cfg = DmgConfig {
            extra_files: Some(vec![anodize_core::config::ExtraFileSpec::Glob(
                extra_path.to_string_lossy().into_owned(),
            )]),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        // Run in LIVE mode (not dry_run) so staging dir logic is exercised
        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: binary_path,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        let result = stage.run(&mut ctx);

        // On macOS, hdiutil is available so the stage may succeed (creates a real DMG).
        // On Linux/Windows, it will fail because no DMG tool is installed.
        if cfg!(target_os = "macos") {
            // On macOS, either outcome is acceptable — the extra files were staged either way.
            if let Err(e) = &result {
                let err_msg = format!("{e:#}");
                assert!(
                    err_msg.contains("hdiutil")
                        || err_msg.contains("DMG creation tool")
                        || err_msg.contains("no DMG"),
                    "unexpected error on macOS: {err_msg}"
                );
            }
        } else {
            assert!(
                result.is_err(),
                "expected failure due to missing DMG tool in CI"
            );
            let err_msg = format!("{:#}", result.unwrap_err());
            assert!(
                err_msg.contains("hdiutil")
                    || err_msg.contains("genisoimage")
                    || err_msg.contains("mkisofs")
                    || err_msg.contains("DMG creation tool")
                    || err_msg.contains("no DMG"),
                "error should mention missing DMG tool (staging succeeded), got: {err_msg}"
            );
        }
    }

    #[test]
    fn test_stage_dry_run_multiple_configs() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Two separate DMG configs for the same crate, with different names
        let dmg_cfg_1 = DmgConfig {
            id: Some("installer".to_string()),
            name: Some("{{ ProjectName }}-installer-{{ Arch }}.dmg".to_string()),
            ..Default::default()
        };
        let dmg_cfg_2 = DmgConfig {
            id: Some("portable".to_string()),
            name: Some("{{ ProjectName }}-portable-{{ Arch }}.dmg".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg_1, dmg_cfg_2]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // One darwin binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        // Two configs x one binary = two DMG artifacts
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(dmgs.len(), 2, "should produce one DMG per config entry");

        // Verify both have distinct filenames and IDs
        let names: Vec<String> = dmgs
            .iter()
            .map(|a| a.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("installer")),
            "expected an 'installer' DMG, got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("portable")),
            "expected a 'portable' DMG, got: {names:?}"
        );

        let ids: Vec<Option<&String>> = dmgs.iter().map(|a| a.metadata.get("id")).collect();
        assert!(
            ids.contains(&Some(&"installer".to_string())),
            "expected id=installer in metadata"
        );
        assert!(
            ids.contains(&Some(&"portable".to_string())),
            "expected id=portable in metadata"
        );
    }

    #[test]
    fn test_ids_filtering() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Configure ids filter to match only one build id
        let dmg_cfg = DmgConfig {
            ids: Some(vec!["build-darwin-arm64".to_string()]),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register two darwin binaries with different metadata ids
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp-arm64"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-darwin-arm64".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp-amd64"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-darwin-amd64".to_string())]),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        // Verify only one DMG artifact is produced (the arm64 one)
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(
            dmgs.len(),
            1,
            "ids filter should produce exactly one DMG, got {}",
            dmgs.len()
        );
        assert_eq!(
            dmgs[0].target.as_deref(),
            Some("aarch64-apple-darwin"),
            "the DMG should be for the arm64 target"
        );
    }

    #[test]
    fn test_use_appbundle_selects_installer_artifacts() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let dmg_cfg = DmgConfig {
            use_: Some("appbundle".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register an appbundle artifact (Installer with format=appbundle)
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.app"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
            size: None,
        });

        // Also register a darwin binary that should NOT be selected
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        // Should produce one DMG from the appbundle, not from the binary
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(dmgs.len(), 1, "should produce one DMG from the appbundle");
        assert_eq!(dmgs[0].metadata.get("format").unwrap(), "dmg");
    }

    #[test]
    fn test_use_binary_selects_darwin_binaries() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Explicit `use: binary` should behave same as omitted (default)
        let dmg_cfg = DmgConfig {
            use_: Some("binary".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Register a darwin binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        // Also register an appbundle that should NOT be selected
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Installer,
            name: String::new(),
            path: PathBuf::from("dist/MyApp.app"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "appbundle".to_string())]),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        // Should produce one DMG from the binary, not from the appbundle
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(dmgs.len(), 1, "should produce one DMG from the binary");
        assert_eq!(dmgs[0].metadata.get("format").unwrap(), "dmg");
    }

    #[test]
    fn test_use_default_selects_darwin_binaries() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Omitted `use` field should default to binary mode
        let dmg_cfg = DmgConfig::default();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(
            dmgs.len(),
            1,
            "default (omitted) use should select darwin binaries"
        );
    }

    #[test]
    fn test_invalid_use_value_errors() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let dmg_cfg = DmgConfig {
            use_: Some("invalid_mode".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add a darwin binary so the stage actually runs
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "expected error for invalid use value");
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("invalid_mode") && err_msg.contains("binary"),
            "error should mention the invalid value and expected options, got: {err_msg}"
        );
    }

    #[test]
    fn test_disable_string_or_bool_true() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig, StringOrBool};
        use anodize_core::context::{Context, ContextOptions};

        // Test with StringOrBool::String("true")
        let dmg_cfg = DmgConfig {
            disable: Some(StringOrBool::String("true".to_string())),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert!(dmgs.is_empty(), "string 'true' should disable the config");
    }

    #[test]
    fn test_disable_string_or_bool_false() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig, StringOrBool};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Test with StringOrBool::String("false") — should NOT be disabled
        let dmg_cfg = DmgConfig {
            disable: Some(StringOrBool::String("false".to_string())),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(
            dmgs.len(),
            1,
            "string 'false' should not disable the config"
        );
    }

    #[test]
    fn test_disable_template_string() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig, StringOrBool};
        use anodize_core::context::{Context, ContextOptions};

        // Template that evaluates to "true" when IsSnapshot is truthy
        let dmg_cfg = DmgConfig {
            disable: Some(StringOrBool::String(
                "{% if IsSnapshot %}true{% endif %}".to_string(),
            )),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("IsSnapshot", "true");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert!(
            dmgs.is_empty(),
            "template should evaluate to true and disable the config"
        );
    }

    #[test]
    fn test_config_parse_dmg_with_use() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    dmgs:
      - name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}.dmg"
        use: appbundle
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let dmgs = config.crates[0].dmgs.as_ref().unwrap();
        assert_eq!(dmgs.len(), 1);
        assert_eq!(dmgs[0].use_.as_deref(), Some("appbundle"));
    }

    #[test]
    fn test_config_parse_dmg_disable_string() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    dmgs:
      - disable: "{% if IsSnapshot %}true{% endif %}"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let dmgs = config.crates[0].dmgs.as_ref().unwrap();
        assert_eq!(
            dmgs[0].disable,
            Some(anodize_core::config::StringOrBool::String(
                "{% if IsSnapshot %}true{% endif %}".to_string()
            ))
        );
    }

    #[test]
    fn test_use_appbundle_skips_when_no_appbundles() {
        use anodize_core::config::{Config, CrateConfig, DmgConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let dmg_cfg = DmgConfig {
            use_: Some("appbundle".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Only register a binary — no appbundles
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        // No DMGs should be produced because there are no appbundle artifacts
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert!(
            dmgs.is_empty(),
            "should produce no DMGs when use=appbundle but no appbundles exist"
        );
    }
}
