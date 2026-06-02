//! Archive file-spec resolution — translate user `files:` entries
//! (`ArchiveFileSpec::Glob` and `::Detailed`) into concrete on-disk paths
//! plus archive-internal destinations and per-file metadata. Also owns
//! the LICENSE/README/CHANGELOG auto-default behaviour.

use std::path::{Path, PathBuf};

use anyhow::Result;

use anodizer_core::config::ArchiveFileSpec;
use anodizer_core::context::Context;

use crate::formats::{normalize_archive_path, resolve_glob_patterns};

/// Compute the longest common byte prefix of a slice of strings.
/// Returns an empty string when the slice is empty.
pub(crate) fn longest_common_prefix(strs: &[String]) -> String {
    if strs.is_empty() {
        return String::new();
    }
    let mut lcp = strs[0].as_str();
    for s in &strs[1..] {
        let end = lcp
            .bytes()
            .zip(s.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        lcp = &lcp[..end];
    }
    lcp.to_string()
}

/// Render template expressions in `ArchiveFileInfo` fields.
///
/// `owner`, `group`, and `mtime` are processed through the template
/// engine. `mode` is an octal literal and is
/// passed through unchanged.
pub(crate) fn render_file_info(
    info: &anodizer_core::config::ArchiveFileInfo,
    ctx: &Context,
) -> Result<anodizer_core::config::ArchiveFileInfo> {
    Ok(anodizer_core::config::ArchiveFileInfo {
        owner: info
            .owner
            .as_deref()
            .map(|s| ctx.render_template(s))
            .transpose()?,
        group: info
            .group
            .as_deref()
            .map(|s| ctx.render_template(s))
            .transpose()?,
        mode: info.mode,
        mtime: info
            .mtime
            .as_deref()
            .map(|s| ctx.render_template(s))
            .transpose()?,
    })
}

/// A resolved extra file to include in an archive, with optional destination
/// path override and file info (permissions/owner/group).
#[derive(Debug)]
pub struct ResolvedExtraFile {
    pub src: PathBuf,
    /// When Some, use this path inside the archive instead of the filename.
    pub dst: Option<String>,
    /// File metadata to apply to the archive entry.
    pub info: Option<anodizer_core::config::ArchiveFileInfo>,
    /// When true, strip the parent directory from the source path so the file
    /// is placed at the archive root (or directly under wrap_in_directory).
    pub strip_parent: bool,
    /// True when this entry came from the auto-resolved default file list
    /// (LICENSE/README/CHANGELOG glob), not user-configured `files:`.
    /// Defaults to `true` on archive file entries —
    /// useful for diagnostics that need to distinguish user intent from
    /// automatic defaults.
    pub default: bool,
}

/// Resolve a list of ArchiveFileSpec entries into concrete file paths with
/// optional destination overrides and file info.
pub fn resolve_file_specs(specs: &[ArchiveFileSpec]) -> Result<Vec<ResolvedExtraFile>> {
    let mut results = Vec::new();
    for spec in specs {
        match spec {
            ArchiveFileSpec::Glob(pattern) => {
                let paths = resolve_glob_patterns(std::slice::from_ref(pattern))?;
                for p in paths {
                    results.push(ResolvedExtraFile {
                        src: p,
                        dst: None,
                        info: None,
                        strip_parent: false,
                        default: false,
                    });
                }
            }
            ArchiveFileSpec::Detailed {
                src,
                dst,
                info,
                strip_parent,
            } => {
                let paths = resolve_glob_patterns(std::slice::from_ref(src))?;
                let do_strip = strip_parent.unwrap_or(false);

                // When dst is set and strip_parent is false, compute per-file
                // destinations that preserve the directory structure relative
                // to the longest common prefix of all matched paths.
                //
                // When src is a non-glob literal, it is used as the prefix
                // directly, so
                // Rel(file, file) = "." and the file is effectively renamed
                // to dst. We always compute LCP, which for a single file
                // produces dst/filename — more intuitive behavior (e.g.
                // dst: "licenses/" puts the file inside a licenses directory
                // rather than renaming it).
                if let Some(dst_prefix) = dst.as_deref().filter(|_| !do_strip) {
                    let file_strs: Vec<String> = paths
                        .iter()
                        .map(|p| p.to_string_lossy().to_string())
                        .collect();

                    // Compute prefix directory: use the LCP of matched paths,
                    // then take its parent directory if it's not an existing
                    // directory (a dirname fallback).
                    let lcp = longest_common_prefix(&file_strs);
                    let prefix_dir = {
                        let lcp_path = std::path::Path::new(&lcp);
                        if lcp_path.is_dir() {
                            lcp_path.to_path_buf()
                        } else {
                            lcp_path
                                .parent()
                                .unwrap_or_else(|| std::path::Path::new(""))
                                .to_path_buf()
                        }
                    };

                    for p in paths {
                        let rel = p
                            .strip_prefix(&prefix_dir)
                            .unwrap_or(&p)
                            .to_string_lossy()
                            .to_string();
                        let dest =
                            normalize_archive_path(std::path::PathBuf::from(dst_prefix).join(&rel))
                                .to_string_lossy()
                                .to_string();
                        results.push(ResolvedExtraFile {
                            src: p,
                            dst: Some(dest),
                            info: info.clone(),
                            strip_parent: false,
                            default: false,
                        });
                    }
                } else if dst.is_some() && do_strip {
                    // When both dst and
                    // strip_parent are set, each file's destination is
                    // dst/basename(path) so files don't collide at a single dst.
                    let dst_prefix = dst.as_deref().unwrap_or("");
                    for p in paths {
                        let base = p
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let dest = normalize_archive_path(
                            std::path::PathBuf::from(dst_prefix).join(&base),
                        )
                        .to_string_lossy()
                        .to_string();
                        results.push(ResolvedExtraFile {
                            src: p,
                            dst: Some(dest),
                            info: info.clone(),
                            strip_parent: false,
                            default: false,
                        });
                    }
                } else {
                    for p in paths {
                        results.push(ResolvedExtraFile {
                            src: p,
                            dst: dst.clone(),
                            info: info.clone(),
                            strip_parent: do_strip,
                            default: false,
                        });
                    }
                }
            }
        }
    }
    Ok(results)
}

/// When no extra files are explicitly configured, glob for common project files
/// (LICENSE, README, CHANGELOG) in `base_dir`, following the
/// Default() behavior. Non-matching patterns are silently skipped.
///
/// `base_dir` must be the crate's root directory (resolved absolute against the
/// project root). Globbing in CWD is unsafe in test/CI environments where the
/// process working directory may be the workspace root and pull in unrelated
/// files (e.g. the workspace's top-level README).
pub(crate) fn resolve_default_extra_files(base_dir: &Path) -> Vec<ResolvedExtraFile> {
    // Default order: lowercase glob first, then
    // uppercase, for each of license / readme / changelog. On case-insensitive
    // filesystems (macOS HFS+, Windows NTFS default), this controls which file
    // is picked first when both `LICENSE` and `license` exist; this
    // ordering keeps produced archives byte-stable.
    let patterns = [
        "license*",
        "LICENSE*",
        "readme*",
        "README*",
        "changelog*",
        "CHANGELOG*",
    ];
    let mut results = Vec::new();
    for pattern in &patterns {
        let full_pattern = base_dir.join(pattern);
        let Some(pattern_str) = full_pattern.to_str() else {
            continue;
        };
        if let Ok(entries) = glob::glob(pattern_str) {
            for entry in entries.flatten() {
                if !results.iter().any(|r: &ResolvedExtraFile| r.src == entry) {
                    results.push(ResolvedExtraFile {
                        src: entry,
                        dst: None,
                        info: None,
                        strip_parent: false,
                        default: true,
                    });
                }
            }
        }
    }
    results
}
