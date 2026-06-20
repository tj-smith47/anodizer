use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::arch_path_guard::ArchPathGuard;
use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

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
    if anodizer_core::util::find_binary("hdiutil") {
        Some(DmgTool::Hdiutil)
    } else if anodizer_core::util::find_binary("genisoimage") {
        Some(DmgTool::Genisoimage)
    } else if anodizer_core::util::find_binary("mkisofs") {
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
    anodizer_core::target::os_arch_with_default(target, "darwin")
}

/// Stem of the default output filename template `{{ ProjectName }}_{{ Arch }}`
/// (the `.dmg` extension is appended automatically). In workspace per-crate mode
/// the `ProjectName` var is rebound to each crate's name so the rendered
/// filename is distinct per crate. The amd64 micro-architecture variant suffix
/// is appended by [`default_name_template`] from the single source of truth in
/// core, since two amd64 builds share one target triple and `Arch` alone cannot
/// tell them apart.
const DEFAULT_NAME_PREFIX: &str = "{{ ProjectName }}_{{ Arch }}";

/// Compose the default dmg name template: [`DEFAULT_NAME_PREFIX`] plus the
/// shared amd64 variant suffix from core (no clause re-embedded here).
fn default_name_template() -> String {
    format!(
        "{DEFAULT_NAME_PREFIX}{}",
        anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX
    )
}

/// Stage the DMG payload for one source into `staging_dir`, dispatching on
/// `use_mode`, and return the staged path (`staging_dir.join(binary_name)`).
///
/// - `use_mode == "binary"`: `binary_path` is a regular file; it is copied and
///   then forced to mode `0o755` on Unix. `fs::copy` preserves source
///   permissions, so a binary unpacked from an archive that stripped the
///   execute bit would otherwise ship inside the DMG as non-executable.
/// - `use_mode == "appbundle"`: `binary_path` is a `.app` bundle directory; it
///   is copied recursively (preserving the tree, file contents, Unix mode bits,
///   and embedded symlinks). The top level is a directory, so no chmod is
///   applied — the bundle already carries the executable bit on its inner
///   `Contents/MacOS/<binary>`.
pub(crate) fn stage_binary_into(
    staging_dir: &std::path::Path,
    binary_path: &std::path::Path,
    binary_name: &str,
    use_mode: &str,
) -> Result<std::path::PathBuf> {
    let staged_binary = staging_dir.join(binary_name);
    if use_mode == "appbundle" {
        anodizer_core::util::copy_dir_tree(binary_path, &staged_binary)
            .with_context(|| format!("copy app bundle {} to staging dir", binary_path.display()))?;
        return Ok(staged_binary);
    }
    std::fs::copy(binary_path, &staged_binary)
        .with_context(|| format!("copy binary {} to staging dir", binary_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&staged_binary, perms).with_context(|| {
            format!(
                "dmg: set executable permission on {}",
                staged_binary.display()
            )
        })?;
    }
    Ok(staged_binary)
}

/// Insert an `/Applications` symlink into the staging directory when
/// packaging an app bundle, giving mounted DMGs the standard drag-and-drop
/// install UX. No-op for any other `use_mode`.
///
/// On Windows hosts the symlink may not resolve correctly when the image
/// is mounted; this helper is `#[cfg(unix)]` so non-Unix builds skip it
/// silently.
#[cfg(unix)]
pub(crate) fn maybe_create_applications_symlink(
    staging_dir: &std::path::Path,
    use_mode: &str,
) -> Result<()> {
    if use_mode != "appbundle" {
        return Ok(());
    }
    let link_path = staging_dir.join("Applications");
    if link_path.symlink_metadata().is_ok() {
        return Ok(());
    }
    std::os::unix::fs::symlink("/Applications", &link_path).with_context(|| {
        format!(
            "dmg: create /Applications symlink at {}",
            link_path.display()
        )
    })
}

/// Resolve the volume label for a DMG: render the configured `volume_name`
/// template, or fall back to the project name when unset.
pub(crate) fn resolve_volume_name(
    ctx: &Context,
    dmg_cfg: &anodizer_core::config::DmgConfig,
    project_name: &str,
) -> Result<String> {
    match &dmg_cfg.volume_name {
        Some(tmpl) => ctx
            .render_template(tmpl)
            .with_context(|| "dmg: render volume_name template"),
        None => Ok(project_name.to_string()),
    }
}

/// Render a `mod_timestamp` template through Tera, returning the resolved
/// string ready to feed to `apply_mod_timestamp` / `parse_mod_timestamp`.
pub(crate) fn resolve_mod_timestamp(ctx: &Context, tmpl: &str) -> Result<String> {
    ctx.render_template(tmpl)
        .with_context(|| "dmg: render mod_timestamp template")
}

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

        // In workspace per-crate mode the same pipeline run produces a DMG for
        // each crate. Rebinding the `ProjectName` template var to the current
        // crate's name (mirroring the archive stage) keeps default name
        // templates like `{{ ProjectName }}_{{ Arch }}` distinct per crate so
        // two crates' DMGs don't render the same filename and clobber each
        // other. The original value is restored after the loop.
        let multi_crate = crates.len() > 1;
        let original_project_name = ctx
            .template_vars()
            .get("ProjectName")
            .cloned()
            .unwrap_or_else(|| project_name.clone());

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archives_to_remove: Vec<PathBuf> = Vec::new();

        // Capture the loop result rather than `?`-ing inside it: a per-crate
        // failure must still restore the rebound `ProjectName` below before
        // propagating, so the workspace value never leaks past this stage.
        let loop_result: Result<()> = (|| {
            for krate in &crates {
                let Some(dmgs) = krate.dmgs.as_ref() else {
                    continue;
                };
                if multi_crate {
                    ctx.template_vars_mut().set("ProjectName", &krate.name);
                }
                // Per-crate volume-label default: the crate's name in multi-crate
                // mode so the mounted volume disambiguates like the filename does;
                // the workspace project name otherwise.
                let crate_project_name = if multi_crate {
                    krate.name.clone()
                } else {
                    project_name.clone()
                };
                for dmg_cfg in dmgs {
                    let dmg_id_for_log = dmg_cfg.id.as_deref().unwrap_or("default").to_string();

                    // `dmg.if`: template-conditional skip (opt-in).
                    // Render error => hard bail (avoids the W1 silent-skip
                    // footgun: user's typo must surface, not silently ship a
                    // release without the DMG they asked for).
                    let proceed = anodizer_core::config::evaluate_if_condition(
                        dmg_cfg.if_condition.as_deref(),
                        &format!("dmg config '{}' for crate '{}'", dmg_id_for_log, krate.name),
                        |t| ctx.render_template(t),
                    )?;
                    if !proceed {
                        log.status(&format!(
                            "skipped dmg config '{}' for crate {} — `if` condition evaluated falsy",
                            dmg_id_for_log, krate.name
                        ));
                        continue;
                    }

                    // Skip configs marked skip:
                    if let Some(ref d) = dmg_cfg.skip {
                        let off = d
                            .try_evaluates_to_true(|s| ctx.render_template(s))
                            .with_context(|| {
                                format!("dmg: render skip template for crate {}", krate.name)
                            })?;
                        if off {
                            log.status(&format!("dmg config skipped for crate {}", krate.name));
                            continue;
                        }
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

                    // Pre-flight: resolve extra_files through the canonical
                    // resolver so a constant name_template paired with a
                    // multi-match glob (which would silently overwrite every
                    // match to the same dst) fails before any subprocess spawn
                    // and in dry-run too. The resolved set is recomputed at copy
                    // time below.
                    if let Some(extra_files) = &dmg_cfg.extra_files {
                        anodizer_core::extrafiles::resolve(extra_files, &log)
                            .context("dmg: validate extra_files")?;
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
                                    .map(anodizer_core::target::is_darwin)
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

                    // `amd64_variant` filter.
                    // amd64-variant filtering:
                    // only constrains `amd64` artifacts. Non-amd64 always passes.
                    // Unset `amd64_variant` metadata is treated as `v1`.
                    if let Some(ref want) = dmg_cfg.amd64_variant {
                        filtered.retain(|b| {
                            let target = b.target.as_deref().unwrap_or("");
                            let (_, arch) = anodizer_core::target::map_target(target);
                            if arch != "amd64" {
                                return true;
                            }
                            b.metadata
                                .get("amd64_variant")
                                .map(String::as_str)
                                .unwrap_or("v1")
                                == want
                        });
                    }

                    // Warn and skip if no source artifacts found
                    if filtered.is_empty() && source_artifacts.is_empty() {
                        let msg = if use_mode == "appbundle" {
                            format!(
                                "skipped DMG generation for crate '{}' — no appbundle artifacts \
                             found (expected Installer artifacts with format=appbundle)",
                                krate.name
                            )
                        } else {
                            format!(
                                "skipped DMG generation for crate '{}' — no macOS binary \
                             artifacts found (expected binaries targeting darwin/apple)",
                                krate.name
                            )
                        };
                        log.skip_line(ctx.options.show_skipped, &msg);
                        continue;
                    }
                    if filtered.is_empty() {
                        log.warn(&format!(
                            "skipped dmg for crate '{}' — ids filter {:?} matched no artifacts",
                            krate.name, dmg_cfg.ids
                        ));
                        continue;
                    }

                    // Group binaries by (target, amd64 variant) so a multi-binary
                    // crate (e.g. a CLI with several `bin = ` entries) produces ONE
                    // DMG per target containing all binaries — the per-target DMG
                    // layout, not per-binary — while two amd64 builds of the same
                    // target (e.g. a baseline `v1` and a `-Ctarget-cpu=x86-64-v3`
                    // `v3`) split into distinct DMGs instead of being bundled into
                    // one image with both variant binaries.
                    let mut by_target: std::collections::BTreeMap<
                        (Option<String>, Option<String>),
                        Vec<PathBuf>,
                    > = std::collections::BTreeMap::new();
                    for b in &filtered {
                        by_target
                            .entry((b.target.clone(), b.metadata.get("amd64_variant").cloned()))
                            .or_default()
                            .push(b.path.clone());
                    }

                    // Reject a `name` lacking `{{ .Arch }}` that would render the
                    // same dist/macos/<name>.dmg for two arches (silent clobber).
                    let mut arch_guard = ArchPathGuard::new();

                    let default_name = default_name_template();

                    for ((target, amd64_variant), binary_paths) in &by_target {
                        // Derive Os/Arch from the target triple for template rendering
                        let (os, arch) = os_arch_from_target(target.as_deref());

                        // Set Os/Arch/Target in template vars for this iteration
                        ctx.template_vars_mut().set("Os", &os);
                        ctx.template_vars_mut().set("Arch", &arch);
                        ctx.template_vars_mut()
                            .set("Target", target.as_deref().unwrap_or(""));
                        // Seed the amd64 variant so the default (or a custom) name
                        // template disambiguates two amd64 builds of one target.
                        anodizer_core::archive_name::seed_amd64_variant_var(
                            ctx.template_vars_mut(),
                            amd64_variant.as_deref(),
                        );

                        // Determine output filename from name template or default
                        let name_template = dmg_cfg.name.as_deref().unwrap_or(&default_name);

                        let dmg_filename =
                            ctx.render_template(name_template).with_context(|| {
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

                        arch_guard.check(
                            &dmg_path,
                            "dmgs",
                            "image",
                            name_template,
                            &dmg_filename,
                            &krate.name,
                        )?;

                        let vol_name = resolve_volume_name(ctx, dmg_cfg, &crate_project_name)?;

                        // Resolve each source binary's staged leaf name ONCE: the
                        // pre-flight duplicate check and the copy loop below both
                        // need it, so compute the (path, leaf-name) pairs here and
                        // reuse them rather than re-deriving `file_name()` twice.
                        let staged: Vec<(&std::path::PathBuf, String)> = binary_paths
                            .iter()
                            .map(|p| {
                                let name = p
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or(&krate.name)
                                    .to_string();
                                (p, name)
                            })
                            .collect();

                        // Pre-flight: two source paths with the same leaf name would
                        // silently overwrite the first during staging. Detect early so
                        // this fires in both dry-run and live mode.
                        {
                            let mut staged_names: std::collections::HashSet<&str> =
                                std::collections::HashSet::new();
                            for (_, binary_name) in &staged {
                                if !staged_names.insert(binary_name.as_str()) {
                                    anyhow::bail!(
                                        "dmg: duplicate filename '{}' in staging dir for crate \
                                     '{}' target {:?}; two source binaries resolve to the \
                                     same name",
                                        binary_name,
                                        krate.name,
                                        target
                                    );
                                }
                            }
                        }

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
                                    if let Some(v) = amd64_variant {
                                        m.insert("amd64_variant".to_string(), v.clone());
                                    }
                                    m
                                },
                                size: None,
                            });

                            // If replace is set, mark archives for this crate+target for removal
                            archives_to_remove.extend(anodizer_core::util::collect_if_replace(
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

                        // Copy every binary for this target into the staging dir,
                        // reusing the leaf names resolved for the pre-flight above.
                        for (binary_path, binary_name) in &staged {
                            stage_binary_into(staging_dir, binary_path, binary_name, use_mode)?;
                        }

                        #[cfg(unix)]
                        maybe_create_applications_symlink(staging_dir, use_mode)?;

                        // Copy extra files into staging dir via the canonical
                        // resolver (dedup + sort + bail-on-multi-match when a
                        // name_template is set).
                        if let Some(extra_files) = &dmg_cfg.extra_files {
                            let resolved = anodizer_core::extrafiles::resolve(extra_files, &log)
                                .context("dmg: resolve extra_files")?;
                            for rf in resolved {
                                let dst_name = rf
                                    .name_template
                                    .or_else(|| {
                                        rf.path
                                            .file_name()
                                            .and_then(|n| n.to_str())
                                            .map(|s| s.to_string())
                                    })
                                    .unwrap_or_else(|| "extra".to_string());
                                let dst = staging_dir.join(&dst_name);
                                fs::copy(&rf.path, &dst).with_context(|| {
                                    format!("copy extra file {} to staging dir", rf.path.display())
                                })?;
                            }
                        }

                        // Process templated_extra_files: render and copy to staging dir
                        if let Some(ref tpl_specs) = dmg_cfg.templated_extra_files
                            && !tpl_specs.is_empty()
                        {
                            anodizer_core::templated_files::process_templated_extra_files(
                                tpl_specs,
                                ctx,
                                staging_dir,
                                "dmg",
                            )?;
                        }

                        if let Some(ref ts_tmpl) = dmg_cfg.mod_timestamp {
                            let ts = resolve_mod_timestamp(ctx, ts_tmpl)?;
                            anodizer_core::util::apply_mod_timestamp(staging_dir, &ts, &log)?;
                        }

                        // On macOS, detach a stale mount at the same volume path before
                        // creating a new image. Silent best-effort — a non-zero exit
                        // (e.g. nothing mounted) is not an error.
                        if tool == DmgTool::Hdiutil {
                            let mount_path = format!("/Volumes/{vol_name}");
                            let detach = Command::new("hdiutil")
                                .args(["detach", "-force", &mount_path])
                                .output();
                            if let Ok(out) = detach
                                && out.status.success()
                            {
                                log.verbose(&format!("detached stale mount at {mount_path}"));
                            }
                        }

                        // Build and run the command
                        let cmd_args = dmg_command(
                            tool,
                            &vol_name,
                            &staging_dir.to_string_lossy(),
                            &dmg_path.to_string_lossy(),
                        );

                        log.verbose(&format!("running {}", cmd_args.join(" ")));

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

                        log.status(&format!(
                            "built DMG {}",
                            dmg_path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| dmg_path.to_string_lossy().into_owned())
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
                                if let Some(v) = amd64_variant {
                                    m.insert("amd64_variant".to_string(), v.clone());
                                }
                                m
                            },
                            size: None,
                        });

                        // If replace is set, mark archives for this crate+target for removal
                        archives_to_remove.extend(anodizer_core::util::collect_if_replace(
                            dmg_cfg.replace,
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

/// Environment requirements for the dmg stage: one of the image tools the
/// stage's detection ladder accepts (`hdiutil` > `genisoimage` > `mkisofs`)
/// when any active `dmgs:` entry exists and the configured build targets
/// include macOS (the stage only images darwin binaries).
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
        .flat_map(|c| c.dmgs.iter().flatten())
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
    vec![anodizer_core::EnvRequirement::ToolAnyOf {
        names: vec![
            "hdiutil".to_string(),
            "genisoimage".to_string(),
            "mkisofs".to_string(),
        ],
    }]
}

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
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        // DmgStage should be a no-op when crates have no dmg block
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = DmgStage;
        assert!(stage.run(&mut ctx).is_ok());
        assert!(ctx.artifacts.all().is_empty());
    }

    #[test]
    fn test_stage_skips_when_disabled() {
        use anodizer_core::config::{Config, CrateConfig, DmgConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};

        let dmg_cfg = DmgConfig {
            skip: Some(StringOrBool::Bool(true)),
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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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
    fn test_workspace_per_crate_distinct_filenames() {
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Two crates in one workspace, both using the DEFAULT name template
        // (no Version segment, so ProjectName is the only distinguishing token).
        // Before the per-crate ProjectName rebind, both render to
        // `<project_name>_arm64.dmg` and clobber each other.
        let make_crate = |name: &str| CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![DmgConfig::default()]),
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
                path: PathBuf::from(format!("dist/{crate_name}")),
                target: Some("aarch64-apple-darwin".to_string()),
                crate_name: crate_name.to_string(),
                metadata: Default::default(),
                size: None,
            });
        }

        let stage = DmgStage;
        stage.run(&mut ctx).unwrap();

        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(dmgs.len(), 2, "expected one DMG per crate");

        let filenames: Vec<String> = dmgs
            .iter()
            .map(|a| a.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(
            filenames.iter().any(|f| f.contains("alpha")),
            "no DMG filename contains crate name 'alpha': {filenames:?}"
        );
        assert!(
            filenames.iter().any(|f| f.contains("beta")),
            "no DMG filename contains crate name 'beta': {filenames:?}"
        );
        assert_ne!(
            filenames[0], filenames[1],
            "the two crates' DMGs must not share a filename (clobber): {filenames:?}"
        );

        // The ProjectName var must be restored to the workspace value after the
        // stage so downstream stages don't inherit the last crate's name.
        assert_eq!(
            ctx.template_vars().get("ProjectName").map(String::as_str),
            Some("workspace"),
            "ProjectName not restored after per-crate rebind"
        );
    }

    #[test]
    fn test_stage_dry_run_with_name_template() {
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let dmgs = config.crates[0].dmgs.as_ref().unwrap();
        assert_eq!(dmgs.len(), 1);
        assert_eq!(
            dmgs[0].name.as_deref(),
            Some("{{ ProjectName }}_{{ Version }}_{{ Arch }}.dmg")
        );
        assert!(dmgs[0].skip.is_none());
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
        skip: false
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
            dmg.skip,
            Some(anodizer_core::config::StringOrBool::Bool(false))
        );
    }

    #[test]
    fn test_invalid_name_template_errors() {
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Create a fake binary and extra file on disk
        let binary_path = tmp.path().join("myapp");
        fs::write(&binary_path, b"fake-binary").unwrap();

        let extra_path = tmp.path().join("README.md");
        fs::write(&extra_path, b"readme content").unwrap();

        let dmg_cfg = DmgConfig {
            extra_files: Some(vec![anodizer_core::config::ExtraFileSpec::Glob(
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

        // Outcome depends on whether a DMG-imaging tool is installed on this
        // host (hdiutil/genisoimage/mkisofs), not on the OS: with a tool present
        // the stage images the staging dir and succeeds; with none it errors
        // after staging with a tool-missing message.
        if dmg_tool().is_some() {
            assert!(
                result.is_ok(),
                "stage should succeed when a DMG tool is available, got: {:#}",
                result.unwrap_err()
            );
            assert!(
                ctx.artifacts
                    .all()
                    .iter()
                    .any(|a| a.kind == ArtifactKind::DiskImage),
                "a DiskImage artifact should be registered when imaging succeeds"
            );
        } else {
            assert!(result.is_err(), "expected failure due to missing DMG tool");
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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};

        // Test with StringOrBool::String("true")
        let dmg_cfg = DmgConfig {
            skip: Some(StringOrBool::String("true".to_string())),
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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Test with StringOrBool::String("false") — should NOT be disabled
        let dmg_cfg = DmgConfig {
            skip: Some(StringOrBool::String("false".to_string())),
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
        use anodizer_core::config::{Config, CrateConfig, DmgConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};

        // Template that evaluates to "true" when IsSnapshot is truthy
        let dmg_cfg = DmgConfig {
            skip: Some(StringOrBool::String(
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
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
      - skip: "{% if IsSnapshot %}true{% endif %}"
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let dmgs = config.crates[0].dmgs.as_ref().unwrap();
        assert_eq!(
            dmgs[0].skip,
            Some(anodizer_core::config::StringOrBool::String(
                "{% if IsSnapshot %}true{% endif %}".to_string()
            ))
        );
    }

    #[test]
    fn test_use_appbundle_skips_when_no_appbundles() {
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

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

    // --- `dmg.if` template-conditional ---

    fn dmg_if_test_ctx(if_expr: Option<&str>) -> anodizer_core::context::Context {
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        std::fs::create_dir_all(&config.dist).unwrap();
        let dmg_cfg = DmgConfig {
            if_condition: if_expr.map(str::to_string),
            ..Default::default()
        };
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
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
        // Seed a binary so DmgStage has something to package.
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx
    }

    #[test]
    fn test_dmg_if_false_skips_config() {
        let mut ctx = dmg_if_test_ctx(Some("false"));
        DmgStage.run(&mut ctx).unwrap();
        assert_eq!(
            ctx.artifacts.by_kind(ArtifactKind::DiskImage).len(),
            0,
            "dmg if=false should skip, producing no DiskImage artifacts"
        );
    }

    #[test]
    fn test_dmg_if_empty_string_skips_config() {
        let mut ctx = dmg_if_test_ctx(Some("{{ if false }}{{ end }}"));
        DmgStage.run(&mut ctx).unwrap();
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::DiskImage).len(), 0);
    }

    #[test]
    fn test_dmg_if_render_failure_is_hard_error() {
        let mut ctx = dmg_if_test_ctx(Some("{{ undefined_function 42 }}"));
        let err = DmgStage
            .run(&mut ctx)
            .expect_err("unrenderable `if` should hard-error");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("`if` template render failed"),
            "error should name the `if` render failure, got: {msg}"
        );
    }

    // -------------------------------------------------------------------
    // `dmg.amd64_variant` filter
    // -------------------------------------------------------------------

    /// Build a context with three darwin/amd64 binaries (variants v1/v2/v3)
    /// and one darwin/arm64 binary. The `amd64_variant` field on the config
    /// drives which subset of amd64 binaries makes it into DiskImage
    /// artifacts; arm64 is always included regardless.
    fn dmg_amd64_variant_test_ctx(amd64_variant: Option<&str>) -> anodizer_core::context::Context {
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        std::fs::create_dir_all(&config.dist).unwrap();
        let dmg_cfg = DmgConfig {
            amd64_variant: amd64_variant.map(str::to_string),
            ..Default::default()
        };
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
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

        // Three amd64 variants (v1/v2/v3) + one arm64 (no variant tag).
        for variant in ["v1", "v2", "v3"] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from(format!("dist/myapp_{variant}")),
                target: Some("x86_64-apple-darwin".to_string()),
                crate_name: "myapp".to_string(),
                metadata: HashMap::from([("amd64_variant".to_string(), variant.to_string())]),
                size: None,
            });
        }
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_arm"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        ctx
    }

    #[test]
    fn test_dmg_amd64_variant_unset_passes_all_amd64_variants() {
        let mut ctx = dmg_amd64_variant_test_ctx(None);
        DmgStage.run(&mut ctx).unwrap();
        // Grouping key is `(target, amd64_variant)`, so the 3 same-triple amd64
        // builds form 3 groups (distinct names via the amd64 suffix) + 1 arm64.
        // Total: 4 DMGs, all distinctly named (v1 baseline renders no suffix).
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(
            dmgs.len(),
            4,
            "unset amd64_variant should pass every variant as its own DMG"
        );
        let names: Vec<&str> = dmgs
            .iter()
            .map(|a| a.path.file_name().unwrap().to_str().unwrap())
            .collect();
        let distinct: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(
            distinct.len(),
            names.len(),
            "all rendered DMG names must be distinct: {names:?}"
        );
    }

    #[test]
    fn test_dmg_amd64_variant_v3_only_keeps_matching_variant() {
        let mut ctx = dmg_amd64_variant_test_ctx(Some("v3"));
        DmgStage.run(&mut ctx).unwrap();
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        // Only the v3 amd64 binary survives (one amd64 DMG) + the arm64
        // binary (one arm64 DMG). v1 and v2 are filtered out.
        assert_eq!(dmgs.len(), 2);
        let targets: Vec<&str> = dmgs.iter().map(|a| a.target.as_deref().unwrap()).collect();
        assert!(targets.contains(&"x86_64-apple-darwin"));
        assert!(targets.contains(&"aarch64-apple-darwin"));
    }

    #[test]
    fn test_dmg_amd64_variant_filter_does_not_drop_arm64() {
        // Pin: filter only constrains amd64 — arm64 must still pass even
        // when the filter rejects every amd64 variant present.
        let mut ctx = dmg_amd64_variant_test_ctx(Some("v9000")); // matches no variant
        DmgStage.run(&mut ctx).unwrap();
        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        // No amd64 survives; arm64 still produces one DMG.
        assert_eq!(dmgs.len(), 1);
        assert_eq!(dmgs[0].target.as_deref(), Some("aarch64-apple-darwin"));
    }

    // -------------------------------------------------------------------
    // Default name template shape
    // -------------------------------------------------------------------

    #[test]
    fn test_default_name_template_contains_amd64_variant_suffix() {
        assert!(
            default_name_template()
                .contains(anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX),
            "default name template must reuse the shared amd64 variant suffix"
        );
    }

    #[test]
    fn test_default_name_template_matches_gr_shape() {
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![DmgConfig::default()]),
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

        DmgStage.run(&mut ctx).unwrap();

        let dmgs = ctx.artifacts.by_kind(ArtifactKind::DiskImage);
        assert_eq!(dmgs.len(), 1);
        let name = dmgs[0].path.file_name().unwrap().to_string_lossy();
        // Default: ProjectName_Arch (no version segment, .dmg appended)
        assert!(
            name.starts_with("myapp_") && name.ends_with("arm64.dmg"),
            "default name should be ProjectName_Arch.dmg, got: {name}"
        );
        assert!(
            !name.contains("1.0.0"),
            "default name must not embed the version, got: {name}"
        );
    }

    // -------------------------------------------------------------------
    // mod_timestamp template rendering — positive assertion via helper
    // -------------------------------------------------------------------

    #[test]
    fn test_resolve_mod_timestamp_renders_built_in_var() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.2.3");

        let rendered = resolve_mod_timestamp(&ctx, "{{ Version }}").unwrap();
        assert_eq!(rendered, "1.2.3");
    }

    #[test]
    fn test_resolve_mod_timestamp_surfaces_render_errors() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        // Unclosed tag — Tera should reject it.
        let err = resolve_mod_timestamp(&ctx, "{{ Version").expect_err("malformed template");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("mod_timestamp"),
            "error must name mod_timestamp, got: {msg}"
        );
    }

    // -------------------------------------------------------------------
    // volume_name resolution — positive assertion via helper
    // -------------------------------------------------------------------

    #[test]
    fn test_resolve_volume_name_renders_template() {
        use anodizer_core::config::{Config, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ProjectName", "myapp");

        let dmg_cfg = DmgConfig {
            volume_name: Some("{{ ProjectName }}-Installer".to_string()),
            ..Default::default()
        };

        let resolved = resolve_volume_name(&ctx, &dmg_cfg, "myapp").unwrap();
        assert_eq!(resolved, "myapp-Installer");
    }

    #[test]
    fn test_resolve_volume_name_falls_back_to_project_name() {
        use anodizer_core::config::{Config, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        let dmg_cfg = DmgConfig {
            volume_name: None,
            ..Default::default()
        };

        let resolved = resolve_volume_name(&ctx, &dmg_cfg, "fallback-project").unwrap();
        assert_eq!(resolved, "fallback-project");
    }

    #[test]
    fn test_resolve_volume_name_surfaces_render_errors() {
        use anodizer_core::config::{Config, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let ctx = Context::new(Config::default(), ContextOptions::default());
        let dmg_cfg = DmgConfig {
            volume_name: Some("{{ ProjectName".to_string()),
            ..Default::default()
        };
        let err = resolve_volume_name(&ctx, &dmg_cfg, "myapp").expect_err("malformed template");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("volume_name"),
            "error must name volume_name, got: {msg}"
        );
    }

    // -------------------------------------------------------------------
    // extra_files multi-match + constant name_template must bail
    // -------------------------------------------------------------------

    #[test]
    fn test_extra_files_multi_match_name_template_bails() {
        use anodizer_core::config::{Config, CrateConfig, DmgConfig, ExtraFileSpec};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();
        // Create two files that a glob will match.
        fs::write(tmp.path().join("a.txt"), b"a").unwrap();
        fs::write(tmp.path().join("b.txt"), b"b").unwrap();

        let glob_pattern = format!("{}/*.txt", tmp.path().display());
        let spec = ExtraFileSpec::Detailed {
            glob: glob_pattern,
            name_template: Some("output.txt".to_string()),
            allow_empty: false,
        };

        let dmg_cfg = DmgConfig {
            extra_files: Some(vec![spec]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![dmg_cfg]),
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

        let err = DmgStage
            .run(&mut ctx)
            .expect_err("multi-match glob + name_template must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("name_template") && msg.contains("exactly one"),
            "error should mention name_template and single-match requirement, got: {msg}"
        );
    }

    // -------------------------------------------------------------------
    // Duplicate filename in staging bails with a clear error
    // -------------------------------------------------------------------

    #[test]
    fn test_duplicate_staged_filename_bails() {
        use anodizer_core::config::{Config, CrateConfig, DmgConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();
        // Two real binary files with the same leaf name in different dirs.
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        fs::write(dir_a.join("myapp"), b"binary-a").unwrap();
        fs::write(dir_b.join("myapp"), b"binary-b").unwrap();

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            dmgs: Some(vec![DmgConfig::default()]),
            ..Default::default()
        }];

        // Live mode so the staging copy runs.
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.0.0");

        // Two binaries with the same filename under the same target.
        for dir in [&dir_a, &dir_b] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: dir.join("myapp"),
                target: Some("aarch64-apple-darwin".to_string()),
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            });
        }

        let err = DmgStage
            .run(&mut ctx)
            .expect_err("duplicate filename must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("duplicate") && msg.contains("myapp"),
            "error should mention duplicate and the conflicting filename, got: {msg}"
        );
    }

    // -------------------------------------------------------------------
    // /Applications symlink helper — positive + negative assertion
    // -------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn test_applications_symlink_created_for_appbundle() {
        let tmp = tempfile::tempdir().unwrap();
        maybe_create_applications_symlink(tmp.path(), "appbundle").unwrap();

        let link = tmp.path().join("Applications");
        assert!(
            link.symlink_metadata().is_ok(),
            "symlink entry not created at {}",
            link.display()
        );
        let target = std::fs::read_link(&link).unwrap();
        assert_eq!(target, std::path::Path::new("/Applications"));
    }

    #[cfg(unix)]
    #[test]
    fn test_applications_symlink_skipped_for_binary() {
        let tmp = tempfile::tempdir().unwrap();
        maybe_create_applications_symlink(tmp.path(), "binary").unwrap();

        let link = tmp.path().join("Applications");
        assert!(
            link.symlink_metadata().is_err(),
            "no symlink should exist for use=binary, got entry at {}",
            link.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_applications_symlink_idempotent() {
        // Two invocations on the same staging dir must not fail.
        let tmp = tempfile::tempdir().unwrap();
        maybe_create_applications_symlink(tmp.path(), "appbundle").unwrap();
        maybe_create_applications_symlink(tmp.path(), "appbundle").unwrap();
        let link = tmp.path().join("Applications");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::path::Path::new("/Applications")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_stage_binary_into_chmods_binary_use_mode_to_executable() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("payload");
        std::fs::write(&src, b"not really a binary").unwrap();
        // Strip the executable bit on the source to simulate an artifact unpacked
        // from a tarball that did not preserve perms.
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();

        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        let staged = stage_binary_into(&staging, &src, "payload", "binary").unwrap();
        let mode = std::fs::metadata(&staged).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "binary use_mode must produce a 0o755 file, got 0o{mode:o}"
        );
        assert!(
            mode & 0o111 != 0,
            "binary in DMG must be executable, got 0o{mode:o}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_stage_binary_into_copies_app_bundle_directory_tree() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();

        // Build a fake `.app` bundle directory tree.
        let app = tmp.path().join("anodizer.app");
        let macos = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let plist = app.join("Contents/Info.plist");
        std::fs::write(&plist, b"<plist></plist>").unwrap();
        let inner_bin = macos.join("anodizer");
        std::fs::write(&inner_bin, b"\x7fELF fake mach-o").unwrap();
        std::fs::set_permissions(&inner_bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        let staged = stage_binary_into(&staging, &app, "anodizer.app", "appbundle").unwrap();
        assert_eq!(staged, staging.join("anodizer.app"));
        assert!(staged.is_dir(), "staged .app must be a directory");

        // The inner binary must be copied with identical bytes.
        let staged_bin = staged.join("Contents/MacOS/anodizer");
        assert!(staged_bin.exists(), "inner binary must be staged");
        assert_eq!(
            std::fs::read(&staged_bin).unwrap(),
            std::fs::read(&inner_bin).unwrap(),
            "inner binary bytes must match source"
        );

        // The executable bit must survive the recursive copy.
        let mode = std::fs::metadata(&staged_bin).unwrap().permissions().mode();
        assert!(
            mode & 0o100 != 0,
            "inner binary must retain user-exec bit, got 0o{:o}",
            mode & 0o777
        );

        // The plist must be present too.
        assert!(
            staged.join("Contents/Info.plist").exists(),
            "Info.plist must be staged"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_stage_binary_into_recreates_symlinks_in_app_bundle() {
        let tmp = tempfile::tempdir().unwrap();

        let app = tmp.path().join("anodizer.app");
        let versions = app.join("Contents/Frameworks/Foo.framework/Versions");
        std::fs::create_dir_all(versions.join("A")).unwrap();
        std::fs::write(versions.join("A/Foo"), b"framework binary").unwrap();
        // Embedded framework version symlink: Current -> A.
        std::os::unix::fs::symlink("A", versions.join("Current")).unwrap();

        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();

        let staged = stage_binary_into(&staging, &app, "anodizer.app", "appbundle").unwrap();
        let staged_link = staged.join("Contents/Frameworks/Foo.framework/Versions/Current");
        let meta = std::fs::symlink_metadata(&staged_link).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "embedded framework symlink must be recreated as a symlink"
        );
        assert_eq!(
            std::fs::read_link(&staged_link).unwrap(),
            std::path::Path::new("A"),
            "symlink target must be preserved"
        );
    }

    /// Two `hdiutil create -format UDZO` runs over identical source content are
    /// NOT byte-identical, and this records WHY so the verdict is observable
    /// before a release rather than hidden behind a determinism allowlist.
    ///
    /// The UDIF "koly" trailer hdiutil writes embeds a per-segment 128-bit GUID
    /// (`SegmentID`) generated fresh per `hdiutil create`, so the same source
    /// folder yields a different image every run. hdiutil exposes no
    /// reproducible/deterministic flag and does not consult `SOURCE_DATE_EPOCH`,
    /// and anodizer invokes it directly with no post-build koly normalization —
    /// so the native `.dmg` is non-reproducible. The assertion below proves the
    /// drift is real (not a flake) and pins the root cause; flipping it to
    /// byte-equality is the regression signal if hdiutil ever ships
    /// deterministic output or anodizer adds koly-trailer normalization. Runs on
    /// the macOS CI test shard only.
    #[test]
    #[cfg(target_os = "macos")]
    fn dmg_is_byte_reproducible_across_time() {
        if !anodizer_core::util::find_binary("hdiutil") {
            eprintln!("hdiutil unavailable; test skipped hermetically");
            return;
        }
        let staging = tempfile::tempdir().unwrap();
        std::fs::write(
            staging.path().join("payload.txt"),
            b"deterministic payload\n",
        )
        .unwrap();

        let build = || -> Option<Vec<u8>> {
            let out = tempfile::tempdir().unwrap();
            let dmg_path = out.path().join("repro.dmg");
            let argv = dmg_command(
                DmgTool::Hdiutil,
                "ReproProbe",
                &staging.path().to_string_lossy(),
                &dmg_path.to_string_lossy(),
            );
            let status = Command::new(&argv[0])
                .args(&argv[1..])
                .env("SOURCE_DATE_EPOCH", "1700000000")
                .output()
                .ok()?;
            status
                .status
                .success()
                .then(|| std::fs::read(&dmg_path).ok())?
        };

        let (Some(a), Some(b)) = (build(), build()) else {
            eprintln!("hdiutil create failed; test skipped hermetically");
            return;
        };
        assert_ne!(
            a, b,
            "hdiutil writes a fresh per-segment SegmentID GUID into the UDIF koly \
             trailer every run; the native .dmg is NOT byte-reproducible and \
             anodizer does not normalize it. If this now matches, hdiutil gained \
             deterministic output or anodizer added koly-trailer normalization — \
             make the .dmg byte-stable and flip this assertion."
        );
    }
}
