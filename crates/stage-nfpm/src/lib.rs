//! nfpm stage — `Stage` impl + helpers that translate anodizer config into
//! `nfpm.yaml` and drive `nfpm pkg` per crate / format.
//!
//! Module layout:
//! - `yaml`     — serde-serializable nfpm YAML model structs
//! - `builders` — format-specific `build_yaml_*` translators + signing-passphrase
//!   env-var resolution
//! - `generate` — the public `generate_nfpm_yaml{,_with_env}` entry points
//! - `command`  — `nfpm` CLI argv composition + format/architecture validation
//! - `run`      — `NfpmStage` + per-job pipeline (`Stage::run` body)
//! - `filename` — per-packager conventional filename derivation
//! - `tests`    — externalized unit tests

mod builders;
mod command;
mod filename;
mod generate;
mod run;
mod yaml;

pub use command::nfpm_command;
pub use filename::control_arch;
pub use generate::{
    NfpmLibraryPaths, NfpmRenderTarget, generate_nfpm_yaml, generate_nfpm_yaml_with_env,
};
pub use run::{NfpmRenderedConfig, NfpmStage, nfpm_yaml_configs_for_crate};

// Re-exports needed only by the externalized `tests.rs` (consumed via `super::`).
// Scoped under `cfg(test)` so prod builds don't see them as unused.
#[cfg(test)]
pub(crate) use command::{KNOWN_FORMATS, validate_format};
#[cfg(test)]
pub(crate) use run::{format_extension, setup_lintian_overrides};

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests;
