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

    // ---- Synthetic Mach-O fixtures ------------------------------------
    //
    // The `object` crate is built without its `write` feature, so the parse
    // paths (thin/fat headers, LC_BUILD_VERSION vs LC_VERSION_MIN_MACOSX) are
    // exercised against hand-assembled headers. The byte layouts follow
    // `<mach-o/loader.h>`: thin headers are little-endian, fat headers are
    // big-endian.

    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const CPU_TYPE_ARM64: u32 = 0x0100_000c;
    const CPU_TYPE_X86_64: u32 = 0x0100_0007;
    const MH_EXECUTE: u32 = 0x2;
    const PLATFORM_MACOS: u32 = 1;
    const FAT_MAGIC: u32 = 0xcafe_babe;

    /// Pack `major.minor.patch` into the 16.8.8-bit form Mach-O version load
    /// commands use.
    fn pack_ver(major: u16, minor: u16, patch: u16) -> u32 {
        (u32::from(major) << 16) | (u32::from(minor) << 8) | u32::from(patch)
    }

    fn wrap_thin(cputype: u32, load_command: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        out.extend_from_slice(&cputype.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // cpusubtype
        out.extend_from_slice(&MH_EXECUTE.to_le_bytes());
        let ncmds = if load_command.is_empty() { 0 } else { 1 };
        out.extend_from_slice(&(ncmds as u32).to_le_bytes());
        out.extend_from_slice(&(load_command.len() as u32).to_le_bytes()); // sizeofcmds
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved
        out.extend_from_slice(load_command);
        out
    }

    /// Thin 64-bit Mach-O carrying one LC_BUILD_VERSION (modern linkers).
    fn thin_build_version(cputype: u32, minos: u32) -> Vec<u8> {
        let mut lc = Vec::new();
        lc.extend_from_slice(&LC_BUILD_VERSION.to_le_bytes());
        lc.extend_from_slice(&24u32.to_le_bytes()); // cmdsize
        lc.extend_from_slice(&PLATFORM_MACOS.to_le_bytes());
        lc.extend_from_slice(&minos.to_le_bytes());
        lc.extend_from_slice(&0u32.to_le_bytes()); // sdk
        lc.extend_from_slice(&0u32.to_le_bytes()); // ntools
        wrap_thin(cputype, &lc)
    }

    /// Thin 64-bit Mach-O carrying one LC_VERSION_MIN_MACOSX (older targets).
    fn thin_version_min(cputype: u32, version: u32) -> Vec<u8> {
        let mut lc = Vec::new();
        lc.extend_from_slice(&LC_VERSION_MIN_MACOSX.to_le_bytes());
        lc.extend_from_slice(&16u32.to_le_bytes()); // cmdsize
        lc.extend_from_slice(&version.to_le_bytes()); // version at offset 8
        lc.extend_from_slice(&0u32.to_le_bytes()); // sdk at offset 12
        wrap_thin(cputype, &lc)
    }

    /// Fat (universal) 32-bit container over the given `(cputype, slice)`
    /// pairs. Fat headers and arch tables are big-endian.
    fn fat32(slices: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let header_size = 8 + 20 * slices.len();
        let mut arch_table = Vec::new();
        let mut body = Vec::new();
        let mut offset = header_size as u32;
        for (cputype, data) in slices {
            arch_table.extend_from_slice(&cputype.to_be_bytes());
            arch_table.extend_from_slice(&0u32.to_be_bytes()); // cpusubtype
            arch_table.extend_from_slice(&offset.to_be_bytes());
            arch_table.extend_from_slice(&(data.len() as u32).to_be_bytes());
            arch_table.extend_from_slice(&0u32.to_be_bytes()); // align (2^0)
            body.extend_from_slice(data);
            offset += data.len() as u32;
        }
        let mut out = Vec::new();
        out.extend_from_slice(&FAT_MAGIC.to_be_bytes());
        out.extend_from_slice(&(slices.len() as u32).to_be_bytes());
        out.extend_from_slice(&arch_table);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn thin_reads_build_version_minos() {
        let bytes = thin_build_version(CPU_TYPE_ARM64, pack_ver(11, 0, 0));
        assert!(is_macho(&bytes), "synthetic thin Mach-O must be recognized");
        assert_eq!(macho_min_os_version(&bytes).unwrap(), Some((11, 0)));
    }

    #[test]
    fn thin_reads_version_min_macosx_minor() {
        // LC_VERSION_MIN_MACOSX packs 10.13.0; the minor byte must survive.
        let bytes = thin_version_min(CPU_TYPE_X86_64, pack_ver(10, 13, 0));
        assert_eq!(macho_min_os_version(&bytes).unwrap(), Some((10, 13)));
    }

    #[test]
    fn thin_macho_without_version_command_is_none_but_still_macho() {
        // A real Mach-O that declares no version load command is Ok(None)
        // for the version probe yet still `is_macho` — the two must not be
        // conflated (the doc contract the wheel publisher relies on).
        let bytes = wrap_thin(CPU_TYPE_ARM64, &[]);
        assert!(is_macho(&bytes));
        assert_eq!(macho_min_os_version(&bytes).unwrap(), None);
    }

    #[test]
    fn fat_returns_max_minos_across_slices() {
        // Two slices at 11.0 and 12.3: the honest floor for the single fat
        // artifact is the MAX (12.3), the version below which one slice
        // refuses to run.
        let arm = thin_build_version(CPU_TYPE_ARM64, pack_ver(12, 3, 0));
        let intel = thin_build_version(CPU_TYPE_X86_64, pack_ver(11, 0, 0));
        let bytes = fat32(&[(CPU_TYPE_X86_64, intel), (CPU_TYPE_ARM64, arm)]);
        assert!(
            is_macho(&bytes),
            "fat container must be recognized as Mach-O"
        );
        assert_eq!(macho_min_os_version(&bytes).unwrap(), Some((12, 3)));
    }

    #[test]
    fn fat_with_no_versioned_slices_is_none() {
        // A fat container whose slices declare no version command yields
        // Ok(None) — the accumulator never sees a value.
        let bare = wrap_thin(CPU_TYPE_ARM64, &[]);
        let bytes = fat32(&[(CPU_TYPE_ARM64, bare)]);
        assert!(is_macho(&bytes));
        assert_eq!(macho_min_os_version(&bytes).unwrap(), None);
    }
}
