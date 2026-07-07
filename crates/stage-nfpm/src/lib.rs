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

/// Key-material requirements for one nfpm package-signature block: env vars
/// referenced by templated fields, plus loadable PGP key material when
/// `key_file` is a literal path. rpm/deb signatures use PGP keys; apk keys
/// are RSA PEM, so only their env references are required (`pgp: false`).
fn signature_env_requirements(
    sig: &anodizer_core::config::NfpmSignatureConfig,
    pgp: bool,
    out: &mut Vec<anodizer_core::EnvRequirement>,
) {
    use anodizer_core::env_preflight::template_env_refs;
    if let Some(key_file) = sig.key_file.as_deref() {
        let refs = template_env_refs(key_file);
        if !refs.is_empty() {
            out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
        } else if pgp {
            out.push(anodizer_core::EnvRequirement::KeyFile {
                kind: anodizer_core::KeyKind::PgpPrivate,
                path: key_file.to_string(),
            });
        }
    }
    if let Some(passphrase) = sig.key_passphrase.as_deref() {
        let refs = template_env_refs(passphrase);
        if !refs.is_empty() {
            out.push(anodizer_core::EnvRequirement::EnvAllOf { vars: refs });
        }
    }
}

/// ADVISORY tool requirements for the nfpm schema floor: the pre-publish
/// guard cross-checks built packages with the native tooling when present
/// (`dpkg-deb --info` for deb, `rpm -qp` for rpm) and warn+skips when
/// absent, so these must never block a release — but `anodizer tools`
/// still recommends them so an auto-provisioned runner gets the stronger
/// validation. Keyed on the configured `formats`, so an rpm-only config
/// never recommends dpkg-deb.
pub fn advisory_env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let mut want_deb = false;
    let mut want_rpm = false;
    for c in ctx.config.crate_universe() {
        for n in c.nfpms.iter().flatten() {
            for format in &n.formats {
                match format.as_str() {
                    "deb" => want_deb = true,
                    "rpm" => want_rpm = true,
                    _ => {}
                }
            }
        }
    }
    let mut out = Vec::new();
    if want_deb {
        out.push(anodizer_core::EnvRequirement::Tool {
            name: "dpkg-deb".to_string(),
        });
    }
    if want_rpm {
        out.push(anodizer_core::EnvRequirement::Tool {
            name: "rpm".to_string(),
        });
    }
    out
}

/// Environment requirements for the nfpm stage, derived from the same config
/// `run` reads: the `nfpm` binary whenever any crate declares `nfpms:`, plus
/// signing-key material for each configured package signature.
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    let mut out = Vec::new();
    let mut any = false;
    for c in ctx.config.crate_universe() {
        for n in c.nfpms.iter().flatten() {
            any = true;
            if let Some(sig) = n.rpm.as_ref().and_then(|f| f.signature.as_ref()) {
                signature_env_requirements(sig, true, &mut out);
            }
            if let Some(sig) = n.deb.as_ref().and_then(|f| f.signature.as_ref()) {
                signature_env_requirements(sig, true, &mut out);
            }
            if let Some(sig) = n.apk.as_ref().and_then(|f| f.signature.as_ref()) {
                signature_env_requirements(sig, false, &mut out);
            }
        }
    }
    if any {
        out.insert(
            0,
            anodizer_core::EnvRequirement::Tool {
                name: "nfpm".to_string(),
            },
        );
    }
    out
}
