//! Nix publisher — generate a derivation expression for the release
//! artifacts and push it to a configured Nix overlay repository.

mod flake;
mod generate;
mod hashing;
mod license;
mod publish;
pub mod publisher;

#[cfg(test)]
mod tests;

pub(crate) use flake::{
    FLAKE_SYSTEMS, FlakePackage, flake_is_well_formed, generate_flake, nix_delimiters_balanced,
};
pub use generate::{NixParams, SourceRootEntry, generate_nix_expression, validate_nix_license};
pub use hashing::hex_sha256_to_nix_base32;
#[cfg(test)]
pub use hashing::hex_sha256_to_sri;
pub use license::{NixLicense, resolve_nix_license_meta};
pub use publish::publish_to_nix;
pub(crate) use publish::{crate_has_nix_archive, render_nix_for_validation};
pub(crate) use publisher::is_nix_per_crate_configured;
