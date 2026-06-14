//! Chocolatey `chocolateyinstall.ps1` script generation.
//!
//! The emitted cmdlet is routed by the artifact's installer type:
//! - **zip** (archive) → `Install-ChocolateyZipPackage` (unpacks into `tools/`).
//! - **msi** → `Install-ChocolateyPackage` with `-FileType 'msi'` and
//!   `-SilentArgs '/qn /norestart'` plus MSI-standard `-ValidExitCodes`.
//! - **nsis exe** → `Install-ChocolateyPackage` with `-FileType 'exe'` and
//!   `-SilentArgs '/S'`.
//!
//! Each type has dual-arch (both 32- and 64-bit URLs), 64-bit-only, and
//! 32-bit-only flavors; the checksum / checksum64 (sha256) handling is
//! preserved across all of them.

use anyhow::{Context as _, Result};

/// Installer type the routed Windows artifact carries, mapped from the
/// chocolatey `use:` selector (`archive` → [`Zip`], `msi` → [`Msi`],
/// `nsis` → [`NsisExe`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// Archive (`.zip`/`.tar.gz`): unpacked into `tools/` via
    /// `Install-ChocolateyZipPackage`.
    Zip,
    /// Windows Installer (`.msi`): run via `Install-ChocolateyPackage`
    /// with `-FileType 'msi'` and silent MSI switches.
    Msi,
    /// NSIS-generated installer (`.exe`): run via `Install-ChocolateyPackage`
    /// with `-FileType 'exe'` and the NSIS silent switch `/S`.
    NsisExe,
}

impl FileType {
    /// Resolve from the chocolatey `use:` config value. `archive` (or any
    /// unrecognized/`None` value) maps to [`FileType::Zip`].
    pub fn from_use(use_value: Option<&str>) -> Self {
        match use_value {
            Some("msi") => FileType::Msi,
            Some("nsis") => FileType::NsisExe,
            _ => FileType::Zip,
        }
    }

    /// `-FileType` argument value for `Install-ChocolateyPackage` (msi/exe).
    fn choco_file_type(self) -> &'static str {
        match self {
            FileType::Zip => "",
            FileType::Msi => "msi",
            FileType::NsisExe => "exe",
        }
    }

    /// Silent-install switches for the installer. MSI takes the standard
    /// `/qn /norestart`; the NSIS exe takes `/S`. Zip has none (it is
    /// unpacked, not run).
    fn silent_args(self) -> &'static str {
        match self {
            FileType::Zip => "",
            FileType::Msi => "/qn /norestart",
            FileType::NsisExe => "/S",
        }
    }
}

// ---------------------------------------------------------------------------
// zip (archive) templates — unpack into tools/
// ---------------------------------------------------------------------------

/// Dual-arch zip install script (both 32-bit and 64-bit).
const ZIP_TEMPLATE_DUAL: &str = r#"$ErrorActionPreference = 'Stop'

$packageName = '{{ name }}'
$url = '{{ url32 }}'
$url64bit = '{{ url64 }}'
$checksum = '{{ hash32 }}'
$checksum64 = '{{ hash64 }}'
$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
Install-ChocolateyZipPackage $packageName $url $toolsDir $url64bit -Checksum $checksum -ChecksumType 'sha256' -Checksum64 $checksum64 -ChecksumType64 'sha256'
"#;

/// 64-bit-only zip install script.
const ZIP_TEMPLATE_64: &str = r#"$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = '{{ name }}'
  url64bit       = '{{ url }}'
  checksum64     = '{{ hash }}'
  checksumType64 = 'sha256'
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
}

Install-ChocolateyZipPackage @packageArgs
"#;

/// 32-bit-only zip install script.
const ZIP_TEMPLATE_32: &str = r#"$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName   = '{{ name }}'
  url           = '{{ url }}'
  checksum      = '{{ hash }}'
  checksumType  = 'sha256'
  unzipLocation = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
}

Install-ChocolateyZipPackage @packageArgs
"#;

// ---------------------------------------------------------------------------
// msi / nsis exe templates — run the installer silently
// ---------------------------------------------------------------------------

/// Dual-arch installer (msi/exe) install script. `validExitCodes` is emitted
/// only for MSI (the `{{ valid_exit_codes }}` line is omitted for exe).
const PKG_TEMPLATE_DUAL: &str = r#"$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = '{{ name }}'
  fileType       = '{{ file_type }}'
  url            = '{{ url32 }}'
  url64bit       = '{{ url64 }}'
  checksum       = '{{ hash32 }}'
  checksumType   = 'sha256'
  checksum64     = '{{ hash64 }}'
  checksumType64 = 'sha256'
  silentArgs     = '{{ silent_args }}'
{% if valid_exit_codes %}  validExitCodes = @({{ valid_exit_codes }})
{% endif %}}

Install-ChocolateyPackage @packageArgs
"#;

/// 64-bit-only installer (msi/exe) install script.
const PKG_TEMPLATE_64: &str = r#"$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = '{{ name }}'
  fileType       = '{{ file_type }}'
  url64bit       = '{{ url }}'
  checksum64     = '{{ hash }}'
  checksumType64 = 'sha256'
  silentArgs     = '{{ silent_args }}'
{% if valid_exit_codes %}  validExitCodes = @({{ valid_exit_codes }})
{% endif %}}

Install-ChocolateyPackage @packageArgs
"#;

/// 32-bit-only installer (msi/exe) install script.
const PKG_TEMPLATE_32: &str = r#"$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName  = '{{ name }}'
  fileType     = '{{ file_type }}'
  url          = '{{ url }}'
  checksum     = '{{ hash }}'
  checksumType = 'sha256'
  silentArgs   = '{{ silent_args }}'
{% if valid_exit_codes %}  validExitCodes = @({{ valid_exit_codes }})
{% endif %}}

Install-ChocolateyPackage @packageArgs
"#;

/// MSI's documented silent-install exit codes: `0` (success), `1641`
/// (success, reboot initiated), `3010` (success, reboot required). Emitted
/// as `validExitCodes` so a reboot-required MSI is not treated as a failure.
const MSI_VALID_EXIT_CODES: &str = "0, 1641, 3010";

/// Parameters for a dual-arch install script.
pub struct InstallScriptDual<'a> {
    pub name: &'a str,
    pub url32: &'a str,
    pub hash32: &'a str,
    pub url64: &'a str,
    pub hash64: &'a str,
    pub file_type: FileType,
}

/// Generate a single-arch install script for the given installer type.
pub fn generate_install_script(
    name: &str,
    url: &str,
    hash: &str,
    is_32bit: bool,
    file_type: FileType,
) -> Result<String> {
    let template = match (file_type, is_32bit) {
        (FileType::Zip, true) => ZIP_TEMPLATE_32,
        (FileType::Zip, false) => ZIP_TEMPLATE_64,
        (_, true) => PKG_TEMPLATE_32,
        (_, false) => PKG_TEMPLATE_64,
    };
    let tera = anodizer_core::template::parse_static("install", template)
        .context("chocolatey: parse install script template")?;
    let mut ctx = tera::Context::new();
    ctx.insert("name", name);
    ctx.insert("url", url);
    ctx.insert("hash", hash);
    insert_installer_vars(&mut ctx, file_type);
    anodizer_core::template::render_static(&tera, "install", &ctx, "chocolatey")
}

/// Generate a dual-arch install script with both 32-bit and 64-bit URLs.
pub fn generate_install_script_dual(params: &InstallScriptDual<'_>) -> Result<String> {
    let template = match params.file_type {
        FileType::Zip => ZIP_TEMPLATE_DUAL,
        _ => PKG_TEMPLATE_DUAL,
    };
    let tera = anodizer_core::template::parse_static("install", template)
        .context("chocolatey: parse dual install script template")?;
    let mut ctx = tera::Context::new();
    ctx.insert("name", params.name);
    ctx.insert("url32", params.url32);
    ctx.insert("hash32", params.hash32);
    ctx.insert("url64", params.url64);
    ctx.insert("hash64", params.hash64);
    insert_installer_vars(&mut ctx, params.file_type);
    anodizer_core::template::render_static(&tera, "install", &ctx, "chocolatey")
}

/// Insert the installer-type-dependent template variables (`file_type`,
/// `silent_args`, `valid_exit_codes`). For zips the `Install-ChocolateyZipPackage`
/// templates ignore these, so they stay empty.
fn insert_installer_vars(ctx: &mut tera::Context, file_type: FileType) {
    ctx.insert("file_type", file_type.choco_file_type());
    ctx.insert("silent_args", file_type.silent_args());
    // validExitCodes is MSI-only: the documented {0,1641,3010} reboot codes.
    // An empty string makes the `{% if valid_exit_codes %}` guard drop the
    // line for zip/exe.
    let valid_exit_codes = if file_type == FileType::Msi {
        MSI_VALID_EXIT_CODES
    } else {
        ""
    };
    ctx.insert("valid_exit_codes", valid_exit_codes);
}
