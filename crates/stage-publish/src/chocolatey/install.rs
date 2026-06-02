//! Chocolatey `chocolateyinstall.ps1` script generation.
//!
//! Three flavors:
//! - dual-arch (`Install-ChocolateyZipPackage` with both URLs)
//! - 64-bit only
//! - 32-bit only

use anyhow::{Context as _, Result};

/// Dual-arch install script (both 32-bit and 64-bit).
const INSTALL_SCRIPT_TEMPLATE_DUAL: &str = r#"$ErrorActionPreference = 'Stop'

$packageName = '{{ name }}'
$url = '{{ url32 }}'
$url64bit = '{{ url64 }}'
$checksum = '{{ hash32 }}'
$checksum64 = '{{ hash64 }}'
$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
Install-ChocolateyZipPackage $packageName $url $toolsDir $url64bit -Checksum $checksum -ChecksumType 'sha256' -Checksum64 $checksum64 -ChecksumType64 'sha256'
"#;

/// 64-bit-only install script.
const INSTALL_SCRIPT_TEMPLATE_64: &str = r#"$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName    = '{{ name }}'
  url64bit       = '{{ url }}'
  checksum64     = '{{ hash }}'
  checksumType64 = 'sha256'
  unzipLocation  = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
}

Install-ChocolateyZipPackage @packageArgs
"#;

/// 32-bit-only install script.
const INSTALL_SCRIPT_TEMPLATE_32: &str = r#"$ErrorActionPreference = 'Stop'

$packageArgs = @{
  packageName   = '{{ name }}'
  url           = '{{ url }}'
  checksum      = '{{ hash }}'
  checksumType  = 'sha256'
  unzipLocation = "$(Split-Path -Parent $MyInvocation.MyCommand.Definition)"
}

Install-ChocolateyZipPackage @packageArgs
"#;

/// Parameters for a dual-arch install script.
pub struct InstallScriptDual<'a> {
    pub name: &'a str,
    pub url32: &'a str,
    pub hash32: &'a str,
    pub url64: &'a str,
    pub hash64: &'a str,
}

/// Generate a single-arch install script.
pub fn generate_install_script(
    name: &str,
    url: &str,
    hash: &str,
    is_32bit: bool,
) -> Result<String> {
    let template = if is_32bit {
        INSTALL_SCRIPT_TEMPLATE_32
    } else {
        INSTALL_SCRIPT_TEMPLATE_64
    };
    let tera = anodizer_core::template::parse_static("install", template)
        .context("chocolatey: parse install script template")?;
    let mut ctx = tera::Context::new();
    ctx.insert("name", name);
    ctx.insert("url", url);
    ctx.insert("hash", hash);
    anodizer_core::template::render_static(&tera, "install", &ctx, "chocolatey")
}

/// Generate a dual-arch install script with both 32-bit and 64-bit URLs.
pub fn generate_install_script_dual(params: &InstallScriptDual<'_>) -> Result<String> {
    let tera = anodizer_core::template::parse_static("install", INSTALL_SCRIPT_TEMPLATE_DUAL)
        .context("chocolatey: parse dual install script template")?;
    let mut ctx = tera::Context::new();
    ctx.insert("name", params.name);
    ctx.insert("url32", params.url32);
    ctx.insert("hash32", params.hash32);
    ctx.insert("url64", params.url64);
    ctx.insert("hash64", params.hash64);
    anodizer_core::template::render_static(&tera, "install", &ctx, "chocolatey")
}
