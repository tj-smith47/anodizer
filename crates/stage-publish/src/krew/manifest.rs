use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::util::OsArtifact;

// ---------------------------------------------------------------------------
// KrewManifestParams
// ---------------------------------------------------------------------------

/// Parameters for generating a Krew plugin manifest YAML.
pub(crate) struct KrewManifestParams<'a> {
    pub(crate) name: &'a str,
    pub(crate) version: &'a str,
    pub(crate) homepage: &'a str,
    pub(crate) short_description: &'a str,
    pub(crate) description: &'a str,
    pub(crate) caveats: &'a str,
    /// `(os, arch, url, sha256, binary_name)` tuples for each platform.
    pub(crate) platforms: &'a [KrewPlatform],
}

/// A single platform entry in the Krew manifest.
#[derive(Default)]
pub(crate) struct KrewPlatform {
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) url: String,
    pub(crate) sha256: String,
    pub(crate) bin: String,
    /// Per-platform `files:` extraction list (`from`/`to` pairs) selecting
    /// the binary plus any bundled LICENSE / README from the archive. Empty
    /// only when the artifact carried no layout/file metadata (legacy
    /// snapshots); the live path always populates at least the binary entry.
    pub(crate) files: Vec<KrewFileEntry>,
}

/// A single `files:` extraction entry in a krew platform.
///
/// `from` is the path *inside the downloaded archive* (carrying the
/// `wrap_in_directory` prefix for nested layouts); `to` is the destination
/// relative to the plugin's install dir. Real krew plugins (ctx/ns/tree/
/// access-matrix) emit `to: "."` to flatten the binary + LICENSE to the
/// install root, which is why `bin:` references the flat binary name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KrewFileEntry {
    pub(crate) from: String,
    pub(crate) to: String,
}

// ---------------------------------------------------------------------------
// Serde structs for Krew YAML manifest
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KrewManifestYaml {
    #[serde(rename = "apiVersion")]
    api_version: String,
    kind: String,
    metadata: KrewMetadata,
    spec: KrewSpec,
}

#[derive(Serialize)]
struct KrewMetadata {
    name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KrewSpec {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    homepage: Option<String>,
    short_description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caveats: Option<String>,
    platforms: Vec<KrewPlatformYaml>,
}

#[derive(Serialize)]
struct KrewPlatformYaml {
    selector: KrewSelector,
    uri: String,
    sha256: String,
    bin: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<KrewFileYaml>,
}

#[derive(Serialize)]
struct KrewFileYaml {
    from: String,
    to: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KrewSelector {
    match_labels: KrewMatchLabels,
}

#[derive(Serialize)]
struct KrewMatchLabels {
    os: String,
    arch: String,
}

// ---------------------------------------------------------------------------
// generate_manifest
// ---------------------------------------------------------------------------

/// Generate a Krew plugin manifest YAML string.
///
/// Uses `serde_yaml_ng` for proper YAML serialization with correct escaping
/// of special characters. The `description` and `caveats` fields use YAML
/// block scalar style (literal `|`) when present, achieved via post-processing.
pub(crate) fn generate_manifest(params: &KrewManifestParams<'_>) -> Result<String> {
    let mut platforms: Vec<KrewPlatformYaml> = params
        .platforms
        .iter()
        .map(|p| KrewPlatformYaml {
            selector: KrewSelector {
                match_labels: KrewMatchLabels {
                    os: p.os.clone(),
                    arch: krew_arch(&p.arch).to_string(),
                },
            },
            uri: p.url.clone(),
            sha256: p.sha256.clone(),
            bin: p.bin.clone(),
            files: p
                .files
                .iter()
                .map(|f| KrewFileYaml {
                    from: f.from.clone(),
                    to: f.to.clone(),
                })
                .collect(),
        })
        .collect();

    // sort platforms by URI descending.
    platforms.sort_by(|a, b| b.uri.cmp(&a.uri));

    let manifest = KrewManifestYaml {
        api_version: "krew.googlecontainertools.github.com/v1alpha2".to_string(),
        kind: "Plugin".to_string(),
        metadata: KrewMetadata {
            name: params.name.to_string(),
        },
        spec: KrewSpec {
            version: format!("v{}", params.version),
            homepage: if params.homepage.is_empty() {
                None
            } else {
                Some(params.homepage.to_string())
            },
            short_description: params.short_description.to_string(),
            description: if params.description.is_empty() {
                None
            } else {
                Some(params.description.to_string())
            },
            caveats: if params.caveats.is_empty() {
                None
            } else {
                Some(params.caveats.to_string())
            },
            platforms,
        },
    };

    let yaml = serde_yaml_ng::to_string(&manifest).context("krew: serialize manifest")?;

    Ok(format!("{}\n{}", crate::util::GENERATED_FILE_HEADER, yaml))
}

/// Resolve the effective krew plugin name: the `krew.name` override when set,
/// else the crate name, rendered through the template engine.
///
/// This is the single source of truth shared by the manifest `metadata.name`,
/// the `plugins/<name>.yaml` file basename, and the webhook `pluginName`.
/// krew-index CI rejects a plugin whose `metadata.name` disagrees with the
/// manifest filename, so these three must never drift apart.
pub(super) fn resolve_plugin_name(
    name_override: Option<&str>,
    crate_name: &str,
    render: impl Fn(&str) -> Result<String>,
) -> Result<String> {
    let raw = name_override.unwrap_or(crate_name);
    render(raw).with_context(|| format!("krew: render plugin name template for '{}'", crate_name))
}

/// Map the internal arch names to Krew's expected labels.
///
/// This is a publisher-specific mapping layer on top of the generic
/// `infer_arch` in `util.rs`. The util layer produces canonical short
/// forms (`"amd64"`, `"arm64"`), and this function translates them
/// to whatever Krew expects. Today the mapping is a no-op for the
/// common cases, but keeping a separate layer allows adapting to
/// future Krew label changes without touching the shared inference.
pub(super) fn krew_arch(arch: &str) -> &str {
    match arch {
        "amd64" | "x86_64" => "amd64",
        "arm64" | "aarch64" => "arm64",
        other => other,
    }
}

/// The krew-index review convention for `shortDescription` length. The
/// krew-index CI hints at taglines no longer than ~50 characters (exemplars:
/// ctx=35, ns=33); longer ones get flagged in human review. anodizer warns
/// rather than truncating — silently dropping the tail of a tagline risks
/// losing meaning, and the maintainer is best placed to shorten it.
pub(super) const KREW_SHORT_DESCRIPTION_MAX: usize = 50;

/// Warn (loudly, naming the field + the actual length) when a rendered
/// `shortDescription` exceeds the krew-index norm, so the user can shorten it
/// before krew-index review flags the submission. Counts Unicode scalar values,
/// not bytes, to match how a human reads the tagline.
pub(super) fn warn_if_short_description_too_long(
    short_description: &str,
    crate_name: &str,
    log: &StageLogger,
) {
    let len = short_description.chars().count();
    if len > KREW_SHORT_DESCRIPTION_MAX {
        log.warn(&format!(
            "krew: shortDescription for '{}' is {} characters (krew-index review \
             flags taglines longer than ~{}). Shorten `krew.short_description` to \
             keep the submission within the norm: \"{}\"",
            crate_name, len, KREW_SHORT_DESCRIPTION_MAX, short_description
        ));
    }
}

/// True when the artifact's triple targets an OS `kubectl krew` installs on
/// (Linux, macOS, or Windows).
///
/// Excludes Apple-but-not-macOS targets, which carry no kubectl-installable
/// binary: `map_target` folds `*-apple-watchos` / `-tvos` into `os = "darwin"`
/// and `*-apple-ios` into `os = "ios"`. Without this gate a watchOS archive
/// would be published as a `darwin` `KrewPlatform` selector that a real arm64
/// macOS host matches and then fails to run, and an iOS archive would emit a
/// bogus `os: ios` platform. Uses [`is_macos`] (genuine `*-apple-darwin` only),
/// mirroring the homebrew/nix eligibility filter. A target-less artifact
/// matches no predicate and is excluded.
///
/// [`is_macos`]: anodizer_core::target::is_macos
pub(super) fn krew_eligible(a: &OsArtifact) -> bool {
    anodizer_core::target::is_macos(&a.target)
        || anodizer_core::target::is_linux(&a.target)
        || anodizer_core::target::is_windows(&a.target)
}

/// Map the internal OS names to Krew's expected labels.
///
/// See `krew_arch` for the rationale behind keeping a separate mapping
/// layer on top of `infer_os` in `util.rs`.
pub(super) fn krew_os(os: &str) -> &str {
    match os {
        "darwin" | "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        other => other,
    }
}
