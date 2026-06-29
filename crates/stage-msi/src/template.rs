//! `.wxs` template rendering and the MSI stage's template-variable plumbing.
//!
//! Hosts the `.wxs` file render, the per-binary / post-hook template-var
//! population + teardown, and the output-filename resolution
//! ([`compute_msi_filename`] + [`default_msi_name_template`]).

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

/// Coerce a release version into a WiX-legal `Product/@Version` value:
/// the numeric `major.minor.patch` core, each field clamped to WiX's
/// `0..=65534` range.
///
/// WiX's `Product/@Version` (v3) and `Package/@Version` (v4) reject anything
/// that is not a `x.x.x[.x]` numeric tuple — a pre-release release such as
/// `1.0.0-rc.1`, a build-metadata version such as `1.2.3+ci.7`, or a
/// determinism-harness snapshot such as `0.12.1-SNAPSHOT-abc123.0` all make
/// `candle` fail `CNDL0108`. The pre-release / build-metadata suffix carries no
/// meaning to the Windows Installer version comparison (only the numeric tuple
/// is significant), so dropping it is the correct coercion. Exposed to `.wxs`
/// authors as `{{ MsiVersion }}` so the full `{{ Version }}` string stays
/// available for display fields (Description, comments) where WiX permits it.
pub(super) fn msi_legal_version(version: &str) -> String {
    // Strip build metadata (`+...`) then the pre-release (`-...`), leaving the
    // numeric core; a non-numeric or absent field collapses to 0.
    let core = version
        .split('+')
        .next()
        .unwrap_or(version)
        .split('-')
        .next()
        .unwrap_or(version);
    let mut fields = core.split('.');
    let mut field = || {
        fields
            .next()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0)
            .min(65534)
    };
    format!("{}.{}.{}", field(), field(), field())
}

// ---------------------------------------------------------------------------
// Artifact creation helper
// ---------------------------------------------------------------------------

/// Populate per-binary template variables. `BinaryPath` exposes the path
/// to user `.wxs` templates; `MsiProductCode` exposes the deterministic
/// per-version ProductCode so a `.wxs` can pin `Product Id="{{ MsiProductCode }}"`;
/// `MsiVersion` exposes the WiX-legal numeric version for `Product/@Version`
/// (see [`msi_legal_version`]).
#[allow(clippy::too_many_arguments)]
pub(super) fn set_msi_template_vars(
    ctx: &mut Context,
    target: Option<&str>,
    arch: &str,
    msi_arch: &str,
    binary_path: &str,
    product_code: &str,
) {
    let msi_version = msi_legal_version(&ctx.version());
    ctx.template_vars_mut().set("Os", "windows");
    ctx.template_vars_mut().set("Arch", arch);
    ctx.template_vars_mut().set("Target", target.unwrap_or(""));
    ctx.template_vars_mut().set("MsiArch", msi_arch);
    ctx.template_vars_mut().set("BinaryPath", binary_path);
    ctx.template_vars_mut().set("MsiProductCode", product_code);
    ctx.template_vars_mut().set("MsiVersion", &msi_version);
}

/// Default output filename template.
///
/// `MsiArch` is the WiX-native arch (`x86`, `x64`, `arm64`) injected
/// per-target before the name is rendered. The amd64 micro-architecture
/// variant suffix disambiguates two amd64 builds of one target (e.g. `v1` +
/// `v3`). The user controls the extension; `.msi` is appended only when absent.
pub(super) const DEFAULT_MSI_NAME_PREFIX: &str = "{{ ProjectName }}_{{ MsiArch }}";

/// Compose the default msi name template: [`DEFAULT_MSI_NAME_PREFIX`] plus the
/// shared amd64 variant suffix from core. Two amd64 builds share one target
/// triple, so `MsiArch` alone cannot disambiguate them; the suffix keeps their
/// filenames distinct without re-embedding the clause literal here.
pub(super) fn default_msi_name_template() -> String {
    format!(
        "{DEFAULT_MSI_NAME_PREFIX}{}",
        anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX
    )
}

/// Resolve the output `.msi` filename: rendered `name:` template wins
/// (auto-appending `.msi` when absent), otherwise the default
/// `<ProjectName>_<MsiArch>.msi` (rendered from [`default_msi_name_template`]).
pub(super) fn compute_msi_filename(
    ctx: &mut Context,
    msi_cfg: &anodizer_core::config::MsiConfig,
    crate_name: &str,
    target: Option<&str>,
) -> Result<String> {
    let default_name = default_msi_name_template();
    let name_tmpl = msi_cfg.name.as_deref().unwrap_or(&default_name);
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
    ctx.template_vars_mut().set("MsiProductCode", "");
    ctx.template_vars_mut().set("MsiVersion", "");
}

#[cfg(test)]
mod tests {
    use super::msi_legal_version;

    #[test]
    fn plain_release_passes_through() {
        assert_eq!(msi_legal_version("0.13.0"), "0.13.0");
        assert_eq!(msi_legal_version("1.2.3"), "1.2.3");
    }

    #[test]
    fn prerelease_suffix_is_dropped() {
        assert_eq!(msi_legal_version("1.0.0-rc.1"), "1.0.0");
        // The determinism harness snapshot that triggered candle CNDL0108.
        assert_eq!(msi_legal_version("0.12.1-SNAPSHOT-d6ccf57.0"), "0.12.1");
    }

    #[test]
    fn build_metadata_is_dropped() {
        assert_eq!(msi_legal_version("1.2.3+ci.7"), "1.2.3");
        // Build metadata precedes any `-` split, so it strips even when both
        // suffixes are present.
        assert_eq!(msi_legal_version("1.2.3-rc.1+build.9"), "1.2.3");
    }

    #[test]
    fn missing_fields_collapse_to_zero() {
        assert_eq!(msi_legal_version("1"), "1.0.0");
        assert_eq!(msi_legal_version("1.2"), "1.2.0");
        assert_eq!(msi_legal_version(""), "0.0.0");
    }

    #[test]
    fn out_of_range_fields_are_clamped() {
        // WiX caps each field at 65534.
        assert_eq!(msi_legal_version("70000.1.2"), "65534.1.2");
        assert_eq!(msi_legal_version("1.99999.0"), "1.65534.0");
    }

    #[test]
    fn non_numeric_core_field_collapses_to_zero() {
        assert_eq!(msi_legal_version("x.y.z"), "0.0.0");
    }
}
