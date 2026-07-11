//! Serde-serializable nfpm YAML model.
//!
//! These structs mirror the schema nfpm itself reads from `nfpm.yaml`, with
//! `Option`-wrapped fields and `skip_serializing_if` so unset values don't
//! appear in the generated YAML.

use std::collections::HashMap;

use serde::Serialize;

pub(super) fn is_empty_vec<T>(v: &[T]) -> bool {
    v.is_empty()
}

#[derive(Serialize)]
pub(super) struct NfpmYamlConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) name: Option<String>,
    /// Target architecture in nfpm nomenclature (`amd64`, `arm64`, …). nfpm
    /// marks this field required and otherwise defaults a missing value to
    /// `amd64`, silently mislabeling a non-amd64 package, so it is always
    /// emitted from the resolved target.
    pub(super) arch: String,
    pub(super) version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) epoch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) release: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prerelease: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) version_metadata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) maintainer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) section: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) umask: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) mtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scripts: Option<NfpmYamlScripts>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub(super) recommends: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub(super) suggests: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub(super) conflicts: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub(super) replaces: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub(super) provides: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub(super) contents: Vec<NfpmYamlContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) overrides: Option<HashMap<String, serde_yaml_ng::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) depends: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) rpm: Option<NfpmYamlRpm>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) deb: Option<NfpmYamlDeb>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) apk: Option<NfpmYamlApk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) archlinux: Option<NfpmYamlArchlinux>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ipk: Option<NfpmYamlIpk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) msix: Option<NfpmYamlMsix>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) changelog: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlScripts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) preinstall: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postinstall: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) preremove: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postremove: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlContent {
    pub(super) src: String,
    pub(super) dst: String,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) file_info: Option<NfpmYamlFileInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) packager: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) expand: Option<bool>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlFileInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) group: Option<String>,
    /// File permission mode as a YAML integer so nfpm unmarshals into Go's
    /// `fs.FileMode`. Source `FileInfo.mode` is already a `u32` so this
    /// maps straight through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) mode: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) mtime: Option<String>,
}

// ---------------------------------------------------------------------------
// Format-specific YAML model structs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(super) struct NfpmYamlSignature {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) key_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) key_passphrase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) key_name: Option<String>,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) type_: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlRpmScripts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pretrans: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) posttrans: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlRpm {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) compression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) packager: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) prefixes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) signature: Option<NfpmYamlSignature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scripts: Option<NfpmYamlRpmScripts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) build_host: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlDebTriggers {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) interest: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) interest_await: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) interest_noawait: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) activate: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) activate_await: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) activate_noawait: Option<Vec<String>>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlDebScripts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) rules: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) templates: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) config: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlDeb {
    /// Control-file `Architecture` override in packager nomenclature. nfpm
    /// uses this verbatim, bypassing its Go-arch→Debian mapping — the hook
    /// termux.deb uses to stamp Termux arch names (x86_64/aarch64/i686/arm).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) arch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) compression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) predepends: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) triggers: Option<NfpmYamlDebTriggers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) breaks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) signature: Option<NfpmYamlSignature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) fields: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scripts: Option<NfpmYamlDebScripts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) arch_variant: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlApkScripts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) preupgrade: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postupgrade: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlApk {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) signature: Option<NfpmYamlSignature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scripts: Option<NfpmYamlApkScripts>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlArchlinuxScripts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) preupgrade: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postupgrade: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlArchlinux {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pkgbase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) packager: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scripts: Option<NfpmYamlArchlinuxScripts>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlIpk {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) abi_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) alternatives: Option<Vec<NfpmYamlIpkAlternative>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) auto_installed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) essential: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) predepends: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) fields: Option<HashMap<String, String>>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlMsix {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) arch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) publisher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) identity: Option<NfpmYamlMsixIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) properties: Option<NfpmYamlMsixProperties>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) applications: Option<Vec<NfpmYamlMsixApplication>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) dependencies: Option<NfpmYamlMsixDependencies>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) capabilities: Option<NfpmYamlMsixCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) signature: Option<NfpmYamlMsixSignature>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlMsixIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) resource_id: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlMsixProperties {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) publisher_display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) logo: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlMsixApplication {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) executable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) entry_point: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) visual_elements: Option<NfpmYamlMsixVisualElements>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlMsixVisualElements {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) background_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) square150x150_logo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) square44x44_logo: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlMsixDependencies {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) target_device_families: Option<Vec<NfpmYamlMsixTargetDeviceFamily>>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlMsixTargetDeviceFamily {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) min_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) max_version_tested: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlMsixCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) capabilities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) device_capabilities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) restricted: Option<Vec<String>>,
}

/// The msix signature block carries only `pfx_file` — nfpm reads the
/// passphrase from the `NFPM_MSIX_PASSPHRASE` env var of its own process,
/// never from YAML, so anodizer forwards it via the subprocess env instead.
#[derive(Serialize)]
pub(super) struct NfpmYamlMsixSignature {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pfx_file: Option<String>,
}

#[derive(Serialize)]
pub(super) struct NfpmYamlIpkAlternative {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) priority: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) link_name: Option<String>,
}
