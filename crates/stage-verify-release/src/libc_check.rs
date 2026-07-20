use super::*;
use anodizer_core::libc_check;

/// Run the libc-ceiling check on one Linux package's embedded ELF binary.
///
/// Returns whether an ELF was actually extracted and evaluated (or the read
/// failed, which pushed an issue) — `false` on the no-inspectable-ELF skip,
/// so the caller does not count a package that yielded nothing to check.
pub(crate) fn check_one_package_libc(
    log: &StageLogger,
    crate_name: &str,
    pkg_path: std::path::PathBuf,
    ceiling: &str,
    issues: &mut Vec<String>,
) -> bool {
    let elf_bytes = match extract_package_main_elf(&pkg_path) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            log.verbose(&format!(
                "skipped libc check for crate '{crate_name}' {} — \
                 has no inspectable ELF",
                pkg_path.display()
            ));
            return false;
        }
        Err(e) => {
            issues.push(format!(
                "could not read {} of crate '{crate_name}' for the libc check: {e}",
                pkg_path.display()
            ));
            return true;
        }
    };
    match libc_check::check_glibc_ceiling(&elf_bytes, ceiling) {
        Ok(LibcCheckOutcome::NoGlibcRequirement) => {
            log.verbose(&format!(
                "crate '{crate_name}' {} has no glibc requirement \
                 (static/musl) — skipped",
                pkg_path.display()
            ));
        }
        Ok(LibcCheckOutcome::WithinCeiling { max }) => {
            log.verbose(&format!(
                "crate '{crate_name}' {} requires glibc {max} (<= {ceiling})",
                pkg_path.display()
            ));
        }
        Ok(LibcCheckOutcome::ExceedsCeiling { max, ceiling }) => {
            issues.push(format!(
                "{} of crate '{crate_name}' requires glibc {max}, exceeding the \
                 configured ceiling {ceiling}",
                pkg_path.display()
            ));
        }
        Err(e) => {
            issues.push(format!(
                "libc check failed for {} of crate '{crate_name}': {e}",
                pkg_path.display()
            ));
        }
    }
    true
}

/// All Linux-package artifacts for a crate as `(absolute_path, basename,
/// build_target)`.
///
/// The path is canonicalized (falling back to the registered path) so both
/// consumers work: the libc check reads the file, and the smoke-test
/// bind-mounts it into a container (which requires an absolute host path).
/// The target triple (when the package was built for one) lets the smoke-test
/// pin its container to the package's architecture. Callers filter by
/// extension at the call site.
pub(crate) fn linux_packages(
    ctx: &Context,
    crate_name: &str,
) -> Vec<(std::path::PathBuf, String, Option<String>)> {
    ctx.artifacts
        .by_kind_and_crate(ArtifactKind::LinuxPackage, crate_name)
        .into_iter()
        .map(|a| {
            let abs = std::fs::canonicalize(&a.path).unwrap_or_else(|_| a.path.clone());
            (abs, a.name.clone(), a.target.clone())
        })
        .collect()
}

/// Extract the largest executable ELF from a Linux package's payload —
/// `.deb` (ar + `data.tar.{gz,xz,zst}`), `.rpm` (lead + headers + compressed
/// cpio newc payload), or `.apk` (gzipped tar).
///
/// The shipped binary lives under `usr/bin/` (or similar). The largest
/// ELF member is picked as the binary to glibc-check — the common
/// single-binary case. Returns `Ok(None)` when no ELF is found (e.g. a
/// data-only package).
///
/// Extraction is intentionally minimal and dependency-free: it scans the
/// payload for ELF members by magic bytes. A malformed container or a
/// compression codec not linked into this build returns `Ok(None)` rather
/// than erroring — the libc check is best-effort.
pub(crate) fn extract_package_main_elf(pkg_path: &std::path::Path) -> Result<Option<Vec<u8>>> {
    let bytes = std::fs::read(pkg_path)?;
    let name = pkg_path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name.ends_with(".deb") {
        let Some(data_tar) = deb::find_data_tar(&bytes)? else {
            return Ok(None);
        };
        Ok(deb::largest_elf_in_tar(&data_tar))
    } else if name.ends_with(".rpm") {
        rpm::payload_largest_elf(&bytes)
    } else if name.ends_with(".apk") {
        // An apk is (possibly concatenated) gzip streams of tar segments;
        // MultiGzDecoder crosses the stream boundaries so the data segment's
        // members are walked the same way the deb data.tar walk is.
        use std::io::Read as _;
        let mut tar_bytes = Vec::new();
        if flate2::read::MultiGzDecoder::new(bytes.as_slice())
            .read_to_end(&mut tar_bytes)
            .is_err()
        {
            return Ok(None);
        }
        Ok(deb::largest_elf_in_tar(&tar_bytes))
    } else {
        Ok(None)
    }
}
