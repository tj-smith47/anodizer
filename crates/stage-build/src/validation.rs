use std::path::Path;

// ---------------------------------------------------------------------------
// strip_glibc_suffix — strip glibc version suffix like ".2.17" from targets
// ---------------------------------------------------------------------------

/// Strip a glibc version suffix from a target triple.
///
/// Targets like `aarch64-unknown-linux-gnu.2.17` carry a `.X.Y` suffix that
/// tells cargo-zigbuild which glibc version to link against. Cargo itself
/// doesn't understand the suffix, so we strip it when constructing the target
/// directory path. The full target (with suffix) is passed to cargo-zigbuild.
///
/// Returns `(cargo_target, has_suffix)` — when there is no suffix the input
/// is returned unchanged.
pub(crate) fn strip_glibc_suffix(target: &str) -> (&str, bool) {
    // Match patterns like "gnu.2.17", "musl.1.1"
    // The suffix starts with a dot followed by a digit after "gnu" or "musl"
    if let Some(idx) = target.rfind("gnu.").or_else(|| target.rfind("musl.")) {
        let suffix_start = target[idx..].find('.').map(|i| idx + i);
        if let Some(start) = suffix_start {
            // Verify the part after the dot looks like a version (starts with digit)
            let after_dot = &target[start + 1..];
            if after_dot.starts_with(|c: char| c.is_ascii_digit()) {
                return (&target[..start], true);
            }
        }
    }
    (target, false)
}

/// Check if a target has a glibc version suffix and should be validated
/// against the known targets list without the suffix.
pub(crate) fn target_for_validation(target: &str) -> &str {
    strip_glibc_suffix(target).0
}

// ---------------------------------------------------------------------------
// is_dynamically_linked — minimal ELF check for PT_INTERP
// ---------------------------------------------------------------------------

/// Check if a binary at the given path is dynamically linked by reading ELF
/// program headers and looking for a PT_INTERP segment (type 3).
///
/// Returns `false` for non-ELF files, files that can't be read, or statically
/// linked ELF binaries.
pub fn is_dynamically_linked(path: &Path) -> bool {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };

    // Read enough of the header to determine ELF class and program header info
    let mut buf = [0u8; 64];
    if file.read(&mut buf).unwrap_or(0) < 64 {
        return false;
    }

    // Verify ELF magic: 0x7f 'E' 'L' 'F'
    if &buf[0..4] != b"\x7fELF" {
        return false;
    }

    let is_64bit = buf[4] == 2;
    let is_le = buf[5] == 1; // 1 = little-endian, 2 = big-endian

    let read_u16 = |b: &[u8]| -> u16 {
        if is_le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let read_u32 = |b: &[u8]| -> u32 {
        if is_le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    };
    let read_u64 = |b: &[u8]| -> u64 {
        if is_le {
            u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        } else {
            u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        }
    };

    // Parse program header offset, entry size, and count
    let (ph_offset, ph_entry_size, ph_count) = if is_64bit {
        // e_phoff at offset 32 (8 bytes), e_phentsize at 54 (2 bytes), e_phnum at 56 (2 bytes)
        let offset = read_u64(&buf[32..40]);
        let entry_size = read_u16(&buf[54..56]);
        let count = read_u16(&buf[56..58]);
        (offset, entry_size as u64, count)
    } else {
        // 32-bit ELF: e_phoff at offset 28 (4 bytes), e_phentsize at 42, e_phnum at 44
        let offset = read_u32(&buf[28..32]) as u64;
        let entry_size = read_u16(&buf[42..44]);
        let count = read_u16(&buf[44..46]);
        (offset, entry_size as u64, count)
    };

    if ph_count == 0 || ph_entry_size == 0 {
        return false;
    }

    // Read all program headers
    let total_size = ph_entry_size * ph_count as u64;
    let mut ph_buf = vec![0u8; total_size as usize];
    use std::io::Seek;
    if file.seek(std::io::SeekFrom::Start(ph_offset)).is_err() {
        return false;
    }
    if file.read_exact(&mut ph_buf).is_err() {
        return false;
    }

    // Scan for PT_INTERP (type 3)
    let pt_interp: u32 = 3;
    for i in 0..ph_count as usize {
        let entry_start = i * ph_entry_size as usize;
        let p_type = read_u32(&ph_buf[entry_start..entry_start + 4]);
        if p_type == pt_interp {
            return true;
        }
    }

    false
}
