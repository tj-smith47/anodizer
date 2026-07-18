//! `publish_to_nix` orchestrator — resolves config, gathers artifacts,
//! generates the Nix expression, and pushes it to the configured repo.

mod build;
mod orchestrate;

use build::*;
pub use orchestrate::*;

#[cfg(test)]
mod tests;
