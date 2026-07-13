//! Minimal, dependency-free ELF inspection.
//!
//! Pure file reading only (no subprocess), so it is module-boundary-legal to
//! live in core. The single probe here detects whether a binary is
//! dynamically linked by parsing just enough of the ELF header and program
//! headers to find a `PT_INTERP` segment.

use std::path::Path;

/// Little-/big-endian aware 2-byte read (shared by the path- and slice-based
/// probes).
fn read_u16(b: &[u8], is_le: bool) -> u16 {
    if is_le {
        u16::from_le_bytes([b[0], b[1]])
    } else {
        u16::from_be_bytes([b[0], b[1]])
    }
}

/// Little-/big-endian aware 4-byte read.
fn read_u32(b: &[u8], is_le: bool) -> u32 {
    if is_le {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    } else {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    }
}

/// Little-/big-endian aware 8-byte read.
fn read_u64(b: &[u8], is_le: bool) -> u64 {
    if is_le {
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    } else {
        u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    }
}

/// Extract `(program-header offset, entry size, count)` from a parsed ELF header
/// prefix, keyed by ELF class. `hdr` must already be verified long enough for
/// the class (≥ 58 bytes for 64-bit, ≥ 46 for 32-bit) — both probes gate on
/// that before calling. Shared by the path- and slice-based probes so the field
/// offsets live in exactly one place.
fn ph_locator(hdr: &[u8], is_64bit: bool, is_le: bool) -> (u64, u16, u16) {
    if is_64bit {
        // 64-bit ELF: e_phoff at 32 (8 bytes), e_phentsize at 54, e_phnum at 56.
        (
            read_u64(&hdr[32..40], is_le),
            read_u16(&hdr[54..56], is_le),
            read_u16(&hdr[56..58], is_le),
        )
    } else {
        // 32-bit ELF: e_phoff at 28 (4 bytes), e_phentsize at 42, e_phnum at 44.
        (
            read_u32(&hdr[28..32], is_le) as u64,
            read_u16(&hdr[42..44], is_le),
            read_u16(&hdr[44..46], is_le),
        )
    }
}

/// Byte-slice analogue of [`is_dynamically_linked`]: detect a `PT_INTERP`
/// segment in an ELF image already held in memory.
///
/// Returns `false` for a non-ELF, an image too short to hold the program-header
/// table for its own class, a program-header table pointing past the slice, or
/// a statically linked ELF; `true` only when a `PT_INTERP` segment is present.
///
/// A caller that already holds the binary's bytes (e.g. the PyPI wheel builder,
/// which loads each artifact to hash and repackage it) uses this rather than
/// re-reading the file through [`is_dynamically_linked`]. All indexing is
/// bounds-checked, so a malformed image yields `false`, never a panic.
pub fn is_dynamically_linked_bytes(bytes: &[u8]) -> bool {
    // Need the class/endian bytes (4, 5) plus the ELF magic.
    if bytes.len() < 6 || &bytes[0..4] != b"\x7fELF" {
        return false;
    }
    let is_64bit = bytes[4] == 2;
    let is_le = bytes[5] == 1; // 1 = little-endian, 2 = big-endian

    // A file too short to hold the program-header fields for its own class is
    // not an ELF we can inspect — a 32-bit header ends at byte 46, a 64-bit
    // one at 58.
    let min_len = if is_64bit { 58 } else { 46 };
    if bytes.len() < min_len {
        return false;
    }

    let (ph_offset, ph_entry_size, ph_count) = ph_locator(bytes, is_64bit, is_le);
    if ph_count == 0 || ph_entry_size == 0 {
        return false;
    }
    let (ph_offset, ph_entry_size) = (ph_offset as usize, ph_entry_size as usize);

    const PT_INTERP: u32 = 3;
    for i in 0..ph_count as usize {
        // Compute the entry's byte range with checked arithmetic: a malformed
        // header can carry an absurd ph_offset that overflows `usize` (a
        // debug-build panic). Any overflow means the table lies outside the
        // image — stop, reporting "not dynamically linked". A range past the
        // slice we hold is likewise a truncated/malformed image.
        let field = i
            .checked_mul(ph_entry_size)
            .and_then(|rel| rel.checked_add(ph_offset))
            .and_then(|start| start.checked_add(4).map(|end| start..end))
            .and_then(|range| bytes.get(range));
        let Some(field) = field else {
            return false;
        };
        if read_u32(field, is_le) == PT_INTERP {
            return true;
        }
    }
    false
}

/// Check whether the binary at `path` is dynamically linked by reading its ELF
/// program headers and looking for a `PT_INTERP` segment (type 3, the dynamic
/// linker).
///
/// Returns `Ok(false)` for a genuinely-absent path, a non-ELF file (macOS
/// Mach-O, Windows PE, or random bytes), a file too short to contain the header
/// fields for its own ELF class, or a statically linked ELF; `Ok(true)` when a
/// `PT_INTERP` segment is present.
///
/// Returns `Err` when a file that *does* exist cannot be read — an open failure
/// other than not-found, or a short/failed read of a confirmed ELF. A binary a
/// caller cannot inspect is a defect that must surface, never silently
/// "statically linked": swallowing it would, for example, drop
/// `autoPatchelfHook` from a Nix derivation and ship a broken install.
pub fn is_dynamically_linked(path: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        // A genuinely-absent path is not our concern (callers guard on
        // `.exists()` / only feed registered artifacts); any OTHER open
        // failure on a file we were asked to inspect is a real error, not
        // "statically linked".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };

    // Read up to a full 64-bit ELF header. `read` may return fewer bytes than
    // requested; the buffer stays zero-padded beyond what was read, so the
    // magic check below rejects anything that could not supply the first 4
    // bytes. A read *error* (not a short read) is a defect and propagates.
    let mut buf = [0u8; 64];
    let n = file.read(&mut buf)?;

    // Verify ELF magic: 0x7f 'E' 'L' 'F'. A read shorter than 4 bytes leaves
    // zero-padding here and cannot match.
    if &buf[0..4] != b"\x7fELF" {
        return Ok(false);
    }

    let is_64bit = buf[4] == 2;
    let is_le = buf[5] == 1; // 1 = little-endian, 2 = big-endian

    // The header fields we parse differ by ELF class: a 32-bit header is read
    // up to byte 46 (`e_phnum`), a 64-bit header up to byte 58. A file too
    // short to hold the fields for its own class is not an ELF we can inspect;
    // treat it as "not dynamically linked" rather than misreading zero-padding
    // as real header data. (A valid 32-bit header is 52 bytes, a 64-bit header
    // 64 — real binaries always satisfy this; only truncated inputs fail it.)
    let min_len = if is_64bit { 58 } else { 46 };
    if n < min_len {
        return Ok(false);
    }

    // Parse program header offset, entry size, and count.
    let (ph_offset, ph_entry_size, ph_count) = ph_locator(&buf, is_64bit, is_le);
    let ph_entry_size = ph_entry_size as u64;

    if ph_count == 0 || ph_entry_size == 0 {
        return Ok(false);
    }

    // Read all program headers. A confirmed-ELF header pointing at program
    // headers we cannot seek/read is a corrupt or truncated artifact — a defect
    // that propagates rather than masquerading as "statically linked".
    let total_size = ph_entry_size * ph_count as u64;
    let mut ph_buf = vec![0u8; total_size as usize];
    use std::io::Seek;
    file.seek(std::io::SeekFrom::Start(ph_offset))?;
    file.read_exact(&mut ph_buf)?;

    // Scan for PT_INTERP (type 3), the presence of a dynamic linker.
    const PT_INTERP: u32 = 3;
    for i in 0..ph_count as usize {
        let entry_start = i * ph_entry_size as usize;
        let p_type = read_u32(&ph_buf[entry_start..entry_start + 4], is_le);
        if p_type == PT_INTERP {
            return Ok(true);
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{is_dynamically_linked, is_dynamically_linked_bytes};

    /// Build a minimal 64-bit LE ELF image with a single program header of the
    /// given `p_type`, wholly in memory (no file). Mirrors the on-disk fixtures
    /// used by the path-based tests.
    fn elf64_one_phdr(p_type: u32) -> Vec<u8> {
        let phoff: u64 = 64;
        let phentsize: u16 = 56;
        let phnum: u16 = 1;
        let mut bytes = Vec::with_capacity(64 + phentsize as usize);
        bytes.extend_from_slice(b"\x7fELF");
        bytes.push(2); // 64-bit
        bytes.push(1); // little-endian
        bytes.push(1); // EI_VERSION
        bytes.extend_from_slice(&[0u8; 9]);
        bytes.extend_from_slice(&[0u8; 2]); // e_type
        bytes.extend_from_slice(&[0u8; 2]); // e_machine
        bytes.extend_from_slice(&[0u8; 4]); // e_version
        bytes.extend_from_slice(&[0u8; 8]); // e_entry
        bytes.extend_from_slice(&phoff.to_le_bytes()); // e_phoff
        bytes.extend_from_slice(&[0u8; 8]); // e_shoff
        bytes.extend_from_slice(&[0u8; 4]); // e_flags
        bytes.extend_from_slice(&[0u8; 2]); // e_ehsize
        bytes.extend_from_slice(&phentsize.to_le_bytes()); // e_phentsize
        bytes.extend_from_slice(&phnum.to_le_bytes()); // e_phnum
        bytes.extend_from_slice(&[0u8; 6]); // pad to 64
        bytes.extend_from_slice(&p_type.to_le_bytes()); // p_type
        bytes.extend_from_slice(&vec![0u8; phentsize as usize - 4]);
        bytes
    }

    /// The in-memory probe detects `PT_INTERP` (dynamic) and its absence
    /// (static), agreeing with the path-based probe on the same bytes.
    #[test]
    fn bytes_probe_detects_dynamic_and_static() {
        assert!(is_dynamically_linked_bytes(&elf64_one_phdr(3))); // PT_INTERP
        assert!(!is_dynamically_linked_bytes(&elf64_one_phdr(1))); // PT_LOAD only
    }

    /// Non-ELF and truncated images are "not dynamically linked", never a panic.
    #[test]
    fn bytes_probe_rejects_non_elf_and_truncated() {
        assert!(!is_dynamically_linked_bytes(b"not an ELF"));
        assert!(!is_dynamically_linked_bytes(b""));
        // Valid header but the program-header table is truncated away.
        let mut short = elf64_one_phdr(3);
        short.truncate(66); // header + 2 bytes of the phdr — p_type field cut off
        assert!(!is_dynamically_linked_bytes(&short));
    }

    /// A malformed header with an absurd `e_phoff` (all-ones) must not panic on
    /// the `usize` offset arithmetic — the checked math reports "not dynamically
    /// linked" instead of overflowing.
    #[test]
    fn bytes_probe_absurd_ph_offset_does_not_panic() {
        let mut evil = elf64_one_phdr(3);
        // e_phoff lives at bytes 32..40; set it to u64::MAX.
        evil[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(!is_dynamically_linked_bytes(&evil));
    }

    /// A genuinely-absent path is `Ok(false)` (open() NotFound), never an
    /// error: callers guard on existence or only feed registered artifacts.
    #[test]
    fn missing_file_returns_false() {
        assert!(
            !is_dynamically_linked(std::path::Path::new(
                "/nonexistent/path/to/binary/that/cannot/exist"
            ))
            .expect("a missing file is Ok(false), not an error")
        );
    }

    /// A file too short to hold an ELF header returns false (magic fails).
    #[test]
    fn short_file_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("tiny");
        std::fs::write(&p, b"abc").unwrap();
        assert!(!is_dynamically_linked(&p).unwrap());
    }

    /// A path that EXISTS but errors on read is a real defect, not `Ok(false)`:
    /// a directory opens as a File on Unix but errors (EISDIR) on read. A
    /// silent `false` here would mask a build artifact we merely failed to
    /// inspect and, e.g., ship a broken `nix` install.
    #[test]
    #[cfg(unix)]
    fn unreadable_path_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(is_dynamically_linked(tmp.path()).is_err());
    }

    /// A file without ELF magic (Mach-O / PE / random bytes) returns false.
    #[test]
    fn non_elf_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("not-elf");
        // 64 bytes of nonzero non-ELF data.
        let bytes: Vec<u8> = (0..64u8).collect();
        std::fs::write(&p, bytes).unwrap();
        assert!(!is_dynamically_linked(&p).unwrap());
    }

    /// Hand-rolled minimal 64-bit ELF with a single PT_INTERP program header
    /// returns true.
    #[test]
    fn elf64_with_pt_interp_returns_true() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("elf64-dyn");
        // 64-byte ELF header followed by one 56-byte program header, p_type=3.
        let phoff: u64 = 64;
        let phentsize: u16 = 56;
        let phnum: u16 = 1;
        let mut bytes = Vec::with_capacity(64 + phentsize as usize);
        bytes.extend_from_slice(b"\x7fELF"); // magic
        bytes.push(2); // 64-bit
        bytes.push(1); // little-endian
        bytes.push(1); // EI_VERSION
        bytes.extend_from_slice(&[0u8; 9]); // OSABI + padding
        bytes.extend_from_slice(&[0u8; 2]); // e_type
        bytes.extend_from_slice(&[0u8; 2]); // e_machine
        bytes.extend_from_slice(&[0u8; 4]); // e_version
        bytes.extend_from_slice(&[0u8; 8]); // e_entry
        bytes.extend_from_slice(&phoff.to_le_bytes()); // e_phoff (32..40)
        bytes.extend_from_slice(&[0u8; 8]); // e_shoff
        bytes.extend_from_slice(&[0u8; 4]); // e_flags
        bytes.extend_from_slice(&[0u8; 2]); // e_ehsize
        bytes.extend_from_slice(&phentsize.to_le_bytes()); // e_phentsize (54..56)
        bytes.extend_from_slice(&phnum.to_le_bytes()); // e_phnum (56..58)
        bytes.extend_from_slice(&[0u8; 6]); // pad to 64
        debug_assert_eq!(bytes.len(), 64);
        // Program header: p_type=3 (PT_INTERP), 4-byte LE.
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&vec![0u8; phentsize as usize - 4]);
        std::fs::write(&p, &bytes).unwrap();
        assert!(
            is_dynamically_linked(&p).unwrap(),
            "PT_INTERP must be detected"
        );
    }

    /// 64-bit ELF whose only program header is PT_LOAD (1) returns false — the
    /// file is statically linked.
    #[test]
    fn elf64_without_pt_interp_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("elf64-static");
        let phoff: u64 = 64;
        let phentsize: u16 = 56;
        let phnum: u16 = 1;
        let mut bytes = Vec::with_capacity(64 + phentsize as usize);
        bytes.extend_from_slice(b"\x7fELF");
        bytes.push(2);
        bytes.push(1);
        bytes.push(1);
        bytes.extend_from_slice(&[0u8; 9]);
        bytes.extend_from_slice(&[0u8; 2]);
        bytes.extend_from_slice(&[0u8; 2]);
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&phoff.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(&[0u8; 2]);
        bytes.extend_from_slice(&phentsize.to_le_bytes());
        bytes.extend_from_slice(&phnum.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 6]);
        debug_assert_eq!(bytes.len(), 64);
        // p_type = 1 (PT_LOAD), not 3.
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&vec![0u8; phentsize as usize - 4]);
        std::fs::write(&p, &bytes).unwrap();
        assert!(!is_dynamically_linked(&p).unwrap());
    }

    /// 32-bit ELF with PT_INTERP returns true — pins the `is_64bit=false`
    /// branch (phoff/phnum read from 32-bit offsets). The header is exactly 52
    /// bytes, proving the class-aware min-length gate admits a valid 32-bit
    /// ELF that the old 64-byte gate would have wrongly rejected.
    #[test]
    fn elf32_with_pt_interp_returns_true() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("elf32-dyn");
        // For 32-bit ELF: e_entry is 4 bytes (24..28), e_phoff 4 bytes at
        // 28..32, e_phentsize at 42..44, e_phnum at 44..46.
        let phoff: u32 = 52;
        let phentsize: u16 = 32;
        let phnum: u16 = 1;
        let mut bytes = Vec::with_capacity(52 + phentsize as usize);
        bytes.extend_from_slice(b"\x7fELF"); // 0..4
        bytes.push(1); // 32-bit class (4)
        bytes.push(1); // little-endian (5)
        bytes.push(1); // EI_VERSION (6)
        bytes.extend_from_slice(&[0u8; 9]); // osabi + padding (7..16)
        bytes.extend_from_slice(&[0u8; 2]); // e_type (16..18)
        bytes.extend_from_slice(&[0u8; 2]); // e_machine (18..20)
        bytes.extend_from_slice(&[0u8; 4]); // e_version (20..24)
        bytes.extend_from_slice(&[0u8; 4]); // e_entry — 32-bit is 4 bytes (24..28)
        bytes.extend_from_slice(&phoff.to_le_bytes()); // e_phoff (28..32)
        bytes.extend_from_slice(&[0u8; 4]); // e_shoff (32..36)
        bytes.extend_from_slice(&[0u8; 4]); // e_flags (36..40)
        bytes.extend_from_slice(&[0u8; 2]); // e_ehsize (40..42)
        bytes.extend_from_slice(&phentsize.to_le_bytes()); // e_phentsize (42..44)
        bytes.extend_from_slice(&phnum.to_le_bytes()); // e_phnum (44..46)
        bytes.extend_from_slice(&[0u8; 6]); // pad to 52
        debug_assert_eq!(bytes.len(), 52);
        bytes.extend_from_slice(&3u32.to_le_bytes()); // PT_INTERP
        bytes.extend_from_slice(&vec![0u8; phentsize as usize - 4]);
        std::fs::write(&p, &bytes).unwrap();
        assert!(is_dynamically_linked(&p).unwrap());
    }

    /// Big-endian ELF with PT_INTERP returns true — exercises the
    /// `is_le=false` branches of the byte readers.
    #[test]
    fn elf64_big_endian_with_pt_interp_returns_true() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("elf64-be-dyn");
        let phoff: u64 = 64;
        let phentsize: u16 = 56;
        let phnum: u16 = 1;
        let mut bytes = Vec::with_capacity(64 + phentsize as usize);
        bytes.extend_from_slice(b"\x7fELF");
        bytes.push(2);
        bytes.push(2); // big-endian
        bytes.push(1);
        bytes.extend_from_slice(&[0u8; 9]);
        bytes.extend_from_slice(&[0u8; 2]);
        bytes.extend_from_slice(&[0u8; 2]);
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&phoff.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(&[0u8; 2]);
        bytes.extend_from_slice(&phentsize.to_be_bytes());
        bytes.extend_from_slice(&phnum.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 6]);
        debug_assert_eq!(bytes.len(), 64);
        bytes.extend_from_slice(&3u32.to_be_bytes());
        bytes.extend_from_slice(&vec![0u8; phentsize as usize - 4]);
        std::fs::write(&p, &bytes).unwrap();
        assert!(is_dynamically_linked(&p).unwrap());
    }
}
