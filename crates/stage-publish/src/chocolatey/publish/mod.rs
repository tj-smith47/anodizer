//! `publish_to_chocolatey` orchestrator — assembles the nuspec + install
//! script, packs a nupkg natively, and pushes via the NuGet V2 API.

mod build;
mod orchestrate;

use build::*;
pub use orchestrate::*;

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests;
