//! Nix publisher — generate a derivation expression for the release
//! artifacts and push it to a configured Nix overlay repository.

mod binary;
mod generate;
mod hashing;
mod publish;
pub mod publisher;

#[cfg(test)]
mod tests;

pub use generate::{NixParams, SourceRootEntry, generate_nix_expression, validate_nix_license};
pub use hashing::hex_sha256_to_nix_base32;
#[cfg(test)]
pub use hashing::hex_sha256_to_sri;
pub use publish::publish_to_nix;
