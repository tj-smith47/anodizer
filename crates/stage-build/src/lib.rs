/// Binstall metadata emission relocated to [`anodizer_core::binstall`] so the
/// cargo publisher (which depends on core, not this stage) can guarantee the
/// metadata is present on the published manifest even on the build-stage-skipping
/// `--publish-only` path. Re-exported here to keep the former path working.
pub use anodizer_core::binstall;
pub mod version_sync;

// ---------------------------------------------------------------------------
// BuildCommand — a description of the command to run
// ---------------------------------------------------------------------------

mod command;
pub use command::*;

// ---------------------------------------------------------------------------
// cross_tool_requirements — cross-toolchain self-report for `anodizer tools`
// ---------------------------------------------------------------------------

mod cross_requirements;
pub use cross_requirements::cross_tool_requirements;

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

// ---------------------------------------------------------------------------
// check_workspace_package — validate --package flag for workspace crates
// ---------------------------------------------------------------------------

mod workspace;
pub use workspace::{resolve_reproducible_epoch, resolve_reproducible_epoch_with_env};

// ---------------------------------------------------------------------------
// BuildStage
// ---------------------------------------------------------------------------

pub struct BuildStage;

mod run;
mod run_helpers;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests;
