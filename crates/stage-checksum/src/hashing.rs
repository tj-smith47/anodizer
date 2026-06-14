//! Hash function wrappers + algorithm dispatch + sidecar line formatter.
//!
//! Every public hash helper lives here so callers (the stage itself,
//! external consumers, and tests) reach exactly one entry point per
//! algorithm. Algorithm metadata (`SUPPORTED_ALGORITHMS`,
//! `validate_algorithm`, `hash_hex_len`) is colocated with the dispatch
//! function (`hash_file`) it gates so adding a new algorithm requires a
//! single edit here.

use std::path::Path;

use anyhow::{Result, bail};
use blake2::{Blake2b512, Blake2s256};
use md5::Md5;
use sha1::Sha1;
use sha2::{Sha224, Sha384, Sha512};
use sha3::{Sha3_224, Sha3_256, Sha3_384, Sha3_512};

use anodizer_core::hashing::hash_file_with;

pub fn sha1_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha1>(path, "sha1")
}

pub fn sha224_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha224>(path, "sha224")
}

pub use anodizer_core::hashing::sha256_file;

pub fn sha384_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha384>(path, "sha384")
}

pub fn sha512_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha512>(path, "sha512")
}

pub fn blake2b_file(path: &Path) -> Result<String> {
    hash_file_with::<Blake2b512>(path, "blake2b")
}

pub fn blake2s_file(path: &Path) -> Result<String> {
    hash_file_with::<Blake2s256>(path, "blake2s")
}

pub fn sha3_224_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha3_224>(path, "sha3-224")
}

pub fn sha3_256_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha3_256>(path, "sha3-256")
}

pub fn sha3_384_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha3_384>(path, "sha3-384")
}

pub fn sha3_512_file(path: &Path) -> Result<String> {
    hash_file_with::<Sha3_512>(path, "sha3-512")
}

pub fn blake3_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    anodizer_core::hashing::hash_file_streaming(path, "blake3", |chunk| {
        hasher.update(chunk);
    })?;
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn crc32_file(path: &Path) -> Result<String> {
    let mut hasher = crc32fast::Hasher::new();
    anodizer_core::hashing::hash_file_streaming(path, "crc32", |chunk| {
        hasher.update(chunk);
    })?;
    Ok(format!("{:08x}", hasher.finalize()))
}

pub fn md5_file(path: &Path) -> Result<String> {
    hash_file_with::<Md5>(path, "md5")
}

/// Return the hex-encoded output length for a given hash algorithm.
/// Used to generate correctly-sized placeholder hashes in dry-run mode.
pub(super) fn hash_hex_len(algorithm: &str) -> usize {
    match algorithm {
        "md5" => 32,     // 128-bit / 16 bytes
        "sha1" => 40,    // 160-bit / 20 bytes
        "sha224" => 56,  // 224-bit / 28 bytes
        "sha256" => 64,  // 256-bit / 32 bytes
        "sha384" => 96,  // 384-bit / 48 bytes
        "sha512" => 128, // 512-bit / 64 bytes
        "sha3-224" => 56,
        "sha3-256" => 64,
        "sha3-384" => 96,
        "sha3-512" => 128,
        "blake2b" => 128, // Blake2b-512
        "blake2s" => 64,  // Blake2s-256
        "blake3" => 64,   // 256-bit default
        "crc32" => 8,     // 32-bit / 4 bytes
        _ => 64,          // fallback
    }
}

/// Closed set of supported checksum algorithm names. Re-exported from the
/// authoritative [`ChecksumConfig::SUPPORTED_ALGORITHMS`] in `anodizer-core`
/// (colocated with the config field it documents) so the config rustdoc, the
/// `validate_algorithm` Default()-time check, and `hash_file`'s dispatch all
/// share one list. A `tests::hash_file_covers_every_supported_algorithm`
/// drift-guard asserts `hash_file` handles every name in the set.
pub const SUPPORTED_ALGORITHMS: &[&str] =
    anodizer_core::config::ChecksumConfig::SUPPORTED_ALGORITHMS;

/// Validate a configured checksum algorithm name. Call this at stage entry
/// (before any artifact-source iteration) so a typo like `algorithm: sha257`
/// fails fast instead of mid-pipeline after build/archive completes.
pub fn validate_algorithm(algorithm: &str) -> Result<()> {
    if SUPPORTED_ALGORITHMS.contains(&algorithm) {
        Ok(())
    } else {
        bail!(
            "unsupported checksum algorithm: '{}'. Supported: {}",
            algorithm,
            SUPPORTED_ALGORITHMS.join(", ")
        )
    }
}

pub fn hash_file(path: &Path, algorithm: &str) -> Result<String> {
    match algorithm {
        "sha1" => sha1_file(path),
        "sha224" => sha224_file(path),
        "sha256" => sha256_file(path),
        "sha384" => sha384_file(path),
        "sha512" => sha512_file(path),
        "sha3-224" => sha3_224_file(path),
        "sha3-256" => sha3_256_file(path),
        "sha3-384" => sha3_384_file(path),
        "sha3-512" => sha3_512_file(path),
        "blake2b" => blake2b_file(path),
        "blake2s" => blake2s_file(path),
        "blake3" => blake3_file(path),
        "crc32" => crc32_file(path),
        "md5" => md5_file(path),
        _ => bail!("unsupported checksum algorithm: {}", algorithm),
    }
}

pub fn format_checksum_line(hash: &str, filename: &str) -> String {
    format!("{}  {}", hash, filename)
}
