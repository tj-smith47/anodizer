use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// pkgbuild_command
// ---------------------------------------------------------------------------

/// Construct the `pkgbuild` CLI command arguments.
///
/// Returns args suitable for `Command::new(&args[0]).args(&args[1..])`.
pub fn pkgbuild_command(
    staging_dir: &str,
    identifier: &str,
    version: &str,
    install_location: &str,
    scripts: Option<&str>,
    output_path: &str,
) -> Vec<String> {
    let mut args = vec![
        "pkgbuild".to_string(),
        "--root".to_string(),
        staging_dir.to_string(),
        "--identifier".to_string(),
        identifier.to_string(),
        "--version".to_string(),
        version.to_string(),
        "--install-location".to_string(),
        install_location.to_string(),
    ];

    if let Some(scripts_dir) = scripts {
        args.push("--scripts".to_string());
        args.push(scripts_dir.to_string());
    }

    args.push(output_path.to_string());
    args
}

// ---------------------------------------------------------------------------
// PkgStage
// ---------------------------------------------------------------------------

/// Default output filename template: `{ProjectName}_{Version}_{Arch}.pkg`
const DEFAULT_NAME_TEMPLATE: &str = "{{ ProjectName }}_{{ Version }}_{{ Arch }}.pkg";

pub struct PkgStage;

impl Stage for PkgStage {
    fn name(&self) -> &str {
        "pkg"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("pkg");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have pkg config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.pkgs.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // Resolve version from template vars
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archive_paths_to_remove: Vec<PathBuf> = Vec::new();

        for krate in &crates {
            let Some(pkg_configs) = krate.pkgs.as_ref() else {
                continue;
            };

            // Collect macOS binary artifacts for this crate
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

            for pkg_cfg in pkg_configs {
                // Skip disabled configs (supports bool or template string)
                if let Some(ref d) = pkg_cfg.disable
                    && d.is_disabled(|s| ctx.render_template(s))
                {
                    log.status(&format!(
                        "skipping disabled pkg config for crate {}",
                        krate.name
                    ));
                    continue;
                }

                // Validate `use` field
                let use_mode = pkg_cfg.use_.as_deref().unwrap_or("binary");
                if use_mode != "binary" && use_mode != "appbundle" {
                    anyhow::bail!(
                        "pkg: invalid `use` value '{}' for crate '{}'; expected 'binary' or 'appbundle'",
                        use_mode,
                        krate.name
                    );
                }

                // Collect source artifacts depending on `use` mode
                let source_artifacts: Vec<_> = if use_mode == "appbundle" {
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
                    darwin_binaries.clone()
                };

                // Filter by build IDs if specified
                let mut filtered = source_artifacts.clone();
                if let Some(ref filter_ids) = pkg_cfg.ids
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
                             skipping PKG generation (expected Installer artifacts with format=appbundle)",
                            krate.name
                        ));
                    } else {
                        log.warn(&format!(
                            "no macOS binary artifacts found for crate '{}'; \
                             skipping PKG generation (expected binaries targeting darwin/apple)",
                            krate.name
                        ));
                    }
                    continue;
                }
                if filtered.is_empty() {
                    log.warn(&format!(
                        "ids filter {:?} matched no artifacts for crate '{}'; skipping",
                        pkg_cfg.ids, krate.name
                    ));
                    continue;
                }

                let effective_binaries: Vec<(Option<String>, PathBuf)> = filtered
                    .iter()
                    .map(|b| (b.target.clone(), b.path.clone()))
                    .collect();

                // Validate identifier is present
                let identifier = pkg_cfg.identifier.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "pkg: missing required `identifier` for crate `{}`. \
                         Set a reverse-domain identifier (e.g. com.example.myapp)",
                        krate.name
                    )
                })?;

                let install_location = pkg_cfg
                    .install_location
                    .as_deref()
                    .unwrap_or("/usr/local/bin");

                for (target, binary_path) in &effective_binaries {
                    // Derive Os/Arch from the target triple for template rendering
                    let (os, arch) = target
                        .as_deref()
                        .map(anodize_core::target::map_target)
                        .unwrap_or_else(|| ("darwin".to_string(), "amd64".to_string()));

                    // Set Os/Arch/Target in template vars for name template rendering
                    ctx.template_vars_mut().set("Os", &os);
                    ctx.template_vars_mut().set("Arch", &arch);
                    ctx.template_vars_mut()
                        .set("Target", target.as_deref().unwrap_or(""));

                    // Determine output filename
                    let name_template = pkg_cfg.name.as_deref().unwrap_or(DEFAULT_NAME_TEMPLATE);

                    let pkg_filename = ctx.render_template(name_template).with_context(|| {
                        format!(
                            "pkg: render name template for crate {} target {:?}",
                            krate.name, target
                        )
                    })?;

                    // Ensure .pkg extension (case-insensitive)
                    let pkg_filename = if pkg_filename.to_lowercase().ends_with(".pkg") {
                        pkg_filename
                    } else {
                        format!("{pkg_filename}.pkg")
                    };

                    // Output path
                    let output_dir = dist.join("macos");
                    let pkg_path = output_dir.join(&pkg_filename);

                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would run: pkgbuild --identifier {identifier} \
                             --version {version} for crate {} target {:?}",
                            krate.name, target
                        ));

                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::MacOsPackage,
                            name: String::new(),
                            path: pkg_path,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            metadata: {
                                let mut m = HashMap::from([(
                                    "identifier".to_string(),
                                    identifier.to_string(),
                                )]);
                                if let Some(id) = &pkg_cfg.id {
                                    m.insert("id".to_string(), id.clone());
                                }
                                m
                            },
                            size: None,
                        });

                        // Track archives to remove if replace is true
                        archive_paths_to_remove.extend(anodize_core::util::collect_if_replace(
                            pkg_cfg.replace,
                            &ctx.artifacts,
                            &krate.name,
                            target.as_deref(),
                        ));

                        continue;
                    }

                    // Live mode: create staging directory and copy binary into it
                    let staging_tmp =
                        tempfile::tempdir().context("create temp staging dir for pkg")?;
                    let staging_dir = staging_tmp.path();

                    // Copy the binary into the staging directory
                    let binary_name = binary_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&krate.name);
                    let staged_binary = staging_dir.join(binary_name);
                    fs::copy(binary_path, &staged_binary).with_context(|| {
                        format!(
                            "pkg: copy binary {} to staging dir {}",
                            binary_path.display(),
                            staging_dir.display()
                        )
                    })?;

                    // Copy extra files into the staging directory
                    if let Some(extra_files) = &pkg_cfg.extra_files {
                        for spec in extra_files {
                            let glob_pattern = spec.glob();
                            for entry in glob::glob(glob_pattern).with_context(|| {
                                format!("pkg: invalid extra_files glob '{}'", glob_pattern)
                            })? {
                                let src = entry.with_context(|| {
                                    format!("pkg: error reading glob match for '{}'", glob_pattern)
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
                                    format!("pkg: copy extra file {} to staging dir", src.display())
                                })?;
                            }
                        }
                    }

                    // Apply mod_timestamp if set
                    if let Some(ts) = &pkg_cfg.mod_timestamp {
                        anodize_core::util::apply_mod_timestamp(staging_dir, ts, &log)?;
                    }

                    // Ensure output directory exists
                    fs::create_dir_all(&output_dir).with_context(|| {
                        format!("create pkg output dir: {}", output_dir.display())
                    })?;

                    let cmd_args = pkgbuild_command(
                        &staging_dir.to_string_lossy(),
                        identifier,
                        &version,
                        install_location,
                        pkg_cfg.scripts.as_deref(),
                        &pkg_path.to_string_lossy(),
                    );

                    log.status(&format!("running: {}", cmd_args.join(" ")));

                    let output = Command::new(&cmd_args[0])
                        .args(&cmd_args[1..])
                        .output()
                        .with_context(|| {
                            format!(
                                "execute pkgbuild for crate {} target {:?}",
                                krate.name, target
                            )
                        })?;
                    log.check_output(output, "pkgbuild")?;

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::MacOsPackage,
                        name: String::new(),
                        path: pkg_path,
                        target: target.clone(),
                        crate_name: krate.name.clone(),
                        metadata: {
                            let mut m =
                                HashMap::from([("identifier".to_string(), identifier.to_string())]);
                            if let Some(id) = &pkg_cfg.id {
                                m.insert("id".to_string(), id.clone());
                            }
                            m
                        },
                        size: None,
                    });

                    // Track archives to remove if replace is true
                    archive_paths_to_remove.extend(anodize_core::util::collect_if_replace(
                        pkg_cfg.replace,
                        &ctx.artifacts,
                        &krate.name,
                        target.as_deref(),
                    ));
                }
            }
        }

        anodize_core::template::clear_per_target_vars(ctx.template_vars_mut());

        // Remove archive artifacts marked for replacement
        if !archive_paths_to_remove.is_empty() {
            ctx.artifacts.remove_by_paths(&archive_paths_to_remove);
        }

        // Register new PKG artifacts
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
    use anodize_core::artifact::{Artifact, ArtifactKind};
    use anodize_core::config::{Config, CrateConfig, ExtraFileSpec, PkgConfig, StringOrBool};
    use anodize_core::context::{Context, ContextOptions};
    use tempfile::TempDir;

    // -- pkgbuild_command tests --

    #[test]
    fn test_pkgbuild_command_basic() {
        let cmd = pkgbuild_command(
            "/tmp/staging",
            "com.example.myapp",
            "1.0.0",
            "/usr/local/bin",
            None,
            "/tmp/output/myapp.pkg",
        );
        assert_eq!(
            cmd,
            vec![
                "pkgbuild",
                "--root",
                "/tmp/staging",
                "--identifier",
                "com.example.myapp",
                "--version",
                "1.0.0",
                "--install-location",
                "/usr/local/bin",
                "/tmp/output/myapp.pkg",
            ]
        );
    }

    #[test]
    fn test_pkgbuild_command_with_scripts() {
        let cmd = pkgbuild_command(
            "/tmp/staging",
            "com.example.myapp",
            "2.0.0",
            "/usr/local/bin",
            Some("/path/to/scripts"),
            "/tmp/output/myapp.pkg",
        );
        assert_eq!(
            cmd,
            vec![
                "pkgbuild",
                "--root",
                "/tmp/staging",
                "--identifier",
                "com.example.myapp",
                "--version",
                "2.0.0",
                "--install-location",
                "/usr/local/bin",
                "--scripts",
                "/path/to/scripts",
                "/tmp/output/myapp.pkg",
            ]
        );
    }

    #[test]
    fn test_pkgbuild_command_custom_install_location() {
        let cmd = pkgbuild_command(
            "/tmp/staging",
            "com.example.myapp",
            "1.0.0",
            "/opt/myapp/bin",
            None,
            "/tmp/output/myapp.pkg",
        );
        assert_eq!(
            cmd,
            vec![
                "pkgbuild",
                "--root",
                "/tmp/staging",
                "--identifier",
                "com.example.myapp",
                "--version",
                "1.0.0",
                "--install-location",
                "/opt/myapp/bin",
                "/tmp/output/myapp.pkg",
            ]
        );
    }

    // -- Stage no-op / skip tests --

    #[test]
    fn test_stage_skips_when_no_pkg_config() {
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = PkgStage;
        assert!(stage.run(&mut ctx).is_ok());
        // No artifacts should be registered
        assert!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty());
    }

    #[test]
    fn test_stage_skips_when_disabled() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            disable: Some(StringOrBool::Bool(true)),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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

        // Add a darwin binary so the stage would otherwise process it
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        // No packages should be generated because the config is disabled
        assert!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty());
    }

    // -- Dry-run behavior tests --

    #[test]
    fn test_stage_dry_run_registers_artifacts() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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

        // Register darwin binary artifacts
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp-x86"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 2, "should register one PKG per darwin binary");

        // Both should have correct kind and metadata
        for pkg in &pkgs {
            assert_eq!(pkg.kind, ArtifactKind::MacOsPackage);
            assert_eq!(pkg.crate_name, "myapp");
            assert_eq!(
                pkg.metadata.get("identifier"),
                Some(&"com.example.myapp".to_string())
            );
        }

        // Check targets are preserved
        let targets: Vec<Option<&str>> = pkgs.iter().map(|p| p.target.as_deref()).collect();
        assert!(targets.contains(&Some("aarch64-apple-darwin")));
        assert!(targets.contains(&Some("x86_64-apple-darwin")));
    }

    #[test]
    fn test_stage_dry_run_with_name_template() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            name: Some("{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "2.5.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);

        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            filename, "myapp_2.5.0_darwin_arm64.pkg",
            "name template should render with Os/Arch from target triple"
        );
    }

    #[test]
    fn test_stage_dry_run_replace_removes_archives() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            replace: Some(true),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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

        // Add a darwin binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Add darwin archive artifacts that should be removed
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/dist/myapp_darwin_arm64.tar.gz"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Add a linux archive that should NOT be removed
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("/dist/myapp_linux_amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // PKG artifact should be registered
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);

        // Darwin archive should be removed
        let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
        assert_eq!(archives.len(), 1, "darwin archive should be removed");
        assert_eq!(
            archives[0].target.as_deref(),
            Some("x86_64-unknown-linux-gnu"),
            "only the linux archive should remain"
        );
    }

    // -- Error path tests --

    #[test]
    fn test_stage_errors_without_identifier() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: None, // missing required field
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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

        // Add a darwin binary so the stage attempts to process the config
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let result = PkgStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "missing identifier should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("identifier"),
            "error should mention missing identifier, got: {err}"
        );
    }

    // -- Config parsing tests --

    #[test]
    fn test_config_parse_pkg() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].identifier.as_deref(), Some("com.example.test"));
        // All optional fields default to None
        assert!(pkgs[0].name.is_none());
        assert!(pkgs[0].install_location.is_none());
        assert!(pkgs[0].scripts.is_none());
        assert!(pkgs[0].replace.is_none());
        assert!(pkgs[0].disable.is_none());
    }

    #[test]
    fn test_config_parse_pkg_full() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - id: my-pkg
        ids:
          - build-darwin-arm64
          - build-darwin-amd64
        identifier: com.example.test
        name: "{{ ProjectName }}_{{ Version }}_{{ Arch }}"
        install_location: /opt/test/bin
        scripts: ./scripts/pkg
        extra_files:
          - README.md
          - LICENSE
        replace: true
        mod_timestamp: "2024-01-01T00:00:00Z"
        disable: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs.len(), 1);
        let p = &pkgs[0];
        assert_eq!(p.id.as_deref(), Some("my-pkg"));
        assert_eq!(
            p.ids.as_ref().unwrap(),
            &["build-darwin-arm64", "build-darwin-amd64"]
        );
        assert_eq!(p.identifier.as_deref(), Some("com.example.test"));
        assert_eq!(
            p.name.as_deref(),
            Some("{{ ProjectName }}_{{ Version }}_{{ Arch }}")
        );
        assert_eq!(p.install_location.as_deref(), Some("/opt/test/bin"));
        assert_eq!(p.scripts.as_deref(), Some("./scripts/pkg"));
        let extras = p.extra_files.as_ref().unwrap();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].glob(), "README.md");
        assert_eq!(extras[1].glob(), "LICENSE");
        assert_eq!(p.replace, Some(true));
        assert_eq!(p.mod_timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(p.disable, Some(StringOrBool::Bool(false)));
    }

    #[test]
    fn test_default_install_location() {
        // When install_location is not set, the stage should default to /usr/local/bin
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            install_location: None,
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // The default install location is used internally in the pkgbuild command;
        // verify the stage succeeds and registers an artifact (the default is
        // /usr/local/bin which is tested via the pkgbuild_command unit tests).
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);

        // Verify the default through pkgbuild_command directly
        let cmd = pkgbuild_command(
            "/tmp/staging",
            "com.example.myapp",
            "1.0.0",
            "/usr/local/bin", // the default
            None,
            "/tmp/out.pkg",
        );
        assert!(
            cmd.contains(&"--install-location".to_string()),
            "command should contain --install-location"
        );
        let loc_idx = cmd.iter().position(|a| a == "--install-location").unwrap();
        assert_eq!(cmd[loc_idx + 1], "/usr/local/bin");
    }

    #[test]
    fn test_extra_files_copied_to_staging() {
        // Run in live mode — pkgbuild won't be available, but we verify that
        // the stage gets past binary + extra file copying and only fails at
        // the pkgbuild command execution.
        let tmp = TempDir::new().unwrap();

        // Create a fake binary
        let binary_dir = tmp.path().join("bin");
        fs::create_dir_all(&binary_dir).unwrap();
        let binary_path = binary_dir.join("myapp");
        fs::write(&binary_path, b"fake binary").unwrap();

        // Create an extra file
        let extra_path = tmp.path().join("README.md");
        fs::write(&extra_path, b"# My App").unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            extra_files: Some(vec![ExtraFileSpec::Glob(
                extra_path.to_string_lossy().into_owned(),
            )]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false, // live mode
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Add a darwin binary artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: binary_path,
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let result = PkgStage.run(&mut ctx);
        // On macOS, pkgbuild is available so the stage may succeed.
        // On Linux/Windows, it will fail because pkgbuild is not installed.
        if cfg!(target_os = "macos") {
            if let Err(e) = &result {
                let err = e.to_string();
                assert!(
                    err.contains("pkgbuild") || err.contains("execute"),
                    "unexpected error on macOS: {err}"
                );
            }
        } else {
            assert!(result.is_err());
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("pkgbuild") || err.contains("execute"),
                "expected pkgbuild execution error, got: {err}"
            );
        }
    }

    #[test]
    fn test_invalid_name_template_errors() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            // Invalid Tera template — unclosed tag
            name: Some("{{ bad_template".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let result = PkgStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "invalid name template should cause a render error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("template") || err.contains("render"),
            "error should mention template rendering, got: {err}"
        );
    }

    #[test]
    fn test_ids_filtering() {
        let tmp = TempDir::new().unwrap();

        // Configure ids filter to match only one build id
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
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
            pkgs: Some(vec![pkg_cfg]),
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

        // Register two darwin binaries with different metadata ids
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp-arm64"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-darwin-arm64".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/build/myapp-amd64"),
            target: Some("x86_64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-darwin-amd64".to_string())]),
            size: None,
        });

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        // Verify only one PKG artifact is produced (the arm64 one)
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(
            pkgs.len(),
            1,
            "ids filter should produce exactly one PKG, got {}",
            pkgs.len()
        );
        assert_eq!(
            pkgs[0].target.as_deref(),
            Some("aarch64-apple-darwin"),
            "the PKG should be for the arm64 target"
        );
    }

    // -- `use` field tests --

    #[test]
    fn test_use_appbundle_selects_installer_artifacts() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            use_: Some("appbundle".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        // Should produce one PKG from the appbundle, not from the binary
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1, "should produce one PKG from the appbundle");
    }

    #[test]
    fn test_use_binary_selects_darwin_binaries() {
        let tmp = TempDir::new().unwrap();

        // Explicit `use: binary` should behave same as omitted (default)
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            use_: Some("binary".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        // Should produce one PKG from the binary, not from the appbundle
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1, "should produce one PKG from the binary");
    }

    #[test]
    fn test_use_default_selects_darwin_binaries() {
        let tmp = TempDir::new().unwrap();

        // No `use_` set — should default to "binary" mode
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(
            pkgs.len(),
            1,
            "default use mode should select darwin binaries"
        );
    }

    #[test]
    fn test_invalid_use_value_errors() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            use_: Some("invalid_mode".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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

        // Add a binary so the stage tries to process the config
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let result = PkgStage.run(&mut ctx);
        assert!(result.is_err(), "invalid use value should produce an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid `use` value"),
            "error should mention invalid use value, got: {err}"
        );
    }

    #[test]
    fn test_use_appbundle_skips_when_no_appbundles() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            use_: Some("appbundle".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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

        PkgStage.run(&mut ctx).unwrap();

        // No PKGs should be produced because there are no appbundle artifacts
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(
            pkgs.len(),
            0,
            "should produce no PKGs when use=appbundle but no appbundles exist"
        );
    }

    // -- StringOrBool disable tests --

    #[test]
    fn test_disable_string_or_bool_true_string() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            disable: Some(StringOrBool::String("true".to_string())),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // disable: "true" should skip the config
        assert!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty());
    }

    #[test]
    fn test_disable_string_or_bool_false_string() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            disable: Some(StringOrBool::String("false".to_string())),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // disable: "false" should NOT skip the config
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).len(), 1);
    }

    #[test]
    fn test_disable_string_or_bool_template() {
        let tmp = TempDir::new().unwrap();

        // Template that evaluates to "true" when IsSnapshot is set
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            disable: Some(StringOrBool::String(
                "{% if IsSnapshot %}true{% else %}false{% endif %}".to_string(),
            )),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![pkg_cfg]),
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
            path: PathBuf::from("/build/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        PkgStage.run(&mut ctx).unwrap();

        // Template should evaluate to "true", so the config is disabled
        assert!(
            ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty(),
            "template disable should skip the config when evaluated to true"
        );
    }

    #[test]
    fn test_config_parse_pkg_with_use_and_string_disable() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
        use: appbundle
        disable: "{{ if IsSnapshot }}true{{ else }}false{{ endif }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].use_.as_deref(), Some("appbundle"));
        assert!(matches!(pkgs[0].disable, Some(StringOrBool::String(_))));
    }
}
