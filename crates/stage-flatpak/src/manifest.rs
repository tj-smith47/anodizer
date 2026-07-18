use serde::Serialize;

// ---------------------------------------------------------------------------
// Architecture mapping
// ---------------------------------------------------------------------------

/// Map a Go-style or Rust-style architecture name to the Flatpak equivalent.
/// Only x86_64 and aarch64 are supported by Flatpak.
pub(crate) fn arch_to_flatpak(arch: &str) -> Option<&'static str> {
    match arch {
        "amd64" | "x86_64" => Some("x86_64"),
        "arm64" | "aarch64" => Some("aarch64"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Default name template
// ---------------------------------------------------------------------------

/// Stem of the default Flatpak bundle filename template, before the amd64
/// variant suffix and the `.flatpak` extension.
///
/// Flatpak carries the whole go-arch in `Arch` (no arm-split), so the only
/// micro-architecture dimension that can collide on one `Arch` is amd64 —
/// hence the amd64-only suffix appended by [`default_name_template`], not the
/// full Arm/Mips/Amd64 clause.
pub(crate) const DEFAULT_NAME_PREFIX: &str = "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}";

/// Compose the default Flatpak bundle filename template: the
/// [`DEFAULT_NAME_PREFIX`], the shared amd64 variant suffix, then the
/// `.flatpak` extension. Sourced from the single core const so the suffix
/// cannot drift from the other installer namers.
pub(crate) fn default_name_template() -> String {
    format!(
        "{DEFAULT_NAME_PREFIX}{}.flatpak",
        anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX
    )
}

// ---------------------------------------------------------------------------
// Manifest JSON structures
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(crate) struct Manifest {
    pub(crate) id: String,
    pub(crate) runtime: String,
    #[serde(rename = "runtime-version")]
    pub(crate) runtime_version: String,
    pub(crate) sdk: String,
    pub(crate) command: String,
    #[serde(rename = "finish-args", skip_serializing_if = "Vec::is_empty")]
    pub(crate) finish_args: Vec<String>,
    pub(crate) modules: Vec<ManifestModule>,
}

#[derive(Serialize)]
pub(crate) struct ManifestModule {
    pub(crate) name: String,
    pub(crate) buildsystem: String,
    #[serde(rename = "build-commands")]
    pub(crate) build_commands: Vec<String>,
    pub(crate) sources: Vec<ManifestSource>,
}

#[derive(Serialize)]
pub(crate) struct ManifestSource {
    #[serde(rename = "type")]
    pub(crate) type_: String,
    pub(crate) path: String,
    #[serde(rename = "dest-filename", skip_serializing_if = "Option::is_none")]
    pub(crate) dest_filename: Option<String>,
}
