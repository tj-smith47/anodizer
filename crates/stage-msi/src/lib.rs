use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;
use anodizer_core::util::{parse_mod_timestamp, set_file_mtime};

// ---------------------------------------------------------------------------
// WiX version detection
// ---------------------------------------------------------------------------

/// WiX toolset version — determines which CLI commands to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WixVersion {
    /// WiX v3: uses `candle` + `light` two-step compilation.
    V3,
    /// WiX v4: uses the unified `wix build` command.
    V4,
}

/// Commands to execute for building an MSI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsiCommands {
    /// The primary build command (V4: `wix build`, V3: `candle`).
    pub primary: Vec<String>,
    /// Optional second step (V3: `light`, V4: None).
    pub link: Option<Vec<String>>,
}

impl WixVersion {
    /// Detect the WiX version from the content of a `.wxs` file.
    ///
    /// - V3: contains the `http://schemas.microsoft.com/wix/2006/wi` namespace.
    /// - V4: contains the `http://wixtoolset.org/schemas/v4/wxs` namespace, or
    ///   no recognized namespace at all (V4 is the default for bare files).
    pub fn detect_from_wxs(content: &str) -> Self {
        if content.contains("http://schemas.microsoft.com/wix/2006/wi") {
            WixVersion::V3
        } else {
            // V4 namespace or no namespace — both default to V4
            WixVersion::V4
        }
    }

    /// Detect the WiX version from installed tools on the system.
    ///
    /// Checks for the `wix` command (V4) first, then `candle` (V3).
    /// Falls back to V4 if neither is found.
    pub fn detect_from_tools() -> Self {
        // Check for V4 first (preferred)
        if anodizer_core::util::find_binary("wix") {
            return WixVersion::V4;
        }
        // Check for V3 toolchain
        if anodizer_core::util::find_binary("candle") && anodizer_core::util::find_binary("light") {
            return WixVersion::V3;
        }
        // Default to V4
        WixVersion::V4
    }

    /// Parse a version string from config (e.g. "v3", "v4", "V3", "V4", "3", "4").
    pub fn from_config_str(s: &str) -> Option<Self> {
        let normalized = s.to_lowercase().trim_start_matches('v').to_string();
        match normalized.as_str() {
            "3" => Some(WixVersion::V3),
            "4" => Some(WixVersion::V4),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// MSI command construction
// ---------------------------------------------------------------------------

/// Construct the WiX CLI commands for building an MSI.
///
/// `extensions` are WiX extension names (e.g. "WixUIExtension") that should
/// already be rendered through the template engine with empty strings filtered.
pub fn msi_command(
    wix_version: WixVersion,
    wxs_path: &str,
    output_path: &str,
    extensions: &[String],
) -> MsiCommands {
    match wix_version {
        WixVersion::V4 => {
            let mut primary = vec![
                "wix".to_string(),
                "build".to_string(),
                wxs_path.to_string(),
                "-o".to_string(),
                output_path.to_string(),
            ];
            for ext in extensions {
                primary.push("-ext".to_string());
                primary.push(ext.clone());
            }
            MsiCommands {
                primary,
                link: None,
            }
        }
        WixVersion::V3 => {
            // Derive the .wixobj path from the output path
            let wixobj_path = if let Some(prefix) = output_path.strip_suffix(".msi") {
                format!("{prefix}.wixobj")
            } else {
                format!("{output_path}.wixobj")
            };
            let mut primary = vec![
                "candle".to_string(),
                "-nologo".to_string(),
                wxs_path.to_string(),
                "-o".to_string(),
                wixobj_path.clone(),
            ];
            for ext in extensions {
                primary.push("-ext".to_string());
                primary.push(ext.clone());
            }
            let mut link = vec![
                "light".to_string(),
                "-nologo".to_string(),
                wixobj_path,
                "-o".to_string(),
                output_path.to_string(),
            ];
            // Behavioral superset of upstream: GoReleaser docs pass `-ext` only
            // to candle. Passing the same extensions to light as well is
            // harmless (WiX ignores unused ones) but avoids link-time
            // "ExtensionRequired" errors for extensions that supply linker
            // transforms. Documented divergence — keep.
            for ext in extensions {
                link.push("-ext".to_string());
                link.push(ext.clone());
            }
            MsiCommands {
                primary,
                link: Some(link),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Architecture mapping
// ---------------------------------------------------------------------------

/// Convert a Go/Rust-style architecture name to the MSI architecture identifier.
///
/// MSI uses "x64", "x86", "arm64" in installer metadata.
pub fn map_arch_to_msi(arch: &str) -> &str {
    match arch {
        "amd64" | "x86_64" => "x64",
        "386" | "i686" | "i386" | "i586" | "x86" => "x86",
        "arm64" | "aarch64" => "arm64",
        _ => arch,
    }
}

// ---------------------------------------------------------------------------
// .wxs template rendering
// ---------------------------------------------------------------------------

/// Read a `.wxs` file and render it through the template engine.
///
/// Template variables like `{{ .ProjectName }}`, `{{ .Version }}`, `{{ .Arch }}`,
/// `{{ .MsiArch }}` etc. are expanded via the Tera engine.
pub fn render_wxs_template(ctx: &Context, wxs_path: &str) -> Result<String> {
    let content = fs::read_to_string(wxs_path)
        .with_context(|| format!("msi: read .wxs template file: {wxs_path}"))?;
    ctx.render_template(&content)
        .with_context(|| format!("msi: render .wxs template: {wxs_path}"))
}

// ---------------------------------------------------------------------------
// Artifact creation helper
// ---------------------------------------------------------------------------

/// Build an MSI `Artifact` and collect archive paths to remove when `replace` is set.
fn make_msi_artifact(
    msi_path: PathBuf,
    target: &Option<String>,
    crate_name: &str,
    wix_version: WixVersion,
    msi_cfg: &anodizer_core::config::MsiConfig,
    ctx: &Context,
    archives_to_remove: &mut Vec<PathBuf>,
) -> Artifact {
    let mut metadata = HashMap::from([
        ("format".to_string(), "msi".to_string()),
        (
            "wix_version".to_string(),
            match wix_version {
                WixVersion::V3 => "v3",
                WixVersion::V4 => "v4",
            }
            .to_string(),
        ),
    ]);
    if let Some(id) = &msi_cfg.id {
        metadata.insert("id".to_string(), id.clone());
    }

    // Handle replace option — collect matching archives for removal
    archives_to_remove.extend(anodizer_core::util::collect_if_replace(
        msi_cfg.replace,
        &ctx.artifacts,
        crate_name,
        target.as_deref(),
    ));

    Artifact {
        kind: ArtifactKind::Installer,
        name: String::new(),
        path: msi_path,
        target: target.clone(),
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    }
}

// ---------------------------------------------------------------------------
// MsiStage
// ---------------------------------------------------------------------------

pub struct MsiStage;

impl Stage for MsiStage {
    fn name(&self) -> &str {
        "msi"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("msi");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have MSI config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.msis.is_some())
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
        let mut archives_to_remove: Vec<PathBuf> = Vec::new();

        for krate in &crates {
            let Some(msi_configs) = krate.msis.as_ref() else {
                continue;
            };

            // Collect all Windows binary artifacts for this crate
            let windows_binaries: Vec<_> = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                .into_iter()
                .filter(|b| {
                    b.target
                        .as_deref()
                        .map(anodizer_core::target::is_windows)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            for msi_cfg in msi_configs {
                let msi_id_for_log = msi_cfg.id.as_deref().unwrap_or("default").to_string();

                if should_skip_msi_config(
                    ctx,
                    msi_cfg,
                    &msi_id_for_log,
                    &krate.name,
                    dry_run,
                    &log,
                )? {
                    continue;
                }

                let Some(effective_binaries) =
                    filter_msi_binaries(msi_cfg, &windows_binaries, &krate.name, &log)
                else {
                    continue;
                };

                // Validate wxs is present
                let wxs_path = msi_cfg.wxs.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "msi: `wxs` field is required but missing for crate {}",
                        krate.name
                    )
                })?;

                for (target, binary_path) in &effective_binaries {
                    let (_os, arch) = target
                        .as_deref()
                        .map(anodizer_core::target::map_target)
                        .unwrap_or_else(|| ("windows".to_string(), "amd64".to_string()));
                    let msi_arch = map_arch_to_msi(&arch).to_string();

                    set_msi_template_vars(ctx, target.as_deref(), &arch, &msi_arch, binary_path);

                    let wix_version = resolve_wix_version(msi_cfg, wxs_path, &log);

                    let output_dir = dist.join("windows");
                    let msi_filename = compute_msi_filename(
                        ctx,
                        msi_cfg,
                        &krate.name,
                        target.as_deref(),
                        &version,
                        &msi_arch,
                    )?;
                    let msi_path = output_dir.join(&msi_filename);

                    let rendered_extensions = render_msi_extensions(ctx, msi_cfg, &log);

                    if dry_run {
                        log_msi_dry_run(
                            &log,
                            &msi_filename,
                            wix_version,
                            &krate.name,
                            target.as_deref(),
                            msi_cfg,
                            &rendered_extensions,
                        );
                        new_artifacts.push(make_msi_artifact(
                            msi_path,
                            target,
                            &krate.name,
                            wix_version,
                            msi_cfg,
                            ctx,
                            &mut archives_to_remove,
                        ));
                        continue;
                    }

                    fs::create_dir_all(&output_dir).with_context(|| {
                        format!("msi: create output dir: {}", output_dir.display())
                    })?;

                    let (tmp_dir, rendered_wxs_path) =
                        prepare_wxs_build_context(ctx, msi_cfg, wxs_path, &log)?;

                    execute_msi_build(
                        wix_version,
                        msi_cfg,
                        &rendered_wxs_path,
                        &msi_path,
                        &rendered_extensions,
                        &krate.name,
                        target.as_deref(),
                        &log,
                    )?;
                    drop(tmp_dir);

                    new_artifacts.push(make_msi_artifact(
                        msi_path,
                        target,
                        &krate.name,
                        wix_version,
                        msi_cfg,
                        ctx,
                        &mut archives_to_remove,
                    ));
                }

                run_msi_hook(
                    ctx,
                    msi_cfg.hooks.as_ref().and_then(|h| h.post.as_ref()),
                    "post-msi",
                    &msi_id_for_log,
                    &krate.name,
                    dry_run,
                    &log,
                )?;
            }
        }

        clear_msi_template_vars(ctx);

        if !archives_to_remove.is_empty() {
            ctx.artifacts.remove_by_paths(&archives_to_remove);
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Private helpers — sliced out of `MsiStage::run` to keep the body short.
// ---------------------------------------------------------------------------

/// Evaluate per-config skip predicates (`if`, `skip`) and run the
/// `hooks.before` / `pre` lifecycle hooks. Returns `Ok(true)` when the
/// caller should `continue` (skip this config).
fn should_skip_msi_config(
    ctx: &mut Context,
    msi_cfg: &anodizer_core::config::MsiConfig,
    msi_id_for_log: &str,
    crate_name: &str,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<bool> {
    if let Some(ref condition) = msi_cfg.if_condition {
        let rendered = ctx.render_template(condition).with_context(|| {
            format!(
                "msi config '{}' for crate '{}': `if` template render failed (expression: {})",
                msi_id_for_log, crate_name, condition
            )
        })?;
        let trimmed = rendered.trim();
        if trimmed.is_empty() || trimmed == "false" {
            log.status(&format!(
                "skipping msi config '{}' for crate {}: if condition evaluated to '{}'",
                msi_id_for_log, crate_name, trimmed
            ));
            return Ok(true);
        }
    }

    if let Some(ref d) = msi_cfg.skip {
        let off = d
            .try_evaluates_to_true(|s| ctx.render_template(s))
            .with_context(|| format!("msi: render skip template for crate {}", crate_name))?;
        if off {
            log.status(&format!("MSI config skipped for crate {}", crate_name));
            return Ok(true);
        }
    }

    run_msi_hook(
        ctx,
        msi_cfg.hooks.as_ref().and_then(|h| h.pre.as_ref()),
        "pre-msi",
        msi_id_for_log,
        crate_name,
        dry_run,
        log,
    )?;

    Ok(false)
}

/// Apply the ids + goamd64 filters to the collected Windows binaries.
/// Returns `Some` with `(target, binary_path)` pairs to drive the per-target
/// build, or `None` when the caller should `continue` (no matching binaries).
fn filter_msi_binaries(
    msi_cfg: &anodizer_core::config::MsiConfig,
    windows_binaries: &[Artifact],
    crate_name: &str,
    log: &anodizer_core::log::StageLogger,
) -> Option<Vec<(Option<String>, String)>> {
    let mut filtered: Vec<&Artifact> = windows_binaries.iter().collect();

    if let Some(ref filter_ids) = msi_cfg.ids
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

    if let Some(ref want) = msi_cfg.goamd64 {
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

    if filtered.is_empty() && windows_binaries.is_empty() {
        log.warn(&format!(
            "no Windows binary artifacts found for crate '{}'; \
             skipping MSI generation (expected binaries targeting windows/msvc)",
            crate_name
        ));
        return None;
    }
    if filtered.is_empty() {
        log.warn(&format!(
            "ids filter {:?} matched no binaries for crate '{}'; skipping",
            msi_cfg.ids, crate_name
        ));
        return None;
    }

    Some(
        filtered
            .into_iter()
            .map(|b| (b.target.clone(), b.path.to_string_lossy().into_owned()))
            .collect(),
    )
}

/// Populate per-binary template variables. `BinaryPath` exposes the path
/// to user `.wxs` templates (I3).
fn set_msi_template_vars(
    ctx: &mut Context,
    target: Option<&str>,
    arch: &str,
    msi_arch: &str,
    binary_path: &str,
) {
    ctx.template_vars_mut().set("Os", "windows");
    ctx.template_vars_mut().set("Arch", arch);
    ctx.template_vars_mut().set("Target", target.unwrap_or(""));
    ctx.template_vars_mut().set("MsiArch", msi_arch);
    ctx.template_vars_mut().set("BinaryPath", binary_path);
}

/// Determine the WiX toolchain version: explicit `version:` config wins,
/// otherwise sniff the `.wxs` namespace, otherwise probe installed tools.
fn resolve_wix_version(
    msi_cfg: &anodizer_core::config::MsiConfig,
    wxs_path: &str,
    log: &anodizer_core::log::StageLogger,
) -> WixVersion {
    let detect_from_wxs_or_tools = || {
        fs::read_to_string(wxs_path)
            .map(|c| WixVersion::detect_from_wxs(&c))
            .unwrap_or_else(|_| WixVersion::detect_from_tools())
    };
    if let Some(ver_str) = &msi_cfg.version {
        WixVersion::from_config_str(ver_str).unwrap_or_else(|| {
            log.status(&format!(
                "unrecognized WiX version '{}', auto-detecting",
                ver_str
            ));
            detect_from_wxs_or_tools()
        })
    } else {
        detect_from_wxs_or_tools()
    }
}

/// Resolve the output `.msi` filename: rendered `name:` template wins
/// (auto-appending `.msi` when absent), otherwise
/// `<ProjectName>_<version>_<msi_arch>.msi`.
fn compute_msi_filename(
    ctx: &mut Context,
    msi_cfg: &anodizer_core::config::MsiConfig,
    crate_name: &str,
    target: Option<&str>,
    version: &str,
    msi_arch: &str,
) -> Result<String> {
    if let Some(name_tmpl) = &msi_cfg.name {
        let rendered = ctx.render_template(name_tmpl).with_context(|| {
            format!(
                "msi: render name template for crate {} target {:?}",
                crate_name, target
            )
        })?;
        if rendered.to_lowercase().ends_with(".msi") {
            Ok(rendered)
        } else {
            Ok(format!("{rendered}.msi"))
        }
    } else {
        let project_name = ctx
            .template_vars()
            .get("ProjectName")
            .cloned()
            .unwrap_or_else(|| crate_name.to_string());
        Ok(format!("{project_name}_{version}_{msi_arch}.msi"))
    }
}

/// Render each `extensions:` template entry through Tera, dropping empties
/// and logging (but not erroring on) per-entry render failures.
fn render_msi_extensions(
    ctx: &mut Context,
    msi_cfg: &anodizer_core::config::MsiConfig,
    log: &anodizer_core::log::StageLogger,
) -> Vec<String> {
    let Some(exts) = msi_cfg.extensions.as_ref() else {
        return Vec::new();
    };
    exts.iter()
        .filter_map(|ext_tmpl| match ctx.render_template(ext_tmpl) {
            Ok(rendered) => {
                let trimmed = rendered.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            }
            Err(e) => {
                log.warn(&format!(
                    "failed to render extension template '{}': {}",
                    ext_tmpl, e
                ));
                None
            }
        })
        .collect()
}

/// Emit the dry-run logging for a planned MSI build: the headline build
/// line, any `mod_timestamp:`, `extra_files:`, and `extensions:` entries
/// that would be applied.
fn log_msi_dry_run(
    log: &anodizer_core::log::StageLogger,
    msi_filename: &str,
    wix_version: WixVersion,
    crate_name: &str,
    target: Option<&str>,
    msi_cfg: &anodizer_core::config::MsiConfig,
    rendered_extensions: &[String],
) {
    log.status(&format!(
        "(dry-run) would build MSI: {} (WiX {:?}) for crate {} target {:?}",
        msi_filename, wix_version, crate_name, target
    ));
    if let Some(ts) = &msi_cfg.mod_timestamp {
        log.status(&format!("(dry-run) would apply mod_timestamp={ts}"));
    }
    if let Some(ref extras) = msi_cfg.extra_files {
        for f in extras {
            log.status(&format!(
                "(dry-run) would copy extra file '{f}' to build context"
            ));
        }
    }
    for ext in rendered_extensions {
        log.status(&format!("(dry-run) would add WiX extension: -ext {ext}"));
    }
}

/// Render the `.wxs` template, write it into a fresh tempdir, copy any
/// configured `extra_files:` next to it, and apply the rendered file's
/// `mod_timestamp:` mtime. Returns the tempdir handle (which must outlive
/// the build) and the path to the rendered `.wxs`.
fn prepare_wxs_build_context(
    ctx: &Context,
    msi_cfg: &anodizer_core::config::MsiConfig,
    wxs_path: &str,
    log: &anodizer_core::log::StageLogger,
) -> Result<(tempfile::TempDir, PathBuf)> {
    let rendered_wxs = render_wxs_template(ctx, wxs_path)?;

    let tmp_dir = tempfile::tempdir().context("msi: create temp dir for .wxs")?;
    let rendered_wxs_path = tmp_dir.path().join("rendered.wxs");
    fs::write(&rendered_wxs_path, &rendered_wxs).with_context(|| {
        format!(
            "msi: write rendered .wxs to {}",
            rendered_wxs_path.display()
        )
    })?;

    if let Some(ref extras) = msi_cfg.extra_files {
        for filename in extras {
            let src = PathBuf::from(filename);
            if !src.exists() {
                anyhow::bail!("msi: extra_file '{}' does not exist", filename);
            }
            let dest_name = src
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new(filename));
            let dest = tmp_dir.path().join(dest_name);
            fs::copy(&src, &dest).with_context(|| {
                format!(
                    "msi: copy extra file '{}' to build context '{}'",
                    filename,
                    dest.display()
                )
            })?;
            log.status(&format!(
                "copied extra file '{}' to build context",
                filename
            ));
        }
    }

    if let Some(ts) = &msi_cfg.mod_timestamp {
        log.status(&format!("applying mod_timestamp={ts} to rendered .wxs"));
        let mtime = parse_mod_timestamp(ts)?;
        set_file_mtime(&rendered_wxs_path, mtime)?;
    }

    Ok((tmp_dir, rendered_wxs_path))
}

/// Compose and execute the WiX build commands (primary + optional link
/// step for v3), then apply `mod_timestamp:` to the resulting `.msi`. The
/// `-d BindTimestamp=<ts>` flag is appended for v4 builds; v3 logs the
/// limitation but otherwise mtime-stamps the same way.
#[allow(clippy::too_many_arguments)]
fn execute_msi_build(
    wix_version: WixVersion,
    msi_cfg: &anodizer_core::config::MsiConfig,
    rendered_wxs_path: &std::path::Path,
    msi_path: &std::path::Path,
    rendered_extensions: &[String],
    crate_name: &str,
    target: Option<&str>,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let mut commands = msi_command(
        wix_version,
        &rendered_wxs_path.to_string_lossy(),
        &msi_path.to_string_lossy(),
        rendered_extensions,
    );

    if let Some(ts) = &msi_cfg.mod_timestamp {
        match wix_version {
            WixVersion::V4 => {
                commands.primary.push("-d".to_string());
                commands.primary.push(format!("BindTimestamp={ts}"));
            }
            WixVersion::V3 => {
                log.status(&format!(
                    "note: mod_timestamp={ts} noted; WiX v3 has limited \
                     timestamp support (applied to .wxs and output .msi)"
                ));
            }
        }
    }

    log.status(&format!("running: {}", commands.primary.join(" ")));
    let output = Command::new(&commands.primary[0])
        .args(&commands.primary[1..])
        .output()
        .with_context(|| {
            format!(
                "msi: execute {} for crate {} target {:?}",
                commands.primary[0], crate_name, target
            )
        })?;
    log.check_output(output, &commands.primary[0])?;

    if let Some(link_cmd) = &commands.link {
        log.status(&format!("running: {}", link_cmd.join(" ")));
        let output = Command::new(&link_cmd[0])
            .args(&link_cmd[1..])
            .output()
            .with_context(|| {
                format!(
                    "msi: execute {} for crate {} target {:?}",
                    link_cmd[0], crate_name, target
                )
            })?;
        log.check_output(output, &link_cmd[0])?;
    }

    if let Some(ts) = &msi_cfg.mod_timestamp
        && msi_path.exists()
    {
        let mtime = parse_mod_timestamp(ts)?;
        set_file_mtime(msi_path, mtime)?;
        log.status(&format!(
            "applied mod_timestamp={ts} to {}",
            msi_path.display()
        ));
    }

    Ok(())
}

/// Run a single pre- or post-MSI hook chain with the current template-var
/// snapshot. `kind` is either "pre-msi" or "post-msi" and surfaces in the
/// error context (and is forwarded to the underlying hook runner).
fn run_msi_hook(
    ctx: &Context,
    hook: Option<&Vec<anodizer_core::config::HookEntry>>,
    kind: &'static str,
    msi_id_for_log: &str,
    crate_name: &str,
    dry_run: bool,
    log: &anodizer_core::log::StageLogger,
) -> Result<()> {
    let Some(hook) = hook else {
        return Ok(());
    };
    let tmpl_vars = ctx.template_vars().clone();
    anodizer_core::hooks::run_hooks(hook, kind, dry_run, log, Some(&tmpl_vars)).with_context(|| {
        format!(
            "msi config '{}' for crate '{}': {} hooks failed",
            msi_id_for_log, crate_name, kind
        )
    })
}

/// Clear the per-target template vars on stage exit so they don't leak
/// into downstream stages (`announce`, `publish`).
fn clear_msi_template_vars(ctx: &mut Context) {
    ctx.template_vars_mut().set("Os", "");
    ctx.template_vars_mut().set("Arch", "");
    ctx.template_vars_mut().set("Target", "");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // WiX version detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_wix_v3_from_wxs() {
        let wxs = r#"<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://schemas.microsoft.com/wix/2006/wi">
  <Product Id="*" Name="MyApp" Version="1.0.0" />
</Wix>"#;
        assert_eq!(WixVersion::detect_from_wxs(wxs), WixVersion::V3);
    }

    #[test]
    fn test_detect_wix_v4_from_wxs() {
        let wxs = r#"<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">
  <Package Name="MyApp" Version="1.0.0" />
</Wix>"#;
        assert_eq!(WixVersion::detect_from_wxs(wxs), WixVersion::V4);
    }

    #[test]
    fn test_detect_wix_default_v4() {
        // No recognized namespace — defaults to V4
        let wxs = r#"<?xml version="1.0" encoding="UTF-8"?>
<Wix>
  <Package Name="MyApp" Version="1.0.0" />
</Wix>"#;
        assert_eq!(WixVersion::detect_from_wxs(wxs), WixVersion::V4);

        // Completely unrelated content also defaults to V4
        assert_eq!(
            WixVersion::detect_from_wxs("some random content"),
            WixVersion::V4
        );

        // Empty content defaults to V4
        assert_eq!(WixVersion::detect_from_wxs(""), WixVersion::V4);
    }

    // -----------------------------------------------------------------------
    // MSI command construction tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_msi_command_v4() {
        let cmds = msi_command(WixVersion::V4, "/tmp/app.wxs", "/out/app.msi", &[]);
        assert_eq!(
            cmds.primary,
            vec!["wix", "build", "/tmp/app.wxs", "-o", "/out/app.msi"]
        );
        assert!(cmds.link.is_none());
    }

    #[test]
    fn test_msi_command_v3() {
        let cmds = msi_command(WixVersion::V3, "/tmp/app.wxs", "/out/app.msi", &[]);
        assert_eq!(
            cmds.primary,
            vec!["candle", "-nologo", "/tmp/app.wxs", "-o", "/out/app.wixobj"]
        );
        let link = cmds.link.unwrap();
        assert_eq!(
            link,
            vec!["light", "-nologo", "/out/app.wixobj", "-o", "/out/app.msi"]
        );
    }

    // -----------------------------------------------------------------------
    // Architecture mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_arch_to_msi() {
        // amd64 variants -> x64
        assert_eq!(map_arch_to_msi("amd64"), "x64");
        assert_eq!(map_arch_to_msi("x86_64"), "x64");

        // 32-bit variants -> x86
        assert_eq!(map_arch_to_msi("386"), "x86");
        assert_eq!(map_arch_to_msi("i686"), "x86");
        assert_eq!(map_arch_to_msi("i386"), "x86");
        assert_eq!(map_arch_to_msi("x86"), "x86");

        // arm64 variants -> arm64
        assert_eq!(map_arch_to_msi("arm64"), "arm64");
        assert_eq!(map_arch_to_msi("aarch64"), "arm64");

        // Unknown -> passthrough
        assert_eq!(map_arch_to_msi("riscv64"), "riscv64");
    }

    // -----------------------------------------------------------------------
    // Stage behavior tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_skips_when_no_msi_config() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = MsiStage;
        assert!(stage.run(&mut ctx).is_ok());
        // No artifacts should be registered
        assert!(ctx.artifacts.by_kind(ArtifactKind::Installer).is_empty());
    }

    #[test]
    fn test_stage_skips_when_disabled() {
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Write a dummy .wxs file
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // No artifacts because config is disabled
        assert!(ctx.artifacts.by_kind(ArtifactKind::Installer).is_empty());
    }

    #[test]
    fn test_stage_dry_run_registers_artifacts() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Write a dummy .wxs file
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // Register a Windows binary artifact so the stage picks it up
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
        assert_eq!(installers[0].kind, ArtifactKind::Installer);
        assert_eq!(installers[0].crate_name, "myapp");
        assert_eq!(
            installers[0].metadata.get("format"),
            Some(&"msi".to_string())
        );
        assert!(
            installers[0]
                .path
                .to_string_lossy()
                .contains("myapp_1.0.0_x64.msi")
        );
        assert_eq!(
            installers[0].target,
            Some("x86_64-pc-windows-msvc".to_string())
        );
    }

    #[test]
    fn test_stage_dry_run_with_name_template() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            name: Some("{{ .ProjectName }}-{{ .Version }}-{{ .MsiArch }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // Register a Windows binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("aarch64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);

        let path_str = installers[0].path.file_name().unwrap().to_str().unwrap();
        assert_eq!(path_str, "myapp-2.5.0-arm64.msi");
    }

    #[test]
    fn test_stage_errors_without_wxs() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Config with no wxs field
        let msi_cfg = MsiConfig {
            wxs: None,
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // Register a Windows binary so the stage doesn't skip before wxs check
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("wxs") && err.contains("required"),
            "error should mention wxs is required, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Config parsing roundtrip tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_msi() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(msis.len(), 1);
        assert_eq!(msis[0].wxs.as_deref(), Some("app.wxs"));
        assert!(msis[0].name.is_none());
        assert!(msis[0].version.is_none());
        assert!(msis[0].replace.is_none());
        assert!(msis[0].skip.is_none());
    }

    #[test]
    fn test_config_parse_msi_full() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - id: my-msi
        ids:
          - build-win-amd64
        wxs: installer/app.wxs
        name: "myapp-{{ .Version }}-{{ .MsiArch }}"
        version: v4
        replace: true
        mod_timestamp: "2024-01-01T00:00:00Z"
        skip: false
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(msis.len(), 1);

        let msi = &msis[0];
        assert_eq!(msi.id.as_deref(), Some("my-msi"));
        assert_eq!(msi.ids.as_ref().unwrap(), &["build-win-amd64".to_string()]);
        assert_eq!(msi.wxs.as_deref(), Some("installer/app.wxs"));
        assert_eq!(
            msi.name.as_deref(),
            Some("myapp-{{ .Version }}-{{ .MsiArch }}")
        );
        assert_eq!(msi.version.as_deref(), Some("v4"));
        assert_eq!(msi.replace, Some(true));
        assert_eq!(msi.mod_timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(
            msi.skip,
            Some(anodizer_core::config::StringOrBool::Bool(false))
        );
    }

    // -----------------------------------------------------------------------
    // WXS template rendering test
    // -----------------------------------------------------------------------

    #[test]
    fn test_wxs_template_rendering() {
        use anodizer_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let wxs_content = r#"<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">
  <Package Name="{{ .ProjectName }}" Version="{{ .Version }}" Manufacturer="Test">
    <File Source="{{ .ProjectName }}.exe" />
  </Package>
</Wix>"#;

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, wxs_content).unwrap();

        let ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v3.0.0")
            .build();

        let rendered = render_wxs_template(&ctx, &wxs_path.to_string_lossy()).unwrap();
        assert!(rendered.contains("Name=\"myapp\""));
        assert!(rendered.contains("Version=\"3.0.0\""));
        assert!(rendered.contains("Source=\"myapp.exe\""));
        // Original template vars should be expanded
        assert!(!rendered.contains("{{ .ProjectName }}"));
        assert!(!rendered.contains("{{ .Version }}"));
    }

    // -----------------------------------------------------------------------
    // Invalid template error test
    // -----------------------------------------------------------------------

    #[test]
    fn test_invalid_name_template_errors() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
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
            msis: Some(vec![msi_cfg]),
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

        // Register a Windows binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let result = MsiStage.run(&mut ctx);
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

    // -----------------------------------------------------------------------
    // WiX version config string parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_wix_version_from_config_str() {
        assert_eq!(WixVersion::from_config_str("v3"), Some(WixVersion::V3));
        assert_eq!(WixVersion::from_config_str("v4"), Some(WixVersion::V4));
        assert_eq!(WixVersion::from_config_str("V3"), Some(WixVersion::V3));
        assert_eq!(WixVersion::from_config_str("V4"), Some(WixVersion::V4));
        assert_eq!(WixVersion::from_config_str("3"), Some(WixVersion::V3));
        assert_eq!(WixVersion::from_config_str("4"), Some(WixVersion::V4));
        assert_eq!(WixVersion::from_config_str("v5"), None);
        assert_eq!(WixVersion::from_config_str("invalid"), None);
    }

    // -----------------------------------------------------------------------
    // Replace option removes archives
    // -----------------------------------------------------------------------

    #[test]
    fn test_replace_removes_archive_artifacts() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
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
            msis: Some(vec![msi_cfg]),
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

        // Register a Windows binary and a corresponding archive
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp_1.0.0_windows_amd64.zip"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "zip".to_string())]),
            size: None,
        });

        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Archive).len(), 1);

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // Archive should have been removed
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Archive).len(), 0);
        // MSI artifact should exist
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Installer).len(), 1);
    }

    // -----------------------------------------------------------------------
    // No binaries — warns and skips (I1)
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_skips_with_warning_when_no_binaries() {
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // No binary artifacts registered — should skip with warning, not create synthetic
        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // No installer artifacts should be produced
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert!(
            installers.is_empty(),
            "expected no installers when no Windows binaries exist, got {}",
            installers.len()
        );
    }

    // -----------------------------------------------------------------------
    // ids filtering (C2)
    // -----------------------------------------------------------------------

    #[test]
    fn test_ids_filtering_retains_matching_binaries() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ids: Some(vec!["build-win-amd64".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // Register two Windows binaries with different ids
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp-amd64.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-win-amd64".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp-arm64.exe"),
            target: Some("aarch64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-win-arm64".to_string())]),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(
            installers.len(),
            1,
            "ids filter should keep only one binary"
        );
        assert_eq!(
            installers[0].target,
            Some("x86_64-pc-windows-msvc".to_string())
        );
    }

    #[test]
    fn test_ids_filtering_skips_when_no_match() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ids: Some(vec!["nonexistent-id".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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
            metadata: HashMap::from([("id".to_string(), "build-win-amd64".to_string())]),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert!(
            installers.is_empty(),
            "non-matching ids should produce no installers"
        );
    }

    // -----------------------------------------------------------------------
    // S2: id stored in artifact metadata
    // -----------------------------------------------------------------------

    #[test]
    fn test_artifact_stores_config_id_in_metadata() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            id: Some("my-msi-id".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
        assert_eq!(
            installers[0].metadata.get("id"),
            Some(&"my-msi-id".to_string()),
            "artifact metadata should contain the config id"
        );
    }

    // -----------------------------------------------------------------------
    // Multiple MSI configs per crate
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_dry_run_multiple_configs() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Two MSI configs with different wxs files
        let wxs_path_a = tmp.path().join("a.wxs");
        let wxs_path_b = tmp.path().join("b.wxs");
        fs::write(&wxs_path_a, "<Wix/>").unwrap();
        fs::write(&wxs_path_b, "<Wix/>").unwrap();

        let msi_cfg_a = MsiConfig {
            wxs: Some(wxs_path_a.to_string_lossy().into_owned()),
            id: Some("msi-a".to_string()),
            name: Some("myapp-a-{{ .MsiArch }}".to_string()),
            ..Default::default()
        };
        let msi_cfg_b = MsiConfig {
            wxs: Some(wxs_path_b.to_string_lossy().into_owned()),
            id: Some("msi-b".to_string()),
            name: Some("myapp-b-{{ .MsiArch }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg_a, msi_cfg_b]),
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

        // Add a Windows binary artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // Verify both produce MSI artifacts
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(
            installers.len(),
            2,
            "two MSI configs should produce two installer artifacts"
        );

        let ids: Vec<_> = installers
            .iter()
            .filter_map(|a| a.metadata.get("id").cloned())
            .collect();
        assert!(ids.contains(&"msi-a".to_string()));
        assert!(ids.contains(&"msi-b".to_string()));

        let filenames: Vec<_> = installers
            .iter()
            .map(|a| a.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert!(filenames.contains(&"myapp-a-x64.msi".to_string()));
        assert!(filenames.contains(&"myapp-b-x64.msi".to_string()));
    }

    // -----------------------------------------------------------------------
    // mod_timestamp adds -d BindTimestamp for V4
    // -----------------------------------------------------------------------

    #[test]
    fn test_mod_timestamp_adds_bind_timestamp_v4() {
        // Build commands for V4 with mod_timestamp should include -d BindTimestamp=...
        let mut commands = msi_command(WixVersion::V4, "/tmp/app.wxs", "/out/app.msi", &[]);
        let ts = "1704067200";
        // Simulate what the stage does for V4 with mod_timestamp
        commands.primary.push("-d".to_string());
        commands.primary.push(format!("BindTimestamp={ts}"));

        assert!(
            commands.primary.contains(&"-d".to_string()),
            "V4 command should have -d flag"
        );
        assert!(
            commands
                .primary
                .contains(&"BindTimestamp=1704067200".to_string()),
            "V4 command should have BindTimestamp value"
        );

        // Verify the full command looks correct
        assert_eq!(
            commands.primary,
            vec![
                "wix",
                "build",
                "/tmp/app.wxs",
                "-o",
                "/out/app.msi",
                "-d",
                "BindTimestamp=1704067200"
            ]
        );

        // V3 should NOT get -d BindTimestamp
        let v3_commands = msi_command(WixVersion::V3, "/tmp/app.wxs", "/out/app.msi", &[]);
        assert!(
            !v3_commands.primary.contains(&"-d".to_string()),
            "V3 command should not have -d flag"
        );
    }

    // -----------------------------------------------------------------------
    // BinaryPath template variable is set
    // -----------------------------------------------------------------------

    #[test]
    fn test_binary_path_template_var_set() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Write a .wxs that uses {{ .BinaryPath }}
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        let binary_path_str = "dist/myapp.exe";
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from(binary_path_str),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // After run, the BinaryPath template var should have been set to the
        // last processed binary's path
        let bp = ctx.template_vars().get("BinaryPath").cloned();
        assert_eq!(
            bp,
            Some(binary_path_str.to_string()),
            "BinaryPath template variable should be set to the binary's path"
        );
    }

    // -----------------------------------------------------------------------
    // extra_files config parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_extra_files() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
        extra_files:
          - README.md
          - LICENSE
          - doc/guide.pdf
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        let extras = msis[0].extra_files.as_ref().unwrap();
        assert_eq!(extras.len(), 3);
        assert_eq!(extras[0], "README.md");
        assert_eq!(extras[1], "LICENSE");
        assert_eq!(extras[2], "doc/guide.pdf");
    }

    // -----------------------------------------------------------------------
    // extensions config parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_extensions() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
        extensions:
          - WixUIExtension
          - WixUtilExtension
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        let exts = msis[0].extensions.as_ref().unwrap();
        assert_eq!(exts.len(), 2);
        assert_eq!(exts[0], "WixUIExtension");
        assert_eq!(exts[1], "WixUtilExtension");
    }

    // -----------------------------------------------------------------------
    // disable as StringOrBool
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_disable_bool_true() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
        skip: true
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(
            msis[0].skip,
            Some(anodizer_core::config::StringOrBool::Bool(true))
        );
    }

    #[test]
    fn test_config_parse_disable_string_true() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
        skip: "true"
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(
            msis[0].skip,
            Some(anodizer_core::config::StringOrBool::String(
                "true".to_string()
            ))
        );
    }

    #[test]
    fn test_config_parse_disable_template_string() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
        skip: "{{ .Env.SKIP_MSI }}"
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(
            msis[0].skip,
            Some(anodizer_core::config::StringOrBool::String(
                "{{ .Env.SKIP_MSI }}".to_string()
            ))
        );
    }

    #[test]
    fn test_stage_disable_with_string_true() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
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
            msis: Some(vec![msi_cfg]),
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
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // skip: "true" should skip
        assert!(ctx.artifacts.by_kind(ArtifactKind::Installer).is_empty());
    }

    // -----------------------------------------------------------------------
    // extensions passed to WiX commands
    // -----------------------------------------------------------------------

    #[test]
    fn test_msi_command_v4_with_extensions() {
        let exts = vec!["WixUIExtension".to_string(), "WixUtilExtension".to_string()];
        let cmds = msi_command(WixVersion::V4, "/tmp/app.wxs", "/out/app.msi", &exts);
        assert_eq!(
            cmds.primary,
            vec![
                "wix",
                "build",
                "/tmp/app.wxs",
                "-o",
                "/out/app.msi",
                "-ext",
                "WixUIExtension",
                "-ext",
                "WixUtilExtension",
            ]
        );
        assert!(cmds.link.is_none());
    }

    #[test]
    fn test_msi_command_v3_with_extensions() {
        let exts = vec!["WixUIExtension".to_string()];
        let cmds = msi_command(WixVersion::V3, "/tmp/app.wxs", "/out/app.msi", &exts);

        // candle gets -ext too
        assert_eq!(
            cmds.primary,
            vec![
                "candle",
                "-nologo",
                "/tmp/app.wxs",
                "-o",
                "/out/app.wixobj",
                "-ext",
                "WixUIExtension",
            ]
        );

        // light also gets -ext
        let link = cmds.link.unwrap();
        assert_eq!(
            link,
            vec![
                "light",
                "-nologo",
                "/out/app.wixobj",
                "-o",
                "/out/app.msi",
                "-ext",
                "WixUIExtension",
            ]
        );
    }

    // -----------------------------------------------------------------------
    // extra_files copied to build context (live mode)
    // -----------------------------------------------------------------------

    #[test]
    fn test_extra_files_copied_to_build_context() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Create extra files
        let readme_path = tmp.path().join("README.md");
        let license_path = tmp.path().join("LICENSE");
        fs::write(&readme_path, "# My App").unwrap();
        fs::write(&license_path, "MIT License").unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            extra_files: Some(vec![
                readme_path.to_string_lossy().into_owned(),
                license_path.to_string_lossy().into_owned(),
            ]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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
            metadata: HashMap::new(),
            size: None,
        });

        // In dry-run mode, extra_files are only logged, not copied.
        // We verify the config is accepted and the stage runs successfully.
        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(
            installers.len(),
            1,
            "should produce MSI artifact even with extra_files"
        );
    }

    // -----------------------------------------------------------------------
    // extensions dry-run with template rendering
    // -----------------------------------------------------------------------

    #[test]
    fn test_extensions_in_dry_run() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            extensions: Some(vec![
                "WixUIExtension".to_string(),
                "WixUtilExtension".to_string(),
            ]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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
            metadata: HashMap::new(),
            size: None,
        });

        // Should succeed — extensions are logged in dry-run mode
        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Full config roundtrip with new fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_msi_full_with_new_fields() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - id: my-msi
        ids:
          - build-win-amd64
        wxs: installer/app.wxs
        name: "myapp-{{ .Version }}-{{ .MsiArch }}"
        version: v4
        replace: true
        mod_timestamp: "2024-01-01T00:00:00Z"
        skip: "{{ .Env.SKIP_MSI }}"
        extra_files:
          - README.md
          - LICENSE
        extensions:
          - WixUIExtension
          - WixUtilExtension
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msi = &config.crates[0].msis.as_ref().unwrap()[0];

        assert_eq!(msi.id.as_deref(), Some("my-msi"));
        assert_eq!(msi.ids.as_ref().unwrap(), &["build-win-amd64".to_string()]);
        assert_eq!(msi.wxs.as_deref(), Some("installer/app.wxs"));
        assert_eq!(
            msi.name.as_deref(),
            Some("myapp-{{ .Version }}-{{ .MsiArch }}")
        );
        assert_eq!(msi.version.as_deref(), Some("v4"));
        assert_eq!(msi.replace, Some(true));
        assert_eq!(msi.mod_timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(
            msi.skip,
            Some(anodizer_core::config::StringOrBool::String(
                "{{ .Env.SKIP_MSI }}".to_string()
            ))
        );
        assert_eq!(
            msi.extra_files.as_ref().unwrap(),
            &["README.md".to_string(), "LICENSE".to_string()]
        );
        assert_eq!(
            msi.extensions.as_ref().unwrap(),
            &["WixUIExtension".to_string(), "WixUtilExtension".to_string()]
        );
    }

    // --- `msi.if` + `msi.hooks` (GoReleaser Pro) ---

    fn msi_test_ctx_with_if(if_expr: Option<&str>) -> anodizer_core::context::Context {
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        std::fs::create_dir_all(&config.dist).unwrap();
        let msi_cfg = MsiConfig {
            wxs: Some("dummy.wxs".to_string()),
            if_condition: if_expr.map(str::to_string),
            ..Default::default()
        };
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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
        ctx.template_vars_mut().set("Os", "windows");
        ctx
    }

    #[test]
    fn test_msi_if_false_skips_config() {
        let mut ctx = msi_test_ctx_with_if(Some("false"));
        MsiStage.run(&mut ctx).unwrap();
        assert_eq!(
            ctx.artifacts.by_kind(ArtifactKind::Installer).len(),
            0,
            "msi if=false should skip"
        );
    }

    #[test]
    fn test_msi_if_render_failure_is_hard_error() {
        let mut ctx = msi_test_ctx_with_if(Some("{{ undefined_function 42 }}"));
        let err = MsiStage
            .run(&mut ctx)
            .expect_err("unrenderable `if` should hard-error");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("`if` template render failed"),
            "error should name `if` render failure, got: {msg}"
        );
    }

    #[test]
    fn test_config_parse_msi_hooks_pre_post() {
        // BuildHooksConfig uses `pre:` / `post:` (matching GoReleaser's build pipe).
        use anodizer_core::config::Config;
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: installer.wxs
        hooks:
          pre:
            - echo pre-msi-build
          post:
            - echo post-msi-build
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        let hooks = msis[0].hooks.as_ref().unwrap();
        let pre = hooks
            .pre
            .as_ref()
            .expect("`pre:` yaml should populate `pre`");
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0], "echo pre-msi-build");
        let post = hooks
            .post
            .as_ref()
            .expect("`post:` yaml should populate `post`");
        assert_eq!(post.len(), 1);
        assert_eq!(post[0], "echo post-msi-build");
    }

    // -------------------------------------------------------------------
    // M8 — `msi.goamd64` filter (GR Pro `msi.goamd64: string`)
    // -------------------------------------------------------------------

    /// Build a context with three windows/amd64 binaries (variants v1/v2/v3)
    /// plus one windows/arm64 binary. The `goamd64` field on the config drives
    /// which subset of amd64 binaries reaches `Installer` artifact creation.
    fn msi_goamd64_test_ctx(goamd64: Option<&str>) -> anodizer_core::context::Context {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            goamd64: goamd64.map(str::to_string),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // 3 amd64 variants — same target triple, different `amd64_variant`
        // metadata. Path differs so artifact-add doesn't dedup.
        for variant in ["v1", "v2", "v3"] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from(format!("dist/myapp_{variant}.exe")),
                target: Some("x86_64-pc-windows-msvc".to_string()),
                crate_name: "myapp".to_string(),
                metadata: HashMap::from([("amd64_variant".to_string(), variant.to_string())]),
                size: None,
            });
        }
        // arm64 binary — outside the amd64 filter's scope.
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_arm.exe"),
            target: Some("aarch64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx
    }

    #[test]
    fn test_msi_goamd64_unset_passes_all_amd64_variants() {
        let mut ctx = msi_goamd64_test_ctx(None);
        MsiStage.run(&mut ctx).unwrap();
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        // 3 amd64 binaries + 1 arm64 binary -> 4 MSIs (one per binary path).
        assert_eq!(
            installers.len(),
            4,
            "unset goamd64 should pass every amd64 variant + non-amd64"
        );
    }

    #[test]
    fn test_msi_goamd64_v3_only_keeps_matching_variant() {
        let mut ctx = msi_goamd64_test_ctx(Some("v3"));
        MsiStage.run(&mut ctx).unwrap();
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        // Only v3 amd64 + arm64 (always passes) -> 2 MSIs.
        assert_eq!(installers.len(), 2);
        let targets: Vec<&str> = installers
            .iter()
            .map(|a| a.target.as_deref().unwrap())
            .collect();
        assert!(targets.contains(&"x86_64-pc-windows-msvc"));
        assert!(targets.contains(&"aarch64-pc-windows-msvc"));
    }

    #[test]
    fn test_msi_goamd64_filter_does_not_drop_arm64() {
        // Pin: filter only constrains amd64. arm64 must still pass even
        // when no amd64 variant matches.
        let mut ctx = msi_goamd64_test_ctx(Some("v9000"));
        MsiStage.run(&mut ctx).unwrap();
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
        assert_eq!(
            installers[0].target.as_deref(),
            Some("aarch64-pc-windows-msvc")
        );
    }
}
