//! SBOM (Software Bill of Materials) generation stage for anodizer.
//!
//! Supports two modes:
//! 1. **Built-in**: Parses `Cargo.lock` to generate CycloneDX 1.5 or SPDX 2.3 JSON.
//!    This is a Rust-specific value-add.
//! 2. **External command**: Runs an external tool (default: `syft`) to catalog artifacts.
//!    Standard SBOM-generation behavior.

mod builtin;
mod expected;
mod helpers;
mod stage;

pub use builtin::*;
pub use expected::expected_sbom_assets;
pub(crate) use helpers::*;
pub use stage::*;

#[cfg(test)]
mod tests;
