use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anodize_core::util::{parse_mod_timestamp, set_file_mtime};

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
        if anodize_core::util::find_binary("wix") {
            return WixVersion::V4;
        }
        // Check for V3 toolchain
        if anodize_core::util::find_binary("candle") && anodize_core::util::find_binary("light") {
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
    msi_cfg: &anodize_core::config::MsiConfig,
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
    archives_to_remove.extend(anodize_core::util::collect_if_replace(
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
                        .map(anodize_core::target::is_windows)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            for msi_cfg in msi_configs {
                let msi_id_for_log = msi_cfg.id.as_deref().unwrap_or("default").to_string();

                // GoReleaser Pro `msi.if`: template-conditional skip (opt-in).
                // Rendered "false"/empty => skip; render error => hard bail (W1 avoidance).
                if let Some(ref condition) = msi_cfg.if_condition {
                    let rendered = ctx.render_template(condition).with_context(|| {
                        format!(
                            "msi config '{}' for crate '{}': `if` template render failed (expression: {})",
                            msi_id_for_log, krate.name, condition
                        )
                    })?;
                    let trimmed = rendered.trim();
                    if trimmed.is_empty() || trimmed == "false" {
                        log.status(&format!(
                            "skipping msi config '{}' for crate {}: if condition evaluated to '{}'",
                            msi_id_for_log, krate.name, trimmed
                        ));
                        continue;
                    }
                }

                // Skip disabled configs (supports bool or template string)
                if let Some(ref d) = msi_cfg.disable
                    && d.is_disabled(|s| ctx.render_template(s))
                {
                    log.status(&format!(
                        "skipping disabled MSI config for crate {}",
                        krate.name
                    ));
                    continue;
                }

                // GoReleaser Pro `msi.hooks.before` (alias `pre`): run once per MSI
                // config before any artifacts are built. Hard-errors on hook failure.
                if let Some(pre) = msi_cfg.hooks.as_ref().and_then(|h| h.pre.as_ref()) {
                    let tmpl_vars = ctx.template_vars().clone();
                    anodize_core::hooks::run_hooks(pre, "pre-msi", dry_run, &log, Some(&tmpl_vars))
                        .with_context(|| {
                            format!(
                                "msi config '{}' for crate '{}': pre-msi hooks failed",
                                msi_id_for_log, krate.name
                            )
                        })?;
                }

                // C2: Apply ids filtering
                let mut filtered = windows_binaries.clone();
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

                // I1: Warn instead of silently creating synthetic binary
                if filtered.is_empty() && windows_binaries.is_empty() {
                    log.warn(&format!(
                        "no Windows binary artifacts found for crate '{}'; \
                         skipping MSI generation (expected binaries targeting windows/msvc)",
                        krate.name
                    ));
                    continue;
                }
                if filtered.is_empty() {
                    log.warn(&format!(
                        "ids filter {:?} matched no binaries for crate '{}'; skipping",
                        msi_cfg.ids, krate.name
                    ));
                    continue;
                }

                let effective_binaries: Vec<(Option<String>, String)> = filtered
                    .iter()
                    .map(|b| (b.target.clone(), b.path.to_string_lossy().into_owned()))
                    .collect();

                // Validate wxs is present
                let wxs_path = msi_cfg.wxs.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "msi: `wxs` field is required but missing for crate {}",
                        krate.name
                    )
                })?;

                for (target, binary_path) in &effective_binaries {
                    // Derive Os/Arch from the target triple
                    let (_os, arch) = target
                        .as_deref()
                        .map(anodize_core::target::map_target)
                        .unwrap_or_else(|| ("windows".to_string(), "amd64".to_string()));

                    let msi_arch = map_arch_to_msi(&arch).to_string();

                    // Set template vars for this binary
                    ctx.template_vars_mut().set("Os", "windows");
                    ctx.template_vars_mut().set("Arch", &arch);
                    ctx.template_vars_mut()
                        .set("Target", target.as_deref().unwrap_or(""));
                    ctx.template_vars_mut().set("MsiArch", &msi_arch);

                    // I3: Expose binary path as template variable
                    ctx.template_vars_mut().set("BinaryPath", binary_path);

                    // Determine WiX version
                    let wix_version = if let Some(ver_str) = &msi_cfg.version {
                        WixVersion::from_config_str(ver_str).unwrap_or_else(|| {
                            log.status(&format!(
                                "unrecognized WiX version '{}', auto-detecting",
                                ver_str
                            ));
                            // Try reading .wxs content for detection, fall back to tools
                            fs::read_to_string(wxs_path)
                                .map(|c| WixVersion::detect_from_wxs(&c))
                                .unwrap_or_else(|_| WixVersion::detect_from_tools())
                        })
                    } else {
                        // Auto-detect: try .wxs content first, then tools
                        fs::read_to_string(wxs_path)
                            .map(|c| WixVersion::detect_from_wxs(&c))
                            .unwrap_or_else(|_| WixVersion::detect_from_tools())
                    };

                    // Determine output filename
                    let output_dir = dist.join("windows");
                    let msi_filename = if let Some(name_tmpl) = &msi_cfg.name {
                        let rendered = ctx.render_template(name_tmpl).with_context(|| {
                            format!(
                                "msi: render name template for crate {} target {:?}",
                                krate.name, target
                            )
                        })?;
                        // Ensure .msi extension (case-insensitive)
                        if rendered.to_lowercase().ends_with(".msi") {
                            rendered
                        } else {
                            format!("{rendered}.msi")
                        }
                    } else {
                        format!(
                            "{}_{}_{}",
                            ctx.template_vars()
                                .get("ProjectName")
                                .cloned()
                                .unwrap_or_else(|| krate.name.clone()),
                            version,
                            msi_arch
                        ) + ".msi"
                    };
                    let msi_path = output_dir.join(&msi_filename);

                    // Render WiX extensions through the template engine, filtering
                    // out empty strings (templates that evaluate to nothing).
                    let rendered_extensions: Vec<String> =
                        if let Some(ref exts) = msi_cfg.extensions {
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
                        } else {
                            Vec::new()
                        };

                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would build MSI: {} (WiX {:?}) for crate {} target {:?}",
                            msi_filename, wix_version, krate.name, target
                        ));

                        // C3: Log mod_timestamp in dry-run mode
                        if let Some(ts) = &msi_cfg.mod_timestamp {
                            log.status(&format!("(dry-run) would apply mod_timestamp={ts}"));
                        }

                        // Log extra_files in dry-run mode
                        if let Some(ref extras) = msi_cfg.extra_files {
                            for f in extras {
                                log.status(&format!(
                                    "(dry-run) would copy extra file '{f}' to build context"
                                ));
                            }
                        }

                        // Log extensions in dry-run mode
                        for ext in &rendered_extensions {
                            log.status(&format!("(dry-run) would add WiX extension: -ext {ext}"));
                        }

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

                    // Live mode: create output directory
                    fs::create_dir_all(&output_dir).with_context(|| {
                        format!("msi: create output dir: {}", output_dir.display())
                    })?;

                    // Read and render the .wxs template
                    let rendered_wxs = render_wxs_template(ctx, wxs_path)?;

                    // Write rendered .wxs to temp dir
                    let tmp_dir = tempfile::tempdir().context("msi: create temp dir for .wxs")?;
                    let rendered_wxs_path = tmp_dir.path().join("rendered.wxs");
                    fs::write(&rendered_wxs_path, &rendered_wxs).with_context(|| {
                        format!(
                            "msi: write rendered .wxs to {}",
                            rendered_wxs_path.display()
                        )
                    })?;

                    // Copy extra_files into the temp/build context directory
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

                    // C3: Apply mod_timestamp to rendered .wxs if set
                    if let Some(ts) = &msi_cfg.mod_timestamp {
                        log.status(&format!("applying mod_timestamp={ts} to rendered .wxs"));
                        let mtime = parse_mod_timestamp(ts)?;
                        set_file_mtime(&rendered_wxs_path, mtime)?;
                    }

                    // Build commands
                    let mut commands = msi_command(
                        wix_version,
                        &rendered_wxs_path.to_string_lossy(),
                        &msi_path.to_string_lossy(),
                        &rendered_extensions,
                    );

                    // C3: For WiX v4, add -d BindTimestamp={ts} if mod_timestamp is set
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

                    // Execute primary command
                    log.status(&format!("running: {}", commands.primary.join(" ")));
                    let output = Command::new(&commands.primary[0])
                        .args(&commands.primary[1..])
                        .output()
                        .with_context(|| {
                            format!(
                                "msi: execute {} for crate {} target {:?}",
                                commands.primary[0], krate.name, target
                            )
                        })?;
                    log.check_output(output, &commands.primary[0])?;

                    // Execute link command if V3
                    if let Some(link_cmd) = &commands.link {
                        log.status(&format!("running: {}", link_cmd.join(" ")));
                        let output = Command::new(&link_cmd[0])
                            .args(&link_cmd[1..])
                            .output()
                            .with_context(|| {
                                format!(
                                    "msi: execute {} for crate {} target {:?}",
                                    link_cmd[0], krate.name, target
                                )
                            })?;
                        log.check_output(output, &link_cmd[0])?;
                    }

                    // C3: Apply mod_timestamp to output .msi if set
                    if let Some(ts) = &msi_cfg.mod_timestamp
                        && msi_path.exists()
                    {
                        let mtime = parse_mod_timestamp(ts)?;
                        set_file_mtime(&msi_path, mtime)?;
                        log.status(&format!(
                            "applied mod_timestamp={ts} to {}",
                            msi_path.display()
                        ));
                    }

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

                // GoReleaser Pro `msi.hooks.after` (alias `post`): run once per MSI
                // config after all artifacts are built. Hard-errors on hook failure.
                if let Some(post) = msi_cfg.hooks.as_ref().and_then(|h| h.post.as_ref()) {
                    let tmpl_vars = ctx.template_vars().clone();
                    anodize_core::hooks::run_hooks(
                        post,
                        "post-msi",
                        dry_run,
                        &log,
                        Some(&tmpl_vars),
                    )
                    .with_context(|| {
                        format!(
                            "msi config '{}' for crate '{}': post-msi hooks failed",
                            msi_id_for_log, krate.name
                        )
                    })?;
                }
            }
        }

        // Clear per-target template vars so they don't leak to downstream stages.
        ctx.template_vars_mut().set("Os", "");
        ctx.template_vars_mut().set("Arch", "");
        ctx.template_vars_mut().set("Target", "");

        // Remove replaced archive artifacts
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
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = MsiStage;
        assert!(stage.run(&mut ctx).is_ok());
        // No artifacts should be registered
        assert!(ctx.artifacts.by_kind(ArtifactKind::Installer).is_empty());
    }

    #[test]
    fn test_stage_skips_when_disabled() {
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Write a dummy .wxs file
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            disable: Some(anodize_core::config::StringOrBool::Bool(true)),
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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(msis.len(), 1);
        assert_eq!(msis[0].wxs.as_deref(), Some("app.wxs"));
        assert!(msis[0].name.is_none());
        assert!(msis[0].version.is_none());
        assert!(msis[0].replace.is_none());
        assert!(msis[0].disable.is_none());
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
        disable: false
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
            msi.disable,
            Some(anodize_core::config::StringOrBool::Bool(false))
        );
    }

    // -----------------------------------------------------------------------
    // WXS template rendering test
    // -----------------------------------------------------------------------

    #[test]
    fn test_wxs_template_rendering() {
        use anodize_core::test_helpers::TestContextBuilder;

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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
        disable: true
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(
            msis[0].disable,
            Some(anodize_core::config::StringOrBool::Bool(true))
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
        disable: "true"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(
            msis[0].disable,
            Some(anodize_core::config::StringOrBool::String(
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
        disable: "{{ .Env.SKIP_MSI }}"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(
            msis[0].disable,
            Some(anodize_core::config::StringOrBool::String(
                "{{ .Env.SKIP_MSI }}".to_string()
            ))
        );
    }

    #[test]
    fn test_stage_disable_with_string_true() {
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig, StringOrBool};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
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

        // disable: "true" should skip
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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::artifact::Artifact;
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        disable: "{{ .Env.SKIP_MSI }}"
        extra_files:
          - README.md
          - LICENSE
        extensions:
          - WixUIExtension
          - WixUtilExtension
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
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
            msi.disable,
            Some(anodize_core::config::StringOrBool::String(
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

    fn msi_test_ctx_with_if(if_expr: Option<&str>) -> anodize_core::context::Context {
        use anodize_core::config::{Config, CrateConfig, MsiConfig};
        use anodize_core::context::{Context, ContextOptions};
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
    fn test_config_parse_msi_hooks_before_after_aliases() {
        // Serde aliases on BuildHooksConfig mean `before:`/`after:` (GoReleaser
        // docs) and `pre:`/`post:` both populate the same fields.
        use anodize_core::config::Config;
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: installer.wxs
        hooks:
          before:
            - echo pre-msi-build
          after:
            - echo post-msi-build
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        let hooks = msis[0].hooks.as_ref().unwrap();
        let pre = hooks
            .pre
            .as_ref()
            .expect("`before:` yaml should populate `pre`");
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0], "echo pre-msi-build");
        let post = hooks
            .post
            .as_ref()
            .expect("`after:` yaml should populate `post`");
        assert_eq!(post.len(), 1);
        assert_eq!(post[0], "echo post-msi-build");
    }
}
