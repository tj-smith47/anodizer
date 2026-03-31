use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Default NSIS script
// ---------------------------------------------------------------------------

/// Generate a default `.nsi` script that installs the binary and creates an
/// uninstaller. The caller passes concrete values via `-D` defines to
/// `makensis`, so the script uses `${PRODUCT_NAME}`, `${OUTPUT_FILE}`,
/// `${BINARY_PATH}`, and `${BINARY_NAME}` placeholders.
pub fn default_nsi_script() -> &'static str {
    r#"!include "MUI2.nsh"
Name "${PRODUCT_NAME}"
OutFile "${OUTPUT_FILE}"
InstallDir "$PROGRAMFILES\${PRODUCT_NAME}"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
Section "Install"
    SetOutPath "$INSTDIR"
    File "${BINARY_PATH}"
    CreateShortCut "$DESKTOP\${PRODUCT_NAME}.lnk" "$INSTDIR\${BINARY_NAME}"
SectionEnd
Section "Uninstall"
    Delete "$INSTDIR\${BINARY_NAME}"
    Delete "$DESKTOP\${PRODUCT_NAME}.lnk"
    RMDir "$INSTDIR"
SectionEnd
"#
}

// ---------------------------------------------------------------------------
// makensis command construction
// ---------------------------------------------------------------------------

/// Build the `makensis` CLI arguments.
///
/// - `script_path`: path to the `.nsi` script file
/// - `defines`: key-value pairs passed as `-DKEY=VALUE` flags
pub fn nsis_command(script_path: &str, defines: &[(&str, &str)]) -> Vec<String> {
    let mut args = vec!["makensis".to_string()];
    for (key, value) in defines {
        args.push(format!("-D{key}={value}"));
    }
    args.push(script_path.to_string());
    args
}

// ---------------------------------------------------------------------------
// NsisStage
// ---------------------------------------------------------------------------

pub struct NsisStage;

/// Parse Os and Arch from a Rust target triple using the shared mapping.
fn os_arch_from_target(target: Option<&str>) -> (String, String) {
    target
        .map(anodize_core::target::map_target)
        .unwrap_or_else(|| ("windows".to_string(), "amd64".to_string()))
}

/// Default output filename template: `{ProjectName}_{Version}_{Arch}_setup.exe`
const DEFAULT_NAME_TEMPLATE: &str = "{{ ProjectName }}_{{ Version }}_{{ Arch }}_setup.exe";

impl Stage for NsisStage {
    fn name(&self) -> &str {
        "nsis"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("nsis");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have NSIS config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.nsis.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        let project_name = ctx.config.project_name.clone();

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archives_to_remove: Vec<PathBuf> = Vec::new();

        for krate in &crates {
            let nsis_configs = krate.nsis.as_ref().unwrap();

            // Collect Windows binary artifacts for this crate
            let windows_binaries: Vec<_> = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                .into_iter()
                .filter(|b| {
                    b.target
                        .as_deref()
                        .map(anodize_core::target::is_windows)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            for nsis_cfg in nsis_configs {
                // Skip disabled configs (template-based: evaluate and check for "true")
                if let Some(disable_str) = &nsis_cfg.disable {
                    let rendered = ctx
                        .render_template(disable_str)
                        .unwrap_or_else(|_| disable_str.clone());
                    if rendered.trim() == "true" {
                        log.status(&format!(
                            "skipping disabled NSIS config for crate {}",
                            krate.name
                        ));
                        continue;
                    }
                }

                // Filter by build IDs if specified
                let mut filtered = windows_binaries.clone();
                if let Some(ref filter_ids) = nsis_cfg.ids
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

                // Warn and skip if no Windows binaries found
                if filtered.is_empty() && windows_binaries.is_empty() {
                    log.warn(&format!(
                        "no Windows binary artifacts found for crate '{}'; \
                         skipping NSIS generation (expected binaries targeting windows)",
                        krate.name
                    ));
                    continue;
                }
                if filtered.is_empty() {
                    log.warn(&format!(
                        "ids filter {:?} matched no binaries for crate '{}'; skipping",
                        nsis_cfg.ids, krate.name
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

                    // Set Os/Arch in template vars for this iteration
                    ctx.template_vars_mut().set("Os", &os);
                    ctx.template_vars_mut().set("Arch", &arch);

                    // Determine output filename from name template or default
                    let name_template =
                        nsis_cfg.name.as_deref().unwrap_or(DEFAULT_NAME_TEMPLATE);

                    let exe_filename = ctx.render_template(name_template).with_context(|| {
                        format!(
                            "nsis: render name template for crate {} target {:?}",
                            krate.name, target
                        )
                    })?;

                    // Ensure the filename ends with .exe (case-insensitive)
                    let exe_filename = if exe_filename.to_lowercase().ends_with(".exe") {
                        exe_filename
                    } else {
                        format!("{exe_filename}.exe")
                    };

                    // Output goes in dist/windows/
                    let output_dir = dist.join("windows");
                    let exe_path = output_dir.join(&exe_filename);

                    let binary_name = binary_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&krate.name);

                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would create NSIS installer {} for crate {} target {:?}",
                            exe_filename, krate.name, target
                        ));

                        if let Some(ts) = &nsis_cfg.mod_timestamp {
                            log.status(&format!("(dry-run) would apply mod_timestamp={ts}"));
                        }

                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::Installer,
                            name: String::new(),
                            path: exe_path,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            metadata: {
                                let mut m =
                                    HashMap::from([("format".to_string(), "nsis".to_string())]);
                                if let Some(id) = &nsis_cfg.id {
                                    m.insert("id".to_string(), id.clone());
                                }
                                m
                            },
                        });

                        // If replace is set, mark archives for this crate+target for removal
                        if nsis_cfg.replace.unwrap_or(false) {
                            archives_to_remove.extend(
                                anodize_core::util::collect_replace_archives(
                                    &ctx.artifacts,
                                    &krate.name,
                                    target.as_deref(),
                                ),
                            );
                        }

                        continue;
                    }

                    // Live mode — check that makensis is available
                    if !anodize_core::util::find_binary("makensis") {
                        anyhow::bail!(
                            "makensis not found on PATH; install NSIS to create Windows installers"
                        );
                    }

                    // Create output directory
                    fs::create_dir_all(&output_dir).with_context(|| {
                        format!("create NSIS output dir: {}", output_dir.display())
                    })?;

                    // Create staging directory
                    let staging_tmp =
                        tempfile::tempdir().context("create temp dir for NSIS staging")?;
                    let staging_dir = staging_tmp.path();

                    // Copy binary into staging dir
                    let staged_binary = staging_dir.join(binary_name);
                    fs::copy(binary_path, &staged_binary).with_context(|| {
                        format!("copy binary {} to staging dir", binary_path.display())
                    })?;

                    // Copy extra files into staging dir (ExtraFileSpec: resolve globs)
                    if let Some(extra_files) = &nsis_cfg.extra_files {
                        for spec in extra_files {
                            let pattern = spec.glob();
                            match glob::glob(pattern) {
                                Ok(entries) => {
                                    for entry in entries.flatten() {
                                        if entry.is_file() {
                                            let dst_name = entry
                                                .file_name()
                                                .and_then(|n| n.to_str())
                                                .unwrap_or("extra");
                                            let dst = staging_dir.join(dst_name);
                                            fs::copy(&entry, &dst).with_context(|| {
                                                format!(
                                                    "copy extra file {} to staging dir",
                                                    entry.display()
                                                )
                                            })?;
                                        }
                                    }
                                }
                                Err(e) => {
                                    log.warn(&format!(
                                        "invalid extra_files glob pattern '{}': {}",
                                        pattern, e
                                    ));
                                }
                            }
                        }
                    }

                    // Determine the .nsi script to use
                    let nsi_script_path = if let Some(script_tmpl) = &nsis_cfg.script {
                        // User-provided script: read, render through template engine, write
                        // to staging dir
                        let script_content = fs::read_to_string(script_tmpl).with_context(|| {
                            format!("nsis: read script template: {script_tmpl}")
                        })?;
                        let rendered = ctx.render_template(&script_content).with_context(|| {
                            format!("nsis: render script template: {script_tmpl}")
                        })?;
                        let rendered_path = staging_dir.join("installer.nsi");
                        fs::write(&rendered_path, &rendered).with_context(|| {
                            format!("nsis: write rendered script to {}", rendered_path.display())
                        })?;
                        rendered_path
                    } else {
                        // Use default script
                        let default_path = staging_dir.join("installer.nsi");
                        fs::write(&default_path, default_nsi_script()).with_context(|| {
                            format!(
                                "nsis: write default script to {}",
                                default_path.display()
                            )
                        })?;
                        default_path
                    };

                    // Apply mod_timestamp if set (to staging dir contents)
                    if let Some(ts) = &nsis_cfg.mod_timestamp {
                        anodize_core::util::apply_mod_timestamp(staging_dir, ts, &log)?;
                    }

                    // Build makensis command with -D defines
                    let output_file_str = exe_path.to_string_lossy().into_owned();
                    let binary_path_str = staged_binary.to_string_lossy().into_owned();
                    let script_path_str = nsi_script_path.to_string_lossy().into_owned();
                    let defines = [
                        ("PRODUCT_NAME", project_name.as_str()),
                        ("OUTPUT_FILE", output_file_str.as_str()),
                        ("BINARY_PATH", binary_path_str.as_str()),
                        ("BINARY_NAME", binary_name),
                    ];
                    let cmd_args = nsis_command(&script_path_str, &defines);

                    log.status(&format!("running: {}", cmd_args.join(" ")));

                    let output = Command::new(&cmd_args[0])
                        .args(&cmd_args[1..])
                        .output()
                        .with_context(|| {
                            format!(
                                "execute makensis for crate {} target {:?}",
                                krate.name, target
                            )
                        })?;
                    log.check_output(output, "nsis")?;

                    // Apply mod_timestamp to the output .exe if set
                    if let Some(ts) = &nsis_cfg.mod_timestamp
                        && exe_path.exists()
                    {
                        let mtime = anodize_core::util::parse_mod_timestamp(ts)?;
                        anodize_core::util::set_file_mtime(&exe_path, mtime)?;
                        log.status(&format!(
                            "applied mod_timestamp={ts} to {}",
                            exe_path.display()
                        ));
                    }

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::Installer,
                        name: String::new(),
                        path: exe_path,
                        target: target.clone(),
                        crate_name: krate.name.clone(),
                        metadata: {
                            let mut m =
                                HashMap::from([("format".to_string(), "nsis".to_string())]);
                            if let Some(id) = &nsis_cfg.id {
                                m.insert("id".to_string(), id.clone());
                            }
                            m
                        },
                    });

                    // If replace is set, mark archives for this crate+target for removal
                    if nsis_cfg.replace.unwrap_or(false) {
                        archives_to_remove.extend(anodize_core::util::collect_replace_archives(
                            &ctx.artifacts,
                            &krate.name,
                            target.as_deref(),
                        ));
                    }
                }
            }
        }

        // Remove replaced archives
        if !archives_to_remove.is_empty() {
            ctx.artifacts.remove_by_paths(&archives_to_remove);
        }

        // Register new NSIS artifacts
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
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Default NSI script generation
    // -----------------------------------------------------------------------

    #[test]
    fn test_default_nsi_script_generation() {
        let script = default_nsi_script();

        // Verify the script contains the expected NSIS sections
        assert!(
            script.contains("!include \"MUI2.nsh\""),
            "should include MUI2"
        );
        assert!(
            script.contains("Name \"${PRODUCT_NAME}\""),
            "should reference PRODUCT_NAME define"
        );
        assert!(
            script.contains("OutFile \"${OUTPUT_FILE}\""),
            "should reference OUTPUT_FILE define"
        );
        assert!(
            script.contains("InstallDir \"$PROGRAMFILES\\${PRODUCT_NAME}\""),
            "should set install dir under Program Files"
        );
        assert!(
            script.contains("Section \"Install\""),
            "should have Install section"
        );
        assert!(
            script.contains("File \"${BINARY_PATH}\""),
            "should include the binary"
        );
        assert!(
            script.contains("Section \"Uninstall\""),
            "should have Uninstall section"
        );
        assert!(
            script.contains("Delete \"$INSTDIR\\${BINARY_NAME}\""),
            "uninstaller should delete the binary"
        );
        assert!(
            script.contains("RMDir \"$INSTDIR\""),
            "uninstaller should remove the install directory"
        );
        assert!(
            script.contains("CreateShortCut"),
            "should create a desktop shortcut"
        );
    }

    // -----------------------------------------------------------------------
    // Output filename extension enforcement
    // -----------------------------------------------------------------------

    #[test]
    fn test_nsis_output_filename_extension() {
        // When a filename already ends with .exe, it should not be doubled
        let name = "myapp_1.0.0_amd64_setup.exe";
        assert!(name.to_lowercase().ends_with(".exe"));

        // When a filename does not end with .exe, it should be appended
        let name_no_ext = "myapp_1.0.0_amd64_setup";
        assert!(!name_no_ext.to_lowercase().ends_with(".exe"));
        let fixed = format!("{name_no_ext}.exe");
        assert_eq!(fixed, "myapp_1.0.0_amd64_setup.exe");

        // Case-insensitive check
        let name_upper = "myapp_1.0.0_amd64_setup.EXE";
        assert!(name_upper.to_lowercase().ends_with(".exe"));
    }

    // -----------------------------------------------------------------------
    // makensis command construction
    // -----------------------------------------------------------------------

    #[test]
    fn test_nsis_command_args() {
        let defines = [
            ("PRODUCT_NAME", "MyApp"),
            ("OUTPUT_FILE", "/tmp/out/MyApp_setup.exe"),
            ("BINARY_PATH", "/tmp/staging/myapp.exe"),
            ("BINARY_NAME", "myapp.exe"),
        ];
        let cmd = nsis_command("/tmp/staging/installer.nsi", &defines);

        assert_eq!(cmd[0], "makensis");
        assert_eq!(cmd[1], "-DPRODUCT_NAME=MyApp");
        assert_eq!(cmd[2], "-DOUTPUT_FILE=/tmp/out/MyApp_setup.exe");
        assert_eq!(cmd[3], "-DBINARY_PATH=/tmp/staging/myapp.exe");
        assert_eq!(cmd[4], "-DBINARY_NAME=myapp.exe");
        assert_eq!(cmd[5], "/tmp/staging/installer.nsi");
        assert_eq!(cmd.len(), 6);
    }

    #[test]
    fn test_nsis_command_no_defines() {
        let cmd = nsis_command("/tmp/script.nsi", &[]);
        assert_eq!(cmd, vec!["makensis", "/tmp/script.nsi"]);
    }

    // -----------------------------------------------------------------------
    // Stage behavior tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_skips_when_no_nsis_config() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = NsisStage;
        assert!(stage.run(&mut ctx).is_ok());
        assert!(ctx.artifacts.all().is_empty());
    }

    #[test]
    fn test_stage_skips_when_disabled() {
        use anodize_core::config::{Config, CrateConfig, NsisConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let nsis_cfg = NsisConfig {
            disable: Some("true".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nsis: Some(vec![nsis_cfg]),
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

        // Add a Windows binary so the stage has something to potentially process
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = NsisStage;
        stage.run(&mut ctx).unwrap();

        // No installer artifacts should be produced because config is disabled
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert!(installers.is_empty());
    }

    #[test]
    fn test_stage_skips_when_disabled_via_template() {
        use anodize_core::config::{Config, CrateConfig, NsisConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Template evaluates to "true" when IsSnapshot is set
        let nsis_cfg = NsisConfig {
            disable: Some("{{ IsSnapshot }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nsis: Some(vec![nsis_cfg]),
            ..Default::default()
        }];

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
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = NsisStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert!(installers.is_empty(), "should be disabled by template");
    }

    #[test]
    fn test_stage_dry_run_registers_artifacts() {
        use anodize_core::config::{Config, CrateConfig, NsisConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let nsis_cfg = NsisConfig::default();

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nsis: Some(vec![nsis_cfg]),
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

        // Register Windows binary artifacts
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_arm.exe"),
            target: Some("aarch64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = NsisStage;
        stage.run(&mut ctx).unwrap();

        // Two Windows binaries -> two installer artifacts
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 2);

        // All should have format=nsis metadata
        for inst in &installers {
            assert_eq!(inst.metadata.get("format").unwrap(), "nsis");
            assert_eq!(inst.kind, ArtifactKind::Installer);
        }

        // Check targets are preserved
        let targets: Vec<&str> = installers
            .iter()
            .map(|a| a.target.as_deref().unwrap())
            .collect();
        assert!(targets.contains(&"x86_64-pc-windows-msvc"));
        assert!(targets.contains(&"aarch64-pc-windows-msvc"));
    }

    #[test]
    fn test_stage_dry_run_with_name_template() {
        use anodize_core::config::{Config, CrateConfig, NsisConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let nsis_cfg = NsisConfig {
            name: Some("{{ ProjectName }}-{{ Version }}-{{ Arch }}-setup.exe".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nsis: Some(vec![nsis_cfg]),
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
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = NsisStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);

        let installer_path = installers[0].path.to_string_lossy();
        assert!(
            installer_path.ends_with("myapp-2.0.0-amd64-setup.exe"),
            "expected template-rendered name, got: {installer_path}"
        );
    }

    #[test]
    fn test_stage_dry_run_replace_removes_archives() {
        use anodize_core::config::{Config, CrateConfig, NsisConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let nsis_cfg = NsisConfig {
            replace: Some(true),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nsis: Some(vec![nsis_cfg]),
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

        // Register a Windows binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        // Register an archive artifact for the same crate+target
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp_1.0.0_windows_amd64.zip"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "zip".to_string())]),
        });

        // Also register a Linux archive that should NOT be removed
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp_1.0.0_linux_amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "tar.gz".to_string())]),
        });

        let stage = NsisStage;
        stage.run(&mut ctx).unwrap();

        // NSIS installer artifact should be registered
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);

        // The Windows archive should have been removed (replace: true)
        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1, "only the Linux archive should remain");
        assert!(
            archives[0].target.as_deref().unwrap().contains("linux"),
            "remaining archive should be the Linux one"
        );
    }

    #[test]
    fn test_stage_ignores_non_windows_binaries() {
        use anodize_core::config::{Config, CrateConfig, NsisConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let nsis_cfg = NsisConfig::default();

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nsis: Some(vec![nsis_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Only add Linux and macOS binaries — no Windows binaries
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_darwin"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = NsisStage;
        stage.run(&mut ctx).unwrap();

        // No installer artifacts — no Windows binaries available
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert!(
            installers.is_empty(),
            "should produce no installers for non-Windows binaries"
        );
    }

    #[test]
    fn test_config_parse_nsis() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nsis:
      - name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}_setup.exe"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nsis_configs = config.crates[0].nsis.as_ref().unwrap();
        assert_eq!(nsis_configs.len(), 1);
        assert_eq!(
            nsis_configs[0].name.as_deref(),
            Some("{{ ProjectName }}_{{ Version }}_{{ Arch }}_setup.exe")
        );
        assert!(nsis_configs[0].disable.is_none());
        assert!(nsis_configs[0].replace.is_none());
        assert!(nsis_configs[0].script.is_none());
    }

    #[test]
    fn test_config_parse_nsis_full() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nsis:
      - id: windows-nsis
        ids:
          - build_windows_amd64
          - build_windows_arm64
        name: "myapp-{{ Version }}-{{ Arch }}-setup.exe"
        script: "installer.nsi"
        extra_files:
          - README.md
          - LICENSE
        replace: true
        mod_timestamp: "{{ .CommitTimestamp }}"
        disable: "false"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nsis_configs = config.crates[0].nsis.as_ref().unwrap();
        assert_eq!(nsis_configs.len(), 1);

        let nsis = &nsis_configs[0];
        assert_eq!(nsis.id.as_deref(), Some("windows-nsis"));
        assert_eq!(
            nsis.ids.as_ref().unwrap(),
            &vec![
                "build_windows_amd64".to_string(),
                "build_windows_arm64".to_string()
            ]
        );
        assert_eq!(
            nsis.name.as_deref(),
            Some("myapp-{{ Version }}-{{ Arch }}-setup.exe")
        );
        assert_eq!(nsis.script.as_deref(), Some("installer.nsi"));
        assert_eq!(nsis.replace, Some(true));
        assert_eq!(
            nsis.mod_timestamp.as_deref(),
            Some("{{ .CommitTimestamp }}")
        );
        assert_eq!(nsis.disable.as_deref(), Some("false"));
    }

    #[test]
    fn test_invalid_name_template_errors() {
        use anodize_core::config::{Config, CrateConfig, NsisConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let nsis_cfg = NsisConfig {
            // Tera will error on unclosed tags
            name: Some("{{ ProjectName }}_{{ Version".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nsis: Some(vec![nsis_cfg]),
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

        // Add a Windows binary so we actually attempt to render the template
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = NsisStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "should error on invalid template");
    }

    #[test]
    fn test_stage_ids_filter() {
        use anodize_core::config::{Config, CrateConfig, NsisConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let nsis_cfg = NsisConfig {
            ids: Some(vec!["build_amd64".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nsis: Some(vec![nsis_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add two Windows binaries with different IDs
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_amd64.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build_amd64".to_string())]),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_arm64.exe"),
            target: Some("aarch64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build_arm64".to_string())]),
        });

        let stage = NsisStage;
        stage.run(&mut ctx).unwrap();

        // Only the amd64 binary should produce an installer
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
        assert_eq!(
            installers[0].target.as_deref().unwrap(),
            "x86_64-pc-windows-msvc"
        );
    }

    #[test]
    fn test_stage_exe_extension_appended_in_dry_run() {
        use anodize_core::config::{Config, CrateConfig, NsisConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Name template without .exe extension
        let nsis_cfg = NsisConfig {
            name: Some("{{ ProjectName }}_{{ Version }}_{{ Arch }}_setup".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nsis: Some(vec![nsis_cfg]),
            ..Default::default()
        }];

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
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = NsisStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);

        let path = installers[0].path.to_string_lossy();
        assert!(
            path.ends_with(".exe"),
            ".exe should be appended when missing, got: {path}"
        );
        assert!(
            path.ends_with("myapp_1.0.0_amd64_setup.exe"),
            "unexpected filename: {path}"
        );
    }
}
