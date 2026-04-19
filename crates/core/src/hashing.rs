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

/// Open a file, feed its bytes through any `Digest` hasher, return hex.
pub fn hash_file_with<D: Digest>(path: &Path, algo_name: &str) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("{algo_name}: open {}", path.display()))?;
    let mut hasher = D::new();
    let mut buf = [0u8; 8192];
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
    Ok(result.iter().map(|b| format!("{:02x}", b)).collect())
}

/// Compute the hex-encoded SHA-256 digest of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha256>(path, "sha256")
}

/// Stream a file through `update`, calling it with each 8 KiB chunk.
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
    let mut buf = [0u8; 8192];
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
