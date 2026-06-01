//! Minimal `.deb` payload extraction for the libc-ceiling check.
//!
//! A `.deb` is a Unix `ar` archive whose members are typically
//! `debian-binary`, `control.tar.*`, and `data.tar.*`. The shipped
//! executable(s) live inside `data.tar.{gz,xz,zst}`. This module parses the
//! `ar` container (no external crate — the format is trivial), decompresses
//! the data tarball with whichever codec it uses, and returns the largest ELF
//! member as the binary to glibc-check (the common single-binary case).
//!
//! Everything here is pure (operates on in-memory bytes) so it is
//! unit-testable from a synthetic `.deb` built in-process with no Docker or
//! network.

use std::io::Read;

use anyhow::{Context, Result};

/// ELF magic — the first four bytes of every ELF file.
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";

/// Length of an `ar` global header (`!<arch>\n`).
const AR_MAGIC: &[u8] = b"!<arch>\n";

/// Find and decompress the `data.tar.*` member of a `.deb`'s `ar` archive,
/// returning the raw tar bytes.
///
/// Returns `Ok(None)` when there is no `data.tar.*` member or its compression
/// codec is not supported in this build (the libc check is best-effort, so an
/// unknown codec degrades to "skip" rather than error).
pub fn find_data_tar(deb_bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    if !deb_bytes.starts_with(AR_MAGIC) {
        // Not an ar archive (corrupt or not actually a .deb).
        return Ok(None);
    }
    let mut pos = AR_MAGIC.len();
    while pos + 60 <= deb_bytes.len() {
        // ar header: 16-byte name, then fields, with the size at offset 48
        // (10 bytes, decimal ASCII) and a 2-byte `\x60\n` terminator at 58.
        let header = &deb_bytes[pos..pos + 60];
        let name = std::str::from_utf8(&header[0..16])
            .unwrap_or("")
            .trim_end_matches(' ')
            .trim_end_matches('/')
            .to_string();
        let size: usize = std::str::from_utf8(&header[48..58])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .context("verify-release: parse ar member size in .deb")?;
        let data_start = pos + 60;
        let data_end = data_start
            .checked_add(size)
            .filter(|&e| e <= deb_bytes.len())
            .context("verify-release: ar member size overruns .deb")?;
        let member = &deb_bytes[data_start..data_end];

        if let Some(decompressed) = decompress_data_tar(&name, member)? {
            return Ok(Some(decompressed));
        }

        // ar members are padded to an even byte boundary.
        pos = data_end + (size & 1);
    }
    Ok(None)
}

/// Decompress a `data.tar.*` member based on its name suffix. Returns
/// `Ok(None)` for any member that is not `data.tar*` or uses an unsupported
/// codec.
fn decompress_data_tar(name: &str, member: &[u8]) -> Result<Option<Vec<u8>>> {
    if !name.starts_with("data.tar") {
        return Ok(None);
    }
    let mut out = Vec::new();
    if name.ends_with(".gz") {
        flate2::read::GzDecoder::new(member)
            .read_to_end(&mut out)
            .context("verify-release: gunzip .deb data.tar.gz")?;
    } else if name.ends_with(".xz") {
        xz2::read::XzDecoder::new(member)
            .read_to_end(&mut out)
            .context("verify-release: unxz .deb data.tar.xz")?;
    } else if name.ends_with(".zst") {
        zstd::stream::copy_decode(member, &mut out)
            .context("verify-release: unzstd .deb data.tar.zst")?;
    } else if name == "data.tar" {
        out.extend_from_slice(member);
    } else {
        // Unknown codec (e.g. .lzma/.bz2) — skip rather than fail.
        return Ok(None);
    }
    Ok(Some(out))
}

/// Scan a tar archive's members and return the bytes of the LARGEST member
/// whose contents begin with the ELF magic, or `None` when there is no ELF.
pub fn largest_elf_in_tar(tar_bytes: &[u8]) -> Option<Vec<u8>> {
    let mut archive = tar::Archive::new(tar_bytes);
    let mut best: Option<Vec<u8>> = None;
    let Ok(entries) = archive.entries() else {
        return None;
    };
    for entry in entries.flatten() {
        let mut entry = entry;
        let mut buf = Vec::new();
        if entry.read_to_end(&mut buf).is_err() {
            continue;
        }
        if buf.len() >= 4
            && &buf[0..4] == ELF_MAGIC
            && best.as_ref().is_none_or(|b| buf.len() > b.len())
        {
            best = Some(buf);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a tar archive in memory with the given (path, bytes) members.
    fn make_tar(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// Gzip-compress bytes.
    fn gz(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    /// Build a minimal `.deb` ar archive with a single `data.tar.gz` member.
    fn make_deb(data_tar_gz: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(AR_MAGIC);
        let name = "data.tar.gz";
        let mut header = vec![b' '; 60];
        header[0..name.len()].copy_from_slice(name.as_bytes());
        let size_str = data_tar_gz.len().to_string();
        header[48..48 + size_str.len()].copy_from_slice(size_str.as_bytes());
        header[58] = b'\x60';
        header[59] = b'\n';
        out.extend_from_slice(&header);
        out.extend_from_slice(data_tar_gz);
        if data_tar_gz.len() % 2 == 1 {
            out.push(b'\n');
        }
        out
    }

    #[test]
    fn extracts_largest_elf_from_synthetic_deb() {
        let small_elf = [b"\x7fELF".as_slice(), &[1u8; 20]].concat();
        let big_elf = [b"\x7fELF".as_slice(), &[2u8; 100]].concat();
        let not_elf = b"#!/bin/sh\necho hi\n".to_vec();
        let tar = make_tar(&[
            ("usr/share/doc/readme", &not_elf),
            ("usr/bin/small", &small_elf),
            ("usr/bin/myapp", &big_elf),
        ]);
        let deb = make_deb(&gz(&tar));

        let data_tar = find_data_tar(&deb).unwrap().expect("data.tar.gz found");
        let elf = largest_elf_in_tar(&data_tar).expect("an ELF member");
        assert_eq!(elf, big_elf, "must pick the largest ELF (the binary)");
    }

    #[test]
    fn non_ar_input_returns_none() {
        assert!(find_data_tar(b"not a deb").unwrap().is_none());
    }

    #[test]
    fn tar_with_no_elf_returns_none() {
        let tar = make_tar(&[("usr/share/doc/readme", b"plain text")]);
        assert!(largest_elf_in_tar(&tar).is_none());
    }

    #[test]
    fn unknown_codec_member_is_skipped() {
        // A data.tar.bz2 member is unsupported -> find_data_tar yields None.
        let mut out = Vec::new();
        out.extend_from_slice(AR_MAGIC);
        let name = "data.tar.bz2";
        let payload = b"whatever";
        let mut header = vec![b' '; 60];
        header[0..name.len()].copy_from_slice(name.as_bytes());
        let size_str = payload.len().to_string();
        header[48..48 + size_str.len()].copy_from_slice(size_str.as_bytes());
        header[58] = b'\x60';
        header[59] = b'\n';
        out.extend_from_slice(&header);
        out.extend_from_slice(payload);
        assert!(find_data_tar(&out).unwrap().is_none());
    }
}
