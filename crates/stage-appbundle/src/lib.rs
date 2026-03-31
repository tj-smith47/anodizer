use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::ArchiveFileSpec;
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Info.plist generation
// ---------------------------------------------------------------------------

/// Generate a macOS `Info.plist` XML string.
///
/// If `icon_filename` is `None`, the `CFBundleIconFile` key is omitted.
pub fn generate_info_plist(
    binary_name: &str,
    bundle_id: &str,
    project_name: &str,
    version: &str,
    icon_filename: Option<&str>,
) -> String {
    let icon_entry = if let Some(icon) = icon_filename {
        format!(
            "\n    <key>CFBundleIconFile</key>\n    <string>{icon}</string>"
        )
    } else {
        String::new()
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{binary_name}</string>
    <key>CFBundleIdentifier</key>
    <string>{bundle_id}</string>
    <key>CFBundleName</key>
    <string>{project_name}</string>
    <key>CFBundleVersion</key>
    <string>{version}</string>
    <key>CFBundleShortVersionString</key>
    <string>{version}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>{icon_entry}
</dict>
</plist>
"#
    )
}

// ---------------------------------------------------------------------------
// AppBundleStage
// ---------------------------------------------------------------------------

pub struct AppBundleStage;

/// Parse Os and Arch from a Rust target triple using the shared mapping.
fn os_arch_from_target(target: Option<&str>) -> (String, String) {
    target
        .map(anodize_core::target::map_target)
        .unwrap_or_else(|| ("darwin".to_string(), "amd64".to_string()))
}

/// Default output bundle name template: `{ProjectName}.app`
const DEFAULT_NAME_TEMPLATE: &str = "{{ ProjectName }}.app";

/// Copy extra files specified by `ArchiveFileSpec` entries into the app bundle.
///
/// - `Glob(s)`: resolve the glob and copy each match into `Contents/Resources/`.
/// - `Detailed { src, dst, .. }`: resolve `src` as a glob and copy matches to
///   `{app_dir}/{dst}` if `dst` is provided, otherwise into `Contents/Resources/`.
fn copy_extra_files(
    specs: &[ArchiveFileSpec],
    app_dir: &Path,
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    for spec in specs {
        match spec {
            ArchiveFileSpec::Glob(pattern) => {
                match glob::glob(pattern) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            if entry.is_file() {
                                let dst_name = entry
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("extra");
                                let dst = app_dir.join("Contents/Resources").join(dst_name);
                                fs::copy(&entry, &dst).with_context(|| {
                                    format!(
                                        "copy extra file {} to {}",
                                        entry.display(),
                                        dst.display()
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
            ArchiveFileSpec::Detailed { src, dst, .. } => {
                let dest_base = if let Some(dst_path) = dst {
                    app_dir.join(dst_path)
                } else {
                    app_dir.join("Contents/Resources")
                };

                match glob::glob(src) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            if entry.is_file() {
                                let dst_name = entry
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("extra");
                                // Ensure the destination directory exists
                                if let Some(parent) = dest_base.join(dst_name).parent() {
                                    fs::create_dir_all(parent).with_context(|| {
                                        format!(
                                            "create parent directory for extra file: {}",
                                            parent.display()
                                        )
                                    })?;
                                }
                                let dst = dest_base.join(dst_name);
                                fs::copy(&entry, &dst).with_context(|| {
                                    format!(
                                        "copy extra file {} to {}",
                                        entry.display(),
                                        dst.display()
                                    )
                                })?;
                            }
                        }
                    }
                    Err(e) => {
                        log.warn(&format!(
                            "invalid extra_files glob pattern '{}': {}",
                            src, e
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Recursively apply a mod_timestamp to all files in a directory tree.
fn apply_mod_timestamp_recursive(
    dir: &Path,
    raw: &str,
    log: &anodize_core::log::StageLogger,
) -> Result<()> {
    let mtime = anodize_core::util::parse_mod_timestamp(raw)?;

    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_file() {
            anodize_core::util::set_file_mtime(&entry.path(), mtime)?;
        } else if ft.is_dir() {
            apply_mod_timestamp_recursive(&entry.path(), raw, log)?;
        }
    }

    log.status(&format!(
        "applied mod_timestamp={raw} to {}",
        dir.display()
    ));
    Ok(())
}

impl Stage for AppBundleStage {
    fn name(&self) -> &str {
        "appbundle"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("appbundle");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have app_bundles config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.app_bundles.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        let project_name = ctx.config.project_name.clone();

        // Resolve version from template vars
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());

        let mut new_artifacts: Vec<Artifact> = Vec::new();

        for krate in &crates {
            let bundle_configs = krate.app_bundles.as_ref().unwrap();

            // Collect macOS (darwin) binary artifacts for this crate
            let darwin_binaries: Vec<_> = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                .into_iter()
                .filter(|b| {
                    b.target
                        .as_deref()
                        .map(anodize_core::target::is_darwin)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            for bundle_cfg in bundle_configs {
                // Filter by build IDs if specified
                let mut filtered = darwin_binaries.clone();
                if let Some(ref filter_ids) = bundle_cfg.ids
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

                // Warn and skip if no darwin binaries found
                if filtered.is_empty() && darwin_binaries.is_empty() {
                    log.warn(&format!(
                        "no macOS binary artifacts found for crate '{}'; \
                         skipping app bundle generation (expected binaries targeting darwin/apple)",
                        krate.name
                    ));
                    continue;
                }
                if filtered.is_empty() {
                    log.warn(&format!(
                        "ids filter {:?} matched no binaries for crate '{}'; skipping",
                        bundle_cfg.ids, krate.name
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

                    // Determine output bundle name from name template or default
                    let name_template =
                        bundle_cfg.name.as_deref().unwrap_or(DEFAULT_NAME_TEMPLATE);

                    let app_name = ctx.render_template(name_template).with_context(|| {
                        format!(
                            "appbundle: render name template for crate {} target {:?}",
                            krate.name, target
                        )
                    })?;

                    // Ensure the name ends with .app
                    let app_name = if app_name.to_lowercase().ends_with(".app") {
                        app_name
                    } else {
                        format!("{app_name}.app")
                    };

                    // Output goes in dist/macos/
                    let output_dir = dist.join("macos");
                    let app_dir = output_dir.join(&app_name);

                    // Derive the binary name from the file path
                    let binary_name = binary_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&krate.name);

                    // Determine the bundle identifier
                    let bundle_id = bundle_cfg
                        .bundle
                        .as_deref()
                        .map(String::from)
                        .unwrap_or_else(|| format!("com.anodize.{project_name}"));

                    // Render icon path and extract filename (if icon is configured)
                    let rendered_icon_path = if let Some(icon_tmpl) = &bundle_cfg.icon {
                        Some(ctx.render_template(icon_tmpl).with_context(|| {
                            format!("appbundle: render icon template for crate {}", krate.name)
                        })?)
                    } else {
                        None
                    };
                    let icon_filename = rendered_icon_path.as_ref().map(|p| {
                        PathBuf::from(p)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("icon.icns")
                            .to_string()
                    });

                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would create app bundle {} for crate {} target {:?}",
                            app_name, krate.name, target
                        ));

                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::Installer,
                            name: String::new(),
                            path: app_dir,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            metadata: {
                                let mut m = HashMap::from([(
                                    "format".to_string(),
                                    "appbundle".to_string(),
                                )]);
                                if let Some(id) = &bundle_cfg.id {
                                    m.insert("id".to_string(), id.clone());
                                }
                                m
                            },
                        });

                        continue;
                    }

                    // Live mode — create .app directory structure

                    // Create Contents/MacOS/ directory
                    let macos_dir = app_dir.join("Contents/MacOS");
                    fs::create_dir_all(&macos_dir).with_context(|| {
                        format!("create app bundle MacOS dir: {}", macos_dir.display())
                    })?;

                    // Create Contents/Resources/ directory
                    let resources_dir = app_dir.join("Contents/Resources");
                    fs::create_dir_all(&resources_dir).with_context(|| {
                        format!(
                            "create app bundle Resources dir: {}",
                            resources_dir.display()
                        )
                    })?;

                    // Copy binary into Contents/MacOS/
                    let staged_binary = macos_dir.join(binary_name);
                    fs::copy(binary_path, &staged_binary).with_context(|| {
                        format!(
                            "copy binary {} to {}",
                            binary_path.display(),
                            staged_binary.display()
                        )
                    })?;

                    // Copy icon into Contents/Resources/ if provided
                    if let Some(icon_path_str) = &rendered_icon_path {
                        let icon_src = PathBuf::from(icon_path_str);
                        let icon_name = icon_filename.as_deref().unwrap_or("icon.icns");
                        let icon_dst = resources_dir.join(icon_name);
                        fs::copy(&icon_src, &icon_dst).with_context(|| {
                            format!(
                                "copy icon {} to {}",
                                icon_src.display(),
                                icon_dst.display()
                            )
                        })?;
                    }

                    // Generate and write Info.plist
                    let plist_content = generate_info_plist(
                        binary_name,
                        &bundle_id,
                        &project_name,
                        &version,
                        icon_filename.as_deref(),
                    );
                    let plist_path = app_dir.join("Contents/Info.plist");
                    fs::write(&plist_path, &plist_content).with_context(|| {
                        format!("write Info.plist to {}", plist_path.display())
                    })?;

                    // Copy extra files
                    if let Some(extra_files) = &bundle_cfg.extra_files {
                        copy_extra_files(extra_files, &app_dir, &log)?;
                    }

                    // Apply mod_timestamp if set
                    if let Some(ts) = &bundle_cfg.mod_timestamp {
                        apply_mod_timestamp_recursive(&app_dir, ts, &log)?;
                    }

                    log.status(&format!(
                        "created app bundle {} for crate {} target {:?}",
                        app_name, krate.name, target
                    ));

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::Installer,
                        name: String::new(),
                        path: app_dir,
                        target: target.clone(),
                        crate_name: krate.name.clone(),
                        metadata: {
                            let mut m = HashMap::from([(
                                "format".to_string(),
                                "appbundle".to_string(),
                            )]);
                            if let Some(id) = &bundle_cfg.id {
                                m.insert("id".to_string(), id.clone());
                            }
                            m
                        },
                    });
                }
            }
        }

        // Register new app bundle artifacts
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
    // Info.plist generation
    // -----------------------------------------------------------------------

    #[test]
    fn test_info_plist_generation() {
        let plist = generate_info_plist("myapp", "com.example.myapp", "MyApp", "1.2.3", Some("app.icns"));

        // Verify XML structure
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(plist.contains("<!DOCTYPE plist"));
        assert!(plist.contains("<plist version=\"1.0\">"));
        assert!(plist.contains("</plist>"));

        // Verify field values
        assert!(plist.contains("<key>CFBundleExecutable</key>"));
        assert!(plist.contains("<string>myapp</string>"));
        assert!(plist.contains("<key>CFBundleIdentifier</key>"));
        assert!(plist.contains("<string>com.example.myapp</string>"));
        assert!(plist.contains("<key>CFBundleName</key>"));
        assert!(plist.contains("<string>MyApp</string>"));
        assert!(plist.contains("<key>CFBundleVersion</key>"));
        assert!(plist.contains("<string>1.2.3</string>"));
        assert!(plist.contains("<key>CFBundleShortVersionString</key>"));
        assert!(plist.contains("<key>CFBundlePackageType</key>"));
        assert!(plist.contains("<string>APPL</string>"));
        assert!(plist.contains("<key>CFBundleInfoDictionaryVersion</key>"));
        assert!(plist.contains("<string>6.0</string>"));
        assert!(plist.contains("<key>CFBundleIconFile</key>"));
        assert!(plist.contains("<string>app.icns</string>"));
    }

    #[test]
    fn test_info_plist_without_icon() {
        let plist = generate_info_plist("myapp", "com.example.myapp", "MyApp", "1.0.0", None);

        // CFBundleIconFile key should be omitted
        assert!(
            !plist.contains("CFBundleIconFile"),
            "CFBundleIconFile should be omitted when no icon provided"
        );

        // Other keys should still be present
        assert!(plist.contains("<key>CFBundleExecutable</key>"));
        assert!(plist.contains("<string>myapp</string>"));
        assert!(plist.contains("<key>CFBundleIdentifier</key>"));
        assert!(plist.contains("<string>com.example.myapp</string>"));
    }

    // -----------------------------------------------------------------------
    // App name extension enforcement
    // -----------------------------------------------------------------------

    #[test]
    fn test_app_name_extension() {
        // Already has .app — should not be doubled
        let name = "MyApp.app";
        assert!(name.to_lowercase().ends_with(".app"));

        // Missing .app — should be appended
        let name_no_ext = "MyApp";
        assert!(!name_no_ext.to_lowercase().ends_with(".app"));
        let fixed = format!("{name_no_ext}.app");
        assert_eq!(fixed, "MyApp.app");

        // Case-insensitive check
        let name_upper = "MyApp.APP";
        assert!(name_upper.to_lowercase().ends_with(".app"));
    }

    // -----------------------------------------------------------------------
    // Default bundle ID
    // -----------------------------------------------------------------------

    #[test]
    fn test_default_bundle_id() {
        // When no bundle ID is provided, should default to com.anodize.{project_name}
        let project_name = "myapp";
        let default_id = format!("com.anodize.{project_name}");
        assert_eq!(default_id, "com.anodize.myapp");

        // Verify this appears in the plist
        let plist = generate_info_plist("myapp", &default_id, project_name, "1.0.0", None);
        assert!(plist.contains("<string>com.anodize.myapp</string>"));
    }

    // -----------------------------------------------------------------------
    // Stage behavior tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_skips_no_config() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        // AppBundleStage should be a no-op when crates have no app_bundles block
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = AppBundleStage;
        assert!(stage.run(&mut ctx).is_ok());
        assert!(ctx.artifacts.all().is_empty());
    }

    #[test]
    fn test_stage_dry_run() {
        use anodize_core::config::{AppBundleConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let bundle_cfg = AppBundleConfig {
            bundle: Some("com.example.myapp".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            app_bundles: Some(vec![bundle_cfg]),
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
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_x86"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = AppBundleStage;
        stage.run(&mut ctx).unwrap();

        // Two darwin binaries -> two app bundle artifacts
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 2);

        // All should have format=appbundle metadata
        for inst in &installers {
            assert_eq!(inst.metadata.get("format").unwrap(), "appbundle");
            assert_eq!(inst.kind, ArtifactKind::Installer);
        }

        // Check targets are preserved
        let targets: Vec<&str> = installers
            .iter()
            .map(|a| a.target.as_deref().unwrap())
            .collect();
        assert!(targets.contains(&"aarch64-apple-darwin"));
        assert!(targets.contains(&"x86_64-apple-darwin"));
    }

    #[test]
    fn test_stage_filters_darwin_only() {
        use anodize_core::config::{AppBundleConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let bundle_cfg = AppBundleConfig::default();

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            app_bundles: Some(vec![bundle_cfg]),
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

        // Only add Linux and Windows binaries — no darwin binaries
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
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = AppBundleStage;
        stage.run(&mut ctx).unwrap();

        // No installer artifacts — no darwin binaries available
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert!(
            installers.is_empty(),
            "should produce no app bundles for non-darwin binaries"
        );
    }

    #[test]
    fn test_stage_dry_run_with_name_template() {
        use anodize_core::config::{AppBundleConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let bundle_cfg = AppBundleConfig {
            name: Some("{{ ProjectName }}-{{ Version }}-{{ Arch }}.app".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            app_bundles: Some(vec![bundle_cfg]),
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
        });

        let stage = AppBundleStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);

        let installer_path = installers[0].path.to_string_lossy();
        assert!(
            installer_path.ends_with("myapp-2.0.0-arm64.app"),
            "expected template-rendered name, got: {installer_path}"
        );
    }

    #[test]
    fn test_stage_dry_run_app_extension_appended() {
        use anodize_core::config::{AppBundleConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Name template without .app extension
        let bundle_cfg = AppBundleConfig {
            name: Some("{{ ProjectName }}_{{ Version }}_{{ Arch }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            app_bundles: Some(vec![bundle_cfg]),
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
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
        });

        let stage = AppBundleStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);

        let path = installers[0].path.to_string_lossy();
        assert!(
            path.ends_with(".app"),
            ".app should be appended when missing, got: {path}"
        );
        assert!(
            path.ends_with("myapp_1.0.0_arm64.app"),
            "unexpected filename: {path}"
        );
    }

    #[test]
    fn test_stage_ids_filter() {
        use anodize_core::config::{AppBundleConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let bundle_cfg = AppBundleConfig {
            ids: Some(vec!["build-darwin-arm64".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            app_bundles: Some(vec![bundle_cfg]),
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

        // Add two darwin binaries with different IDs
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp-arm64"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-darwin-arm64".to_string())]),
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp-amd64"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-darwin-amd64".to_string())]),
        });

        let stage = AppBundleStage;
        stage.run(&mut ctx).unwrap();

        // Only the arm64 binary should produce an app bundle
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(
            installers.len(),
            1,
            "ids filter should produce exactly one app bundle"
        );
        assert_eq!(
            installers[0].target.as_deref(),
            Some("aarch64-apple-darwin"),
            "the app bundle should be for the arm64 target"
        );
    }

    #[test]
    fn test_stage_default_bundle_id_in_dry_run() {
        use anodize_core::config::{AppBundleConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // No bundle ID specified — should default to com.anodize.{project_name}
        let bundle_cfg = AppBundleConfig::default();

        let mut config = Config::default();
        config.project_name = "coolapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "coolapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            app_bundles: Some(vec![bundle_cfg]),
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
            path: PathBuf::from("dist/coolapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "coolapp".to_string(),
            metadata: Default::default(),
        });

        let stage = AppBundleStage;
        stage.run(&mut ctx).unwrap();

        // Verify an artifact was produced (the default bundle ID is used internally)
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
        assert_eq!(installers[0].metadata.get("format").unwrap(), "appbundle");
    }

    #[test]
    fn test_stage_live_creates_directory_structure() {
        use anodize_core::config::{AppBundleConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Create a fake binary on disk
        let binary_path = tmp.path().join("myapp");
        fs::write(&binary_path, b"fake-binary").unwrap();

        let bundle_cfg = AppBundleConfig {
            bundle: Some("com.test.myapp".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            app_bundles: Some(vec![bundle_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

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
        });

        let stage = AppBundleStage;
        stage.run(&mut ctx).unwrap();

        // Verify .app directory structure
        let app_dir = tmp.path().join("dist/macos/myapp.app");
        assert!(app_dir.exists(), "app bundle directory should exist");
        assert!(
            app_dir.join("Contents/MacOS/myapp").exists(),
            "binary should be in Contents/MacOS/"
        );
        assert!(
            app_dir.join("Contents/Resources").exists(),
            "Contents/Resources/ should exist"
        );
        assert!(
            app_dir.join("Contents/Info.plist").exists(),
            "Info.plist should exist"
        );

        // Verify Info.plist content
        let plist = fs::read_to_string(app_dir.join("Contents/Info.plist")).unwrap();
        assert!(plist.contains("<string>myapp</string>"));
        assert!(plist.contains("<string>com.test.myapp</string>"));
        assert!(plist.contains("<string>1.0.0</string>"));
        assert!(
            !plist.contains("CFBundleIconFile"),
            "no icon was specified, should not have CFBundleIconFile"
        );

        // Verify binary content was copied
        let copied_binary = fs::read(app_dir.join("Contents/MacOS/myapp")).unwrap();
        assert_eq!(copied_binary, b"fake-binary");

        // Verify artifact was registered
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
        assert_eq!(installers[0].metadata.get("format").unwrap(), "appbundle");
    }

    #[test]
    fn test_stage_live_with_icon() {
        use anodize_core::config::{AppBundleConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Create a fake binary and icon on disk
        let binary_path = tmp.path().join("myapp");
        fs::write(&binary_path, b"fake-binary").unwrap();
        let icon_path = tmp.path().join("app.icns");
        fs::write(&icon_path, b"fake-icon-data").unwrap();

        let bundle_cfg = AppBundleConfig {
            bundle: Some("com.test.myapp".to_string()),
            icon: Some(icon_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            app_bundles: Some(vec![bundle_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

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
        });

        let stage = AppBundleStage;
        stage.run(&mut ctx).unwrap();

        let app_dir = tmp.path().join("dist/macos/myapp.app");

        // Verify icon was copied
        assert!(
            app_dir.join("Contents/Resources/app.icns").exists(),
            "icon should be in Contents/Resources/"
        );
        let icon_data = fs::read(app_dir.join("Contents/Resources/app.icns")).unwrap();
        assert_eq!(icon_data, b"fake-icon-data");

        // Verify Info.plist includes icon reference
        let plist = fs::read_to_string(app_dir.join("Contents/Info.plist")).unwrap();
        assert!(plist.contains("<key>CFBundleIconFile</key>"));
        assert!(plist.contains("<string>app.icns</string>"));
    }

    #[test]
    fn test_invalid_name_template_errors() {
        use anodize_core::config::{AppBundleConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let bundle_cfg = AppBundleConfig {
            // Tera will error on unclosed tags
            name: Some("{{ ProjectName }}_{{ Version".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            app_bundles: Some(vec![bundle_cfg]),
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
        });

        let stage = AppBundleStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "should error on invalid template");
    }

    #[test]
    fn test_config_parse_app_bundle() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    app_bundles:
      - name: "{{ ProjectName }}.app"
        bundle: "com.example.test"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let bundles = config.crates[0].app_bundles.as_ref().unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(
            bundles[0].name.as_deref(),
            Some("{{ ProjectName }}.app")
        );
        assert_eq!(
            bundles[0].bundle.as_deref(),
            Some("com.example.test")
        );
    }

    #[test]
    fn test_config_parse_app_bundle_full() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    app_bundles:
      - id: macos-bundle
        ids:
          - build_darwin_arm64
          - build_darwin_amd64
        name: "myapp-{{ Version }}-{{ Arch }}.app"
        icon: "assets/app.icns"
        bundle: "com.example.myapp"
        extra_files:
          - README.md
          - src: "docs/*.txt"
            dst: "Contents/SharedSupport"
        mod_timestamp: "{{ .CommitTimestamp }}"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let bundles = config.crates[0].app_bundles.as_ref().unwrap();
        assert_eq!(bundles.len(), 1);

        let b = &bundles[0];
        assert_eq!(b.id.as_deref(), Some("macos-bundle"));
        assert_eq!(
            b.ids.as_ref().unwrap(),
            &vec![
                "build_darwin_arm64".to_string(),
                "build_darwin_amd64".to_string()
            ]
        );
        assert_eq!(
            b.name.as_deref(),
            Some("myapp-{{ Version }}-{{ Arch }}.app")
        );
        assert_eq!(b.icon.as_deref(), Some("assets/app.icns"));
        assert_eq!(b.bundle.as_deref(), Some("com.example.myapp"));
        assert_eq!(
            b.mod_timestamp.as_deref(),
            Some("{{ .CommitTimestamp }}")
        );

        // Verify extra_files
        let extras = b.extra_files.as_ref().unwrap();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0], "README.md");
        match &extras[1] {
            ArchiveFileSpec::Detailed { src, dst, .. } => {
                assert_eq!(src, "docs/*.txt");
                assert_eq!(dst.as_deref(), Some("Contents/SharedSupport"));
            }
            other => panic!("expected Detailed variant, got: {other:?}"),
        }
    }

    #[test]
    fn test_stage_multiple_configs() {
        use anodize_core::config::{AppBundleConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        let bundle_cfg_1 = AppBundleConfig {
            id: Some("standard".to_string()),
            name: Some("{{ ProjectName }}-standard-{{ Arch }}.app".to_string()),
            ..Default::default()
        };
        let bundle_cfg_2 = AppBundleConfig {
            id: Some("pro".to_string()),
            name: Some("{{ ProjectName }}-pro-{{ Arch }}.app".to_string()),
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            app_bundles: Some(vec![bundle_cfg_1, bundle_cfg_2]),
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
        });

        let stage = AppBundleStage;
        stage.run(&mut ctx).unwrap();

        // Two configs x one binary = two app bundle artifacts
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(
            installers.len(),
            2,
            "should produce one app bundle per config entry"
        );

        // Verify both have distinct filenames and IDs
        let names: Vec<String> = installers
            .iter()
            .map(|a| a.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("standard")),
            "expected a 'standard' app bundle, got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("pro")),
            "expected a 'pro' app bundle, got: {names:?}"
        );

        let ids: Vec<Option<&String>> =
            installers.iter().map(|a| a.metadata.get("id")).collect();
        assert!(
            ids.contains(&Some(&"standard".to_string())),
            "expected id=standard in metadata"
        );
        assert!(
            ids.contains(&Some(&"pro".to_string())),
            "expected id=pro in metadata"
        );
    }
}
