//! `anodizer-stage-checksum` — checksum computation for release artifacts.
//!
//! Layout:
//!
//! - [`hashing`] — hash function wrappers (`sha1_file` … `md5_file`),
//!   algorithm metadata (`SUPPORTED_ALGORITHMS`, `validate_algorithm`),
//!   the dispatch entry point (`hash_file`), and the sidecar line
//!   formatter (`format_checksum_line`).
//! - [`run`] — `ChecksumStage` (pipeline integration) and
//!   `refresh_combined_checksums` (post-sign rewrite hook).

mod hashing;
mod run;

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests;

pub use hashing::{
    SUPPORTED_ALGORITHMS, blake2b_file, blake2s_file, blake3_file, crc32_file,
    format_checksum_line, hash_file, md5_file, sha1_file, sha3_224_file, sha3_256_file,
    sha3_384_file, sha3_512_file, sha224_file, sha256_file, sha384_file, sha512_file,
    validate_algorithm,
};
pub use run::{ChecksumStage, refresh_combined_checksums};
