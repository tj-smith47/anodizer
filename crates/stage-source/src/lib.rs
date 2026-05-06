//! `stage-source` — emit a `git archive` source tarball and accompanying SBOMs.
//!
//! The crate is organised as:
//! - [`archive`] — `git archive` invocation and extra-files staging.
//! - [`sbom`] — Cargo.lock parsing plus CycloneDX / SPDX renderers.
//! - [`run`] — the [`SourceStage`] orchestrator that drives both halves.

mod archive;
mod run;
mod sbom;

#[cfg(test)]
mod tests;

pub use run::SourceStage;
pub use sbom::{
    CargoPackage, deterministic_uuid_from, generate_cyclonedx, generate_spdx, parse_cargo_lock,
};

// `SourceArchiveInputs` and `create_source_archive` are crate-internal helpers
// that `tests.rs` reaches via `crate::archive::*` directly; no re-export
// needed.
