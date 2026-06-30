//! Public `generate_nfpm_yaml` entry points.
//!
//! `generate_nfpm_yaml_with_env` is the workhorse that consumes anodizer's
//! `NfpmConfig` (plus binaries, library paths, and ctx env map) and emits
//! the YAML string nfpm reads.  `generate_nfpm_yaml` is a thin wrapper that
//! defers to it with an empty env map (used by tests / non-stage callers).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};

use anodizer_core::config::NfpmConfig;

use crate::builders::{
    build_yaml_apk, build_yaml_archlinux, build_yaml_deb, build_yaml_ipk, build_yaml_rpm,
};
use crate::yaml::{NfpmYamlConfig, NfpmYamlContent, NfpmYamlFileInfo, NfpmYamlScripts};

/// Drop `provides` entries that would make the package uninstallable for the
/// target format.
///
/// apk auto-provides a package's own name; an EXPLICIT self-provide
/// (versioned or not) registers a second provider of that name, and apk's
/// solver rejects the package as conflicting with itself
/// (`conflicts: <pkg>[<pkg>]`) — the package cannot be installed at all.
/// dpkg and rpm treat a self-provide as a redundant no-op, so only the apk
/// format filters it. The provide's name is the text before any version
/// operator (`=`, `<`, `>`, `~`). An empty package name never reaches a
/// built package (nfpm rejects a nameless config), so no filtering applies.
fn filter_provides_for_format(
    provides: Vec<String>,
    package_name: &str,
    format: Option<&str>,
) -> Vec<String> {
    if format != Some("apk") || package_name.is_empty() {
        return provides;
    }
    provides
        .into_iter()
        .filter(|p| {
            let name = p.split(['=', '<', '>', '~']).next().unwrap_or(p).trim();
            name != package_name
        })
        .collect()
}

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

/// The resolved per-target render inputs for one nfpm YAML config: the values
/// that vary per (target × format) and together select the package
/// architecture, version, packager, and signing behavior.
///
/// Bundling them keeps the render entry points to a small, stable argument
/// list as the per-target dimension grows.
pub struct NfpmRenderTarget<'a> {
    /// The RESOLVED package name (explicit `package_name`, then project
    /// name, then crate name — `resolve_pkg_name`'s precedence). Emitted as
    /// the YAML `name:` AND used to detect format-invalid self-provides, so
    /// the two can never disagree. Empty when the caller has no name source
    /// at all; the YAML then omits `name:` and nfpm rejects the config.
    pub pkg_name: &'a str,
    /// Resolved OS in nfpm nomenclature (`linux`, `iphoneos-arm64`, …). Read
    /// by the per-target template vars on the build path; unused by the YAML
    /// generator itself.
    pub os: &'a str,
    /// Resolved package architecture in nfpm nomenclature (`amd64`, `arm64`,
    /// …) — always stamped so nfpm never silently defaults a package to
    /// `amd64`.
    pub arch: &'a str,
    /// Target triple this config renders for, or `None` for a host build with
    /// no triple.
    pub target: Option<&'a str>,
    /// The packager format selecting format-specific dependencies, or `None`
    /// to merge deps from every format.
    pub format: Option<&'a str>,
    /// The resolved package version.
    pub version: &'a str,
    /// When `true`, all signing/signature configuration is zeroed out.
    pub skip_sign: bool,
}

/// Generate an nfpm YAML configuration string from the anodizer nfpm config.
///
/// `format` is the target packager format (e.g. "deb", "rpm") used to select
/// format-specific dependencies from the `dependencies` HashMap.  Pass `None`
/// to include deps for *all* formats.
///
/// `skip_sign` — when `true`, all signing/signature configuration is zeroed
/// out in the YAML output.
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
    //
    // `amd64` matches the architecture nfpm itself defaults an unset `arch`
    // to, so non-stage callers (tests, introspection) keep their prior
    // output; the production path threads the target-resolved arch instead.
    let empty_env = HashMap::new();
    let target = NfpmRenderTarget {
        // No project/crate context here — explicit `package_name` is the
        // only name source for non-stage callers, matching prior output.
        pkg_name: config.package_name.as_deref().unwrap_or(""),
        os: "linux",
        arch: "amd64",
        target: None,
        format,
        version,
        skip_sign,
    };
    generate_nfpm_yaml_with_env(config, &target, binary_paths, library_paths, &empty_env)
}

/// Generate nfpm YAML using the anodizer ctx env map (project `env:` +
/// `env_files:` + process env) for passphrase resolution. Matches
/// reads the passphrase from the env
/// rather than `os.Getenv`, so `NFPM_PASSPHRASE` defined in project YAML
/// is visible to the signer.
pub fn generate_nfpm_yaml_with_env(
    config: &NfpmConfig,
    target: &NfpmRenderTarget<'_>,
    binary_paths: &[String],
    library_paths: &NfpmLibraryPaths,
    env_map: &HashMap<String, String>,
) -> Result<String> {
    let version = target.version;
    let arch = target.arch;
    let format = target.format;
    let skip_sign = target.skip_sign;
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

    // `bin_alias` renames the installed binary inside the package only.
    // It applies solely to the single-binary case — renaming every entry of a
    // multi-binary package to one name would clobber, so a config that bundles
    // multiple binaries keeps each binary's own name regardless of the alias.
    let single_binary = binary_paths.len() == 1;
    let mut contents = if is_meta {
        // Meta packages have no binary contents — only dependencies
        Vec::new()
    } else {
        // All binaries for the same platform are grouped into one package.
        // Each binary gets its own content entry pointing to bindir.
        binary_paths
            .iter()
            .map(|bp| {
                let binary_name = PathBuf::from(bp)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("binary")
                    .to_string();
                let dst_name = match config.bin_alias.as_deref() {
                    Some(alias) if single_binary && !alias.is_empty() => alias,
                    _ => binary_name.as_str(),
                };
                NfpmYamlContent {
                    src: bp.clone(),
                    dst: format!("{bindir}/{dst_name}"),
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
    // Defaults:
    //   Header    = "/usr/include"
    //   CArchive  = "/usr/lib"
    //   CShared   = "/usr/lib"
    //
    // Apply libdirs defaults unconditionally (set at default time
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

    // Convert serde_json::Value overrides to serde_yaml_ng::Value. A conversion
    // failure propagates rather than silently dropping a user's per-format
    // override (JSON ⊂ YAML makes this effectively infallible, but a dropped
    // override would otherwise vanish with no diagnostic).
    let overrides = match config.overrides.as_ref().filter(|m| !m.is_empty()) {
        Some(m) => {
            let mut out: HashMap<String, serde_yaml_ng::Value> = HashMap::with_capacity(m.len());
            for (k, v) in m {
                let json_str = serde_json::to_string(v)
                    .with_context(|| format!("nfpm: serialize override '{k}'"))?;
                let yaml_val: serde_yaml_ng::Value = serde_yaml_ng::from_str(&json_str)
                    .with_context(|| format!("nfpm: convert override '{k}' to YAML"))?;
                out.insert(k.clone(), yaml_val);
            }
            Some(out)
        }
        None => None,
    };

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
        name: (!target.pkg_name.is_empty()).then(|| target.pkg_name.to_string()),
        arch: arch.to_string(),
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
        provides: filter_provides_for_format(
            config.provides.clone().unwrap_or_default(),
            target.pkg_name,
            format,
        ),
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
