//! `.wxs` template rendering and the MSI stage's template-variable plumbing.
//!
//! Hosts the `.wxs` file render, the per-binary / post-hook template-var
//! population + teardown, and the output-filename resolution
//! ([`compute_msi_filename`] + [`DEFAULT_MSI_NAME_TEMPLATE`]).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context as _, Result};

use anodizer_core::context::Context;

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

/// Populate per-binary template variables. `BinaryPath` exposes the path
/// to user `.wxs` templates.
pub(super) fn set_msi_template_vars(
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

/// Default output filename template — matches GoReleaser Pro's default.
///
/// `MsiArch` is the WiX-native arch (`x86`, `x64`, `arm64`) injected
/// per-target before the name is rendered. The user controls the extension;
/// `.msi` is appended only when absent.
const DEFAULT_MSI_NAME_TEMPLATE: &str = "{{ ProjectName }}_{{ MsiArch }}";

/// Resolve the output `.msi` filename: rendered `name:` template wins
/// (auto-appending `.msi` when absent), otherwise the GR-compatible default
/// `<ProjectName>_<MsiArch>.msi` (rendered from `DEFAULT_MSI_NAME_TEMPLATE`).
pub(super) fn compute_msi_filename(
    ctx: &mut Context,
    msi_cfg: &anodizer_core::config::MsiConfig,
    crate_name: &str,
    target: Option<&str>,
    _version: &str,
    _msi_arch: &str,
) -> Result<String> {
    let name_tmpl = msi_cfg.name.as_deref().unwrap_or(DEFAULT_MSI_NAME_TEMPLATE);
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
}

/// Build the per-target post-hook template-var snapshot.
///
/// Clones the stage's current vars and overlays `ArtifactPath` (absolute,
/// canonicalized when the file exists; otherwise resolved against the
/// current working directory so dry-run paths are still absolute),
/// `ArtifactName` (filename), and `ArtifactExt` (`.msi`).
pub(super) fn build_post_hook_template_vars(
    ctx: &Context,
    msi_path: &std::path::Path,
) -> anodizer_core::template::TemplateVars {
    let mut tmpl_vars = ctx.template_vars().clone();
    let abs_path: PathBuf = std::fs::canonicalize(msi_path).unwrap_or_else(|_| {
        if msi_path.is_absolute() {
            msi_path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(msi_path))
                .unwrap_or_else(|_| msi_path.to_path_buf())
        }
    });
    let abs_path_str = abs_path.to_string_lossy();
    let filename = msi_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    tmpl_vars.set("ArtifactPath", &abs_path_str);
    tmpl_vars.set("ArtifactName", &filename);
    tmpl_vars.set("ArtifactExt", ".msi");
    tmpl_vars
}

/// Clear the per-target template vars on stage exit so they don't leak
/// into downstream stages (`announce`, `publish`).
pub(super) fn clear_msi_template_vars(ctx: &mut Context) {
    ctx.template_vars_mut().set("Os", "");
    ctx.template_vars_mut().set("Arch", "");
    ctx.template_vars_mut().set("Target", "");
    ctx.template_vars_mut().set("MsiArch", "");
    ctx.template_vars_mut().set("BinaryPath", "");
}
