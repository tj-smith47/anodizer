//! Minimal `.rpm` payload extraction for the libc-ceiling check.
//!
//! An `.rpm` is a 96-byte lead, a signature header (padded to an 8-byte
//! boundary), a main header, and then the payload: a cpio *newc* archive
//! compressed with gzip, xz, or zstd. This module hand-rolls just enough of
//! the container — header skipping, payload-codec sniffing by magic, and a
//! newc cpio walk — to return the largest ELF member as the binary to
//! glibc-check.
//!
//! Everything here is pure (operates on in-memory bytes) so it is
//! unit-testable from a synthetic `.rpm` built in-process. Malformed input
//! degrades to `Ok(None)` — the libc check is best-effort, mirroring the
//! `.deb` extractor's posture.

use std::io::Read;

use anyhow::{Context, Result};

/// RPM lead magic — the first four bytes of every `.rpm`.
const RPM_LEAD_MAGIC: &[u8; 4] = b"\xed\xab\xee\xdb";

/// RPM header-section magic (signature and main headers both start with it).
const RPM_HEADER_MAGIC: &[u8; 4] = b"\x8e\xad\xe8\x01";

/// Byte length of the fixed rpm lead.
const LEAD_LEN: usize = 96;

/// Extract the largest ELF member from an `.rpm`'s cpio payload, or `None`
/// when the input is not a parseable rpm / carries no ELF / uses a codec not
/// linked into this build.
pub fn payload_largest_elf(rpm_bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    let Some(payload) = find_payload(rpm_bytes)? else {
        return Ok(None);
    };
    let Some(cpio) = decompress_payload(&payload)? else {
        return Ok(None);
    };
    Ok(largest_elf_in_cpio_newc(&cpio))
}

/// Locate the compressed payload: skip the lead, the signature header
/// (8-byte aligned), and the main header.
fn find_payload(rpm_bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    if !rpm_bytes.starts_with(RPM_LEAD_MAGIC) || rpm_bytes.len() < LEAD_LEN {
        return Ok(None);
    }
    let sig_end = match header_section_end(rpm_bytes, LEAD_LEN)? {
        Some(end) => end,
        None => return Ok(None),
    };
    // Only the signature header is padded to an 8-byte boundary; the main
    // header follows the padding directly and the payload follows it raw.
    let main_start = sig_end.div_ceil(8) * 8;
    let payload_start = match header_section_end(rpm_bytes, main_start)? {
        Some(end) => end,
        None => return Ok(None),
    };
    if payload_start >= rpm_bytes.len() {
        return Ok(None);
    }
    Ok(Some(rpm_bytes[payload_start..].to_vec()))
}

/// End offset (exclusive) of the header section starting at `start`, or
/// `None` when the bytes there are not a header section.
fn header_section_end(rpm_bytes: &[u8], start: usize) -> Result<Option<usize>> {
    // Header layout: 4-byte magic + 4 reserved bytes, then big-endian u32
    // index-entry count and u32 data-store size; each index entry is 16 B.
    let Some(fixed) = rpm_bytes.get(start..start + 16) else {
        return Ok(None);
    };
    if &fixed[0..4] != RPM_HEADER_MAGIC {
        return Ok(None);
    }
    let nindex = u32::from_be_bytes([fixed[8], fixed[9], fixed[10], fixed[11]]) as usize;
    let hsize = u32::from_be_bytes([fixed[12], fixed[13], fixed[14], fixed[15]]) as usize;
    let end = start
        .checked_add(16)
        .and_then(|v| v.checked_add(nindex.checked_mul(16)?))
        .and_then(|v| v.checked_add(hsize))
        .context("verify-release: rpm header size overflows")?;
    if end > rpm_bytes.len() {
        return Ok(None);
    }
    Ok(Some(end))
}

/// Decompress the payload by sniffing its codec magic (gzip / xz / zstd).
/// Uncompressed cpio (`070701`) is passed through; unknown codecs yield
/// `Ok(None)`.
fn decompress_payload(payload: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    if payload.starts_with(b"\x1f\x8b") {
        flate2::read::GzDecoder::new(payload)
            .read_to_end(&mut out)
            .context("verify-release: gunzip rpm payload")?;
    } else if payload.starts_with(b"\xfd7zXZ\x00") {
        xz2::read::XzDecoder::new(payload)
            .read_to_end(&mut out)
            .context("verify-release: unxz rpm payload")?;
    } else if payload.starts_with(b"\x28\xb5\x2f\xfd") {
        zstd::stream::copy_decode(payload, &mut out)
            .context("verify-release: unzstd rpm payload")?;
    } else if payload.starts_with(b"070701") || payload.starts_with(b"070702") {
        out.extend_from_slice(payload);
    } else {
        return Ok(None);
    }
    Ok(Some(out))
}

/// Walk a cpio *newc* archive and return the bytes of the LARGEST member
/// whose contents begin with the ELF magic, or `None` when there is no ELF
/// (or the archive is malformed — best-effort, like the tar walk).
fn largest_elf_in_cpio_newc(cpio: &[u8]) -> Option<Vec<u8>> {
    // newc header: 6-byte magic then 13 fields of 8 hex chars each (110 B
    // total). The name follows, NUL-terminated, padded so header+name is a
    // multiple of 4; file data follows, itself padded to 4.
    const HEADER_LEN: usize = 110;
    let hex_field = |bytes: &[u8], idx: usize| -> Option<usize> {
        let f = bytes.get(6 + idx * 8..6 + (idx + 1) * 8)?;
        usize::from_str_radix(std::str::from_utf8(f).ok()?, 16).ok()
    };
    let mut best: Option<Vec<u8>> = None;
    let mut pos = 0usize;
    while pos + HEADER_LEN <= cpio.len() {
        let header = &cpio[pos..pos + HEADER_LEN];
        if &header[0..6] != b"070701" && &header[0..6] != b"070702" {
            break;
        }
        let filesize = hex_field(header, 6)?;
        let namesize = hex_field(header, 11)?;
        let name_start = pos + HEADER_LEN;
        let name_end = name_start.checked_add(namesize)?;
        if name_end > cpio.len() {
            break;
        }
        let name = &cpio[name_start..name_end];
        if name.strip_suffix(&[0]).is_some_and(|n| n == b"TRAILER!!!") {
            break;
        }
        let data_start = name_end.div_ceil(4) * 4;
        let data_end = data_start.checked_add(filesize)?;
        if data_end > cpio.len() {
            break;
        }
        let data = &cpio[data_start..data_end];
        if data.starts_with(b"\x7fELF") && best.as_ref().is_none_or(|b| data.len() > b.len()) {
            best = Some(data.to_vec());
        }
        pos = data_end.div_ceil(4) * 4;
    }
    best
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// Build a cpio newc archive in memory from `(name, bytes)` members.
    pub(crate) fn make_cpio_newc(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut push_entry = |name: &str, data: &[u8]| {
            let mut header = Vec::new();
            header.extend_from_slice(b"070701");
            // ino, mode, uid, gid, nlink, mtime, filesize, devmajor,
            // devminor, rdevmajor, rdevminor, namesize, check.
            let fields = [
                0usize,
                0o100755,
                0,
                0,
                1,
                0,
                data.len(),
                0,
                0,
                0,
                0,
                name.len() + 1,
                0,
            ];
            for f in fields {
                header.extend_from_slice(format!("{f:08x}").as_bytes());
            }
            out.extend_from_slice(&header);
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
            out.extend_from_slice(data);
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
        };
        for (name, data) in members {
            push_entry(name, data);
        }
        push_entry("TRAILER!!!", b"");
        out
    }

    /// Build an empty rpm header section (magic + reserved + 0 entries).
    fn empty_header() -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(RPM_HEADER_MAGIC);
        h.extend_from_slice(&[0u8; 4]);
        h.extend_from_slice(&0u32.to_be_bytes());
        h.extend_from_slice(&0u32.to_be_bytes());
        h
    }

    /// Build a minimal `.rpm`: lead + empty signature header (8-aligned) +
    /// empty main header + `payload`.
    pub(crate) fn make_rpm(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; LEAD_LEN];
        out[0..4].copy_from_slice(RPM_LEAD_MAGIC);
        out.extend_from_slice(&empty_header());
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        out.extend_from_slice(&empty_header());
        out.extend_from_slice(payload);
        out
    }

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn extracts_largest_elf_from_synthetic_rpm_gzip() {
        let small = [b"\x7fELF".as_slice(), &[1u8; 8]].concat();
        let big = [b"\x7fELF".as_slice(), &[2u8; 64]].concat();
        let cpio = make_cpio_newc(&[
            ("usr/share/doc/readme", b"plain text"),
            ("usr/bin/small", &small),
            ("usr/bin/app", &big),
        ]);
        let rpm = make_rpm(&gz(&cpio));
        let elf = payload_largest_elf(&rpm)
            .expect("parse rpm")
            .expect("an ELF member");
        assert_eq!(elf, big, "must pick the largest ELF (the binary)");
    }

    #[test]
    fn extracts_elf_from_zstd_and_xz_payloads() {
        let elf = [b"\x7fELF".as_slice(), &[3u8; 16]].concat();
        let cpio = make_cpio_newc(&[("usr/bin/app", &elf)]);

        let mut zst = Vec::new();
        zstd::stream::copy_encode(cpio.as_slice(), &mut zst, 1).unwrap();
        assert_eq!(
            payload_largest_elf(&make_rpm(&zst)).unwrap().as_deref(),
            Some(elf.as_slice()),
            "zstd payload"
        );

        let mut xz = Vec::new();
        xz2::read::XzEncoder::new(cpio.as_slice(), 1)
            .read_to_end(&mut xz)
            .unwrap();
        assert_eq!(
            payload_largest_elf(&make_rpm(&xz)).unwrap().as_deref(),
            Some(elf.as_slice()),
            "xz payload"
        );
    }

    #[test]
    fn non_rpm_input_returns_none() {
        assert!(payload_largest_elf(b"not an rpm").unwrap().is_none());
    }

    #[test]
    fn cpio_with_no_elf_returns_none() {
        let cpio = make_cpio_newc(&[("usr/share/doc/readme", b"plain text")]);
        assert!(
            payload_largest_elf(&make_rpm(&gz(&cpio)))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unknown_payload_codec_returns_none() {
        // bzip2 magic — not linked into this build; degrade to None.
        assert!(payload_largest_elf(&make_rpm(b"BZh9")).unwrap().is_none());
    }
}
