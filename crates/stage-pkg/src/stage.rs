use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::arch_path_guard::ArchPathGuard;
use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

use crate::build::build_flat_pkg_linux;
use crate::builder::{PkgBuilder, pkgbuild_command, resolve_pkg_builder};

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
const DEFAULT_NAME_PREFIX: &str = "{{ ProjectName }}_{{ Arch }}";

/// Compose the default pkg name template: [`DEFAULT_NAME_PREFIX`] plus the
/// shared amd64 variant suffix from core. Two amd64 builds share one target
/// triple, so `Arch` alone cannot disambiguate them; the suffix keeps their
/// filenames distinct without re-embedding the clause literal here.
pub(crate) fn default_name_template() -> String {
    format!(
        "{DEFAULT_NAME_PREFIX}{}",
        anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX
    )
}

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
            .crate_universe()
            .into_iter()
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

                // One guard spans every config of this crate so two configs that
                // render the same dist/macos/<name>.pkg (same target, default/
                // identical name) collide loudly instead of the second silently
                // clobbering the first; it resets per crate, so distinct crates are
                // unaffected.
                let mut arch_guard = ArchPathGuard::new();
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
                            "skipped pkg config '{}' for crate {} — `if` condition evaluated falsy",
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
                        // Wrap the macOS `.app` directory bundle for this crate.
                        ctx.artifacts
                            .by_kind_and_crate(ArtifactKind::Installer, &krate.name)
                            .into_iter()
                            .filter(|a| anodizer_core::artifact::is_directory_bundle_artifact(a))
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
                        let msg = if use_mode == "appbundle" {
                            format!(
                                "skipped PKG generation for crate '{}' — no appbundle artifacts \
                             found (expected Installer artifacts with format=appbundle)",
                                krate.name
                            )
                        } else {
                            format!(
                                "skipped PKG generation for crate '{}' — no macOS binary \
                             artifacts found (expected binaries targeting darwin/apple)",
                                krate.name
                            )
                        };
                        log.skip_line(ctx.options.show_skipped, &msg);
                        continue;
                    }
                    if filtered.is_empty() {
                        log.warn(&format!(
                            "skipped pkg for crate '{}' — ids filter {:?} matched no artifacts",
                            krate.name, pkg_cfg.ids
                        ));
                        continue;
                    }

                    let effective_binaries: Vec<(Option<String>, Option<String>, PathBuf)> =
                        filtered
                            .iter()
                            .map(|b| {
                                (
                                    b.target.clone(),
                                    b.metadata.get("amd64_variant").cloned(),
                                    b.path.clone(),
                                )
                            })
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

                    // Resolve the build path once per config entry before the
                    // per-binary loop so the error surfaces early naming both
                    // options. `pkgbuild` (macOS) is preferred; otherwise the
                    // Linux flat-package toolchain assembles the identical XAR
                    // layout by hand. Dry-run never requires a tool.
                    let builder = if dry_run {
                        PkgBuilder::Pkgbuild
                    } else {
                        resolve_pkg_builder(anodizer_core::tool_detect::on_path)
                            .map_err(anyhow::Error::msg)?
                    };

                    // One .pkg is produced per binary — pkg installers are single-binary
                    // by design. Unlike DMG (which groups multiple binaries into one
                    // container image), each pkg wraps exactly one payload binary so that
                    // Homebrew formula installers and macOS Installer.app each target a
                    // discrete, independently versionable package. Multi-binary crates
                    // therefore emit N packages per target triple.
                    //
                    // Reject a `name` lacking `{{ .Arch }}` that would render the same
                    // dist/macos/<name>.pkg for two build targets (silent clobber).
                    let default_name = default_name_template();

                    for (target, amd64_variant, binary_path) in &effective_binaries {
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
                        // Seed the amd64 variant so the default (or a custom) name
                        // template disambiguates two amd64 builds of one target.
                        anodizer_core::archive_name::seed_amd64_variant_var(
                            ctx.template_vars_mut(),
                            &arch,
                            amd64_variant.as_deref(),
                        );

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
                        let name_template = pkg_cfg.name.as_deref().unwrap_or(&default_name);

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

                        arch_guard.check(
                            &pkg_path,
                            "pkgs",
                            "package",
                            name_template,
                            &pkg_filename,
                            &krate.name,
                        )?;

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
                                    if let Some(v) = amd64_variant {
                                        m.insert("amd64_variant".to_string(), v.clone());
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
                        // In appbundle mode `binary_path` is a `.app` directory;
                        // fs::copy rejects directories, so recurse (symlink-safe,
                        // preserving the bundle's framework version links).
                        if binary_path.is_dir() {
                            anodizer_core::util::copy_dir_tree(binary_path, &staged_binary)
                                .with_context(|| {
                                    format!(
                                        "pkg: copy app bundle {} to staging dir {}",
                                        binary_path.display(),
                                        staging_dir.display()
                                    )
                                })?;
                        } else {
                            fs::copy(binary_path, &staged_binary).with_context(|| {
                                format!(
                                    "pkg: copy binary {} to staging dir {}",
                                    binary_path.display(),
                                    staging_dir.display()
                                )
                            })?;
                        }

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

                        match builder {
                            PkgBuilder::Pkgbuild => {
                                let cmd_args = pkgbuild_command(
                                    &staging_dir.to_string_lossy(),
                                    &identifier,
                                    &version,
                                    &install_location,
                                    scripts_rendered.as_deref(),
                                    pkg_cfg.min_os_version.as_deref(),
                                    &pkg_path.to_string_lossy(),
                                );

                                log.verbose(&format!("running {}", cmd_args.join(" ")));

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

                                // Stamp the output mtime for native-vs-Linux
                                // parity (the Linux path stamps its .pkg too).
                                // The mtime is not in the archive bytes, so
                                // checksums are unaffected.
                                if let Some(ts) = &mod_timestamp_rendered {
                                    let mtime = anodizer_core::util::parse_mod_timestamp(ts)?;
                                    anodizer_core::util::set_file_mtime(&pkg_path, mtime)?;
                                }
                            }
                            PkgBuilder::Linux => {
                                log.status(&format!(
                                    "assembling flat .pkg (Linux xar/mkbom/cpio/gzip) for crate {} target {:?}",
                                    krate.name, target
                                ));
                                build_flat_pkg_linux(
                                    staging_dir,
                                    &identifier,
                                    &version,
                                    &install_location,
                                    scripts_rendered.as_deref(),
                                    pkg_cfg.min_os_version.as_deref(),
                                    mod_timestamp_rendered.as_deref(),
                                    &pkg_path,
                                    &log,
                                )?;
                            }
                        }

                        log.status(&format!(
                            "built pkg {}",
                            pkg_path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| pkg_path.to_string_lossy().into_owned())
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
                                if let Some(v) = amd64_variant {
                                    m.insert("amd64_variant".to_string(), v.clone());
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
