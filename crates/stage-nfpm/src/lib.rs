use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

mod filename;

use anyhow::{Context as _, Result, bail};
use serde::Serialize;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    NfpmApkConfig, NfpmArchlinuxConfig, NfpmConfig, NfpmDebConfig, NfpmIpkConfig, NfpmRpmConfig,
    NfpmScripts, NfpmSignatureConfig,
};
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

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
    umask: Option<u32>,
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
    #[serde(skip_serializing_if = "is_empty_vec")]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    ipk: Option<NfpmYamlIpk>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changelog: Option<String>,
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
    /// File permission mode as a YAML integer so nfpm unmarshals into Go's
    /// `fs.FileMode`. Source `FileInfo.mode` is already a `u32` post-WAVE 5.1
    /// (SCH-3), so this maps straight through.
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<u32>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    arch_variant: Option<String>,
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

#[derive(Serialize)]
struct NfpmYamlIpk {
    #[serde(skip_serializing_if = "Option::is_none")]
    abi_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alternatives: Option<Vec<NfpmYamlIpkAlternative>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_installed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    essential: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    predepends: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fields: Option<HashMap<String, String>>,
}

#[derive(Serialize)]
struct NfpmYamlIpkAlternative {
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    link_name: Option<String>,
}

// ---------------------------------------------------------------------------
// generate_nfpm_yaml
// ---------------------------------------------------------------------------

/// Paths to C library artifacts grouped by type.
///
/// nfpm includes Header, CArchive, and
/// CShared artifact types alongside Binary.  These are routed to the
/// directories specified in the `libdirs` configuration block.
#[derive(Default)]
pub struct NfpmLibraryPaths {
    pub headers: Vec<String>,
    pub c_archives: Vec<String>,
    pub c_shared: Vec<String>,
}

/// Generate an nfpm YAML configuration string from the anodizer nfpm config.
///
/// `format` is the target packager format (e.g. "deb", "rpm") used to select
/// format-specific dependencies from the `dependencies` HashMap.  Pass `None`
/// to include deps for *all* formats.
///
/// `skip_sign` — when `true`, all signing/signature configuration is zeroed
/// out in the YAML output (GoReleaser parity: nfpm.go skips.Sign).
///
/// `library_paths` — paths to C library artifacts (Header, CArchive, CShared)
/// that should be routed to the appropriate libdirs directories.
pub fn generate_nfpm_yaml(
    config: &NfpmConfig,
    version: &str,
    binary_paths: &[String],
    format: Option<&str>,
    skip_sign: bool,
    library_paths: &NfpmLibraryPaths,
) -> Result<String> {
    // Default env map: empty. The passphrase resolver falls back to process
    // env for unknown keys, so behavior is preserved for callers that don't
    // pass a ctx env map. `generate_nfpm_yaml_with_env` is the production
    // entrypoint that passes the real anodizer ctx env map.
    let empty_env = std::collections::HashMap::new();
    generate_nfpm_yaml_with_env(
        config,
        version,
        binary_paths,
        format,
        skip_sign,
        library_paths,
        &empty_env,
    )
}

/// Generate nfpm YAML using the anodizer ctx env map (project `env:` +
/// `env_files:` + process env) for passphrase resolution. Matches
/// GoReleaser internal/pipe/nfpm/nfpm.go:640 which reads from `ctx.Env`
/// rather than `os.Getenv`, so `NFPM_PASSPHRASE` defined in project YAML
/// is visible to the signer.
pub fn generate_nfpm_yaml_with_env(
    config: &NfpmConfig,
    version: &str,
    binary_paths: &[String],
    format: Option<&str>,
    skip_sign: bool,
    library_paths: &NfpmLibraryPaths,
    env_map: &std::collections::HashMap<String, String>,
) -> Result<String> {
    let is_meta = config.meta == Some(true);

    // Build binary content entries for ALL binaries on this platform (skip for meta packages)
    let raw_bindir = config.bindir.as_deref().unwrap_or("/usr/bin");
    // For termux.deb, rewrite bindir to the Termux filesystem prefix
    let bindir = if format == Some("termux.deb") && raw_bindir.starts_with("/usr") {
        format!("/data/data/com.termux/files{raw_bindir}")
    } else {
        raw_bindir.to_string()
    };
    let bindir = bindir.as_str();

    let mut contents = if is_meta {
        // Meta packages have no binary contents — only dependencies
        Vec::new()
    } else {
        // GoReleaser groups all binaries for the same platform into one package.
        // Each binary gets its own content entry pointing to bindir.
        binary_paths
            .iter()
            .map(|bp| {
                let binary_name = PathBuf::from(bp)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("binary")
                    .to_string();
                NfpmYamlContent {
                    src: bp.clone(),
                    dst: format!("{bindir}/{binary_name}"),
                    content_type: None,
                    file_info: Some(NfpmYamlFileInfo {
                        owner: None,
                        group: None,
                        mode: Some(0o755),
                        mtime: None,
                    }),
                    packager: None,
                    expand: None,
                }
            })
            .collect()
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
                    mode: fi.mode.map(|m| m.value()),
                    mtime: fi.mtime.clone(),
                }),
                packager: entry.packager.clone(),
                expand: entry.expand,
            });
        }
    }

    // Libdirs: install CGo library outputs to the specified directories.
    // GoReleaser defaults (nfpm.go:59-67):
    //   Header    = "/usr/include"
    //   CArchive  = "/usr/lib"
    //   CShared   = "/usr/lib"
    //
    // Apply GoReleaser-aligned libdirs defaults unconditionally (matching
    // `internal/pipe/nfpm/nfpm.go:59-67`, which sets these in `Default()`
    // regardless of whether any library artifacts exist). The inner emit-loop
    // still iterates the actual library artifact paths, so when none are
    // present this block is a no-op for the resulting package — the change
    // only affects resolved-config introspection.
    {
        let libdirs = config.libdirs.as_ref();
        let header_dir = libdirs
            .and_then(|l| l.header.clone())
            .or_else(|| Some("/usr/include".to_string()));
        let carchive_dir = libdirs
            .and_then(|l| l.carchive.clone())
            .or_else(|| Some("/usr/lib".to_string()));
        let cshared_dir = libdirs
            .and_then(|l| l.cshared.clone())
            .or_else(|| Some("/usr/lib".to_string()));

        // unconditionally prefix libdirs for termux.deb — the prefix is never
        // guarded by /usr or /etc; any non-empty path is prefixed.
        let (header_dir, carchive_dir, cshared_dir) = if format == Some("termux.deb") {
            (
                header_dir.map(|d| {
                    if d.is_empty() {
                        d
                    } else {
                        format!("/data/data/com.termux/files{d}")
                    }
                }),
                carchive_dir.map(|d| {
                    if d.is_empty() {
                        d
                    } else {
                        format!("/data/data/com.termux/files{d}")
                    }
                }),
                cshared_dir.map(|d| {
                    if d.is_empty() {
                        d
                    } else {
                        format!("/data/data/com.termux/files{d}")
                    }
                }),
            )
        } else {
            (header_dir, carchive_dir, cshared_dir)
        };

        // only add library content entries when actual
        // library artifacts are present. The libdirs config specifies
        // *destination directories* for actual artifacts, not synthetic paths.
        let lib_groups: &[(&Option<String>, &[String], u32)] = &[
            (&header_dir, &library_paths.headers, 0o644),
            (&carchive_dir, &library_paths.c_archives, 0o644),
            (&cshared_dir, &library_paths.c_shared, 0o755),
        ];
        for (dir_opt, paths, mode) in lib_groups {
            if let Some(dir) = dir_opt {
                for src_path in *paths {
                    let filename = PathBuf::from(src_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("lib")
                        .to_string();
                    contents.push(NfpmYamlContent {
                        src: src_path.clone(),
                        dst: format!("{dir}/{filename}"),
                        content_type: None,
                        file_info: Some(NfpmYamlFileInfo {
                            owner: None,
                            group: None,
                            mode: Some(*mode),
                            mtime: None,
                        }),
                        packager: None,
                        expand: None,
                    });
                }
            }
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
        .map(|r| build_yaml_rpm(r, nfpm_id, format, skip_sign, env_map));
    let deb = config
        .deb
        .as_ref()
        .filter(|d| !d.is_empty())
        .map(|d| build_yaml_deb(d, nfpm_id, format, skip_sign, env_map));
    let apk = config
        .apk
        .as_ref()
        .filter(|a| !a.is_empty())
        .map(|a| build_yaml_apk(a, nfpm_id, format, skip_sign, env_map));
    let archlinux = config
        .archlinux
        .as_ref()
        .filter(|a| !a.is_empty())
        .map(build_yaml_archlinux);
    let ipk = config
        .ipk
        .as_ref()
        .filter(|i| !i.is_empty())
        .map(build_yaml_ipk);

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
        // Emit umask as a plain decimal int — nfpm parses into `fs.FileMode`
        // (uint32) and rejects YAML strings (`'0o002'`) with
        // `cannot unmarshal !!str into fs.FileMode`. Octal-input form on the
        // anodizer side is preserved by `StringOrU32`'s deserializer.
        umask: config.umask.map(|u| u.value()),
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
        ipk,
        changelog: config.changelog.clone(),
    };

    // serde_yaml_ng::to_string fails only on un-serialisable values (e.g. maps
    // with non-string keys). NfpmYamlConfig is composed of Strings/Vecs/Options
    // so this is effectively infallible — but `overrides:` can carry arbitrary
    // user-supplied YAML values, so propagate the error rather than panic.
    let yaml = serde_yaml_ng::to_string(&yaml_config).context("failed to serialize nfpm YAML")?;
    // serde_yaml_ng emits a trailing newline; trim it for consistency
    Ok(yaml.trim_end().to_string())
}

// ---------------------------------------------------------------------------
// Format-specific YAML builders
// ---------------------------------------------------------------------------

/// Resolve the signing passphrase using GoReleaser's 3-level env var fallback:
///   1. NFPM_{ID}_{format}_PASSPHRASE  (format preserved as-is, e.g. `deb`/`rpm`)
///   2. NFPM_{ID}_PASSPHRASE
///   3. NFPM_PASSPHRASE
///
/// `env_map` is the anodizer ctx env map (process env + project `env:` +
/// `env_files:`). Looking up here — instead of `std::env::var` directly —
/// means values defined in `.anodizer.yaml` `env:` are visible to the signer,
/// matching GoReleaser internal/pipe/nfpm/nfpm.go:640 which reads from
/// `ctx.Env` rather than `os.Getenv`.
///
/// Returns `None` if no env var is set at any level.
fn resolve_passphrase_from_env(
    env_map: &std::collections::HashMap<String, String>,
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
    // Level 1: NFPM_{ID}_{format}_PASSPHRASE (format preserved as-is, per GoReleaser)
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

fn build_yaml_signature(
    sig: &NfpmSignatureConfig,
    nfpm_id: &str,
    format: Option<&str>,
    env_map: &std::collections::HashMap<String, String>,
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

fn build_yaml_rpm(
    rpm: &NfpmRpmConfig,
    nfpm_id: &str,
    format: Option<&str>,
    skip_sign: bool,
    env_map: &std::collections::HashMap<String, String>,
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

fn build_yaml_deb(
    deb: &NfpmDebConfig,
    nfpm_id: &str,
    format: Option<&str>,
    skip_sign: bool,
    env_map: &std::collections::HashMap<String, String>,
) -> NfpmYamlDeb {
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

fn build_yaml_apk(
    apk: &NfpmApkConfig,
    nfpm_id: &str,
    format: Option<&str>,
    skip_sign: bool,
    env_map: &std::collections::HashMap<String, String>,
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

fn build_yaml_ipk(ipk: &NfpmIpkConfig) -> NfpmYamlIpk {
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

/// A fully-staged nfpm job: config YAML written, filename decided,
/// subprocess args composed. Phase 1 (serial, `&mut ctx`) renders all
/// templates and writes the YAML into `_tmp_dir`; Phase 2 (parallel)
/// runs `nfpm pkg --packager <format>`. `_tmp_dir` keeps the config
/// file alive until the worker thread finishes.
struct NfpmJob {
    _tmp_dir: tempfile::TempDir,
    pkg_path: std::path::PathBuf,
    format: String,
    cmd_args: Vec<String>,
    /// Pre-parsed mtime for reproducible-build mtime stamping, or None
    /// when the config leaves `mtime` unset.
    mtime: Option<std::time::SystemTime>,
    mtime_repr: Option<String>,
    target: Option<String>,
    crate_name: String,
    pkg_metadata: std::collections::HashMap<String, String>,
}

impl Stage for NfpmStage {
    fn name(&self) -> &str {
        "nfpm"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("nfpm");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();
        let parallelism = ctx.options.parallelism.max(1);

        // Collect crates that have nfpm config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.nfpms.is_some())
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

        // when the global skip_sign is active, zero out
        // all nFPM signature configuration in the generated YAML.
        let skip_sign = ctx.should_skip("sign");

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut jobs: Vec<NfpmJob> = Vec::new();

        // Validate nfpm config ID uniqueness across all crates (GoReleaser parity)
        {
            let mut seen_ids = std::collections::HashSet::new();
            for krate in &crates {
                if let Some(ref nfpm_configs) = krate.nfpms {
                    for cfg in nfpm_configs {
                        let id = cfg.id.as_deref().unwrap_or("default");
                        if !seen_ids.insert(id.to_string()) {
                            bail!(
                                "nfpm: duplicate config ID '{}' (each nfpm config must have a unique ID)",
                                id
                            );
                        }
                    }
                }
            }
        }

        for krate in &crates {
            let Some(nfpm_configs) = krate.nfpms.as_ref() else {
                continue;
            };

            // Collect all nfpm-eligible artifacts for this crate.
            // ByTypes(Binary, Header, CArchive, CShared)
            // filtered by ByGooses("linux", "ios", "android", "aix").
            let nfpm_artifact_kinds = &[
                ArtifactKind::Binary,
                ArtifactKind::Header,
                ArtifactKind::CArchive,
                ArtifactKind::CShared,
            ];
            let linux_binaries: Vec<_> = ctx
                .artifacts
                .by_kinds_and_crate(nfpm_artifact_kinds, &krate.name)
                .into_iter()
                .filter(|b| {
                    b.target
                        .as_deref()
                        .map(anodizer_core::target::is_nfpm_target)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            for nfpm_cfg in nfpm_configs {
                let nfpm_id_for_log = nfpm_cfg.id.as_deref().unwrap_or("default").to_string();

                // GoReleaser Pro `nfpm.if`: template-conditional skip.
                // Rendered "false"/empty => skip with info log; render error => hard bail.
                // Hard-error on render failure intentionally diverges from stage-sign's
                // silent-skip-on-render-error (that is tracked as W1 in pro-features-audit.md
                // and must be fixed there too). A render failure means the user's template
                // references an unknown var; silently skipping would ship a release without
                // the packages the user asked for.
                if let Some(ref condition) = nfpm_cfg.if_condition {
                    let rendered = ctx.render_template(condition).with_context(|| {
                        format!(
                            "nfpm config '{}': `if` template render failed (expression: {})",
                            nfpm_id_for_log, condition
                        )
                    })?;
                    let trimmed = rendered.trim();
                    if trimmed.is_empty() || trimmed == "false" {
                        let reason = format!("if condition evaluated to '{}'", trimmed);
                        log.verbose(&format!(
                            "skipping nfpm config '{}': {}",
                            nfpm_id_for_log, reason
                        ));
                        ctx.remember_skip("nfpm", &nfpm_id_for_log, &reason);
                        continue;
                    }
                }

                // warn and skip when no output formats configured
                if nfpm_cfg.formats.is_empty() {
                    let nfpm_id = nfpm_id_for_log.as_str();
                    ctx.strict_guard(
                        &log,
                        &format!(
                            "nfpm config '{}': no output formats configured, skipping",
                            nfpm_id
                        ),
                    )?;
                    continue;
                }

                // warn when maintainer is empty (required for deb)
                let maintainer = nfpm_cfg.maintainer.as_deref().unwrap_or("");
                if maintainer.is_empty() {
                    let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
                    log.warn(&format!(
                        "nfpm config '{}': maintainer is empty (required for deb packages)",
                        nfpm_id
                    ));
                }

                let is_meta = nfpm_cfg.meta == Some(true);

                // GoReleaser groups all artifacts by platform and creates ONE
                // package per platform containing ALL artifacts for that platform.
                // The tuple contains: (target, binary_paths, library_paths).
                let platform_groups: Vec<(Option<String>, Vec<String>, NfpmLibraryPaths)> =
                    if is_meta {
                        // Meta packages have no binary contents — use a synthetic entry
                        // so the loop below runs once per target (or once with no target).
                        if linux_binaries.is_empty() {
                            vec![(None, Vec::new(), NfpmLibraryPaths::default())]
                        } else {
                            let mut seen = std::collections::HashSet::new();
                            linux_binaries
                                .iter()
                                .filter(|b| {
                                    let key = b.target.clone().unwrap_or_default();
                                    seen.insert(key)
                                })
                                .map(|b| {
                                    (b.target.clone(), Vec::new(), NfpmLibraryPaths::default())
                                })
                                .collect()
                        }
                    } else {
                        // Apply ids filter: when the nfpm config specifies `ids`,
                        // only include artifacts whose metadata "id" is in the list.
                        let filtered: Vec<_> = if let Some(ref ids) = nfpm_cfg.ids {
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

                        // If the ids filter matched nothing but there ARE artifacts,
                        // warn and skip — the user likely misconfigured ids.
                        if filtered.is_empty() && !linux_binaries.is_empty() {
                            let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
                            log.warn(&format!(
                                "nfpm config '{}': ids filter matched no binaries, skipping",
                                nfpm_id
                            ));
                            continue;
                        }

                        // If no artifacts found at all, use a single synthetic
                        // entry with a default path.
                        if filtered.is_empty() {
                            vec![(
                                None,
                                vec![format!("dist/{}", krate.name)],
                                NfpmLibraryPaths::default(),
                            )]
                        } else {
                            // Group by target: all artifacts for the same platform
                            // go into one package (GoReleaser parity).
                            // Split Binary artifacts from C library artifacts.
                            struct PlatformArtifacts {
                                binaries: Vec<String>,
                                libs: NfpmLibraryPaths,
                            }
                            let mut groups: std::collections::BTreeMap<
                                Option<String>,
                                PlatformArtifacts,
                            > = std::collections::BTreeMap::new();
                            for b in &filtered {
                                let entry = groups.entry(b.target.clone()).or_insert_with(|| {
                                    PlatformArtifacts {
                                        binaries: Vec::new(),
                                        libs: NfpmLibraryPaths::default(),
                                    }
                                });
                                let path = b.path.to_string_lossy().into_owned();
                                match b.kind {
                                    ArtifactKind::Header => entry.libs.headers.push(path),
                                    ArtifactKind::CArchive => entry.libs.c_archives.push(path),
                                    ArtifactKind::CShared => entry.libs.c_shared.push(path),
                                    _ => entry.binaries.push(path),
                                }
                            }
                            groups
                                .into_iter()
                                .map(|(t, pa)| (t, pa.binaries, pa.libs))
                                .collect()
                        }
                    };

                for (target, binary_paths, lib_paths) in &platform_groups {
                    // Derive Os/Arch from the target triple for template rendering
                    let (base_os, base_arch) = target
                        .as_deref()
                        .map(anodizer_core::target::map_target)
                        .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()));

                    for format in &nfpm_cfg.formats {
                        validate_format(format)
                            .with_context(|| format!("nfpm config for crate {}", krate.name))?;

                        // platform-format
                        // restrictions for iOS and AIX.
                        let (os, arch) = match base_os.as_str() {
                            "ios" => {
                                if format == "deb" {
                                    ("iphoneos-arm64".to_string(), base_arch.clone())
                                } else {
                                    log.status(&format!(
                                        "skipping ios for format '{}': only deb is supported",
                                        format
                                    ));
                                    continue;
                                }
                            }
                            "aix" => {
                                if base_arch != "ppc64" {
                                    log.status(&format!(
                                        "skipping aix/{}: only ppc64 is supported",
                                        base_arch
                                    ));
                                    continue;
                                }
                                if format == "rpm" {
                                    ("aix7.2".to_string(), "ppc".to_string())
                                } else {
                                    log.status(&format!(
                                        "skipping aix for format '{}': only rpm is supported",
                                        format
                                    ));
                                    continue;
                                }
                            }
                            _ => (base_os.clone(), base_arch.clone()),
                        };

                        // Validate architecture compatibility per format
                        if let Some(triple) = target.as_deref()
                            && !is_arch_supported_for_format(triple, format)
                        {
                            ctx.strict_guard(
                                &log,
                                &format!(
                                    "nfpm: skipping format '{}' for target '{}': architecture not supported",
                                    format, triple
                                ),
                            )?;
                            continue;
                        }

                        // Template-render key string fields before generating YAML.
                        // Errors are propagated (not silently swallowed) to match GoReleaser.
                        //
                        // GoReleaser Pro parity: fall back to project-level `metadata.*` when
                        // the nfpm config's own field is unset. Before this, `metadata.homepage`
                        // / `license` / `description` / `maintainers` were collected but silently
                        // unused (config-must-wire).
                        let mut rendered_cfg = nfpm_cfg.clone();
                        if rendered_cfg.description.is_none() {
                            rendered_cfg.description =
                                ctx.config.meta_description().map(str::to_string);
                        }
                        if rendered_cfg.maintainer.is_none() {
                            rendered_cfg.maintainer =
                                ctx.config.meta_first_maintainer().map(str::to_string);
                        }
                        if rendered_cfg.homepage.is_none() {
                            rendered_cfg.homepage = ctx.config.meta_homepage().map(str::to_string);
                        }
                        if rendered_cfg.license.is_none() {
                            rendered_cfg.license = ctx.config.meta_license().map(str::to_string);
                        }
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
                        if let Some(ref s) = rendered_cfg.changelog {
                            rendered_cfg.changelog = Some(ctx.render_template(s)?);
                        }
                        // Template-render bindir and mtime (GoReleaser parity)
                        if let Some(ref s) = rendered_cfg.bindir {
                            rendered_cfg.bindir = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref s) = rendered_cfg.mtime {
                            rendered_cfg.mtime = Some(ctx.render_template(s)?);
                        }
                        // Template-render script paths
                        if let Some(ref mut scripts) = rendered_cfg.scripts {
                            if let Some(ref s) = scripts.preinstall {
                                scripts.preinstall = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = scripts.postinstall {
                                scripts.postinstall = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = scripts.preremove {
                                scripts.preremove = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = scripts.postremove {
                                scripts.postremove = Some(ctx.render_template(s)?);
                            }
                        }
                        // Template-render signature key_file and key_name
                        if let Some(ref mut deb) = rendered_cfg.deb
                            && let Some(ref mut sig) = deb.signature
                            && let Some(ref s) = sig.key_file
                        {
                            sig.key_file = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref mut rpm) = rendered_cfg.rpm
                            && let Some(ref mut sig) = rpm.signature
                            && let Some(ref s) = sig.key_file
                        {
                            sig.key_file = Some(ctx.render_template(s)?);
                        }
                        if let Some(ref mut apk) = rendered_cfg.apk
                            && let Some(ref mut sig) = apk.signature
                        {
                            if let Some(ref s) = sig.key_file {
                                sig.key_file = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = sig.key_name {
                                sig.key_name = Some(ctx.render_template(s)?);
                            }
                        }
                        // Template-render libdirs
                        if let Some(ref mut libdirs) = rendered_cfg.libdirs {
                            if let Some(ref s) = libdirs.header {
                                libdirs.header = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = libdirs.cshared {
                                libdirs.cshared = Some(ctx.render_template(s)?);
                            }
                            if let Some(ref s) = libdirs.carchive {
                                libdirs.carchive = Some(ctx.render_template(s)?);
                            }
                        }

                        // Template-render contents: src, dst, file_info.owner/group/mtime
                        if let Some(ref mut entries) = rendered_cfg.contents {
                            for entry in entries.iter_mut() {
                                entry.src = ctx.render_template(&entry.src)?;
                                entry.dst = ctx.render_template(&entry.dst)?;
                                if let Some(ref mut fi) = entry.file_info {
                                    if let Some(ref s) = fi.owner {
                                        fi.owner = Some(ctx.render_template(s)?);
                                    }
                                    if let Some(ref s) = fi.group {
                                        fi.group = Some(ctx.render_template(s)?);
                                    }
                                    if let Some(ref s) = fi.mtime {
                                        fi.mtime = Some(ctx.render_template(s)?);
                                    }
                                }
                            }
                        }

                        // GoReleaser Pro `templated_contents`: for each entry, read `src`,
                        // render its body through Tera, write to a temp file under
                        // `dist/nfpm-tmp/<crate>/<nfpm_id>/`, and append to `contents` using
                        // the temp file as the real source. User-supplied `dst` + `file_info`
                        // are preserved; only `src` is rewritten to the rendered temp path.
                        if let Some(templated_entries) = rendered_cfg.templated_contents.take()
                            && !templated_entries.is_empty()
                        {
                            {
                                let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
                                let tmpl_dir =
                                    dist.join("nfpm-tmp").join(&krate.name).join(nfpm_id);
                                if !dry_run {
                                    fs::create_dir_all(&tmpl_dir).with_context(|| {
                                        format!(
                                            "nfpm: create templated-contents dir: {}",
                                            tmpl_dir.display()
                                        )
                                    })?;
                                }
                                let rendered_contents =
                                    rendered_cfg.contents.get_or_insert_with(Vec::new);
                                for (idx, mut entry) in templated_entries.into_iter().enumerate() {
                                    entry.src = ctx.render_template(&entry.src)?;
                                    entry.dst = ctx.render_template(&entry.dst)?;
                                    let body =
                                        fs::read_to_string(&entry.src).with_context(|| {
                                            format!(
                                                "nfpm: read templated_contents src: {}",
                                                entry.src
                                            )
                                        })?;
                                    let rendered_body =
                                        ctx.render_template(&body).with_context(|| {
                                            format!(
                                                "nfpm: render templated_contents body for {}",
                                                entry.src
                                            )
                                        })?;
                                    let base = std::path::Path::new(&entry.src)
                                        .file_name()
                                        .map(|s| s.to_string_lossy().into_owned())
                                        .unwrap_or_else(|| format!("tmpl-{idx}"));
                                    let out_path = tmpl_dir.join(format!("{idx:03}-{base}"));
                                    if !dry_run {
                                        fs::write(&out_path, rendered_body.as_bytes())
                                            .with_context(|| {
                                                format!(
                                                    "nfpm: write rendered templated_contents: {}",
                                                    out_path.display()
                                                )
                                            })?;
                                    }
                                    entry.src = out_path.to_string_lossy().into_owned();
                                    rendered_contents.push(entry);
                                }
                            }
                        }

                        // GoReleaser Pro `templated_scripts`: same idea for lifecycle scripts.
                        // Each set field names a script file whose contents we render, write
                        // to a temp path, and substitute into `rendered_cfg.scripts`. Templated
                        // version wins over a same-named plain `scripts` entry.
                        if let Some(templated_scripts) = rendered_cfg.templated_scripts.take() {
                            let any = templated_scripts.preinstall.is_some()
                                || templated_scripts.postinstall.is_some()
                                || templated_scripts.preremove.is_some()
                                || templated_scripts.postremove.is_some();
                            if any {
                                let nfpm_id = nfpm_cfg.id.as_deref().unwrap_or("default");
                                let tmpl_dir =
                                    dist.join("nfpm-tmp").join(&krate.name).join(nfpm_id);
                                if !dry_run {
                                    fs::create_dir_all(&tmpl_dir).with_context(|| {
                                        format!(
                                            "nfpm: create templated-scripts dir: {}",
                                            tmpl_dir.display()
                                        )
                                    })?;
                                }
                                let scripts_out = rendered_cfg
                                    .scripts
                                    .get_or_insert_with(NfpmScripts::default);
                                let render_and_write =
                                    |name: &str,
                                     src_path: &str,
                                     ctx: &mut Context|
                                     -> Result<String> {
                                        let rendered_src = ctx.render_template(src_path)?;
                                        let body = fs::read_to_string(&rendered_src).with_context(
                                            || {
                                                format!(
                                                    "nfpm: read templated_script {}: {}",
                                                    name, rendered_src
                                                )
                                            },
                                        )?;
                                        let rendered_body =
                                            ctx.render_template(&body).with_context(|| {
                                                format!(
                                                    "nfpm: render templated_script {}: {}",
                                                    name, rendered_src
                                                )
                                            })?;
                                        let out_path = tmpl_dir.join(format!("script-{}", name));
                                        if !dry_run {
                                            fs::write(&out_path, rendered_body.as_bytes())
                                                .with_context(|| {
                                                    format!(
                                                        "nfpm: write rendered templated_script: {}",
                                                        out_path.display()
                                                    )
                                                })?;
                                        }
                                        Ok(out_path.to_string_lossy().into_owned())
                                    };
                                if let Some(ref s) = templated_scripts.preinstall {
                                    scripts_out.preinstall =
                                        Some(render_and_write("preinstall", s, ctx)?);
                                }
                                if let Some(ref s) = templated_scripts.postinstall {
                                    scripts_out.postinstall =
                                        Some(render_and_write("postinstall", s, ctx)?);
                                }
                                if let Some(ref s) = templated_scripts.preremove {
                                    scripts_out.preremove =
                                        Some(render_and_write("preremove", s, ctx)?);
                                }
                                if let Some(ref s) = templated_scripts.postremove {
                                    scripts_out.postremove =
                                        Some(render_and_write("postremove", s, ctx)?);
                                }
                            }
                        }

                        // Fill deb.arch_variant from artifact amd64 microarch
                        // when unset; explicit user config wins.
                        if let Some(ref mut deb) = rendered_cfg.deb
                            && deb.arch_variant.is_none()
                            && let Some(t) = target.as_deref()
                        {
                            let variant = linux_binaries
                                .iter()
                                .find(|b| b.target.as_deref() == Some(t))
                                .and_then(|b| b.metadata.get("amd64_variant").cloned());
                            deb.arch_variant = variant;
                        }

                        // Generate YAML per format so format-specific deps are selected.
                        // Pass the anodizer ctx env map so passphrase lookups
                        // see project `env:` / `env_files:` values (W6 fix).
                        let yaml_content = generate_nfpm_yaml_with_env(
                            &rendered_cfg,
                            &version,
                            binary_paths,
                            Some(format),
                            skip_sign,
                            lib_paths,
                            ctx.template_vars().all_env(),
                        )?;

                        // Ensure output directory exists
                        let output_dir = dist.join("linux");
                        if !dry_run {
                            fs::create_dir_all(&output_dir).with_context(|| {
                                format!("create nfpm output dir: {}", output_dir.display())
                            })?;
                        }

                        // Determine package file name (template or default).
                        // GoReleaser nfpm.go:68-70 — default is ProjectName
                        // (not the crate/binary name). Fall back to crate name
                        // only if project_name is empty. Copy into an owned
                        // String so we can mutably reborrow ctx for template vars.
                        let pkg_name_owned: String =
                            if let Some(n) = nfpm_cfg.package_name.as_deref() {
                                n.to_string()
                            } else if !ctx.config.project_name.is_empty() {
                                ctx.config.project_name.clone()
                            } else {
                                krate.name.clone()
                            };
                        let pkg_name: &str = pkg_name_owned.as_str();
                        let ext = format_extension(format);

                        // Set nfpm-specific template vars (Os, Arch, Format,
                        // PackageName, ConventionalExtension, ConventionalFileName,
                        // Release, Epoch) before rendering file_name_template.
                        ctx.template_vars_mut().set("Os", &os);
                        ctx.template_vars_mut().set("Arch", &arch);
                        ctx.template_vars_mut()
                            .set("Target", target.as_deref().unwrap_or(""));
                        ctx.template_vars_mut().set("Format", format);
                        ctx.template_vars_mut().set("PackageName", pkg_name);
                        ctx.template_vars_mut().set("ConventionalExtension", ext);
                        // Per-packager ConventionalFileName (nfpm v2.44 parity):
                        // deb / rpm / apk / archlinux / ipk each have
                        // distinct filename conventions and arch
                        // translations. Falls back to the hand-rolled
                        // default for formats we don't recognise.
                        let fn_info = filename::FileNameInfo::from_config(
                            nfpm_cfg, pkg_name, &version, &arch, format,
                        );
                        let conventional = filename::conventional_filename(format, &fn_info)
                            .unwrap_or_else(|| format!("{pkg_name}_{version}_{os}_{arch}{ext}"));
                        ctx.template_vars_mut()
                            .set("ConventionalFileName", &conventional);
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

                        // Render mtime once in Phase 1 so Phase 2 doesn't touch
                        // ctx; pre-parse into SystemTime so workers can call
                        // set_file_mtime directly.
                        let (mtime, mtime_repr) = if let Some(ref raw_mtime) = nfpm_cfg.mtime {
                            let rendered_mtime = ctx
                                .render_template(raw_mtime)
                                .unwrap_or_else(|_| raw_mtime.clone());
                            match anodizer_core::util::parse_mod_timestamp(&rendered_mtime) {
                                Ok(mt) => (Some(mt), Some(rendered_mtime)),
                                Err(e) => {
                                    log.warn(&format!(
                                        "nfpm: invalid mtime '{rendered_mtime}': {e}"
                                    ));
                                    (None, None)
                                }
                            }
                        } else {
                            (None, None)
                        };

                        jobs.push(NfpmJob {
                            _tmp_dir: tmp_dir,
                            pkg_path: pkg_path.clone(),
                            format: format.clone(),
                            cmd_args,
                            mtime,
                            mtime_repr,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            pkg_metadata,
                        });
                    }
                }
            }
        }

        anodizer_core::template::clear_per_target_vars(ctx.template_vars_mut());
        // nfpm also uses its own per-format / per-packaging vars; clear
        // them here so user-template state doesn't leak into downstream
        // stages like announce or publish.
        for extra in [
            "Format",
            "PackageName",
            "ConventionalExtension",
            "ConventionalFileName",
            "Release",
            "Epoch",
        ] {
            ctx.template_vars_mut().set(extra, "");
        }

        // ----------------------------------------------------------------
        // Phase 2 (parallel): run `nfpm pkg --packager <format>` per job.
        // Bounded concurrency via chunks(parallelism). Each worker returns
        // the populated Artifact; Phase 3 registers them serially.
        // ----------------------------------------------------------------
        if !jobs.is_empty() {
            let run_job = |job: &NfpmJob| -> Result<Artifact> {
                let thread_log = anodizer_core::log::StageLogger::new("nfpm", log.verbosity());

                thread_log.status(&format!("running: {}", job.cmd_args.join(" ")));

                let output = Command::new(&job.cmd_args[0])
                    .args(&job.cmd_args[1..])
                    .output()
                    .with_context(|| {
                        format!(
                            "execute nfpm for format {} (crate {} target {:?})",
                            job.format, job.crate_name, job.target
                        )
                    })?;
                thread_log.check_output(output, "nfpm")?;

                // Reproducible-build mtime — pre-parsed in Phase 1.
                if let Some(mt) = job.mtime {
                    if let Err(e) = anodizer_core::util::set_file_mtime(&job.pkg_path, mt) {
                        thread_log.warn(&format!(
                            "nfpm: failed to apply mtime to {}: {}",
                            job.pkg_path.display(),
                            e
                        ));
                    } else if let Some(ref repr) = job.mtime_repr {
                        thread_log.verbose(&format!(
                            "nfpm: applied mtime={repr} to {}",
                            job.pkg_path.display()
                        ));
                    }
                }

                Ok(Artifact {
                    kind: ArtifactKind::LinuxPackage,
                    name: String::new(),
                    path: job.pkg_path.clone(),
                    target: job.target.clone(),
                    crate_name: job.crate_name.clone(),
                    metadata: job.pkg_metadata.clone(),
                    size: None,
                })
            };

            let results =
                anodizer_core::parallel::run_parallel_chunks(&jobs, parallelism, "nfpm", run_job)?;
            new_artifacts.extend(results);
        }

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
#[allow(clippy::field_reassign_with_default)]
mod tests;
