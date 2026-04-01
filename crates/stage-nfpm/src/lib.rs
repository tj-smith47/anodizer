use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};
use serde::Serialize;

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{
    NfpmApkConfig, NfpmArchlinuxConfig, NfpmConfig, NfpmDebConfig, NfpmRpmConfig,
    NfpmSignatureConfig,
};
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Serde-serializable nfpm YAML model
// ---------------------------------------------------------------------------

fn is_empty_vec<T>(v: &[T]) -> bool {
    v.is_empty()
}

#[derive(Serialize)]
struct NfpmYamlConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prerelease: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version_metadata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    maintainer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    section: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    meta: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    umask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scripts: Option<NfpmYamlScripts>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    recommends: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    suggests: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    conflicts: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    replaces: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    provides: Vec<String>,
    contents: Vec<NfpmYamlContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    overrides: Option<HashMap<String, serde_yaml_ng::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    depends: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rpm: Option<NfpmYamlRpm>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deb: Option<NfpmYamlDeb>,
    #[serde(skip_serializing_if = "Option::is_none")]
    apk: Option<NfpmYamlApk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    archlinux: Option<NfpmYamlArchlinux>,
}

#[derive(Serialize)]
struct NfpmYamlScripts {
    #[serde(skip_serializing_if = "Option::is_none")]
    preinstall: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    postinstall: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preremove: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    postremove: Option<String>,
}

#[derive(Serialize)]
struct NfpmYamlContent {
    src: String,
    dst: String,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_info: Option<NfpmYamlFileInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    packager: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expand: Option<bool>,
}

#[derive(Serialize)]
struct NfpmYamlFileInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mtime: Option<String>,
}

// ---------------------------------------------------------------------------
// Format-specific YAML model structs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct NfpmYamlSignature {
    #[serde(skip_serializing_if = "Option::is_none")]
    key_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_passphrase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_name: Option<String>,
    #[serde(rename = "type")]
    #[serde(skip_serializing_if = "Option::is_none")]
    type_: Option<String>,
}

#[derive(Serialize)]
struct NfpmYamlRpmScripts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pretrans: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    posttrans: Option<String>,
}

#[derive(Serialize)]
struct NfpmYamlRpm {
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    packager: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefixes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<NfpmYamlSignature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scripts: Option<NfpmYamlRpmScripts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_host: Option<String>,
}

#[derive(Serialize)]
struct NfpmYamlDebTriggers {
    #[serde(skip_serializing_if = "Option::is_none")]
    interest: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interest_await: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interest_noawait: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    activate: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    activate_await: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    activate_noawait: Option<Vec<String>>,
}

#[derive(Serialize)]
struct NfpmYamlDebScripts {
    #[serde(skip_serializing_if = "Option::is_none")]
    rules: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    templates: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<String>,
}

#[derive(Serialize)]
struct NfpmYamlDeb {
    #[serde(skip_serializing_if = "Option::is_none")]
    compression: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    predepends: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    triggers: Option<NfpmYamlDebTriggers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    breaks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lintian_overrides: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<NfpmYamlSignature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fields: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scripts: Option<NfpmYamlDebScripts>,
}

#[derive(Serialize)]
struct NfpmYamlApkScripts {
    #[serde(skip_serializing_if = "Option::is_none")]
    preupgrade: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    postupgrade: Option<String>,
}

#[derive(Serialize)]
struct NfpmYamlApk {
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<NfpmYamlSignature>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scripts: Option<NfpmYamlApkScripts>,
}

#[derive(Serialize)]
struct NfpmYamlArchlinuxScripts {
    #[serde(skip_serializing_if = "Option::is_none")]
    preupgrade: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    postupgrade: Option<String>,
}

#[derive(Serialize)]
struct NfpmYamlArchlinux {
    #[serde(skip_serializing_if = "Option::is_none")]
    pkgbase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    packager: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scripts: Option<NfpmYamlArchlinuxScripts>,
}

// ---------------------------------------------------------------------------
// generate_nfpm_yaml
// ---------------------------------------------------------------------------

/// Generate an nfpm YAML configuration string from the anodize nfpm config.
///
/// `format` is the target packager format (e.g. "deb", "rpm") used to select
/// format-specific dependencies from the `dependencies` HashMap.  Pass `None`
/// to include deps for *all* formats.
pub fn generate_nfpm_yaml(
    config: &NfpmConfig,
    version: &str,
    binary_path: &str,
    format: Option<&str>,
) -> String {
    let is_meta = config.meta == Some(true);

    // Build the binary content entry (skip for meta packages)
    let raw_bindir = config.bindir.as_deref().unwrap_or("/usr/bin");
    // For termux.deb, rewrite bindir to the Termux filesystem prefix
    let bindir = if format == Some("termux.deb") && raw_bindir.starts_with("/usr") {
        format!("/data/data/com.termux/files{raw_bindir}")
    } else {
        raw_bindir.to_string()
    };
    let bindir = bindir.as_str();
    let binary_name = PathBuf::from(binary_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("binary")
        .to_string();

    let mut contents = if is_meta {
        // Meta packages have no binary contents — only dependencies
        Vec::new()
    } else {
        vec![NfpmYamlContent {
            src: binary_path.to_string(),
            dst: format!("{bindir}/{binary_name}"),
            content_type: None,
            file_info: Some(NfpmYamlFileInfo {
                owner: None,
                group: None,
                mode: Some("0755".to_string()),
                mtime: None,
            }),
            packager: None,
            expand: None,
        }]
    };

    // Extra contents from config
    if let Some(cfg_contents) = &config.contents {
        for entry in cfg_contents {
            contents.push(NfpmYamlContent {
                src: entry.src.clone(),
                dst: entry.dst.clone(),
                content_type: entry.content_type.clone(),
                file_info: entry.file_info.as_ref().map(|fi| NfpmYamlFileInfo {
                    owner: fi.owner.clone(),
                    group: fi.group.clone(),
                    mode: fi.mode.clone(),
                    mtime: fi.mtime.clone(),
                }),
                packager: entry.packager.clone(),
                expand: entry.expand,
            });
        }
    }

    // Build scripts section (only if any script is set)
    let scripts = config.scripts.as_ref().and_then(|s| {
        if s.preinstall.is_some()
            || s.postinstall.is_some()
            || s.preremove.is_some()
            || s.postremove.is_some()
        {
            Some(NfpmYamlScripts {
                preinstall: s.preinstall.clone(),
                postinstall: s.postinstall.clone(),
                preremove: s.preremove.clone(),
                postremove: s.postremove.clone(),
            })
        } else {
            None
        }
    });

    // Convert serde_json::Value overrides to serde_yaml_ng::Value
    let overrides = config
        .overrides
        .as_ref()
        .filter(|m| !m.is_empty())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    // Convert JSON Value -> string -> YAML Value.
                    // Skip entries that fail serialisation rather than panicking.
                    let json_str = serde_json::to_string(v).ok()?;
                    let yaml_val: serde_yaml_ng::Value = serde_yaml_ng::from_str(&json_str).ok()?;
                    Some((k.clone(), yaml_val))
                })
                .collect()
        });

    // Flatten the format-keyed dependencies HashMap into a flat Vec<String>.
    // When a target format is supplied we take only deps for that format;
    // otherwise we merge deps from all formats (deduped, order-preserving).
    let depends: Option<Vec<String>> = config.dependencies.as_ref().and_then(|m| {
        let mut flat: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (key, vals) in m {
            if format.is_none_or(|f| f == key) {
                for v in vals {
                    if seen.insert(v.clone()) {
                        flat.push(v.clone());
                    }
                }
            }
        }
        if flat.is_empty() { None } else { Some(flat) }
    });

    // Only emit format-specific YAML sections when the config has at least
    // one non-None field — avoids emitting empty `rpm: {}` blocks.
    let nfpm_id = config.id.as_deref().unwrap_or("default");
    let rpm = config
        .rpm
        .as_ref()
        .filter(|r| !r.is_empty())
        .map(|r| build_yaml_rpm(r, nfpm_id, format));
    let deb = config
        .deb
        .as_ref()
        .filter(|d| !d.is_empty())
        .map(|d| build_yaml_deb(d, nfpm_id, format));
    let apk = config
        .apk
        .as_ref()
        .filter(|a| !a.is_empty())
        .map(|a| build_yaml_apk(a, nfpm_id, format));
    let archlinux = config
        .archlinux
        .as_ref()
        .filter(|a| !a.is_empty())
        .map(build_yaml_archlinux);

    let yaml_config = NfpmYamlConfig {
        name: config.package_name.clone(),
        version: version.to_string(),
        epoch: config.epoch.clone(),
        release: config.release.clone(),
        prerelease: config.prerelease.clone(),
        version_metadata: config.version_metadata.clone(),
        vendor: config.vendor.clone(),
        homepage: config.homepage.clone(),
        maintainer: config.maintainer.clone(),
        description: config.description.clone(),
        license: config.license.clone(),
        section: config.section.clone(),
        priority: config.priority.clone(),
        meta: config.meta,
        umask: config.umask.clone(),
        mtime: config.mtime.clone(),
        scripts,
        recommends: config.recommends.clone().unwrap_or_default(),
        suggests: config.suggests.clone().unwrap_or_default(),
        conflicts: config.conflicts.clone().unwrap_or_default(),
        replaces: config.replaces.clone().unwrap_or_default(),
        provides: config.provides.clone().unwrap_or_default(),
        contents,
        overrides,
        depends,
        rpm,
        deb,
        apk,
        archlinux,
    };

    // SAFETY: serde_yaml_ng::to_string can only fail if the type contains
    // un-serialisable values (e.g. maps with non-string keys). NfpmYamlConfig
    // is composed entirely of Strings, Vecs, and Options thereof, so
    // serialisation is infallible in practice.
    let yaml = serde_yaml_ng::to_string(&yaml_config).expect("failed to serialize nfpm YAML");
    // serde_yaml_ng emits a trailing newline; trim it for consistency
    yaml.trim_end().to_string()
}

// ---------------------------------------------------------------------------
// Format-specific YAML builders
// ---------------------------------------------------------------------------

/// Resolve the signing passphrase using GoReleaser's 3-level env var fallback:
///   1. NFPM_{ID}_{FORMAT}_PASSPHRASE
///   2. NFPM_{ID}_PASSPHRASE
///   3. NFPM_PASSPHRASE
///
/// Returns `None` if no env var is set either.
fn resolve_passphrase_from_env(nfpm_id: &str, format: Option<&str>) -> Option<String> {
    let id_upper = nfpm_id.to_uppercase();
    // Level 1: NFPM_{ID}_{FORMAT}_PASSPHRASE (only when format is known)
    if let Some(fmt) = format {
        let fmt_upper = fmt.to_uppercase();
        let var = format!("NFPM_{id_upper}_{fmt_upper}_PASSPHRASE");
        if let Ok(val) = std::env::var(&var)
            && !val.is_empty()
        {
            return Some(val);
        }
    }
    // Level 2: NFPM_{ID}_PASSPHRASE
    let var = format!("NFPM_{id_upper}_PASSPHRASE");
    if let Ok(val) = std::env::var(&var)
        && !val.is_empty()
    {
        return Some(val);
    }
    // Level 3: NFPM_PASSPHRASE
    if let Ok(val) = std::env::var("NFPM_PASSPHRASE")
        && !val.is_empty()
    {
        return Some(val);
    }
    None
}

fn build_yaml_signature(
    sig: &NfpmSignatureConfig,
    nfpm_id: &str,
    format: Option<&str>,
) -> NfpmYamlSignature {
    let key_passphrase = sig
        .key_passphrase
        .clone()
        .or_else(|| resolve_passphrase_from_env(nfpm_id, format));
    NfpmYamlSignature {
        key_file: sig.key_file.clone(),
        key_id: sig.key_id.clone(),
        key_passphrase,
        key_name: sig.key_name.clone(),
        type_: sig.type_.clone(),
    }
}

fn build_yaml_rpm(rpm: &NfpmRpmConfig, nfpm_id: &str, format: Option<&str>) -> NfpmYamlRpm {
    NfpmYamlRpm {
        summary: rpm.summary.clone(),
        compression: rpm.compression.clone(),
        group: rpm.group.clone(),
        packager: rpm.packager.clone(),
        prefixes: rpm.prefixes.clone(),
        signature: rpm
            .signature
            .as_ref()
            .map(|s| build_yaml_signature(s, nfpm_id, format)),
        scripts: rpm.scripts.as_ref().map(|s| NfpmYamlRpmScripts {
            pretrans: s.pretrans.clone(),
            posttrans: s.posttrans.clone(),
        }),
        build_host: rpm.build_host.clone(),
    }
}

fn build_yaml_deb(deb: &NfpmDebConfig, nfpm_id: &str, format: Option<&str>) -> NfpmYamlDeb {
    NfpmYamlDeb {
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
        lintian_overrides: deb.lintian_overrides.clone(),
        signature: deb
            .signature
            .as_ref()
            .map(|s| build_yaml_signature(s, nfpm_id, format)),
        fields: deb.fields.clone(),
        scripts: deb.scripts.as_ref().map(|s| NfpmYamlDebScripts {
            rules: s.rules.clone(),
            templates: s.templates.clone(),
            config: s.config.clone(),
        }),
    }
}

fn build_yaml_apk(apk: &NfpmApkConfig, nfpm_id: &str, format: Option<&str>) -> NfpmYamlApk {
    NfpmYamlApk {
        signature: apk
            .signature
            .as_ref()
            .map(|s| build_yaml_signature(s, nfpm_id, format)),
        scripts: apk.scripts.as_ref().map(|s| NfpmYamlApkScripts {
            preupgrade: s.preupgrade.clone(),
            postupgrade: s.postupgrade.clone(),
        }),
    }
}

fn build_yaml_archlinux(arch: &NfpmArchlinuxConfig) -> NfpmYamlArchlinux {
    NfpmYamlArchlinux {
        pkgbase: arch.pkgbase.clone(),
        packager: arch.packager.clone(),
        scripts: arch.scripts.as_ref().map(|s| NfpmYamlArchlinuxScripts {
            preupgrade: s.preupgrade.clone(),
            postupgrade: s.postupgrade.clone(),
        }),
    }
}

// ---------------------------------------------------------------------------
// nfpm_command
// ---------------------------------------------------------------------------

/// Construct the nfpm CLI command arguments.
///
/// `target` is the output file path (not directory).  When given a full file
/// path nfpm writes the package to that exact location, which avoids
/// mismatches between the predicted and actual output filename.
pub fn nfpm_command(config_path: &str, format: &str, target: &str) -> Vec<String> {
    vec![
        "nfpm".to_string(),
        "pkg".to_string(),
        "--config".to_string(),
        config_path.to_string(),
        "--packager".to_string(),
        format.to_string(),
        "--target".to_string(),
        target.to_string(),
    ]
}

// ---------------------------------------------------------------------------
// Format validation
// ---------------------------------------------------------------------------

/// Recognized nfpm packager format names.
const KNOWN_FORMATS: &[&str] = &["deb", "rpm", "apk", "archlinux", "termux.deb", "ipk"];

/// Validate that a format string is a known nfpm packager.
fn validate_format(format: &str) -> Result<()> {
    if KNOWN_FORMATS.contains(&format) {
        Ok(())
    } else {
        anyhow::bail!(
            "unknown nfpm packager format {:?} (known: {})",
            format,
            KNOWN_FORMATS.join(", ")
        )
    }
}

// ---------------------------------------------------------------------------
// Architecture validation per format
// ---------------------------------------------------------------------------

/// Check if a target triple's architecture is supported for the given nfpm
/// packager format. Returns `true` for formats with no restrictions or when
/// the architecture is in the supported set.
fn is_arch_supported_for_format(triple: &str, format: &str) -> bool {
    // Extract architecture component from triple
    let first = triple.split('-').next().unwrap_or("");

    match format {
        "archlinux" => {
            // Archlinux only supports: x86_64, i686, aarch64, armv7h
            matches!(first, "x86_64" | "i686" | "aarch64" | "armv7" | "armv7l")
        }
        "termux.deb" => {
            // Termux (Android): aarch64, arm, i686, x86_64
            matches!(
                first,
                "aarch64" | "arm" | "armv7" | "armv7l" | "armv6" | "armv6l" | "i686" | "x86_64"
            )
        }
        // All other formats (deb, rpm, apk, ipk) have broad arch support
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// NfpmStage
// ---------------------------------------------------------------------------

pub struct NfpmStage;

impl Stage for NfpmStage {
    fn name(&self) -> &str {
        "nfpm"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("nfpm");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have nfpm config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.nfpm.is_some())
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

        for krate in &crates {
            let nfpm_configs = krate.nfpm.as_ref().unwrap();

            // Collect all Linux binary artifacts for this crate
            let linux_binaries: Vec<_> = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                .into_iter()
                .filter(|b| {
                    b.target
                        .as_deref()
                        .map(|t| t.contains("linux"))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            for nfpm_cfg in nfpm_configs {
                let is_meta = nfpm_cfg.meta == Some(true);

                // Meta packages have no binary contents — use a synthetic entry
                // so the loop below runs once per target (or once with no target).
                let effective_binaries: Vec<(Option<String>, String)> = if is_meta {
                    // For meta packages, produce one package per unique target
                    // from all linux binaries (for arch), or a single default
                    // entry if no binaries exist.
                    if linux_binaries.is_empty() {
                        vec![(None, String::new())]
                    } else {
                        let mut seen = std::collections::HashSet::new();
                        linux_binaries
                            .iter()
                            .filter(|b| {
                                let key = b.target.clone().unwrap_or_default();
                                seen.insert(key)
                            })
                            .map(|b| (b.target.clone(), String::new()))
                            .collect()
                    }
                } else {
                    // Apply ids filter: when the nfpm config specifies `ids`,
                    // only include binaries whose metadata "id" is in the list.
                    let filtered_binaries: Vec<_> = if let Some(ref ids) = nfpm_cfg.ids {
                        linux_binaries
                            .iter()
                            .filter(|b| {
                                b.metadata
                                    .get("id")
                                    .map(|bid| ids.contains(bid))
                                    .unwrap_or(false)
                            })
                            .collect()
                    } else {
                        linux_binaries.iter().collect()
                    };

                    // If the ids filter matched nothing but there ARE linux
                    // binaries, warn and skip — the user likely misconfigured ids.
                    if filtered_binaries.is_empty() && !linux_binaries.is_empty() {
                        let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
                        log.warn(&format!(
                            "nfpm config '{}': ids filter matched no binaries, skipping",
                            nfpm_id
                        ));
                        continue;
                    }

                    // If no linux binaries found at all, use a single synthetic
                    // entry with a default path.
                    if filtered_binaries.is_empty() {
                        vec![(None, format!("dist/{}", krate.name))]
                    } else {
                        filtered_binaries
                            .iter()
                            .map(|b| (b.target.clone(), b.path.to_string_lossy().into_owned()))
                            .collect()
                    }
                };

                for (target, binary_path) in &effective_binaries {
                    // Derive Os/Arch from the target triple for template rendering
                    let (os, arch) = target
                        .as_deref()
                        .map(anodize_core::target::map_target)
                        .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()));

                    for format in &nfpm_cfg.formats {
                        validate_format(format)
                            .with_context(|| format!("nfpm config for crate {}", krate.name))?;

                        // Validate architecture compatibility per format
                        if let Some(triple) = target.as_deref()
                            && !is_arch_supported_for_format(triple, format)
                        {
                            log.warn(&format!(
                                "skipping format '{}' for target '{}': architecture not supported",
                                format, triple
                            ));
                            continue;
                        }

                        // Template-render key string fields before generating YAML.
                        // Errors are propagated (not silently swallowed) to match GoReleaser.
                        let mut rendered_cfg = nfpm_cfg.clone();
                        if let Some(ref s) = rendered_cfg.description {
                            rendered_cfg.description = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.maintainer {
                            rendered_cfg.maintainer = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.homepage {
                            rendered_cfg.homepage = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.license {
                            rendered_cfg.license = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.vendor {
                            rendered_cfg.vendor = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.section {
                            rendered_cfg.section = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.priority {
                            rendered_cfg.priority = Some(ctx.render_template(s)?);
                        }

                        // Generate YAML per format so format-specific deps are selected
                        let yaml_content =
                            generate_nfpm_yaml(&rendered_cfg, &version, binary_path, Some(format));

                        // Ensure output directory exists
                        let output_dir = dist.join("linux");
                        if !dry_run {
                            fs::create_dir_all(&output_dir).with_context(|| {
                                format!("create nfpm output dir: {}", output_dir.display())
                            })?;
                        }

                        // Determine package file name (template or default)
                        let pkg_name = nfpm_cfg.package_name.as_deref().unwrap_or(&krate.name);
                        let ext = format_extension(format);

                        // Set nfpm-specific template vars (Os, Arch, Format,
                        // PackageName, ConventionalExtension, ConventionalFileName,
                        // Release, Epoch) before rendering file_name_template.
                        ctx.template_vars_mut().set("Os", &os);
                        ctx.template_vars_mut().set("Arch", &arch);
                        ctx.template_vars_mut().set("Format", format);
                        ctx.template_vars_mut().set("PackageName", pkg_name);
                        ctx.template_vars_mut().set("ConventionalExtension", ext);
                        ctx.template_vars_mut().set(
                            "ConventionalFileName",
                            &format!("{pkg_name}_{version}_{os}_{arch}{ext}"),
                        );
                        ctx.template_vars_mut()
                            .set("Release", nfpm_cfg.release.as_deref().unwrap_or(""));
                        ctx.template_vars_mut()
                            .set("Epoch", nfpm_cfg.epoch.as_deref().unwrap_or(""));

                        let pkg_filename = if let Some(tmpl) = &nfpm_cfg.file_name_template {
                            let rendered = ctx.render_template(tmpl).with_context(|| {
                                format!(
                                    "nfpm: render file_name_template for crate {} target {:?}",
                                    krate.name, target
                                )
                            })?;
                            // If the rendered template already ends with the
                            // format extension (e.g. the user used
                            // ConventionalExtension or ConventionalFileName),
                            // don't double-append it.
                            if !ext.is_empty() && rendered.ends_with(ext) {
                                rendered
                            } else {
                                format!("{rendered}{ext}")
                            }
                        } else {
                            format!("{pkg_name}_{version}_{os}_{arch}{ext}")
                        };
                        let pkg_path = output_dir.join(&pkg_filename);

                        // Build metadata: always include format, optionally include nfpm id
                        let mut pkg_metadata =
                            HashMap::from([("format".to_string(), format.clone())]);
                        if let Some(ref id) = nfpm_cfg.id {
                            pkg_metadata.insert("id".to_string(), id.clone());
                        }

                        if dry_run {
                            log.status(&format!(
                                "(dry-run) would run: nfpm pkg --packager {format} for crate {} target {:?}",
                                krate.name, target
                            ));
                            new_artifacts.push(Artifact {
                                kind: ArtifactKind::LinuxPackage,
                                name: String::new(),
                                path: pkg_path,
                                target: target.clone(),
                                crate_name: krate.name.clone(),
                                metadata: pkg_metadata,
                                size: None,
                            });
                            continue;
                        }

                        // Write temp nfpm YAML config
                        let tmp_dir =
                            tempfile::tempdir().context("create temp dir for nfpm config")?;
                        let config_path = tmp_dir.path().join("nfpm.yaml");
                        fs::write(&config_path, &yaml_content).with_context(|| {
                            format!("write nfpm config to {}", config_path.display())
                        })?;

                        // Pass the full file path (not directory) to nfpm
                        // --target so the output lands at the exact path we
                        // registered as the artifact.  This avoids mismatches
                        // between our predicted filename and nfpm's own naming.
                        let cmd_args = nfpm_command(
                            &config_path.to_string_lossy(),
                            format,
                            &pkg_path.to_string_lossy(),
                        );

                        log.status(&format!("running: {}", cmd_args.join(" ")));

                        let output = Command::new(&cmd_args[0])
                            .args(&cmd_args[1..])
                            .output()
                            .with_context(|| {
                                format!(
                                    "execute nfpm for format {format} (crate {} target {:?})",
                                    krate.name, target
                                )
                            })?;
                        log.check_output(output, "nfpm")?;

                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::LinuxPackage,
                            name: String::new(),
                            path: pkg_path,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            metadata: pkg_metadata,
                            size: None,
                        });
                    }
                }
            }
        }

        // Clear per-target template vars so they don't leak to downstream stages.
        ctx.template_vars_mut().set("Os", "");
        ctx.template_vars_mut().set("Arch", "");
        ctx.template_vars_mut().set("Format", "");
        ctx.template_vars_mut().set("PackageName", "");
        ctx.template_vars_mut().set("ConventionalExtension", "");
        ctx.template_vars_mut().set("ConventionalFileName", "");
        ctx.template_vars_mut().set("Release", "");
        ctx.template_vars_mut().set("Epoch", "");

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

/// Return the file extension for a given nfpm packager format.
fn format_extension(format: &str) -> &str {
    match format {
        "deb" | "termux.deb" => ".deb",
        "rpm" => ".rpm",
        "apk" => ".apk",
        "archlinux" => ".pkg.tar.zst",
        "ipk" => ".ipk",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_generate_nfpm_yaml() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            vendor: Some("Test Vendor".to_string()),
            homepage: Some("https://example.com".to_string()),
            maintainer: Some("test@example.com".to_string()),
            description: Some("A test app".to_string()),
            license: Some("MIT".to_string()),
            bindir: Some("/usr/bin".to_string()),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/path/to/binary", None);
        assert!(yaml.contains("name: myapp"));
        assert!(yaml.contains("version: 1.0.0"));
        assert!(yaml.contains("vendor: Test Vendor"));
        assert!(yaml.contains("/usr/bin/"));
    }

    #[test]
    fn test_nfpm_command() {
        let cmd = nfpm_command("/tmp/nfpm.yaml", "deb", "/tmp/output");
        assert_eq!(cmd[0], "nfpm");
        assert!(cmd.contains(&"pkg".to_string()));
        assert!(cmd.contains(&"deb".to_string()));
    }

    #[test]
    fn test_stage_skips_when_no_nfpm_config() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        // NfpmStage should be a no-op when crates have no nfpm block
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = NfpmStage;
        // Should succeed (no-op)
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_generate_nfpm_yaml_with_contents() {
        use anodize_core::config::NfpmContent;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["rpm".to_string()],
            description: Some("desc".to_string()),
            contents: Some(vec![NfpmContent {
                src: "/src/config".to_string(),
                dst: "/etc/myapp/config".to_string(),
                content_type: None,
                file_info: None,
                packager: None,
                expand: None,
            }]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "2.0.0", "/dist/myapp", None);
        assert!(yaml.contains("version: 2.0.0"));
        assert!(yaml.contains("/etc/myapp/config"));
        assert!(yaml.contains("/usr/bin/myapp"));
    }

    #[test]
    fn test_nfpm_command_structure() {
        let cmd = nfpm_command("/etc/nfpm.yaml", "rpm", "/out");
        assert_eq!(
            cmd,
            vec![
                "nfpm",
                "pkg",
                "--config",
                "/etc/nfpm.yaml",
                "--packager",
                "rpm",
                "--target",
                "/out",
            ]
        );
    }

    #[test]
    fn test_stage_dry_run_registers_artifacts() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string(), "rpm".to_string()],
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        let stage = NfpmStage;
        stage.run(&mut ctx).unwrap();

        // In dry-run mode, two artifacts (deb + rpm) should be registered
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 2);

        let formats: Vec<&str> = pkgs
            .iter()
            .map(|a| a.metadata.get("format").unwrap().as_str())
            .collect();
        assert!(formats.contains(&"deb"));
        assert!(formats.contains(&"rpm"));
    }

    #[test]
    fn test_generate_nfpm_yaml_with_scripts() {
        use anodize_core::config::NfpmScripts;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            scripts: Some(NfpmScripts {
                preinstall: Some("/scripts/preinstall.sh".to_string()),
                postinstall: Some("/scripts/postinstall.sh".to_string()),
                preremove: Some("/scripts/preremove.sh".to_string()),
                postremove: None,
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("scripts:"));
        assert!(yaml.contains("  preinstall: /scripts/preinstall.sh"));
        assert!(yaml.contains("  postinstall: /scripts/postinstall.sh"));
        assert!(yaml.contains("  preremove: /scripts/preremove.sh"));
        assert!(!yaml.contains("postremove"));
    }

    #[test]
    fn test_generate_nfpm_yaml_with_package_metadata() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            recommends: Some(vec!["libfoo".to_string(), "libbar".to_string()]),
            suggests: Some(vec!["optional-pkg".to_string()]),
            conflicts: Some(vec!["old-myapp".to_string()]),
            replaces: Some(vec!["old-myapp".to_string()]),
            provides: Some(vec!["myapp-bin".to_string()]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("recommends:"));
        assert!(yaml.contains("- libfoo"));
        assert!(yaml.contains("- libbar"));
        assert!(yaml.contains("suggests:"));
        assert!(yaml.contains("- optional-pkg"));
        assert!(yaml.contains("conflicts:"));
        assert!(yaml.contains("- old-myapp"));
        assert!(yaml.contains("replaces:"));
        assert!(yaml.contains("provides:"));
        assert!(yaml.contains("- myapp-bin"));
    }

    #[test]
    fn test_generate_nfpm_yaml_with_contents_type_and_file_info() {
        use anodize_core::config::{NfpmContent, NfpmFileInfo};
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            contents: Some(vec![NfpmContent {
                src: "/src/myapp.conf".to_string(),
                dst: "/etc/myapp/myapp.conf".to_string(),
                content_type: Some("config".to_string()),
                file_info: Some(NfpmFileInfo {
                    owner: Some("root".to_string()),
                    group: Some("root".to_string()),
                    mode: Some("0644".to_string()),
                    ..Default::default()
                }),
                packager: None,
                expand: None,
            }]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("  type: config"));
        assert!(yaml.contains("  file_info:"));
        assert!(yaml.contains("    owner: root"));
        assert!(yaml.contains("    group: root"));
        assert!(yaml.contains("    mode: '0644'"));
    }

    #[test]
    fn test_generate_nfpm_yaml_contents_without_file_info() {
        use anodize_core::config::NfpmContent;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            contents: Some(vec![NfpmContent {
                src: "/src/data".to_string(),
                dst: "/var/lib/myapp/data".to_string(),
                content_type: Some("dir".to_string()),
                file_info: None,
                packager: None,
                expand: None,
            }]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("  type: dir"));
        // The binary entry always has file_info with mode 0755, but the
        // extra "dir" content entry should NOT have file_info
        assert!(
            yaml.contains("mode: '0755'"),
            "binary should have mode 0755"
        );
    }

    #[test]
    fn test_config_parse_nfpm_scripts() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        scripts:
          preinstall: /scripts/pre.sh
          postinstall: /scripts/post.sh
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = config.crates[0].nfpm.as_ref().unwrap();
        let scripts = nfpm[0].scripts.as_ref().unwrap();
        assert_eq!(scripts.preinstall.as_deref(), Some("/scripts/pre.sh"));
        assert_eq!(scripts.postinstall.as_deref(), Some("/scripts/post.sh"));
        assert!(scripts.preremove.is_none());
        assert!(scripts.postremove.is_none());
    }

    #[test]
    fn test_config_parse_nfpm_package_relationships() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        recommends:
          - libfoo
        suggests:
          - libbar
        conflicts:
          - old-test
        replaces:
          - old-test
        provides:
          - test-bin
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = config.crates[0].nfpm.as_ref().unwrap();
        assert_eq!(nfpm[0].recommends.as_ref().unwrap(), &["libfoo"]);
        assert_eq!(nfpm[0].suggests.as_ref().unwrap(), &["libbar"]);
        assert_eq!(nfpm[0].conflicts.as_ref().unwrap(), &["old-test"]);
        assert_eq!(nfpm[0].replaces.as_ref().unwrap(), &["old-test"]);
        assert_eq!(nfpm[0].provides.as_ref().unwrap(), &["test-bin"]);
    }

    #[test]
    fn test_config_parse_nfpm_contents_with_type_and_file_info() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        contents:
          - src: /src/conf
            dst: /etc/test/conf
            type: config
            file_info:
              owner: root
              group: wheel
              mode: "0755"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = config.crates[0].nfpm.as_ref().unwrap();
        let contents = nfpm[0].contents.as_ref().unwrap();
        assert_eq!(contents[0].content_type.as_deref(), Some("config"));
        let fi = contents[0].file_info.as_ref().unwrap();
        assert_eq!(fi.owner.as_deref(), Some("root"));
        assert_eq!(fi.group.as_deref(), Some("wheel"));
        assert_eq!(fi.mode.as_deref(), Some("0755"));
    }

    #[test]
    fn test_generate_nfpm_yaml_empty_lists_omitted() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            recommends: Some(vec![]),
            suggests: None,
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        // Empty lists should not produce a section
        assert!(!yaml.contains("recommends:"));
        assert!(!yaml.contains("suggests:"));
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests -- config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_scripts_block_appears_in_generated_yaml() {
        use anodize_core::config::NfpmScripts;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            scripts: Some(NfpmScripts {
                preinstall: Some("/scripts/pre.sh".to_string()),
                postinstall: Some("/scripts/post.sh".to_string()),
                preremove: Some("/scripts/prerm.sh".to_string()),
                postremove: Some("/scripts/postrm.sh".to_string()),
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("scripts:"));
        assert!(yaml.contains("  preinstall: /scripts/pre.sh"));
        assert!(yaml.contains("  postinstall: /scripts/post.sh"));
        assert!(yaml.contains("  preremove: /scripts/prerm.sh"));
        assert!(yaml.contains("  postremove: /scripts/postrm.sh"));
    }

    #[test]
    fn test_all_package_relationship_fields_in_yaml() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            recommends: Some(vec!["libfoo".to_string(), "libbar".to_string()]),
            suggests: Some(vec!["opt-pkg".to_string()]),
            conflicts: Some(vec!["old-myapp".to_string()]),
            replaces: Some(vec!["old-myapp".to_string()]),
            provides: Some(vec!["myapp-bin".to_string()]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);

        // Each field should appear with its items
        assert!(yaml.contains("recommends:\n- libfoo\n- libbar"));
        assert!(yaml.contains("suggests:\n- opt-pkg"));
        assert!(yaml.contains("conflicts:\n- old-myapp"));
        assert!(yaml.contains("replaces:\n- old-myapp"));
        assert!(yaml.contains("provides:\n- myapp-bin"));
    }

    #[test]
    fn test_contents_type_and_file_info_serialize_correctly() {
        use anodize_core::config::{NfpmContent, NfpmFileInfo};
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            contents: Some(vec![
                NfpmContent {
                    src: "/src/config.toml".to_string(),
                    dst: "/etc/myapp/config.toml".to_string(),
                    content_type: Some("config".to_string()),
                    file_info: Some(NfpmFileInfo {
                        owner: Some("root".to_string()),
                        group: Some("admin".to_string()),
                        mode: Some("0640".to_string()),
                        ..Default::default()
                    }),
                    packager: None,
                    expand: None,
                },
                NfpmContent {
                    src: "/src/readme".to_string(),
                    dst: "/usr/share/doc/myapp/README".to_string(),
                    content_type: Some("doc".to_string()),
                    file_info: None,
                    packager: None,
                    expand: None,
                },
            ]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "2.0.0", "/dist/myapp", None);

        // First content entry with type and file_info
        assert!(yaml.contains("- src: /src/config.toml"));
        assert!(yaml.contains("  dst: /etc/myapp/config.toml"));
        assert!(yaml.contains("  type: config"));
        assert!(yaml.contains("  file_info:"));
        assert!(yaml.contains("    owner: root"));
        assert!(yaml.contains("    group: admin"));
        assert!(yaml.contains("    mode: '0640'"));

        // Second content entry with type but no file_info
        assert!(yaml.contains("- src: /src/readme"));
        assert!(yaml.contains("  type: doc"));
    }

    #[test]
    fn test_multiple_formats_in_one_pass() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string(), "rpm".to_string(), "apk".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        // Should register 3 artifacts (one per format)
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 3);

        let formats: Vec<&str> = pkgs
            .iter()
            .map(|a| a.metadata.get("format").unwrap().as_str())
            .collect();
        assert!(formats.contains(&"deb"));
        assert!(formats.contains(&"rpm"));
        assert!(formats.contains(&"apk"));
    }

    #[test]
    fn test_file_name_template_rendering() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            file_name_template: Some(
                "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}".to_string(),
            ),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "3.0.0");

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);

        // The file path should use the rendered template + extension
        let path_str = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        assert!(
            path_str.starts_with("myapp_3.0.0_"),
            "expected file_name_template to be rendered, got: {}",
            path_str
        );
        assert!(path_str.ends_with(".deb"));
    }

    #[test]
    fn test_artifact_registration_of_linux_package() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].kind, ArtifactKind::LinuxPackage);
        assert_eq!(pkgs[0].crate_name, "myapp");
        assert_eq!(pkgs[0].metadata.get("format"), Some(&"deb".to_string()));
    }

    #[test]
    fn test_format_extension_mapping() {
        assert_eq!(format_extension("deb"), ".deb");
        assert_eq!(format_extension("rpm"), ".rpm");
        assert_eq!(format_extension("apk"), ".apk");
        assert_eq!(format_extension("archlinux"), ".pkg.tar.zst");
        assert_eq!(format_extension("unknown"), "");
    }

    #[test]
    fn test_nfpm_yaml_binary_path_included_in_contents() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            bindir: Some("/usr/local/bin".to_string()),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/build/myapp", None);

        // Binary should appear in the contents section
        assert!(yaml.contains("contents:"));
        assert!(yaml.contains("- src: /build/myapp"));
        assert!(yaml.contains("dst: /usr/local/bin/myapp"));
    }

    #[test]
    fn test_nfpm_yaml_custom_bindir() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            bindir: Some("/opt/myapp/bin".to_string()),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/build/myapp", None);
        assert!(yaml.contains("dst: /opt/myapp/bin/myapp"));
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_nfpm_missing_binary_produces_error_in_live_mode() {
        // When nfpm binary is missing, the stage should fail with a clear error
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false, // live mode
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        let stage = NfpmStage;
        let result = stage.run(&mut ctx);
        // nfpm binary likely not installed in test environment
        assert!(result.is_err(), "nfpm binary missing should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nfpm") || err.contains("execute"),
            "error should mention nfpm or execution failure, got: {err}"
        );
    }

    #[test]
    fn test_format_extension_unknown_returns_empty() {
        // Unknown format returns empty extension
        assert_eq!(format_extension("invalid-format"), "");
        assert_eq!(format_extension(""), "");
        assert_eq!(format_extension("snap"), "");
    }

    #[test]
    fn test_generate_nfpm_yaml_without_package_name() {
        // When package_name is None, it should not appear in YAML
        let nfpm_cfg = NfpmConfig {
            package_name: None,
            formats: vec!["deb".to_string()],
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            !yaml.contains("name:"),
            "no name: line should appear when package_name is None"
        );
        assert!(yaml.contains("version: 1.0.0"));
    }

    #[test]
    fn test_generate_nfpm_yaml_minimal_config() {
        // A minimal config with just formats should still produce valid YAML
        let nfpm_cfg = NfpmConfig {
            formats: vec!["deb".to_string()],
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "0.1.0", "/bin/test", None);
        assert!(yaml.contains("version: 0.1.0"));
        assert!(yaml.contains("contents:"));
        assert!(yaml.contains("- src: /bin/test"));
        assert!(yaml.contains("dst: /usr/bin/test"));
    }

    #[test]
    fn test_invalid_file_name_template_errors() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            // Invalid Tera template -- unclosed tag
            file_name_template: Some("{{ bad_template".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true, // dry-run still renders the template
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        let result = NfpmStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "invalid file_name_template should cause a render error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("template") || err.contains("render"),
            "error should mention template rendering, got: {err}"
        );
    }

    #[test]
    fn test_create_output_dir_failure_errors() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        // Use an impossible path that create_dir_all will fail on
        config.dist = std::path::PathBuf::from("/dev/null/impossible/dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false, // live mode triggers create_dir_all
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        let result = NfpmStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "creating output dir under /dev/null should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nfpm") || err.contains("dir") || err.contains("create"),
            "error should mention directory creation context, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // ids filtering and id metadata tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_ids_filter_includes_matching_binaries_only() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // nfpm config that only wants binaries with id "build-server"
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ids: Some(vec!["build-server".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        // Add two linux binary artifacts: one matching the ids filter, one not
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp-server"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-server".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp-cli"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-cli".to_string())]),
            size: None,
        });

        NfpmStage.run(&mut ctx).unwrap();

        // Only the "build-server" binary should produce a package
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1, "only one binary matched ids filter");
    }

    #[test]
    fn test_ids_filter_no_match_produces_no_packages() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ids: Some(vec!["nonexistent-build".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        // Binary exists but its id doesn't match the filter
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-default".to_string())]),
            size: None,
        });

        NfpmStage.run(&mut ctx).unwrap();

        // No packages should be created since filter matched nothing
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 0, "no binaries matched ids filter");
    }

    #[test]
    fn test_no_ids_includes_all_binaries() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // No ids set -- should include all binaries (backward compat)
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ids: None,
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        // Add two linux binary artifacts with different ids
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp-server"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-server".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp-cli"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-cli".to_string())]),
            size: None,
        });

        NfpmStage.run(&mut ctx).unwrap();

        // Both binaries should produce packages
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 2, "all binaries included when ids is None");
    }

    #[test]
    fn test_id_metadata_set_on_created_artifacts() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            id: Some("server-pkg".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(
            pkgs[0].metadata.get("id"),
            Some(&"server-pkg".to_string()),
            "nfpm config id should be in artifact metadata"
        );
        // format should still be present
        assert_eq!(pkgs[0].metadata.get("format"), Some(&"deb".to_string()),);
    }

    #[test]
    fn test_no_id_means_no_id_in_metadata() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            id: None,
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(
            pkgs[0].metadata.get("id"),
            None,
            "no id in metadata when nfpm config has no id"
        );
    }

    #[test]
    fn test_ids_filter_with_multiple_matching_ids() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // ids filter accepts both "build-server" and "build-cli"
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ids: Some(vec!["build-server".to_string(), "build-cli".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        // Add three binaries: two match, one does not
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp-server"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-server".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp-cli"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-cli".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp-worker"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-worker".to_string())]),
            size: None,
        });

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 2, "two binaries should match the ids filter");
    }

    #[test]
    fn test_nfpm_yaml_dependencies_serializes_as_flat_depends() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            dependencies: Some({
                let mut m = HashMap::new();
                m.insert(
                    "deb".to_string(),
                    vec!["libc6".to_string(), "libssl-dev".to_string()],
                );
                m
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/usr/bin/myapp", Some("deb"));
        // The YAML key must be "depends" (what nfpm expects), not "dependencies"
        assert!(
            yaml.contains("depends:"),
            "YAML should contain 'depends:' key, got:\n{}",
            yaml
        );
        assert!(
            !yaml.contains("dependencies:"),
            "YAML should NOT contain 'dependencies:' key, got:\n{}",
            yaml
        );
        // Should be a flat list, not a nested map
        assert!(
            yaml.contains("- libc6"),
            "YAML should contain flat dep 'libc6', got:\n{}",
            yaml
        );
        assert!(
            yaml.contains("- libssl-dev"),
            "YAML should contain flat dep 'libssl-dev', got:\n{}",
            yaml
        );
    }

    #[test]
    fn test_nfpm_yaml_dependencies_format_filtering() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string(), "rpm".to_string()],
            dependencies: Some({
                let mut m = HashMap::new();
                m.insert("deb".to_string(), vec!["libc6".to_string()]);
                m.insert("rpm".to_string(), vec!["glibc".to_string()]);
                m
            }),
            ..Default::default()
        };

        // When generating for deb, only deb deps should appear
        let yaml_deb = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/usr/bin/myapp", Some("deb"));
        assert!(
            yaml_deb.contains("- libc6"),
            "deb deps expected:\n{yaml_deb}"
        );
        assert!(
            !yaml_deb.contains("glibc"),
            "rpm deps should not appear in deb yaml:\n{yaml_deb}"
        );

        // When generating for rpm, only rpm deps should appear
        let yaml_rpm = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/usr/bin/myapp", Some("rpm"));
        assert!(
            yaml_rpm.contains("- glibc"),
            "rpm deps expected:\n{yaml_rpm}"
        );
        assert!(
            !yaml_rpm.contains("libc6"),
            "deb deps should not appear in rpm yaml:\n{yaml_rpm}"
        );
    }

    #[test]
    fn test_nfpm_yaml_dependencies_none_format_merges_all() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string(), "rpm".to_string()],
            dependencies: Some({
                let mut m = HashMap::new();
                m.insert("deb".to_string(), vec!["libc6".to_string()]);
                m.insert("rpm".to_string(), vec!["glibc".to_string()]);
                m
            }),
            ..Default::default()
        };

        // When format is None, all deps should be merged
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/usr/bin/myapp", None);
        assert!(yaml.contains("depends:"), "depends key expected:\n{yaml}");
        assert!(
            yaml.contains("- libc6") || yaml.contains("- glibc"),
            "at least some deps expected:\n{yaml}"
        );
    }

    // -----------------------------------------------------------------------
    // Task 9: nFPM parity -- versioning, metadata, format-specific, mtime
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_nfpm_yaml_version_fields() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            epoch: Some("1".to_string()),
            release: Some("2".to_string()),
            prerelease: Some("beta1".to_string()),
            version_metadata: Some("git.abc123".to_string()),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            yaml.contains("epoch: '1'"),
            "epoch missing from YAML:\n{yaml}"
        );
        assert!(
            yaml.contains("release: '2'"),
            "release missing from YAML:\n{yaml}"
        );
        assert!(
            yaml.contains("prerelease: beta1"),
            "prerelease missing from YAML:\n{yaml}"
        );
        assert!(
            yaml.contains("version_metadata: git.abc123"),
            "version_metadata missing from YAML:\n{yaml}"
        );
    }

    #[test]
    fn test_generate_nfpm_yaml_package_metadata_fields() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            section: Some("utils".to_string()),
            priority: Some("optional".to_string()),
            meta: Some(true),
            umask: Some("0o002".to_string()),
            mtime: Some("2023-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("section: utils"), "section missing:\n{yaml}");
        assert!(
            yaml.contains("priority: optional"),
            "priority missing:\n{yaml}"
        );
        assert!(yaml.contains("meta: true"), "meta missing:\n{yaml}");
        assert!(yaml.contains("umask: '0o002'"), "umask missing:\n{yaml}");
        assert!(
            yaml.contains("mtime: 2023-01-01T00:00:00Z")
                || yaml.contains("mtime: '2023-01-01T00:00:00Z'"),
            "mtime missing:\n{yaml}"
        );
    }

    #[test]
    fn test_generate_nfpm_yaml_metadata_fields_omitted_when_none() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(!yaml.contains("epoch:"), "epoch should not appear:\n{yaml}");
        assert!(
            !yaml.contains("release:"),
            "release should not appear:\n{yaml}"
        );
        assert!(
            !yaml.contains("prerelease:"),
            "prerelease should not appear:\n{yaml}"
        );
        assert!(
            !yaml.contains("version_metadata:"),
            "version_metadata should not appear:\n{yaml}"
        );
        assert!(
            !yaml.contains("section:"),
            "section should not appear:\n{yaml}"
        );
        assert!(
            !yaml.contains("priority:"),
            "priority should not appear:\n{yaml}"
        );
        assert!(!yaml.contains("meta:"), "meta should not appear:\n{yaml}");
        assert!(!yaml.contains("umask:"), "umask should not appear:\n{yaml}");
        assert!(
            !yaml.contains("mtime:"),
            "top-level mtime should not appear:\n{yaml}"
        );
        assert!(!yaml.contains("rpm:"), "rpm should not appear:\n{yaml}");
        assert!(!yaml.contains("deb:"), "deb should not appear:\n{yaml}");
        assert!(!yaml.contains("apk:"), "apk should not appear:\n{yaml}");
        assert!(
            !yaml.contains("archlinux:"),
            "archlinux should not appear:\n{yaml}"
        );
    }

    #[test]
    fn test_generate_nfpm_yaml_file_info_mtime() {
        use anodize_core::config::{NfpmContent, NfpmFileInfo};
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            contents: Some(vec![NfpmContent {
                src: "/src/file".to_string(),
                dst: "/usr/bin/file".to_string(),
                content_type: None,
                file_info: Some(NfpmFileInfo {
                    owner: Some("root".to_string()),
                    group: Some("root".to_string()),
                    mode: Some("0755".to_string()),
                    mtime: Some("2023-01-01T00:00:00Z".to_string()),
                }),
                packager: None,
                expand: None,
            }]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            yaml.contains("file_info:"),
            "file_info block missing:\n{yaml}"
        );
        assert!(
            yaml.contains("mtime: 2023-01-01T00:00:00Z")
                || yaml.contains("mtime: '2023-01-01T00:00:00Z'"),
            "file_info mtime missing:\n{yaml}"
        );
        assert!(yaml.contains("owner: root"), "owner missing:\n{yaml}");
        assert!(yaml.contains("mode: '0755'"), "mode missing:\n{yaml}");
    }

    #[test]
    fn test_generate_nfpm_yaml_rpm_config() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["rpm".to_string()],
            rpm: Some(NfpmRpmConfig {
                summary: Some("My package summary".to_string()),
                compression: Some("lzma".to_string()),
                group: Some("System/Tools".to_string()),
                packager: Some("Build Team <build@example.com>".to_string()),
                signature: Some(NfpmSignatureConfig {
                    key_file: Some("/path/to/key.gpg".to_string()),
                    key_id: Some("ABCD1234".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("rpm:"), "rpm section missing:\n{yaml}");
        assert!(
            yaml.contains("summary: My package summary"),
            "rpm summary missing:\n{yaml}"
        );
        assert!(
            yaml.contains("compression: lzma"),
            "rpm compression missing:\n{yaml}"
        );
        assert!(
            yaml.contains("group: System/Tools"),
            "rpm group missing:\n{yaml}"
        );
        assert!(
            yaml.contains("packager: Build Team <build@example.com>"),
            "rpm packager missing:\n{yaml}"
        );
        assert!(
            yaml.contains("signature:"),
            "rpm signature missing:\n{yaml}"
        );
        assert!(
            yaml.contains("key_file: /path/to/key.gpg"),
            "rpm key_file missing:\n{yaml}"
        );
        assert!(
            yaml.contains("key_id: ABCD1234"),
            "rpm key_id missing:\n{yaml}"
        );
    }

    #[test]
    fn test_generate_nfpm_yaml_rpm_prefixes() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["rpm".to_string()],
            rpm: Some(NfpmRpmConfig {
                prefixes: Some(vec!["/usr".to_string(), "/etc".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("prefixes:"), "rpm prefixes missing:\n{yaml}");
        assert!(yaml.contains("- /usr"), "rpm prefix /usr missing:\n{yaml}");
        assert!(yaml.contains("- /etc"), "rpm prefix /etc missing:\n{yaml}");
    }

    #[test]
    fn test_generate_nfpm_yaml_deb_config() {
        use anodize_core::config::NfpmDebTriggers;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            deb: Some(NfpmDebConfig {
                triggers: Some(NfpmDebTriggers {
                    interest: Some(vec!["/usr/share/applications".to_string()]),
                    activate: Some(vec!["ldconfig".to_string()]),
                    ..Default::default()
                }),
                breaks: Some(vec!["oldpackage (<< 2.0)".to_string()]),
                lintian_overrides: Some(vec!["statically-linked-binary".to_string()]),
                signature: Some(NfpmSignatureConfig {
                    key_file: Some("/path/to/key.gpg".to_string()),
                    ..Default::default()
                }),
                fields: Some({
                    let mut m = HashMap::new();
                    m.insert(
                        "Bugs".to_string(),
                        "https://github.com/example/project/issues".to_string(),
                    );
                    m
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("deb:"), "deb section missing:\n{yaml}");
        assert!(yaml.contains("triggers:"), "deb triggers missing:\n{yaml}");
        assert!(
            yaml.contains("interest:"),
            "deb interest triggers missing:\n{yaml}"
        );
        assert!(
            yaml.contains("- /usr/share/applications"),
            "deb interest value missing:\n{yaml}"
        );
        assert!(
            yaml.contains("activate:"),
            "deb activate triggers missing:\n{yaml}"
        );
        assert!(
            yaml.contains("- ldconfig"),
            "deb activate value missing:\n{yaml}"
        );
        assert!(yaml.contains("breaks:"), "deb breaks missing:\n{yaml}");
        assert!(
            yaml.contains("- oldpackage (<< 2.0)"),
            "deb breaks value missing:\n{yaml}"
        );
        assert!(
            yaml.contains("lintian_overrides:"),
            "deb lintian_overrides missing:\n{yaml}"
        );
        assert!(
            yaml.contains("- statically-linked-binary"),
            "deb lintian_overrides value missing:\n{yaml}"
        );
        assert!(
            yaml.contains("signature:"),
            "deb signature missing:\n{yaml}"
        );
        assert!(
            yaml.contains("key_file: /path/to/key.gpg"),
            "deb key_file missing:\n{yaml}"
        );
        assert!(yaml.contains("fields:"), "deb fields missing:\n{yaml}");
        assert!(
            yaml.contains("Bugs:"),
            "deb fields Bugs key missing:\n{yaml}"
        );
    }

    #[test]
    fn test_generate_nfpm_yaml_deb_compression_and_predepends() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            deb: Some(NfpmDebConfig {
                compression: Some("xz".to_string()),
                predepends: Some(vec!["libc6".to_string(), "dpkg".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            yaml.contains("compression: xz"),
            "deb compression missing:\n{yaml}"
        );
        assert!(
            yaml.contains("predepends:"),
            "deb predepends missing:\n{yaml}"
        );
        assert!(
            yaml.contains("- libc6"),
            "predepends libc6 missing:\n{yaml}"
        );
        assert!(yaml.contains("- dpkg"), "predepends dpkg missing:\n{yaml}");
    }

    #[test]
    fn test_generate_nfpm_yaml_apk_config() {
        use anodize_core::config::NfpmApkConfig;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["apk".to_string()],
            apk: Some(NfpmApkConfig {
                signature: Some(NfpmSignatureConfig {
                    key_file: Some("/path/to/key.rsa".to_string()),
                    ..Default::default()
                }),
                scripts: None,
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("apk:"), "apk section missing:\n{yaml}");
        assert!(
            yaml.contains("signature:"),
            "apk signature missing:\n{yaml}"
        );
        assert!(
            yaml.contains("key_file: /path/to/key.rsa"),
            "apk key_file missing:\n{yaml}"
        );
    }

    #[test]
    fn test_generate_nfpm_yaml_archlinux_config() {
        use anodize_core::config::{NfpmArchlinuxConfig, NfpmArchlinuxScripts};
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["archlinux".to_string()],
            archlinux: Some(NfpmArchlinuxConfig {
                pkgbase: Some("myapp-base".to_string()),
                packager: Some("Build Team <build@example.com>".to_string()),
                scripts: Some(NfpmArchlinuxScripts {
                    preupgrade: Some("scripts/preupgrade.sh".to_string()),
                    postupgrade: Some("scripts/postupgrade.sh".to_string()),
                }),
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            yaml.contains("archlinux:"),
            "archlinux section missing:\n{yaml}"
        );
        assert!(
            yaml.contains("pkgbase: myapp-base"),
            "archlinux pkgbase missing:\n{yaml}"
        );
        assert!(
            yaml.contains("packager: Build Team <build@example.com>"),
            "archlinux packager missing:\n{yaml}"
        );
        assert!(
            yaml.contains("scripts:"),
            "archlinux scripts missing:\n{yaml}"
        );
        assert!(
            yaml.contains("preupgrade: scripts/preupgrade.sh"),
            "archlinux preupgrade missing:\n{yaml}"
        );
        assert!(
            yaml.contains("postupgrade: scripts/postupgrade.sh"),
            "archlinux postupgrade missing:\n{yaml}"
        );
    }

    #[test]
    fn test_generate_nfpm_yaml_signature_key_passphrase() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["rpm".to_string()],
            rpm: Some(NfpmRpmConfig {
                signature: Some(NfpmSignatureConfig {
                    key_file: Some("/path/to/key.gpg".to_string()),
                    key_id: Some("ABCD1234".to_string()),
                    key_passphrase: Some("secret123".to_string()),
                    key_name: None,
                    type_: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            yaml.contains("key_passphrase: secret123"),
            "key_passphrase missing from signature:\n{yaml}"
        );
    }

    #[test]
    fn test_generate_nfpm_yaml_deb_triggers_all_fields() {
        use anodize_core::config::NfpmDebTriggers;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            deb: Some(NfpmDebConfig {
                triggers: Some(NfpmDebTriggers {
                    interest: Some(vec!["/usr/share/apps".to_string()]),
                    interest_await: Some(vec!["/usr/share/icons".to_string()]),
                    interest_noawait: Some(vec!["/usr/share/mime".to_string()]),
                    activate: Some(vec!["ldconfig".to_string()]),
                    activate_await: Some(vec!["triggers-await".to_string()]),
                    activate_noawait: Some(vec!["triggers-noawait".to_string()]),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("interest:"), "interest missing:\n{yaml}");
        assert!(
            yaml.contains("interest_await:"),
            "interest_await missing:\n{yaml}"
        );
        assert!(
            yaml.contains("interest_noawait:"),
            "interest_noawait missing:\n{yaml}"
        );
        assert!(yaml.contains("activate:"), "activate missing:\n{yaml}");
        assert!(
            yaml.contains("activate_await:"),
            "activate_await missing:\n{yaml}"
        );
        assert!(
            yaml.contains("activate_noawait:"),
            "activate_noawait missing:\n{yaml}"
        );
    }

    #[test]
    fn test_format_extension_termux_deb() {
        assert_eq!(format_extension("termux.deb"), ".deb");
    }

    #[test]
    fn test_format_extension_ipk() {
        assert_eq!(format_extension("ipk"), ".ipk");
    }

    #[test]
    fn test_termux_deb_format_produces_artifact() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["termux.deb".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(
            pkgs[0].metadata.get("format"),
            Some(&"termux.deb".to_string())
        );
        let path_str = pkgs[0].path.to_string_lossy();
        assert!(
            path_str.ends_with(".deb"),
            "termux.deb package should have .deb extension, got: {path_str}"
        );
    }

    #[test]
    fn test_ipk_format_produces_artifact() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["ipk".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].metadata.get("format"), Some(&"ipk".to_string()));
        let path_str = pkgs[0].path.to_string_lossy();
        assert!(
            path_str.ends_with(".ipk"),
            "ipk package should have .ipk extension, got: {path_str}"
        );
    }

    #[test]
    fn test_config_parse_nfpm_all_new_fields() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        epoch: "1"
        release: "2"
        prerelease: beta1
        version_metadata: git.abc123
        section: utils
        priority: optional
        meta: true
        umask: "0o002"
        mtime: "2023-01-01T00:00:00Z"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        assert_eq!(nfpm.epoch.as_deref(), Some("1"));
        assert_eq!(nfpm.release.as_deref(), Some("2"));
        assert_eq!(nfpm.prerelease.as_deref(), Some("beta1"));
        assert_eq!(nfpm.version_metadata.as_deref(), Some("git.abc123"));
        assert_eq!(nfpm.section.as_deref(), Some("utils"));
        assert_eq!(nfpm.priority.as_deref(), Some("optional"));
        assert_eq!(nfpm.meta, Some(true));
        assert_eq!(nfpm.umask.as_deref(), Some("0o002"));
        assert_eq!(nfpm.mtime.as_deref(), Some("2023-01-01T00:00:00Z"));
    }

    #[test]
    fn test_config_parse_nfpm_rpm_config() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [rpm]
        rpm:
          summary: "My package summary"
          compression: lzma
          group: System/Tools
          packager: "Build Team <build@example.com>"
          prefixes:
            - /usr
            - /etc
          signature:
            key_file: /path/to/key.gpg
            key_id: ABCD1234
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        let rpm = nfpm.rpm.as_ref().unwrap();
        assert_eq!(rpm.summary.as_deref(), Some("My package summary"));
        assert_eq!(rpm.compression.as_deref(), Some("lzma"));
        assert_eq!(rpm.group.as_deref(), Some("System/Tools"));
        assert_eq!(
            rpm.packager.as_deref(),
            Some("Build Team <build@example.com>")
        );
        assert_eq!(rpm.prefixes.as_ref().unwrap(), &["/usr", "/etc"]);
        let sig = rpm.signature.as_ref().unwrap();
        assert_eq!(sig.key_file.as_deref(), Some("/path/to/key.gpg"));
        assert_eq!(sig.key_id.as_deref(), Some("ABCD1234"));
    }

    #[test]
    fn test_config_parse_nfpm_deb_config() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        deb:
          compression: xz
          predepends:
            - libc6
          triggers:
            interest:
              - /usr/share/applications
            activate:
              - ldconfig
          breaks:
            - "oldpackage (<< 2.0)"
          lintian_overrides:
            - statically-linked-binary
          signature:
            key_file: /path/to/key.gpg
          fields:
            Bugs: "https://github.com/example/project/issues"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        let deb = nfpm.deb.as_ref().unwrap();
        assert_eq!(deb.compression.as_deref(), Some("xz"));
        assert_eq!(deb.predepends.as_ref().unwrap(), &["libc6"]);
        let triggers = deb.triggers.as_ref().unwrap();
        assert_eq!(
            triggers.interest.as_ref().unwrap(),
            &["/usr/share/applications"]
        );
        assert_eq!(triggers.activate.as_ref().unwrap(), &["ldconfig"]);
        assert_eq!(deb.breaks.as_ref().unwrap(), &["oldpackage (<< 2.0)"]);
        assert_eq!(
            deb.lintian_overrides.as_ref().unwrap(),
            &["statically-linked-binary"]
        );
        let sig = deb.signature.as_ref().unwrap();
        assert_eq!(sig.key_file.as_deref(), Some("/path/to/key.gpg"));
        let fields = deb.fields.as_ref().unwrap();
        assert_eq!(
            fields.get("Bugs").unwrap(),
            "https://github.com/example/project/issues"
        );
    }

    #[test]
    fn test_config_parse_nfpm_apk_config() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [apk]
        apk:
          signature:
            key_file: /path/to/key.rsa
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        let apk = nfpm.apk.as_ref().unwrap();
        let sig = apk.signature.as_ref().unwrap();
        assert_eq!(sig.key_file.as_deref(), Some("/path/to/key.rsa"));
    }

    #[test]
    fn test_config_parse_nfpm_archlinux_config() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [archlinux]
        archlinux:
          pkgbase: myapp-base
          packager: "Build Team <build@example.com>"
          scripts:
            preupgrade: scripts/preupgrade.sh
            postupgrade: scripts/postupgrade.sh
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        let arch = nfpm.archlinux.as_ref().unwrap();
        assert_eq!(arch.pkgbase.as_deref(), Some("myapp-base"));
        assert_eq!(
            arch.packager.as_deref(),
            Some("Build Team <build@example.com>")
        );
        let scripts = arch.scripts.as_ref().unwrap();
        assert_eq!(scripts.preupgrade.as_deref(), Some("scripts/preupgrade.sh"));
        assert_eq!(
            scripts.postupgrade.as_deref(),
            Some("scripts/postupgrade.sh")
        );
    }

    #[test]
    fn test_config_parse_nfpm_file_info_mtime() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        contents:
          - src: /src/file
            dst: /usr/bin/file
            file_info:
              owner: root
              group: root
              mode: "0755"
              mtime: "2023-01-01T00:00:00Z"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        let fi = nfpm.contents.as_ref().unwrap()[0]
            .file_info
            .as_ref()
            .unwrap();
        assert_eq!(fi.owner.as_deref(), Some("root"));
        assert_eq!(fi.mtime.as_deref(), Some("2023-01-01T00:00:00Z"));
    }

    #[test]
    fn test_config_parse_nfpm_signature_config() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [rpm]
        rpm:
          signature:
            key_file: /path/to/key.gpg
            key_id: ABCD1234
            key_passphrase: secret
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        let sig = nfpm.rpm.as_ref().unwrap().signature.as_ref().unwrap();
        assert_eq!(sig.key_file.as_deref(), Some("/path/to/key.gpg"));
        assert_eq!(sig.key_id.as_deref(), Some("ABCD1234"));
        assert_eq!(sig.key_passphrase.as_deref(), Some("secret"));
    }

    #[test]
    fn test_config_parse_nfpm_deb_triggers_all_fields() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        deb:
          triggers:
            interest:
              - /usr/share/apps
            interest_await:
              - /usr/share/icons
            interest_noawait:
              - /usr/share/mime
            activate:
              - ldconfig
            activate_await:
              - triggers-await
            activate_noawait:
              - triggers-noawait
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        let triggers = nfpm.deb.as_ref().unwrap().triggers.as_ref().unwrap();
        assert_eq!(triggers.interest.as_ref().unwrap(), &["/usr/share/apps"]);
        assert_eq!(
            triggers.interest_await.as_ref().unwrap(),
            &["/usr/share/icons"]
        );
        assert_eq!(
            triggers.interest_noawait.as_ref().unwrap(),
            &["/usr/share/mime"]
        );
        assert_eq!(triggers.activate.as_ref().unwrap(), &["ldconfig"]);
        assert_eq!(
            triggers.activate_await.as_ref().unwrap(),
            &["triggers-await"]
        );
        assert_eq!(
            triggers.activate_noawait.as_ref().unwrap(),
            &["triggers-noawait"]
        );
    }

    #[test]
    fn test_generate_nfpm_yaml_all_format_sections_together() {
        use anodize_core::config::{
            NfpmApkConfig, NfpmArchlinuxConfig, NfpmArchlinuxScripts, NfpmDebTriggers,
        };
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec![
                "deb".to_string(),
                "rpm".to_string(),
                "apk".to_string(),
                "archlinux".to_string(),
            ],
            epoch: Some("2".to_string()),
            release: Some("3".to_string()),
            section: Some("devel".to_string()),
            priority: Some("required".to_string()),
            meta: Some(false),
            umask: Some("0o022".to_string()),
            mtime: Some("2024-06-01T12:00:00Z".to_string()),
            rpm: Some(NfpmRpmConfig {
                summary: Some("RPM summary".to_string()),
                compression: Some("xz".to_string()),
                ..Default::default()
            }),
            deb: Some(NfpmDebConfig {
                triggers: Some(NfpmDebTriggers {
                    interest: Some(vec!["/trigger/path".to_string()]),
                    ..Default::default()
                }),
                breaks: Some(vec!["broken-pkg".to_string()]),
                ..Default::default()
            }),
            apk: Some(NfpmApkConfig {
                signature: Some(NfpmSignatureConfig {
                    key_file: Some("/apk/key.rsa".to_string()),
                    ..Default::default()
                }),
                scripts: None,
            }),
            archlinux: Some(NfpmArchlinuxConfig {
                pkgbase: Some("base-pkg".to_string()),
                packager: Some("Packager <p@example.com>".to_string()),
                scripts: Some(NfpmArchlinuxScripts {
                    preupgrade: Some("pre.sh".to_string()),
                    postupgrade: Some("post.sh".to_string()),
                }),
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "2.0.0", "/dist/myapp", None);

        // Verify all sections present
        assert!(yaml.contains("epoch:"), "epoch missing:\n{yaml}");
        assert!(yaml.contains("release:"), "release missing:\n{yaml}");
        assert!(yaml.contains("section: devel"), "section missing:\n{yaml}");
        assert!(
            yaml.contains("priority: required"),
            "priority missing:\n{yaml}"
        );
        assert!(yaml.contains("meta: false"), "meta missing:\n{yaml}");
        assert!(yaml.contains("umask:"), "umask missing:\n{yaml}");
        assert!(yaml.contains("mtime:"), "mtime missing:\n{yaml}");
        assert!(yaml.contains("rpm:"), "rpm section missing:\n{yaml}");
        assert!(
            yaml.contains("summary: RPM summary"),
            "rpm summary missing:\n{yaml}"
        );
        assert!(yaml.contains("deb:"), "deb section missing:\n{yaml}");
        assert!(yaml.contains("breaks:"), "deb breaks missing:\n{yaml}");
        assert!(yaml.contains("apk:"), "apk section missing:\n{yaml}");
        assert!(
            yaml.contains("archlinux:"),
            "archlinux section missing:\n{yaml}"
        );
        assert!(
            yaml.contains("pkgbase: base-pkg"),
            "archlinux pkgbase missing:\n{yaml}"
        );
    }

    #[test]
    fn test_config_parse_nfpm_termux_deb_and_ipk_formats() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb, termux.deb, ipk, rpm]
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        assert_eq!(nfpm.formats, vec!["deb", "termux.deb", "ipk", "rpm"]);
    }

    #[test]
    fn test_meta_false_emits_in_yaml() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            meta: Some(false),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            yaml.contains("meta: false"),
            "meta: false should appear in YAML:\n{yaml}"
        );
    }

    #[test]
    fn test_meta_none_omits_from_yaml() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            meta: None,
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            !yaml.contains("meta:"),
            "meta should not appear when None:\n{yaml}"
        );
    }

    // -----------------------------------------------------------------------
    // Format validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_format_accepts_known_formats() {
        for fmt in KNOWN_FORMATS {
            assert!(validate_format(fmt).is_ok(), "format {fmt} should be valid");
        }
    }

    #[test]
    fn test_validate_format_rejects_unknown() {
        let result = validate_format("bogus");
        assert!(result.is_err(), "bogus format should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bogus"),
            "error should mention the bad format: {err}"
        );
        assert!(
            err.contains("deb"),
            "error should list known formats: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Default filename includes arch, ConventionalFileName, nfpm --target path
    // -----------------------------------------------------------------------

    #[test]
    fn test_default_filename_includes_arch() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "2.0.0");

        // Add a linux binary so the arch is derived from its target triple
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);
        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        // Should be myapp_2.0.0_linux_arm64.deb (os and arch included in default name)
        assert_eq!(
            filename, "myapp_2.0.0_linux_arm64.deb",
            "default filename should include os and arch, got: {filename}"
        );
    }

    #[test]
    fn test_default_filename_no_overwrite_multiple_arches() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        // Two different arches for the same crate
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp-x86"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp-arm"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 2);
        let filenames: Vec<&str> = pkgs
            .iter()
            .map(|a| a.path.file_name().unwrap().to_str().unwrap())
            .collect();
        // The two packages must have distinct filenames
        assert_ne!(
            filenames[0], filenames[1],
            "multi-arch packages should not share a filename: {:?}",
            filenames
        );
        assert!(
            filenames.iter().any(|f| f.contains("amd64")),
            "should contain amd64 variant: {:?}",
            filenames
        );
        assert!(
            filenames.iter().any(|f| f.contains("arm64")),
            "should contain arm64 variant: {:?}",
            filenames
        );
    }

    #[test]
    fn test_conventional_filename_template_var() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Use ConventionalFileName in the template
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["rpm".to_string()],
            file_name_template: Some("{{ .ConventionalFileName }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "5.0.0");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);
        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        // ConventionalFileName = "myapp_5.0.0_linux_amd64.rpm" (already includes ext).
        // The code detects the rendered template already ends with the format
        // extension and does NOT double-append it.
        assert_eq!(
            filename, "myapp_5.0.0_linux_amd64.rpm",
            "ConventionalFileName should produce the full conventional name, got: {filename}"
        );
    }

    #[test]
    fn test_conventional_extension_template_var() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Use ConventionalExtension in the template
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            file_name_template: Some(
                "{{ .PackageName }}_{{ .Version }}_{{ .Arch }}{{ .ConventionalExtension }}"
                    .to_string(),
            ),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);
        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        // Template renders: "myapp_1.0.0_amd64.deb", then ext ".deb" is appended
        // => "myapp_1.0.0_amd64.deb.deb" -- double extension!
        // This means ConventionalExtension should NOT be used together with
        // the auto-appended extension.  We need to fix the code so that
        // when the rendered template already ends with the extension, we skip
        // appending it.
        assert!(
            filename.ends_with(".deb"),
            "should end with .deb, got: {filename}"
        );
        assert!(
            !filename.ends_with(".deb.deb"),
            "should NOT double the extension, got: {filename}"
        );
    }

    #[test]
    fn test_format_template_var_set() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["rpm".to_string()],
            file_name_template: Some("{{ .PackageName }}-{{ .Format }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);
        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            filename, "myapp-rpm.rpm",
            "Format template var should resolve to the packager format, got: {filename}"
        );
    }

    #[test]
    fn test_nfpm_target_is_file_path_not_directory() {
        // When nfpm_command is called, --target should be a file path
        let cmd = nfpm_command("/tmp/nfpm.yaml", "deb", "/tmp/output/myapp_1.0.0_amd64.deb");
        assert_eq!(cmd[7], "/tmp/output/myapp_1.0.0_amd64.deb");
    }

    #[test]
    fn test_template_vars_cleared_after_stage() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        // All nfpm-specific vars should be cleared after the stage runs
        assert_eq!(ctx.template_vars().get("Format"), Some(&String::new()));
        assert_eq!(ctx.template_vars().get("PackageName"), Some(&String::new()));
        assert_eq!(
            ctx.template_vars().get("ConventionalExtension"),
            Some(&String::new())
        );
        assert_eq!(
            ctx.template_vars().get("ConventionalFileName"),
            Some(&String::new())
        );
    }

    #[test]
    fn test_stage_rejects_unknown_format() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["bogus".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        let result = NfpmStage.run(&mut ctx);
        assert!(result.is_err(), "bogus format should cause an error");
        let err = format!("{:#}", result.unwrap_err());
        assert!(
            err.contains("bogus") || err.contains("unknown"),
            "error should mention the unknown format: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Fix: signature key_name and type_ fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_signature_key_name_and_type_in_yaml() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            deb: Some(NfpmDebConfig {
                signature: Some(NfpmSignatureConfig {
                    key_file: Some("/path/to/key.gpg".to_string()),
                    key_name: Some("mykey.rsa.pub".to_string()),
                    type_: Some("origin".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            yaml.contains("key_name: mykey.rsa.pub"),
            "key_name missing from signature:\n{yaml}"
        );
        assert!(
            yaml.contains("type: origin"),
            "type missing from signature:\n{yaml}"
        );
    }

    #[test]
    fn test_signature_key_name_and_type_omitted_when_none() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["rpm".to_string()],
            rpm: Some(NfpmRpmConfig {
                signature: Some(NfpmSignatureConfig {
                    key_file: Some("/path/to/key.gpg".to_string()),
                    key_name: None,
                    type_: None,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            !yaml.contains("key_name:"),
            "key_name should not appear when None:\n{yaml}"
        );
        // "type:" could appear from content entries, so check specifically
        // within the signature block by verifying it doesn't appear after key_file
        assert!(
            yaml.contains("key_file: /path/to/key.gpg"),
            "key_file should be present:\n{yaml}"
        );
    }

    // -----------------------------------------------------------------------
    // Fix: content packager and expand fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_content_packager_and_expand_in_yaml() {
        use anodize_core::config::NfpmContent;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            contents: Some(vec![NfpmContent {
                src: "/src/config".to_string(),
                dst: "/etc/myapp/config".to_string(),
                content_type: None,
                file_info: None,
                packager: Some("deb".to_string()),
                expand: Some(true),
            }]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            yaml.contains("packager: deb"),
            "content packager missing from YAML:\n{yaml}"
        );
        assert!(
            yaml.contains("expand: true"),
            "content expand missing from YAML:\n{yaml}"
        );
    }

    #[test]
    fn test_content_packager_and_expand_omitted_when_none() {
        use anodize_core::config::NfpmContent;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            contents: Some(vec![NfpmContent {
                src: "/src/data".to_string(),
                dst: "/var/lib/myapp/data".to_string(),
                content_type: None,
                file_info: None,
                packager: None,
                expand: None,
            }]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            !yaml.contains("packager:"),
            "packager should not appear when None:\n{yaml}"
        );
        assert!(
            !yaml.contains("expand:"),
            "expand should not appear when None:\n{yaml}"
        );
    }

    // -----------------------------------------------------------------------
    // Fix: APK scripts field
    // -----------------------------------------------------------------------

    #[test]
    fn test_apk_scripts_in_yaml() {
        use anodize_core::config::{NfpmApkConfig, NfpmApkScripts};
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["apk".to_string()],
            apk: Some(NfpmApkConfig {
                signature: None,
                scripts: Some(NfpmApkScripts {
                    preupgrade: Some("scripts/apk-preupgrade.sh".to_string()),
                    postupgrade: Some("scripts/apk-postupgrade.sh".to_string()),
                }),
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(yaml.contains("apk:"), "apk section missing:\n{yaml}");
        assert!(
            yaml.contains("scripts:"),
            "apk scripts section missing:\n{yaml}"
        );
        assert!(
            yaml.contains("preupgrade: scripts/apk-preupgrade.sh"),
            "apk preupgrade missing:\n{yaml}"
        );
        assert!(
            yaml.contains("postupgrade: scripts/apk-postupgrade.sh"),
            "apk postupgrade missing:\n{yaml}"
        );
    }

    #[test]
    fn test_apk_scripts_omitted_when_none() {
        use anodize_core::config::NfpmApkConfig;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["apk".to_string()],
            apk: Some(NfpmApkConfig {
                signature: Some(NfpmSignatureConfig {
                    key_file: Some("/path/to/key.rsa".to_string()),
                    ..Default::default()
                }),
                scripts: None,
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp", None);
        assert!(
            yaml.contains("apk:"),
            "apk section should be present:\n{yaml}"
        );
        assert!(
            yaml.contains("key_file: /path/to/key.rsa"),
            "apk signature should be present:\n{yaml}"
        );
        // scripts should not appear when None
        // Note: "scripts:" may appear from top-level scripts, so check within the apk section
        let apk_section = yaml.split("apk:").nth(1).unwrap_or("");
        assert!(
            !apk_section.contains("scripts:"),
            "apk scripts should not appear when None:\n{yaml}"
        );
    }

    #[test]
    fn test_config_parse_nfpm_apk_scripts() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [apk]
        apk:
          scripts:
            preupgrade: scripts/pre.sh
            postupgrade: scripts/post.sh
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        let apk = nfpm.apk.as_ref().unwrap();
        let scripts = apk.scripts.as_ref().unwrap();
        assert_eq!(scripts.preupgrade.as_deref(), Some("scripts/pre.sh"));
        assert_eq!(scripts.postupgrade.as_deref(), Some("scripts/post.sh"));
    }

    #[test]
    fn test_config_parse_signature_key_name_and_type() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        deb:
          signature:
            key_file: /path/to/key.gpg
            key_name: mykey.rsa.pub
            type: origin
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        let sig = nfpm.deb.as_ref().unwrap().signature.as_ref().unwrap();
        assert_eq!(sig.key_name.as_deref(), Some("mykey.rsa.pub"));
        assert_eq!(sig.type_.as_deref(), Some("origin"));
    }

    #[test]
    fn test_config_parse_content_packager_and_expand() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        contents:
          - src: /src/conf
            dst: /etc/myapp/conf
            packager: deb
            expand: true
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = &config.crates[0].nfpm.as_ref().unwrap()[0];
        let content = &nfpm.contents.as_ref().unwrap()[0];
        assert_eq!(content.packager.as_deref(), Some("deb"));
        assert_eq!(content.expand, Some(true));
    }

    #[test]
    fn test_release_template_var_in_file_name_template() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["rpm".to_string()],
            release: Some("2".to_string()),
            file_name_template: Some(
                "{{ .PackageName }}_{{ .Version }}-{{ .Release }}_{{ .Arch }}".to_string(),
            ),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);

        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        assert!(
            filename.contains("-2_"),
            "expected Release '2' in filename, got: {filename}"
        );
        assert!(filename.ends_with(".rpm"));
    }

    #[test]
    fn test_epoch_template_var_in_file_name_template() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            epoch: Some("3".to_string()),
            file_name_template: Some(
                "{{ .PackageName }}_{{ .Epoch }}_{{ .Version }}_{{ .Arch }}".to_string(),
            ),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "2.0.0");

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);

        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        assert!(
            filename.starts_with("myapp_3_2.0.0_"),
            "expected Epoch '3' in filename, got: {filename}"
        );
        assert!(filename.ends_with(".deb"));
    }

    #[test]
    fn test_release_and_epoch_default_to_empty_string() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Neither release nor epoch is set — they should default to empty strings
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            file_name_template: Some(
                "{{ .PackageName }}{{ .Release }}{{ .Epoch }}_{{ .Version }}".to_string(),
            ),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
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

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);

        let filename = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        // With empty Release and Epoch, should be "myapp_1.0.0..."
        assert!(
            filename.starts_with("myapp_1.0.0"),
            "expected empty Release/Epoch (no extra text), got: {filename}"
        );
    }
}
