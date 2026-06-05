//! glibc-ceiling check.
//!
//! A binary built against a newer glibc than the oldest distro you support
//! will fail to run there with a cryptic `version 'GLIBC_2.xx' not found`.
//! This check reads the required glibc symbol versions from a `.deb`'s
//! embedded ELF binary (the `.gnu.version_r` / verneed records, surfaced by
//! the `object` crate) and fails if the maximum required version exceeds a
//! configured floor.
//!
//! musl-linked binaries have NO glibc requirement (no verneed entries naming
//! `GLIBC_*`) and are SKIPPED — which is precisely the point: a musl build
//! hides a glibc-floor regression, so "no glibc requirement" is a pass, not a
//! defect.
//!
//! Two layers, both unit-testable without a network or Docker:
//! - [`GlibcVersion`] — numeric, component-wise version parse + compare
//!   (tested on synthetic strings; `2.36` vs `2.4` vs `2.36.1` must order
//!   numerically, NOT lexically).
//! - [`max_glibc_requirement`] — extracts the maximum `GLIBC_*` requirement
//!   from ELF bytes via `object` (tested on a real binary / fixture).

use std::cmp::Ordering;

use anyhow::Result;
use object::elf::{FileHeader32, FileHeader64};
use object::read::elf::FileHeader;
use object::{Endianness, FileKind, SymbolIndex};

/// A dotted glibc version (e.g. `GLIBC_2.36` → `[2, 36]`), compared
/// component-wise as integers so `2.36 > 2.4` (lexical string compare would
/// wrongly order `2.4 > 2.36`).
#[derive(Debug, Clone)]
pub struct GlibcVersion {
    components: Vec<u64>,
    /// The original dotted text, retained for diagnostics.
    raw: String,
}

// Equality is defined by the numeric ordering (trailing-zero-insensitive), so
// `2.36 == 2.36.0`. Deriving `PartialEq` would instead compare the component
// vecs field-wise and the `raw` string, making those two unequal — which would
// contradict `Ord`. Keep the two consistent.
impl PartialEq for GlibcVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for GlibcVersion {}

impl GlibcVersion {
    /// Parse a dotted version body (the part AFTER the `GLIBC_` prefix, e.g.
    /// `2.36` or `2.2.5`). Returns `None` when a component is non-numeric.
    pub fn parse(body: &str) -> Option<Self> {
        let mut components = Vec::new();
        for part in body.split('.') {
            components.push(part.parse::<u64>().ok()?);
        }
        if components.is_empty() {
            return None;
        }
        Some(Self {
            components,
            raw: body.to_string(),
        })
    }

    /// The original dotted text (without the `GLIBC_` prefix).
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl PartialOrd for GlibcVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GlibcVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        // Component-wise numeric compare; a missing trailing component counts
        // as 0 so `2.36` == `2.36.0` and `2.36.1 > 2.36`.
        let len = self.components.len().max(other.components.len());
        for i in 0..len {
            let a = self.components.get(i).copied().unwrap_or(0);
            let b = other.components.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => continue,
                non_eq => return non_eq,
            }
        }
        Ordering::Equal
    }
}

/// Outcome of checking one `.deb`'s embedded binary against a glibc ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibcCheckOutcome {
    /// No `GLIBC_*` requirement found — a static or musl binary. Skipped (a
    /// pass): musl hides the very floor regression this check guards.
    NoGlibcRequirement,
    /// The maximum required glibc version is within the ceiling.
    WithinCeiling {
        /// The highest `GLIBC_*` version the binary requires.
        max: String,
    },
    /// The maximum required glibc version EXCEEDS the ceiling — a defect.
    ExceedsCeiling {
        /// The highest `GLIBC_*` version the binary requires.
        max: String,
        /// The configured ceiling it exceeds.
        ceiling: String,
    },
}

/// Extract the maximum `GLIBC_*` symbol-version requirement from an ELF's
/// `.gnu.version_r` data, regardless of class (32- or 64-bit) or byte order
/// (little- or big-endian).
///
/// Both 32-bit (i686 / armv7) and 64-bit are supported release targets, so the
/// parse dispatches on [`object::FileKind`] and runs the same symbol-version
/// scan over whichever ELF header class the bytes carry. Endianness is read
/// from the header at runtime.
///
/// Returns `Ok(None)` when the bytes have no versioned-symbol table, no
/// `GLIBC_*` requirement (static / musl), OR are not a parseable ELF at all
/// (a non-ELF or unsupported object DEGRADES TO SKIP rather than failing): the
/// check exists only to flag a real glibc-floor violation, so an
/// uninspectable artifact is a pass, not a defect. Returns `Ok(Some(max))`
/// with the numerically-greatest requirement otherwise.
pub fn max_glibc_requirement(elf_bytes: &[u8]) -> Result<Option<GlibcVersion>> {
    match FileKind::parse(elf_bytes) {
        Ok(FileKind::Elf32) => {
            match FileHeader32::<Endianness>::parse(elf_bytes) {
                Ok(elf) => scan_glibc_requirement(elf, elf_bytes),
                // A header that fails the deeper parse is uninspectable, not a
                // glibc violation: degrade to SKIP.
                Err(_) => Ok(None),
            }
        }
        Ok(FileKind::Elf64) => match FileHeader64::<Endianness>::parse(elf_bytes) {
            Ok(elf) => scan_glibc_requirement(elf, elf_bytes),
            Err(_) => Ok(None),
        },
        // Not an ELF (or an uninspected object kind): skip, don't flag.
        _ => Ok(None),
    }
}

/// Scan one parsed ELF header's versioned dynamic symbols for the
/// numerically-greatest `GLIBC_*` requirement.
///
/// Generic over the ELF header class so the same logic serves 32- and 64-bit,
/// little- and big-endian binaries. Returns `Ok(None)` when the ELF has no
/// `.gnu.version` table or no `GLIBC_*` entry (static / musl).
fn scan_glibc_requirement<Elf: FileHeader<Endian = Endianness>>(
    elf: &Elf,
    elf_bytes: &[u8],
) -> Result<Option<GlibcVersion>> {
    // A malformed section/symbol table on an otherwise-valid ELF header is
    // uninspectable, not a glibc violation: each step degrades to SKIP
    // (`Ok(None)`) rather than surfacing an error the caller would log as a
    // false post-release issue.
    let Ok(endian) = elf.endian() else {
        return Ok(None);
    };
    let Ok(sections) = elf.sections(endian, elf_bytes) else {
        return Ok(None);
    };
    let Ok(versions) = sections.versions(endian, elf_bytes) else {
        return Ok(None);
    };
    let Some(version_table) = versions else {
        // No `.gnu.version` table at all — static / musl binary.
        return Ok(None);
    };
    let Ok(symbols) = sections.symbols(endian, elf_bytes, object::elf::SHT_DYNSYM) else {
        return Ok(None);
    };

    let mut max: Option<GlibcVersion> = None;
    for index in 0..symbols.len() {
        let vindex = version_table.version_index(endian, SymbolIndex(index));
        let Ok(Some(version)) = version_table.version(vindex) else {
            continue;
        };
        let Ok(name) = std::str::from_utf8(version.name()) else {
            continue;
        };
        let Some(body) = name.strip_prefix("GLIBC_") else {
            continue;
        };
        let Some(parsed) = GlibcVersion::parse(body) else {
            continue;
        };
        if max.as_ref().is_none_or(|cur| parsed > *cur) {
            max = Some(parsed);
        }
    }
    Ok(max)
}

/// Check one ELF binary's glibc requirement against `ceiling`.
///
/// `ceiling` is the dotted glibc floor (e.g. `"2.36"`). A binary requiring a
/// glibc strictly NEWER than `ceiling` produces [`LibcCheckOutcome::ExceedsCeiling`].
pub fn check_glibc_ceiling(elf_bytes: &[u8], ceiling: &str) -> Result<LibcCheckOutcome> {
    let ceiling_ver = GlibcVersion::parse(ceiling).ok_or_else(|| {
        anyhow::anyhow!("verify-release: invalid glibc_ceiling '{ceiling}' (expected e.g. 2.36)")
    })?;
    match max_glibc_requirement(elf_bytes)? {
        None => Ok(LibcCheckOutcome::NoGlibcRequirement),
        Some(max) if max > ceiling_ver => Ok(LibcCheckOutcome::ExceedsCeiling {
            max: max.raw().to_string(),
            ceiling: ceiling.to_string(),
        }),
        Some(max) => Ok(LibcCheckOutcome::WithinCeiling {
            max: max.raw().to_string(),
        }),
    }
}

/// Compare a set of `GLIBC_*` requirement strings against a ceiling, without
/// touching ELF bytes — the synthetic-symbol-list path used in tests and by
/// any caller that has already extracted requirement names.
pub fn check_glibc_requirements(requirements: &[&str], ceiling: &str) -> Result<LibcCheckOutcome> {
    let ceiling_ver = GlibcVersion::parse(ceiling).ok_or_else(|| {
        anyhow::anyhow!("verify-release: invalid glibc_ceiling '{ceiling}' (expected e.g. 2.36)")
    })?;
    let max = requirements
        .iter()
        .filter_map(|r| r.strip_prefix("GLIBC_"))
        .filter_map(GlibcVersion::parse)
        .max();
    match max {
        None => Ok(LibcCheckOutcome::NoGlibcRequirement),
        Some(max) if max > ceiling_ver => Ok(LibcCheckOutcome::ExceedsCeiling {
            max: max.raw().to_string(),
            ceiling: ceiling.to_string(),
        }),
        Some(max) => Ok(LibcCheckOutcome::WithinCeiling {
            max: max.raw().to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_compare_beats_lexical() {
        // Lexically "2.4" > "2.36" (because '4' > '3'); numerically it's the
        // reverse. This is the canonical bug the check must not have.
        let v2_4 = GlibcVersion::parse("2.4").unwrap();
        let v2_36 = GlibcVersion::parse("2.36").unwrap();
        assert!(v2_36 > v2_4, "2.36 must be greater than 2.4 numerically");
    }

    #[test]
    fn patch_component_ordering() {
        let v2_36 = GlibcVersion::parse("2.36").unwrap();
        let v2_36_1 = GlibcVersion::parse("2.36.1").unwrap();
        assert!(v2_36_1 > v2_36, "2.36.1 > 2.36");
        // Trailing zero equals the shorter form.
        assert_eq!(GlibcVersion::parse("2.36.0").unwrap(), v2_36);
    }

    #[test]
    fn rejects_non_numeric() {
        assert!(GlibcVersion::parse("2.x").is_none());
        assert!(GlibcVersion::parse("").is_none());
    }

    #[test]
    fn requirement_exceeds_ceiling_fails() {
        let out = check_glibc_requirements(&["GLIBC_2.2.5", "GLIBC_2.38"], "2.36").unwrap();
        assert_eq!(
            out,
            LibcCheckOutcome::ExceedsCeiling {
                max: "2.38".to_string(),
                ceiling: "2.36".to_string(),
            }
        );
    }

    #[test]
    fn requirement_within_ceiling_passes() {
        let out = check_glibc_requirements(&["GLIBC_2.2.5", "GLIBC_2.31"], "2.36").unwrap();
        assert_eq!(
            out,
            LibcCheckOutcome::WithinCeiling {
                max: "2.31".to_string()
            }
        );
    }

    #[test]
    fn requirement_equal_to_ceiling_passes() {
        // Equal is within (the ceiling is the floor you support, inclusive).
        let out = check_glibc_requirements(&["GLIBC_2.36"], "2.36").unwrap();
        assert_eq!(
            out,
            LibcCheckOutcome::WithinCeiling {
                max: "2.36".to_string()
            }
        );
    }

    #[test]
    fn no_glibc_requirement_is_skipped() {
        // A musl binary's verneed has no GLIBC_* entries.
        let out = check_glibc_requirements(&["libc.so.6"], "2.36").unwrap();
        assert_eq!(out, LibcCheckOutcome::NoGlibcRequirement);
        let empty = check_glibc_requirements(&[], "2.36").unwrap();
        assert_eq!(empty, LibcCheckOutcome::NoGlibcRequirement);
    }

    #[test]
    fn lexical_max_would_be_wrong() {
        // /bin/ls-style real requirement set: lexical max is "2.9", numeric
        // max is "2.34". The check must use numeric.
        let reqs = [
            "GLIBC_2.29",
            "GLIBC_2.2.5",
            "GLIBC_2.34",
            "GLIBC_2.4",
            "GLIBC_2.9",
        ];
        let out = check_glibc_requirements(&reqs, "2.40").unwrap();
        assert_eq!(
            out,
            LibcCheckOutcome::WithinCeiling {
                max: "2.34".to_string()
            },
            "numeric max is 2.34, not the lexical 2.9"
        );
    }

    #[test]
    fn invalid_ceiling_errors() {
        assert!(check_glibc_requirements(&["GLIBC_2.36"], "not-a-version").is_err());
    }

    /// Build a minimal but structurally-valid 32-bit little-endian ELF header
    /// (`EI_CLASS = ELFCLASS32`, no section headers). It carries no
    /// `.gnu.version` table, so the glibc scan must reach the
    /// `NoGlibcRequirement` (skip) branch — NOT error. This is the i686/armv7
    /// case the old 64-bit-only parse rejected with a false issue.
    fn minimal_elf32_le() -> Vec<u8> {
        // 52-byte ELF32 header. e_shoff/e_shnum left zero → no sections.
        let mut h = vec![0u8; 52];
        h[0..4].copy_from_slice(b"\x7fELF");
        h[4] = 1; // EI_CLASS = ELFCLASS32
        h[5] = 1; // EI_DATA  = ELFDATA2LSB (little-endian)
        h[6] = 1; // EI_VERSION = EV_CURRENT
        // e_type = ET_DYN (3), e_machine = EM_386 (3), e_version = 1.
        h[16] = 3;
        h[18] = 3;
        h[20] = 1;
        h
    }

    #[test]
    fn elf32_parses_without_false_issue() {
        // The 64-bit-only parse returned Err on this header (a 32-bit .deb ELF),
        // which the stage pushed as a false post-release issue. Polymorphic
        // dispatch must parse it and SKIP (no glibc table → NoGlibcRequirement),
        // never error.
        let bytes = minimal_elf32_le();
        assert_eq!(FileKind::parse(bytes.as_slice()).unwrap(), FileKind::Elf32);
        let max = max_glibc_requirement(&bytes)
            .expect("32-bit ELF must parse, not error (false-issue regression)");
        assert!(
            max.is_none(),
            "no .gnu.version table → no glibc requirement"
        );
        // And the ceiling check must report a skip, not an error/exceed.
        let out = check_glibc_ceiling(&bytes, "2.36").unwrap();
        assert_eq!(out, LibcCheckOutcome::NoGlibcRequirement);
    }

    /// Build a structurally-valid 32-bit little-endian ELF whose
    /// `.gnu.version_r` declares a `GLIBC_2.99` requirement on one versioned
    /// dynamic symbol, with a section-header table linking
    /// `.gnu.version` → `.dynsym` → `.dynstr` and `.gnu.version_r` → `.dynstr`
    /// exactly as a real linker emits. The `object` crate's real verneed walk
    /// must extract `GLIBC_2.99` from this — proving 32-bit detection, not just
    /// the 32-bit no-issue skip path.
    ///
    /// Layout (all offsets file-absolute, little-endian):
    ///   [0]   ELF32 header (52 bytes)
    ///   .dynstr   string table: "\0libc.so.6\0GLIBC_2.99\0glibc99\0"
    ///   .dynsym   2 × Sym32 (16 bytes each): index 0 null, index 1 versioned
    ///   .gnu.version  2 × Versym (u16): [0 (local), 2 (our version index)]
    ///   .gnu.version_r  Verneed(16) + Vernaux(16) naming GLIBC_2.99, vna_other=2
    ///   .shstrtab one NUL byte (section names are matched by sh_type, not name)
    ///   section-header table: 6 × SectionHeader32 (40 bytes each)
    fn elf32_le_with_glibc_2_99() -> Vec<u8> {
        const SHT_STRTAB: u32 = 3;
        const SHT_DYNSYM: u32 = 11;
        const SHT_GNU_VERSYM: u32 = 0x6fff_ffff;
        const SHT_GNU_VERNEED: u32 = 0x6fff_fffe;
        // Version index assigned to the GLIBC_2.99 requirement. 0/1 are the
        // reserved local/global indices, so the first real version is 2.
        const VER_IDX: u16 = 2;

        let le32 = |buf: &mut Vec<u8>, v: u32| buf.extend_from_slice(&v.to_le_bytes());
        // .dynstr — index 0 must be the empty string.
        let mut dynstr = vec![0u8];
        let off_libc = dynstr.len() as u32;
        dynstr.extend_from_slice(b"libc.so.6\0");
        let off_glibc = dynstr.len() as u32;
        dynstr.extend_from_slice(b"GLIBC_2.99\0");
        let off_sym = dynstr.len() as u32;
        dynstr.extend_from_slice(b"glibc99\0");

        // .dynsym — Sym32 is 16 bytes: st_name(4) st_value(4) st_size(4)
        // st_info(1) st_other(1) st_shndx(2). Index 0 is the reserved null
        // entry; index 1 is our GLOBAL FUNC referencing the version.
        let mut dynsym = Vec::new();
        dynsym.extend_from_slice(&[0u8; 16]); // index 0: STN_UNDEF
        le32(&mut dynsym, off_sym); // st_name
        le32(&mut dynsym, 0); // st_value
        le32(&mut dynsym, 0); // st_size
        dynsym.push((1 << 4) | 2); // st_info: STB_GLOBAL, STT_FUNC
        dynsym.push(0); // st_other
        dynsym.extend_from_slice(&1u16.to_le_bytes()); // st_shndx (any defined)

        // .gnu.version — one Versym (u16) per dynsym entry. Symbol 0 is local
        // (index 0), symbol 1 carries our version index.
        let mut versym = Vec::new();
        versym.extend_from_slice(&0u16.to_le_bytes());
        versym.extend_from_slice(&VER_IDX.to_le_bytes());

        // .gnu.version_r — Verneed(16) { vn_version, vn_cnt, vn_file, vn_aux,
        // vn_next } then Vernaux(16) { vna_hash, vna_flags, vna_other,
        // vna_name, vna_next }. vn_aux/vna offsets are relative to the verneed
        // entry start (offset 0 here, single entry).
        let mut verneed = Vec::new();
        verneed.extend_from_slice(&1u16.to_le_bytes()); // vn_version = VER_NEED_CURRENT
        verneed.extend_from_slice(&1u16.to_le_bytes()); // vn_cnt = 1 aux
        le32(&mut verneed, off_libc); // vn_file → "libc.so.6"
        le32(&mut verneed, 16); // vn_aux → Vernaux at +16
        le32(&mut verneed, 0); // vn_next = 0 (last)
        le32(&mut verneed, 0); // vna_hash (unused by the scan)
        verneed.extend_from_slice(&0u16.to_le_bytes()); // vna_flags
        verneed.extend_from_slice(&VER_IDX.to_le_bytes()); // vna_other = version index
        le32(&mut verneed, off_glibc); // vna_name → "GLIBC_2.99"
        le32(&mut verneed, 0); // vna_next = 0 (last)

        let shstrtab = vec![0u8];

        // Concatenate section bodies after the 52-byte header, tracking each
        // body's file offset for its section header.
        let mut img = vec![0u8; 52];
        let place = |img: &mut Vec<u8>, body: &[u8]| -> (u32, u32) {
            let off = img.len() as u32;
            img.extend_from_slice(body);
            (off, body.len() as u32)
        };
        let (dynstr_off, dynstr_sz) = place(&mut img, &dynstr);
        let (dynsym_off, dynsym_sz) = place(&mut img, &dynsym);
        let (versym_off, versym_sz) = place(&mut img, &versym);
        let (verneed_off, verneed_sz) = place(&mut img, &verneed);
        let (shstr_off, shstr_sz) = place(&mut img, &shstrtab);

        // Section header table (40 bytes per SectionHeader32). Section name
        // offsets all point at the shstrtab's leading NUL — the scan finds
        // sections by sh_type, never by name.
        let shoff = img.len() as u32;
        let sh = |img: &mut Vec<u8>,
                  sh_type: u32,
                  offset: u32,
                  size: u32,
                  link: u32,
                  info: u32,
                  entsize: u32| {
            le32(img, 0); // sh_name
            le32(img, sh_type);
            le32(img, 0); // sh_flags
            le32(img, 0); // sh_addr
            le32(img, offset);
            le32(img, size);
            le32(img, link);
            le32(img, info);
            le32(img, 0); // sh_addralign
            le32(img, entsize);
        };
        // 0: SHT_NULL (required first entry).
        sh(&mut img, 0, 0, 0, 0, 0, 0);
        // 1: .dynstr.
        sh(&mut img, SHT_STRTAB, dynstr_off, dynstr_sz, 0, 0, 0);
        // 2: .dynsym → links .dynstr (section 1); sh_info = first non-local.
        sh(&mut img, SHT_DYNSYM, dynsym_off, dynsym_sz, 1, 1, 16);
        // 3: .gnu.version → links .dynsym (section 2).
        sh(&mut img, SHT_GNU_VERSYM, versym_off, versym_sz, 2, 0, 2);
        // 4: .gnu.version_r → links .dynstr (section 1); sh_info = verneed cnt.
        sh(&mut img, SHT_GNU_VERNEED, verneed_off, verneed_sz, 1, 1, 0);
        // 5: .shstrtab (named by e_shstrndx below).
        sh(&mut img, SHT_STRTAB, shstr_off, shstr_sz, 0, 0, 0);
        let shnum: u16 = 6;
        let shstrndx: u16 = 5;

        // Fill the ELF32 header now that section-table geometry is known.
        img[0..4].copy_from_slice(b"\x7fELF");
        img[4] = 1; // EI_CLASS = ELFCLASS32
        img[5] = 1; // EI_DATA  = ELFDATA2LSB
        img[6] = 1; // EI_VERSION
        img[16..18].copy_from_slice(&3u16.to_le_bytes()); // e_type = ET_DYN
        img[18..20].copy_from_slice(&3u16.to_le_bytes()); // e_machine = EM_386
        img[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        img[32..36].copy_from_slice(&shoff.to_le_bytes()); // e_shoff
        img[40..42].copy_from_slice(&52u16.to_le_bytes()); // e_ehsize
        img[46..48].copy_from_slice(&40u16.to_le_bytes()); // e_shentsize
        img[48..50].copy_from_slice(&shnum.to_le_bytes()); // e_shnum
        img[50..52].copy_from_slice(&shstrndx.to_le_bytes()); // e_shstrndx
        img
    }

    #[test]
    fn elf32_glibc_requirement_above_ceiling_is_detected() {
        // A 32-bit (i686/armv7) .deb ELF requiring GLIBC_2.99 must be DETECTED
        // as exceeding a 2.36 ceiling via the real `object` verneed walk — not
        // skipped, not passed. This is the 32-bit DETECTION path; the sibling
        // `elf32_parses_without_false_issue` only covers the 32-bit SKIP path.
        let bytes = elf32_le_with_glibc_2_99();
        assert_eq!(FileKind::parse(bytes.as_slice()).unwrap(), FileKind::Elf32);

        // The extraction must surface the real requirement, proving the scan
        // walked the Elf32 .gnu.version_r (a broken 32-bit path would return
        // None → NoGlibcRequirement, failing the assert below).
        let max = max_glibc_requirement(&bytes)
            .expect("32-bit ELF must parse")
            .expect("32-bit .gnu.version_r must yield GLIBC_2.99");
        assert_eq!(max.raw(), "2.99");

        let out = check_glibc_ceiling(&bytes, "2.36").unwrap();
        assert_eq!(
            out,
            LibcCheckOutcome::ExceedsCeiling {
                max: "2.99".to_string(),
                ceiling: "2.36".to_string(),
            },
            "32-bit GLIBC_2.99 > 2.36 must be flagged, not skipped/passed"
        );
    }

    #[test]
    fn non_elf_degrades_to_skip() {
        // A non-ELF or unparseable artifact must DEGRADE TO SKIP, not surface a
        // libc-check error the stage would log as a false post-release issue.
        let max = max_glibc_requirement(b"not an elf at all").unwrap();
        assert!(max.is_none());
        let out = check_glibc_ceiling(b"\x7fELFgarbage", "2.36").unwrap();
        assert_eq!(out, LibcCheckOutcome::NoGlibcRequirement);
    }

    #[test]
    fn real_elf_extraction_matches_numeric_max() {
        // Parse a real glibc-linked binary from the host and assert the
        // extracted max is a sane GLIBC version. This proves the `object`
        // .gnu.version_r path, not just the synthetic compare. Skips
        // gracefully when /bin/ls is absent or not a 64-bit LE ELF.
        let Ok(bytes) = std::fs::read("/bin/ls") else {
            eprintln!("skipping: /bin/ls not readable");
            return;
        };
        match max_glibc_requirement(&bytes) {
            Ok(Some(max)) => {
                // /bin/ls links glibc; the max must parse and be >= 2.2.5.
                let floor = GlibcVersion::parse("2.2.5").unwrap();
                assert!(max >= floor, "extracted glibc max {} too low", max.raw());
            }
            Ok(None) => eprintln!("skipping: /bin/ls reported no glibc requirement"),
            Err(e) => eprintln!("skipping: /bin/ls not a parseable ELF here: {e}"),
        }
    }
}
