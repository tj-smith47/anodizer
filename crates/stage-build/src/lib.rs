pub mod binstall;
pub mod version_sync;

// ---------------------------------------------------------------------------
// BuildCommand — a description of the command to run
// ---------------------------------------------------------------------------

mod command;
pub use command::*;

// ---------------------------------------------------------------------------
// detect_cargo_profile — parse --release / --profile flags from cargo flags
// ---------------------------------------------------------------------------

mod profile;

// ---------------------------------------------------------------------------
// build_universal_binary — run `lipo` to combine arm64 + x86_64 macOS binaries
// ---------------------------------------------------------------------------

mod universal;

// ---------------------------------------------------------------------------
// Build ignore/override helpers
// ---------------------------------------------------------------------------

mod targets;

// ---------------------------------------------------------------------------
// strip_glibc_suffix — strip glibc version suffix like ".2.17" from targets
// ---------------------------------------------------------------------------

mod validation;
pub use validation::*;

// ---------------------------------------------------------------------------
// check_workspace_package — validate --package flag for workspace crates
// ---------------------------------------------------------------------------

mod workspace;

// Re-export internal modules and std/core items into the crate root so that
// `tests.rs`'s `use super::*` can resolve everything it needs without each
// test file duplicating a long import list.
#[cfg(test)]
pub(crate) use anodizer_core::artifact::ArtifactKind;
#[cfg(test)]
pub(crate) use anodizer_core::config::{BuildIgnore, BuildOverride, CrossStrategy};
#[cfg(test)]
pub(crate) use anodizer_core::stage::Stage;
#[cfg(test)]
pub(crate) use profile::*;
#[cfg(test)]
pub(crate) use std::collections::HashMap;
#[cfg(test)]
pub(crate) use std::path::{Path, PathBuf};
#[cfg(test)]
pub(crate) use targets::*;
#[cfg(test)]
pub(crate) use universal::*;
#[cfg(test)]
pub(crate) use workspace::*;

// ---------------------------------------------------------------------------
// BuildStage
// ---------------------------------------------------------------------------

pub struct BuildStage;

mod run;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests;
