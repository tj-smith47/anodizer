use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

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
    min_os_version: Option<&str>,
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

    if let Some(min_os) = min_os_version {
        args.push("--min-os-version".to_string());
        args.push(min_os.to_string());
    }

    args.push(output_path.to_string());
    args
}

// ---------------------------------------------------------------------------
// PkgStage
// ---------------------------------------------------------------------------

/// Default output filename template (no version component).
///
/// The `.pkg` extension is auto-appended (case-insensitively) after rendering
/// when the resolved name does not already end in `.pkg`, so the default emits
/// `<ProjectName>_<Arch>.pkg`. An extensionless installer is not recognized by
/// macOS Installer.app and breaks the homebrew-cask pkg stanza + checksum
/// naming. A user-supplied `name:` ending in `.pkg` is used verbatim (not
/// doubled).
const DEFAULT_NAME_TEMPLATE: &str = "{{ ProjectName }}_{{ Arch }}";

/// Rendered per-binary field values resolved from a [`PkgConfig`].
///
/// Produced by [`render_pkg_fields`] for one (config, target) pairing
/// after `Os`/`Arch`/`Target` template vars are set, so every template
/// expansion sees the binary's effective target triple.
pub struct RenderedPkgFields {
    pub identifier: String,
    pub install_location: String,
    pub scripts: Option<String>,
    pub mod_timestamp: Option<String>,
}

/// Render the four template-bearing `PkgConfig` fields against `ctx`.
///
/// `identifier_template` must already be unwrapped from
/// `pkg_cfg.identifier` (which is required at validation time); the rest
/// are resolved against `pkg_cfg`. `crate_name` and `target` are used
/// only to build error context.
pub fn render_pkg_fields(
    ctx: &mut Context,
    pkg_cfg: &anodizer_core::config::PkgConfig,
    identifier_template: &str,
    crate_name: &str,
    target: Option<&str>,
) -> Result<RenderedPkgFields> {
    let identifier = ctx.render_template(identifier_template).with_context(|| {
        format!(
            "pkg: render identifier template for crate {} target {:?}",
            crate_name, target
        )
    })?;

    let install_location_raw = pkg_cfg
        .install_location
        .as_deref()
        .unwrap_or("/usr/local/bin");
    let install_location = ctx.render_template(install_location_raw).with_context(|| {
        format!(
            "pkg: render install_location template for crate {} target {:?}",
            crate_name, target
        )
    })?;

    let scripts = pkg_cfg
        .scripts
        .as_deref()
        .map(|s| {
            ctx.render_template(s).with_context(|| {
                format!(
                    "pkg: render scripts template for crate {} target {:?}",
                    crate_name, target
                )
            })
        })
        .transpose()?;

    let mod_timestamp = pkg_cfg
        .mod_timestamp
        .as_deref()
        .map(|ts| {
            ctx.render_template(ts).with_context(|| {
                format!(
                    "pkg: render mod_timestamp template for crate {} target {:?}",
                    crate_name, target
                )
            })
        })
        .transpose()?;

    Ok(RenderedPkgFields {
        identifier,
        install_location,
        scripts,
        mod_timestamp,
    })
}

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

        // In workspace per-crate mode the same pipeline run produces a pkg for
        // each crate. Rebinding `ProjectName` to the current crate's name
        // (mirroring the archive stage) keeps default name templates like
        // `{{ ProjectName }}_{{ Arch }}` distinct per crate so two crates'
        // installers don't render the same filename and clobber each other.
        // Restored after the loop.
        let multi_crate = crates.len() > 1;
        let original_project_name = ctx
            .template_vars()
            .get("ProjectName")
            .cloned()
            .unwrap_or_else(|| ctx.config.project_name.clone());

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archive_paths_to_remove: Vec<PathBuf> = Vec::new();

        // Capture the loop result rather than `?`-ing inside it: a per-crate
        // failure must still restore the rebound `ProjectName` below before
        // propagating, so the workspace value never leaks past this stage.
        let loop_result: Result<()> = (|| {
            for krate in &crates {
                let Some(pkg_configs) = krate.pkgs.as_ref() else {
                    continue;
                };
                if multi_crate {
                    ctx.template_vars_mut().set("ProjectName", &krate.name);
                }

                // Collect macOS binary artifacts for this crate
                let darwin_binaries: Vec<_> = ctx
                    .artifacts
                    .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                    .into_iter()
                    .filter(|b| {
                        b.target
                            .as_deref()
                            .map(anodizer_core::target::is_darwin)
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect();

                for pkg_cfg in pkg_configs {
                    let pkg_id_for_log = pkg_cfg.id.as_deref().unwrap_or("default").to_string();

                    // `pkg.if`: template-conditional skip (opt-in).
                    // Render error => hard bail (W1 avoidance).
                    let proceed = anodizer_core::config::evaluate_if_condition(
                        pkg_cfg.if_condition.as_deref(),
                        &format!("pkg config '{}' for crate '{}'", pkg_id_for_log, krate.name),
                        |t| ctx.render_template(t),
                    )?;
                    if !proceed {
                        log.status(&format!(
                            "skipping pkg config '{}' for crate {}: `if` condition evaluated falsy",
                            pkg_id_for_log, krate.name
                        ));
                        continue;
                    }

                    // Skip configs marked skip:
                    if let Some(ref d) = pkg_cfg.skip {
                        let off = d
                            .try_evaluates_to_true(|s| ctx.render_template(s))
                            .with_context(|| {
                                format!("pkg: render skip template for crate {}", krate.name)
                            })?;
                        if off {
                            log.status(&format!("pkg config skipped for crate {}", krate.name));
                            continue;
                        }
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

                    // Validate identifier is present (template render happens inside the
                    // per-binary loop below so Os/Arch vars are set first).
                    let identifier_template = pkg_cfg.identifier.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "pkg: missing required `identifier` for crate `{}`. \
                         Set a reverse-domain identifier (e.g. com.example.myapp)",
                            krate.name
                        )
                    })?;

                    // Probe pkgbuild once per config entry before entering the per-binary
                    // loop so the error surfaces early with an actionable install hint.
                    if !dry_run && !anodizer_core::util::find_binary("pkgbuild") {
                        anyhow::bail!(
                            "pkgbuild not found on PATH; install Xcode Command Line Tools \
                         with `xcode-select --install`"
                        );
                    }

                    // One .pkg is produced per binary — pkg installers are single-binary
                    // by design. Unlike DMG (which groups multiple binaries into one
                    // container image), each pkg wraps exactly one payload binary so that
                    // Homebrew formula installers and macOS Installer.app each target a
                    // discrete, independently versionable package. Multi-binary crates
                    // therefore emit N packages per target triple.
                    for (target, binary_path) in &effective_binaries {
                        // Derive Os/Arch from the target triple for template rendering
                        let (os, arch) = target
                            .as_deref()
                            .map(anodizer_core::target::map_target)
                            .unwrap_or_else(|| ("darwin".to_string(), "amd64".to_string()));

                        // Set Os/Arch/Target in template vars for name template rendering
                        ctx.template_vars_mut().set("Os", &os);
                        ctx.template_vars_mut().set("Arch", &arch);
                        ctx.template_vars_mut()
                            .set("Target", target.as_deref().unwrap_or(""));

                        let rendered = render_pkg_fields(
                            ctx,
                            pkg_cfg,
                            identifier_template,
                            &krate.name,
                            target.as_deref(),
                        )?;
                        let identifier = rendered.identifier;
                        let install_location = rendered.install_location;
                        let scripts_rendered = rendered.scripts;
                        let mod_timestamp_rendered = rendered.mod_timestamp;

                        // Determine output filename
                        let name_template =
                            pkg_cfg.name.as_deref().unwrap_or(DEFAULT_NAME_TEMPLATE);

                        let pkg_filename =
                            ctx.render_template(name_template).with_context(|| {
                                format!(
                                    "pkg: render name template for crate {} target {:?}",
                                    krate.name, target
                                )
                            })?;

                        // Ensure the filename ends with .pkg (case-insensitive). An
                        // extensionless installer is not recognized by macOS
                        // Installer.app and breaks the homebrew-cask pkg stanza +
                        // checksum naming; a user-supplied `name` already ending in
                        // `.pkg` is not doubled. Mirrors stage-dmg's `.dmg` append.
                        let pkg_filename = if pkg_filename.to_ascii_lowercase().ends_with(".pkg") {
                            pkg_filename
                        } else {
                            format!("{pkg_filename}.pkg")
                        };

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
                            archive_paths_to_remove.extend(
                                anodizer_core::util::collect_if_replace(
                                    pkg_cfg.replace,
                                    &ctx.artifacts,
                                    &krate.name,
                                    target.as_deref(),
                                ),
                            );

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
                                        format!(
                                            "pkg: error reading glob match for '{}'",
                                            glob_pattern
                                        )
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
                                        format!(
                                            "pkg: copy extra file {} to staging dir",
                                            src.display()
                                        )
                                    })?;
                                }
                            }
                        }

                        // Render and copy templated_extra_files into the staging directory
                        if let Some(ref tpl_specs) = pkg_cfg.templated_extra_files
                            && !tpl_specs.is_empty()
                        {
                            anodizer_core::templated_files::process_templated_extra_files(
                                tpl_specs,
                                ctx,
                                staging_dir,
                                "pkg",
                            )?;
                        }

                        // Apply mod_timestamp if set. Templates were already expanded
                        // upstream via render_pkg_fields, so values like
                        // `{{ CommitTimestamp }}` reach parse_mod_timestamp as a
                        // valid RFC3339 string rather than the literal template.
                        if let Some(ts) = &mod_timestamp_rendered {
                            anodizer_core::util::apply_mod_timestamp(staging_dir, ts, &log)?;
                        }

                        // Ensure output directory exists
                        fs::create_dir_all(&output_dir).with_context(|| {
                            format!("create pkg output dir: {}", output_dir.display())
                        })?;

                        let cmd_args = pkgbuild_command(
                            &staging_dir.to_string_lossy(),
                            &identifier,
                            &version,
                            &install_location,
                            scripts_rendered.as_deref(),
                            pkg_cfg.min_os_version.as_deref(),
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
                        archive_paths_to_remove.extend(anodizer_core::util::collect_if_replace(
                            pkg_cfg.replace,
                            &ctx.artifacts,
                            &krate.name,
                            target.as_deref(),
                        ));
                    }
                }
            }
            Ok(())
        })();

        if multi_crate {
            ctx.template_vars_mut()
                .set("ProjectName", &original_project_name);
        }
        loop_result?;

        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());

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

/// Environment requirements for the pkg stage: `pkgbuild` when any active
/// `pkgs:` entry exists and the configured build targets include macOS
/// (the stage only packages darwin binaries).
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    if !anodizer_core::env_preflight::configured_build_targets(ctx)
        .iter()
        .any(|t| anodizer_core::target::is_darwin(t))
    {
        return Vec::new();
    }
    let configured = anodizer_core::env_preflight::crate_universe(&ctx.config)
        .into_iter()
        .flat_map(|c| c.pkgs.iter().flatten())
        .any(|cfg| {
            !anodizer_core::env_preflight::entry_inactive(
                ctx,
                cfg.skip.as_ref(),
                None,
                cfg.if_condition.as_deref(),
            )
        });
    if !configured {
        return Vec::new();
    }
    vec![anodizer_core::EnvRequirement::Tool {
        name: "pkgbuild".to_string(),
    }]
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{Config, CrateConfig, ExtraFileSpec, PkgConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};
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
            skip: Some(StringOrBool::Bool(true)),
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
    fn test_workspace_per_crate_distinct_filenames() {
        let tmp = TempDir::new().unwrap();

        // Two crates, both using the DEFAULT name template (no Version segment),
        // so ProjectName is the only distinguishing token. Without the per-crate
        // ProjectName rebind both render to `<project_name>_arm64.pkg` and clobber.
        let make_crate = |name: &str| CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![PkgConfig {
                identifier: Some("com.example.{{ ProjectName }}".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "workspace".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![make_crate("alpha"), make_crate("beta")];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        for crate_name in ["alpha", "beta"] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from(format!("/build/{crate_name}")),
                target: Some("aarch64-apple-darwin".to_string()),
                crate_name: crate_name.to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        let stage = PkgStage;
        stage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 2, "expected one PKG per crate");

        let filenames: Vec<String> = pkgs
            .iter()
            .map(|p| p.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(
            filenames.iter().any(|f| f.contains("alpha")),
            "no PKG filename contains crate name 'alpha': {filenames:?}"
        );
        assert!(
            filenames.iter().any(|f| f.contains("beta")),
            "no PKG filename contains crate name 'beta': {filenames:?}"
        );
        assert_ne!(
            filenames[0], filenames[1],
            "the two crates' PKGs must not share a filename (clobber): {filenames:?}"
        );

        assert_eq!(
            ctx.template_vars().get("ProjectName").map(String::as_str),
            Some("workspace"),
            "ProjectName not restored after per-crate rebind"
        );
    }

    #[test]
    fn test_project_name_restored_after_mid_loop_error() {
        // A per-crate render failure mid-loop must still restore the rebound
        // `ProjectName` before propagating, so the workspace value never leaks
        // out of the stage (the var is process-global on ctx).
        let tmp = TempDir::new().unwrap();

        let good_crate = CrateConfig {
            name: "alpha".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![PkgConfig {
                identifier: Some("com.example.alpha".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let bad_crate = CrateConfig {
            name: "beta".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            pkgs: Some(vec![PkgConfig {
                identifier: Some("com.example.beta".to_string()),
                // Malformed template — unclosed tag forces a mid-loop render error.
                name: Some("{{ bad_template".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "workspace".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![good_crate, bad_crate];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        for crate_name in ["alpha", "beta"] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from(format!("/build/{crate_name}")),
                target: Some("aarch64-apple-darwin".to_string()),
                crate_name: crate_name.to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        let result = PkgStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "malformed name on a crate must fail the stage"
        );

        assert_eq!(
            ctx.template_vars().get("ProjectName").map(String::as_str),
            Some("workspace"),
            "ProjectName must be restored even when the loop errors mid-iteration"
        );
    }

    #[test]
    fn test_stage_dry_run_with_name_template() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            name: Some("{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}.pkg".to_string()),
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
        assert!(pkgs[0].skip.is_none());
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
        skip: false
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
        assert_eq!(p.skip, Some(StringOrBool::Bool(false)));
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
            skip: Some(StringOrBool::String("true".to_string())),
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

        // skip: "true" should skip the config
        assert!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).is_empty());
    }

    #[test]
    fn test_disable_string_or_bool_false_string() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            skip: Some(StringOrBool::String("false".to_string())),
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

        // skip: "false" should NOT skip the config
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).len(), 1);
    }

    #[test]
    fn test_disable_string_or_bool_template() {
        let tmp = TempDir::new().unwrap();

        // Template that evaluates to "true" when IsSnapshot is set
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            skip: Some(StringOrBool::String(
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
        skip: "{{ if IsSnapshot }}true{{ else }}false{{ endif }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].use_.as_deref(), Some("appbundle"));
        assert!(matches!(pkgs[0].skip, Some(StringOrBool::String(_))));
    }

    // --- `pkg.if` template-conditional ---

    fn pkg_if_test_ctx(if_expr: Option<&str>) -> anodizer_core::context::Context {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::{Config, CrateConfig, PkgConfig};
        use anodizer_core::context::{Context, ContextOptions};
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        std::fs::create_dir_all(&config.dist).unwrap();
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            if_condition: if_expr.map(str::to_string),
            ..Default::default()
        };
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
        ctx.template_vars_mut().set("Os", "darwin");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx
    }

    #[test]
    fn test_pkg_if_false_skips_config() {
        use anodizer_core::artifact::ArtifactKind;
        let mut ctx = pkg_if_test_ctx(Some("false"));
        PkgStage.run(&mut ctx).unwrap();
        assert_eq!(
            ctx.artifacts.by_kind(ArtifactKind::MacOsPackage).len(),
            0,
            "pkg if=false should skip"
        );
    }

    #[test]
    fn test_pkg_if_render_failure_is_hard_error() {
        let mut ctx = pkg_if_test_ctx(Some("{{ undefined_function 42 }}"));
        let err = PkgStage
            .run(&mut ctx)
            .expect_err("unrenderable `if` should hard-error");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("`if` template render failed"),
            "error should name `if` render failure, got: {msg}"
        );
    }

    #[test]
    fn test_config_parse_pkg_disable_alias() {
        // The docs show `disable: false`; this must parse without error.
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
        disable: false
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].skip, Some(StringOrBool::Bool(false)));
    }

    #[test]
    fn test_config_parse_pkg_disable_true_alias() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    pkgs:
      - identifier: com.example.test
        disable: true
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let pkgs = config.crates[0].pkgs.as_ref().unwrap();
        assert_eq!(pkgs[0].skip, Some(StringOrBool::Bool(true)));
    }

    #[test]
    fn test_identifier_template_renders() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.{{ ProjectName }}".to_string()),
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

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(
            pkgs[0].metadata.get("identifier").map(|s| s.as_str()),
            Some("com.example.myapp"),
            "identifier template should be rendered"
        );
    }

    /// Build a minimal `Context` with `Version`, `Os`, `Arch`, and `Target` set
    /// so per-binary template renders behave the same as they do inside the
    /// stage loop.
    fn render_fields_test_ctx() -> Context {
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");
        ctx.template_vars_mut().set("Os", "darwin");
        ctx.template_vars_mut().set("Arch", "arm64");
        ctx.template_vars_mut()
            .set("Target", "aarch64-apple-darwin");
        ctx
    }

    #[test]
    fn test_install_location_template_renders() {
        let mut ctx = render_fields_test_ctx();
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            install_location: Some("/opt/{{ ProjectName }}/bin".to_string()),
            ..Default::default()
        };

        let rendered = render_pkg_fields(
            &mut ctx,
            &pkg_cfg,
            pkg_cfg.identifier.as_deref().unwrap(),
            "myapp",
            Some("aarch64-apple-darwin"),
        )
        .unwrap();

        assert_eq!(rendered.install_location, "/opt/myapp/bin");

        let cmd = pkgbuild_command(
            "/tmp/staging",
            &rendered.identifier,
            "1.0.0",
            &rendered.install_location,
            rendered.scripts.as_deref(),
            None,
            "/tmp/out.pkg",
        );
        let loc_idx = cmd.iter().position(|a| a == "--install-location").unwrap();
        assert_eq!(cmd[loc_idx + 1], "/opt/myapp/bin");
    }

    #[test]
    fn test_scripts_template_renders() {
        let mut ctx = render_fields_test_ctx();
        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            scripts: Some("scripts/{{ Os }}".to_string()),
            ..Default::default()
        };

        let rendered = render_pkg_fields(
            &mut ctx,
            &pkg_cfg,
            pkg_cfg.identifier.as_deref().unwrap(),
            "myapp",
            Some("aarch64-apple-darwin"),
        )
        .unwrap();

        assert_eq!(rendered.scripts.as_deref(), Some("scripts/darwin"));

        let cmd = pkgbuild_command(
            "/tmp/staging",
            &rendered.identifier,
            "1.0.0",
            &rendered.install_location,
            rendered.scripts.as_deref(),
            None,
            "/tmp/out.pkg",
        );
        let scripts_idx = cmd.iter().position(|a| a == "--scripts").unwrap();
        assert_eq!(cmd[scripts_idx + 1], "scripts/darwin");
    }

    #[test]
    fn test_mod_timestamp_template_renders() {
        let mut ctx = render_fields_test_ctx();
        ctx.template_vars_mut()
            .set("CommitTimestamp", "2024-06-15T12:34:56Z");

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            mod_timestamp: Some("{{ CommitTimestamp }}".to_string()),
            ..Default::default()
        };

        let rendered = render_pkg_fields(
            &mut ctx,
            &pkg_cfg,
            pkg_cfg.identifier.as_deref().unwrap(),
            "myapp",
            Some("aarch64-apple-darwin"),
        )
        .unwrap();

        assert_eq!(
            rendered.mod_timestamp.as_deref(),
            Some("2024-06-15T12:34:56Z"),
            "mod_timestamp template should expand to the CommitTimestamp value, \
             not be passed literally to parse_mod_timestamp"
        );
        assert_ne!(
            rendered.mod_timestamp.as_deref(),
            Some("{{ CommitTimestamp }}"),
            "literal template string must not reach apply_mod_timestamp"
        );
    }

    #[test]
    fn test_default_name_template_no_version_appends_pkg_extension() {
        // Default template has no version segment; the `.pkg` extension is
        // auto-appended so the default emits ProjectName_Arch.pkg.
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
            filename, "myapp_arm64.pkg",
            "default name template should be ProjectName_Arch with the .pkg extension appended"
        );
    }

    /// A user-supplied `name:` that already ends in `.pkg` is used verbatim —
    /// the auto-append must not double the extension (case-insensitive match).
    #[test]
    fn test_user_name_ending_in_pkg_is_not_doubled() {
        let tmp = TempDir::new().unwrap();

        let pkg_cfg = PkgConfig {
            identifier: Some("com.example.myapp".to_string()),
            name: Some("custom_{{ Arch }}.PKG".to_string()),
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

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::MacOsPackage);
        assert_eq!(pkgs.len(), 1);
        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            filename, "custom_arm64.PKG",
            "a user name already ending in .pkg (any case) must not get a second .pkg"
        );
    }
}
