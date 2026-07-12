//! Mach-O minimum-OS inspection.
//!
//! macOS binary wheels are tagged with the deployment target the binary was
//! built for (`macosx_11_0_arm64`), so publishers need the real `minos` a
//! Mach-O declares rather than a hard-coded guess. Reads
//! `LC_BUILD_VERSION` (modern linkers) falling back to
//! `LC_VERSION_MIN_MACOSX` (older deployment targets). Pure file parsing —
//! no subprocess — via the same `object` crate as [`crate::libc_check`].

use anyhow::{Context as _, Result};
use object::macho::{LC_BUILD_VERSION, LC_VERSION_MIN_MACOSX, MachHeader32, MachHeader64};
use object::read::macho::{FatArch, MachHeader, MachOFatFile32, MachOFatFile64};
use object::{Endianness, FileKind};

/// Minimum macOS version a Mach-O declares, as `(major, minor)`.
///
/// For a universal (fat) binary, returns the MAXIMUM minos across the
/// embedded slices — the version below which at least one slice refuses to
/// run, which is the honest floor for a single artifact serving all arches.
/// Returns `Ok(None)` for files that are not Mach-O or that declare no
/// version load command.
pub fn macho_min_os_version(bytes: &[u8]) -> Result<Option<(u16, u16)>> {
    // An unparseable magic (too-short file, scripts, unknown formats) is
    // "not a Mach-O", not an error — the doc contract is Ok(None) for
    // non-Mach-O input.
    let Ok(kind) = FileKind::parse(bytes) else {
        return Ok(None);
    };
    match kind {
        FileKind::MachO32 => thin_min_os::<MachHeader32<Endianness>>(bytes),
        FileKind::MachO64 => thin_min_os::<MachHeader64<Endianness>>(bytes),
        FileKind::MachOFat32 => fat_max_min_os(
            MachOFatFile32::parse(bytes)
                .context("parse fat header")?
                .arches(),
            bytes,
        ),
        FileKind::MachOFat64 => fat_max_min_os(
            MachOFatFile64::parse(bytes)
                .context("parse fat header")?
                .arches(),
            bytes,
        ),
        _ => Ok(None),
    }
}

/// Maximum `minos` across the slices of a fat (universal) Mach-O — the
/// version below which at least one slice refuses to run, the honest floor
/// for a single artifact serving every arch. Generic over the 32-/64-bit
/// [`FatArch`] so the two fat-header classes share one body (they were
/// token-identical modulo the arch type).
fn fat_max_min_os<A: FatArch>(arches: &[A], bytes: &[u8]) -> Result<Option<(u16, u16)>> {
    let mut max: Option<(u16, u16)> = None;
    for arch in arches {
        let data = arch.data(bytes).context("fat slice bounds")?;
        if let Some(v) = macho_min_os_version(data)? {
            max = Some(max.map_or(v, |m| m.max(v)));
        }
    }
    Ok(max)
}

/// True when `bytes` begin with a Mach-O magic — thin (`MachO32`/`MachO64`)
/// or fat (`MachOFat32`/`MachOFat64`).
///
/// Distinct from [`macho_min_os_version`], which returns `Ok(None)` both for
/// a healthy Mach-O that declares no version load command AND for bytes that
/// are not Mach-O at all. A darwin-target publisher needs to tell those apart:
/// a real Mach-O missing its `LC_BUILD_VERSION` may fall back to a default
/// deployment target, but a non-Mach-O artifact routed under a darwin triple
/// is the wrong binary and must hard-error (the Mach-O analogue of the gnu
/// path's "no GLIBC_* requirement" error).
pub fn is_macho(bytes: &[u8]) -> bool {
    matches!(
        FileKind::parse(bytes),
        Ok(FileKind::MachO32 | FileKind::MachO64 | FileKind::MachOFat32 | FileKind::MachOFat64)
    )
}

/// Scan one thin Mach-O's load commands for a minimum-OS declaration.
fn thin_min_os<Mach: MachHeader<Endian = Endianness>>(bytes: &[u8]) -> Result<Option<(u16, u16)>> {
    let header = Mach::parse(bytes, 0).context("parse Mach-O header")?;
    let endian = header.endian().context("Mach-O endianness")?;
    let mut commands = header
        .load_commands(endian, bytes, 0)
        .context("load commands")?;
    while let Some(cmd) = commands.next().context("read load command")? {
        let raw = match cmd.cmd() {
            LC_BUILD_VERSION => cmd
                .build_version()
                .context("LC_BUILD_VERSION body")?
                .map(|bv| bv.minos.get(endian)),
            LC_VERSION_MIN_MACOSX => {
                // VersionMinCommand: version at offset 8 (u32, X.Y.Z packed
                // as 16.8.8 bits).
                let data = cmd.raw_data();
                if data.len() >= 12 {
                    let mut v = [0u8; 4];
                    v.copy_from_slice(&data[8..12]);
                    Some(match endian {
                        Endianness::Little => u32::from_le_bytes(v),
                        Endianness::Big => u32::from_be_bytes(v),
                    })
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(packed) = raw {
            return Ok(Some(((packed >> 16) as u16, ((packed >> 8) & 0xff) as u16)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_macho_bytes_are_none() {
        assert_eq!(macho_min_os_version(b"\x7fELF-not-really").unwrap(), None);
        assert_eq!(macho_min_os_version(&[]).unwrap(), None);
    }

    #[test]
    fn is_macho_rejects_non_macho_bytes() {
        // An ELF (or any non-Mach-O) under a darwin triple is the wrong
        // binary — is_macho lets the wheel publisher hard-error instead of
        // silently substituting a guessed deployment target.
        assert!(!is_macho(b"\x7fELF-not-really"));
        assert!(!is_macho(&[]));
        assert!(!is_macho(b"#!/bin/sh\n"));
    }
}
