//! Shared hashing helpers.
//!
//! Canonical `hash_file_with<D>` + concrete `sha256_file` used across
//! `stage-checksum`, `stage-publish` (artifactory), and `stage-notarize`.
//! Prior to extraction, `sha256_file` had three independent implementations
//! that were free to drift.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context as _, Result};
use sha2::{Digest, Sha256};

/// Read-buffer length for the streaming hashers. 64 KiB amortizes syscall
/// overhead over the multi-MB release binaries these helpers hash; smaller
/// buffers measurably slow throughput on large artifacts.
const STREAM_BUF_LEN: usize = 64 * 1024;

/// Open a file, feed its bytes through any `Digest` hasher, return hex.
pub fn hash_file_with<D: Digest>(path: &Path, algo_name: &str) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("{algo_name}: open {}", path.display()))?;
    let mut hasher = D::new();
    let mut buf = [0u8; STREAM_BUF_LEN];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("{algo_name}: read {}", path.display()))?;
        if n == 0 {
            break;
        }
        Digest::update(&mut hasher, &buf[..n]);
    }
    let result = hasher.finalize();
    Ok(hex_lower(&result))
}

/// Compute the hex-encoded SHA-256 digest of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha256>(path, "sha256")
}

/// Encode bytes as lowercase hex with a pre-allocated output buffer.
pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{:02x}", b);
    }
    out
}

/// Stream a file through `update`, calling it with each 64 KiB chunk.
/// Use this for hashers that don't implement the `Digest` trait
/// (e.g. blake3, crc32fast) so the chunked-read loop lives in one place
/// instead of being copy-pasted per algorithm.
pub fn hash_file_streaming<F: FnMut(&[u8])>(
    path: &Path,
    algo_name: &str,
    mut update: F,
) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("{algo_name}: open {}", path.display()))?;
    let mut buf = [0u8; STREAM_BUF_LEN];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("{algo_name}: read {}", path.display()))?;
        if n == 0 {
            break;
        }
        update(&buf[..n]);
    }
    Ok(())
}
