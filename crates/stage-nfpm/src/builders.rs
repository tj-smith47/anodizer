//! Format-specific YAML builders.
//!
//! Each `build_yaml_*` translates an `Nfpm*Config` from anodizer-core into
//! the corresponding `NfpmYaml*` struct from `yaml`. `resolve_passphrase_from_env`
//! implements a 3-level env-var fallback for signing passphrases.

use std::collections::HashMap;

use anodizer_core::config::{
    NfpmApkConfig, NfpmArchlinuxConfig, NfpmDebConfig, NfpmIpkConfig, NfpmMsixConfig,
    NfpmRpmConfig, NfpmSignatureConfig,
};

use crate::yaml::{
    NfpmYamlApk, NfpmYamlApkScripts, NfpmYamlArchlinux, NfpmYamlArchlinuxScripts, NfpmYamlDeb,
    NfpmYamlDebScripts, NfpmYamlDebTriggers, NfpmYamlIpk, NfpmYamlIpkAlternative, NfpmYamlMsix,
    NfpmYamlMsixApplication, NfpmYamlMsixCapabilities, NfpmYamlMsixDependencies,
    NfpmYamlMsixIdentity, NfpmYamlMsixProperties, NfpmYamlMsixSignature,
    NfpmYamlMsixTargetDeviceFamily, NfpmYamlMsixVisualElements, NfpmYamlRpm, NfpmYamlRpmScripts,
    NfpmYamlSignature,
};

/// Resolve the signing passphrase using a 3-level env var fallback:
///   1. NFPM_{ID}_{format}_PASSPHRASE  (format preserved as-is, e.g. `deb`/`rpm`)
///   2. NFPM_{ID}_PASSPHRASE
///   3. NFPM_PASSPHRASE
///
/// `env_map` is the anodizer ctx env map (process env + project `env:` +
/// `env_files:`). Looking up here — instead of `std::env::var` directly —
/// means values defined in `.anodizer.yaml` `env:` are visible to the signer,
/// reading from
/// `ctx.Env` rather than `os.Getenv`.
///
/// Returns `None` if no env var is set at any level.
pub(super) fn resolve_passphrase_from_env(
    env_map: &HashMap<String, String>,
    nfpm_id: &str,
    format: Option<&str>,
) -> Option<String> {
    let lookup = |name: &str| -> Option<String> {
        env_map
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
            .filter(|v| !v.is_empty())
    };
    let id_upper = nfpm_id.to_uppercase();
    // Level 1: NFPM_{ID}_{format}_PASSPHRASE (format preserved as-is)
    if let Some(fmt) = format
        && let Some(val) = lookup(&format!("NFPM_{id_upper}_{fmt}_PASSPHRASE"))
    {
        return Some(val);
    }
    // Level 2: NFPM_{ID}_PASSPHRASE
    if let Some(val) = lookup(&format!("NFPM_{id_upper}_PASSPHRASE")) {
        return Some(val);
    }
    // Level 3: NFPM_PASSPHRASE
    lookup("NFPM_PASSPHRASE")
}

pub(super) fn build_yaml_signature(
    sig: &NfpmSignatureConfig,
    nfpm_id: &str,
    format: Option<&str>,
    env_map: &HashMap<String, String>,
) -> NfpmYamlSignature {
    let key_passphrase = sig
        .key_passphrase
        .clone()
        .or_else(|| resolve_passphrase_from_env(env_map, nfpm_id, format));
    NfpmYamlSignature {
        key_file: sig.key_file.clone(),
        key_id: sig.key_id.clone(),
        key_passphrase,
        key_name: sig.key_name.clone(),
        type_: sig.type_.clone(),
    }
}

pub(super) fn build_yaml_rpm(
    rpm: &NfpmRpmConfig,
    nfpm_id: &str,
    format: Option<&str>,
    skip_sign: bool,
    env_map: &HashMap<String, String>,
) -> NfpmYamlRpm {
    NfpmYamlRpm {
        summary: rpm.summary.clone(),
        compression: rpm.compression.clone(),
        group: rpm.group.clone(),
        packager: rpm.packager.clone(),
        prefixes: rpm.prefixes.clone(),
        signature: if skip_sign {
            None
        } else {
            rpm.signature
                .as_ref()
                .map(|s| build_yaml_signature(s, nfpm_id, format, env_map))
        },
        scripts: rpm.scripts.as_ref().map(|s| NfpmYamlRpmScripts {
            pretrans: s.pretrans.clone(),
            posttrans: s.posttrans.clone(),
        }),
        build_host: rpm.build_host.clone(),
    }
}

pub(super) fn build_yaml_deb(
    deb: &NfpmDebConfig,
    nfpm_id: &str,
    format: Option<&str>,
    skip_sign: bool,
    env_map: &HashMap<String, String>,
    arch: Option<String>,
) -> NfpmYamlDeb {
    NfpmYamlDeb {
        arch,
        compression: deb.compression.clone(),
        predepends: deb.predepends.clone(),
        triggers: deb.triggers.as_ref().map(|t| NfpmYamlDebTriggers {
            interest: t.interest.clone(),
            interest_await: t.interest_await.clone(),
            interest_noawait: t.interest_noawait.clone(),
            activate: t.activate.clone(),
            activate_await: t.activate_await.clone(),
            activate_noawait: t.activate_noawait.clone(),
        }),
        breaks: deb.breaks.clone(),
        signature: if skip_sign {
            None
        } else {
            deb.signature
                .as_ref()
                .map(|s| build_yaml_signature(s, nfpm_id, format, env_map))
        },
        fields: deb.fields.clone(),
        scripts: deb.scripts.as_ref().map(|s| NfpmYamlDebScripts {
            rules: s.rules.clone(),
            templates: s.templates.clone(),
            config: s.config.clone(),
        }),
        arch_variant: deb.arch_variant.clone(),
    }
}

pub(super) fn build_yaml_apk(
    apk: &NfpmApkConfig,
    nfpm_id: &str,
    format: Option<&str>,
    skip_sign: bool,
    env_map: &HashMap<String, String>,
) -> NfpmYamlApk {
    NfpmYamlApk {
        signature: if skip_sign {
            None
        } else {
            apk.signature
                .as_ref()
                .map(|s| build_yaml_signature(s, nfpm_id, format, env_map))
        },
        scripts: apk.scripts.as_ref().map(|s| NfpmYamlApkScripts {
            preupgrade: s.preupgrade.clone(),
            postupgrade: s.postupgrade.clone(),
        }),
    }
}

pub(super) fn build_yaml_archlinux(arch: &NfpmArchlinuxConfig) -> NfpmYamlArchlinux {
    NfpmYamlArchlinux {
        pkgbase: arch.pkgbase.clone(),
        packager: arch.packager.clone(),
        scripts: arch.scripts.as_ref().map(|s| NfpmYamlArchlinuxScripts {
            preupgrade: s.preupgrade.clone(),
            postupgrade: s.postupgrade.clone(),
        }),
    }
}

pub(super) fn build_yaml_msix(msix: &NfpmMsixConfig, skip_sign: bool) -> NfpmYamlMsix {
    NfpmYamlMsix {
        arch: msix.arch.clone(),
        publisher: msix.publisher.clone(),
        identity: msix.identity.as_ref().map(|i| NfpmYamlMsixIdentity {
            resource_id: i.resource_id.clone(),
        }),
        properties: msix.properties.as_ref().map(|p| NfpmYamlMsixProperties {
            display_name: p.display_name.clone(),
            publisher_display_name: p.publisher_display_name.clone(),
            logo: p.logo.clone(),
        }),
        applications: msix.applications.as_ref().map(|apps| {
            apps.iter()
                .map(|a| NfpmYamlMsixApplication {
                    id: a.id.clone(),
                    executable: a.executable.clone(),
                    entry_point: a.entry_point.clone(),
                    visual_elements: a.visual_elements.as_ref().map(|v| {
                        NfpmYamlMsixVisualElements {
                            display_name: v.display_name.clone(),
                            description: v.description.clone(),
                            background_color: v.background_color.clone(),
                            square150x150_logo: v.square150x150_logo.clone(),
                            square44x44_logo: v.square44x44_logo.clone(),
                        }
                    }),
                })
                .collect()
        }),
        dependencies: msix
            .dependencies
            .as_ref()
            .map(|d| NfpmYamlMsixDependencies {
                target_device_families: d.target_device_families.as_ref().map(|fams| {
                    fams.iter()
                        .map(|f| NfpmYamlMsixTargetDeviceFamily {
                            name: f.name.clone(),
                            min_version: f.min_version.clone(),
                            max_version_tested: f.max_version_tested.clone(),
                        })
                        .collect()
                }),
            }),
        capabilities: msix
            .capabilities
            .as_ref()
            .map(|c| NfpmYamlMsixCapabilities {
                capabilities: c.capabilities.clone(),
                device_capabilities: c.device_capabilities.clone(),
                restricted: c.restricted.clone(),
            }),
        signature: if skip_sign {
            None
        } else {
            msix.signature.as_ref().map(|s| NfpmYamlMsixSignature {
                pfx_file: s.pfx_file.clone(),
            })
        },
    }
}

/// Derive one MSIX `<Application>` per packaged binary: `executable` is the
/// binary's file name (MSIX packages are rooted at the install location, so
/// the name alone addresses it) and `id` is the file stem sanitized to the
/// AppxManifest `Id` alphabet (letters, digits, periods; must not start with
/// a digit or period).
pub(super) fn derive_msix_applications(binary_paths: &[String]) -> Vec<NfpmYamlMsixApplication> {
    binary_paths
        .iter()
        .filter_map(|bp| {
            let file_name = std::path::Path::new(bp).file_name()?.to_str()?.to_string();
            let stem = std::path::Path::new(&file_name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&file_name);
            let mut id: String = stem
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '.')
                .collect();
            id = id.trim_matches('.').to_string();
            if !id.starts_with(|c: char| c.is_ascii_alphabetic()) {
                id.insert_str(0, "App");
            }
            Some(NfpmYamlMsixApplication {
                id: Some(id),
                executable: Some(file_name),
                entry_point: None,
                visual_elements: None,
            })
        })
        .collect()
}

pub(super) fn build_yaml_ipk(ipk: &NfpmIpkConfig) -> NfpmYamlIpk {
    NfpmYamlIpk {
        abi_version: ipk.abi_version.clone(),
        alternatives: ipk.alternatives.as_ref().map(|alts| {
            alts.iter()
                .map(|a| NfpmYamlIpkAlternative {
                    priority: a.priority,
                    target: a.target.clone(),
                    link_name: a.link_name.clone(),
                })
                .collect()
        }),
        auto_installed: ipk.auto_installed,
        essential: ipk.essential,
        predepends: ipk.predepends.clone(),
        tags: ipk.tags.clone(),
        fields: ipk.fields.clone(),
    }
}
