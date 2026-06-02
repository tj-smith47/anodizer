use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ArtifactKind {
    // --- Build outputs ---
    Binary,
    /// Binary marked for upload (checksummed, signed, released).
    /// Distinct from Binary which is a raw build output.
    UploadableBinary,
    UniversalBinary,
    Library,
    Header,
    CArchive,
    CShared,
    Wasm,

    // --- Packaged archives ---
    Archive,
    SourceArchive,
    Makeself,
    AppImage,

    // --- Linux packages ---
    LinuxPackage,
    Snap,
    PublishableSnapcraft,
    Flatpak,
    SourceRpm,

    // --- macOS/Windows installers ---
    DiskImage,
    Installer,
    MacOsPackage,

    // --- Container images ---
    DockerImage,
    DockerImageV2,
    PublishableDockerImage,
    DockerManifest,
    DockerDigest,

    // --- Publisher manifests ---
    BrewFormula,
    BrewCask,
    Nixpkg,
    ScoopManifest,
    PublishableChocolatey,
    WingetInstaller,
    WingetDefaultLocale,
    WingetVersion,
    PkgBuild,
    SrcInfo,
    SourcePkgBuild,
    SourceSrcInfo,
    KrewPluginManifest,

    // --- Integrity/metadata ---
    Checksum,
    Signature,
    Certificate,
    Sbom,
    Metadata,
    UploadableFile,
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ArtifactKind {
    /// Return the snake_case string representation (matching serde serialization).
    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactKind::Binary => "binary",
            ArtifactKind::UploadableBinary => "uploadable_binary",
            ArtifactKind::UniversalBinary => "universal_binary",
            ArtifactKind::Library => "library",
            ArtifactKind::Header => "header",
            ArtifactKind::CArchive => "c_archive",
            ArtifactKind::CShared => "c_shared",
            ArtifactKind::Wasm => "wasm",
            ArtifactKind::Archive => "archive",
            ArtifactKind::SourceArchive => "source_archive",
            ArtifactKind::Makeself => "makeself",
            ArtifactKind::AppImage => "appimage",
            ArtifactKind::LinuxPackage => "linux_package",
            ArtifactKind::Snap => "snap",
            ArtifactKind::PublishableSnapcraft => "publishable_snapcraft",
            ArtifactKind::Flatpak => "flatpak",
            ArtifactKind::SourceRpm => "source_rpm",
            ArtifactKind::DiskImage => "disk_image",
            ArtifactKind::Installer => "installer",
            ArtifactKind::MacOsPackage => "macos_package",
            ArtifactKind::DockerImage => "docker_image",
            ArtifactKind::DockerImageV2 => "docker_image_v2",
            ArtifactKind::PublishableDockerImage => "publishable_docker_image",
            ArtifactKind::DockerManifest => "docker_manifest",
            ArtifactKind::DockerDigest => "docker_digest",
            ArtifactKind::BrewFormula => "brew_formula",
            ArtifactKind::BrewCask => "brew_cask",
            ArtifactKind::Nixpkg => "nixpkg",
            ArtifactKind::ScoopManifest => "scoop_manifest",
            ArtifactKind::PublishableChocolatey => "publishable_chocolatey",
            ArtifactKind::WingetInstaller => "winget_installer",
            ArtifactKind::WingetDefaultLocale => "winget_default_locale",
            ArtifactKind::WingetVersion => "winget_version",
            ArtifactKind::PkgBuild => "pkg_build",
            ArtifactKind::SrcInfo => "src_info",
            ArtifactKind::SourcePkgBuild => "source_pkg_build",
            ArtifactKind::SourceSrcInfo => "source_src_info",
            ArtifactKind::KrewPluginManifest => "krew_plugin_manifest",
            ArtifactKind::Checksum => "checksum",
            ArtifactKind::Signature => "signature",
            ArtifactKind::Certificate => "certificate",
            ArtifactKind::Sbom => "sbom",
            ArtifactKind::Metadata => "metadata",
            ArtifactKind::UploadableFile => "uploadable_file",
        }
    }

    /// Parse a snake_case string into an ArtifactKind.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "binary" => Some(ArtifactKind::Binary),
            "uploadable_binary" => Some(ArtifactKind::UploadableBinary),
            "universal_binary" => Some(ArtifactKind::UniversalBinary),
            "library" => Some(ArtifactKind::Library),
            "header" => Some(ArtifactKind::Header),
            "c_archive" => Some(ArtifactKind::CArchive),
            "c_shared" => Some(ArtifactKind::CShared),
            "wasm" => Some(ArtifactKind::Wasm),
            "archive" => Some(ArtifactKind::Archive),
            "source_archive" => Some(ArtifactKind::SourceArchive),
            "makeself" => Some(ArtifactKind::Makeself),
            "appimage" => Some(ArtifactKind::AppImage),
            "linux_package" => Some(ArtifactKind::LinuxPackage),
            "snap" => Some(ArtifactKind::Snap),
            "publishable_snapcraft" => Some(ArtifactKind::PublishableSnapcraft),
            "flatpak" => Some(ArtifactKind::Flatpak),
            "source_rpm" => Some(ArtifactKind::SourceRpm),
            "disk_image" => Some(ArtifactKind::DiskImage),
            "installer" => Some(ArtifactKind::Installer),
            "macos_package" => Some(ArtifactKind::MacOsPackage),
            "docker_image" => Some(ArtifactKind::DockerImage),
            "docker_image_v2" => Some(ArtifactKind::DockerImageV2),
            "publishable_docker_image" => Some(ArtifactKind::PublishableDockerImage),
            "docker_manifest" => Some(ArtifactKind::DockerManifest),
            "docker_digest" => Some(ArtifactKind::DockerDigest),
            "brew_formula" => Some(ArtifactKind::BrewFormula),
            "brew_cask" => Some(ArtifactKind::BrewCask),
            "nixpkg" => Some(ArtifactKind::Nixpkg),
            "scoop_manifest" => Some(ArtifactKind::ScoopManifest),
            "publishable_chocolatey" => Some(ArtifactKind::PublishableChocolatey),
            "winget_installer" => Some(ArtifactKind::WingetInstaller),
            "winget_default_locale" => Some(ArtifactKind::WingetDefaultLocale),
            "winget_version" => Some(ArtifactKind::WingetVersion),
            "pkg_build" => Some(ArtifactKind::PkgBuild),
            "src_info" => Some(ArtifactKind::SrcInfo),
            "source_pkg_build" => Some(ArtifactKind::SourcePkgBuild),
            "source_src_info" => Some(ArtifactKind::SourceSrcInfo),
            "krew_plugin_manifest" => Some(ArtifactKind::KrewPluginManifest),
            "checksum" => Some(ArtifactKind::Checksum),
            "signature" => Some(ArtifactKind::Signature),
            "certificate" => Some(ArtifactKind::Certificate),
            "sbom" => Some(ArtifactKind::Sbom),
            "metadata" => Some(ArtifactKind::Metadata),
            "uploadable_file" => Some(ArtifactKind::UploadableFile),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Artifact {
    pub kind: ArtifactKind,
    pub path: PathBuf,
    /// Canonical artifact name, set at add-time from the path's filename (trimmed).
    pub name: String,
    pub target: Option<String>,
    pub crate_name: String,
    #[serde(serialize_with = "serialize_metadata_sorted")]
    pub metadata: HashMap<String, String>,
    /// File size in bytes, populated by report_sizes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Keys whose values are CONTENT hashes — derived from artifact bytes
/// and therefore non-deterministic when the artifact itself is
/// non-deterministic (e.g. `.deb` / `.rpm` whose packagers embed
/// their own timestamps). Stage-checksum writes these into each
/// artifact's metadata for in-process consumers (chocolatey, scoop,
/// winget — which read `ctx.artifacts` directly, not `artifacts.json`),
/// but emitting them in `artifacts.json` makes the manifest's bytes
/// shadow whatever non-determinism the underlying artifact has.
/// The `.sha256` sidecar files on disk remain the canonical hash
/// surface for external tooling.
const METADATA_HASH_KEYS: &[&str] = &[
    "Checksum", "sha256", "sha512", "sha384", "sha224", "sha1", "md5", "blake2b", "blake3", "crc32",
];

/// Serialize the metadata map as a sorted-key JSON object, dropping
/// content-hash keys. Sorted order kills HashMap iteration drift;
/// dropping hashes kills the non-deterministic-content shadow.
fn serialize_metadata_sorted<S>(map: &HashMap<String, String>, ser: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap as _;
    let sorted: std::collections::BTreeMap<&String, &String> = map
        .iter()
        .filter(|(k, _)| !METADATA_HASH_KEYS.contains(&k.as_str()))
        .collect();
    let mut m = ser.serialize_map(Some(sorted.len()))?;
    for (k, v) in sorted {
        m.serialize_entry(k, v)?;
    }
    m.end()
}

impl Artifact {
    /// Return the artifact filename.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the OS component of the target (e.g., "linux", "darwin", "windows").
    pub fn goos(&self) -> Option<String> {
        self.target.as_ref().map(|t| crate::target::map_target(t).0)
    }

    /// Return the arch component of the target (e.g., "amd64", "arm64").
    pub fn goarch(&self) -> Option<String> {
        self.target.as_ref().map(|t| crate::target::map_target(t).1)
    }

    /// Check if this artifact replaces single-arch variants (universal binary dedup).
    /// `OnlyReplacingUnibins` — when a universal binary has
    /// `replaces=true`, it supersedes the per-arch binaries for publisher consumption.
    /// Artifacts without the `replaces` metadata key default to `true` (included).
    pub fn only_replacing_unibins(&self) -> bool {
        self.metadata.get("replaces").is_none_or(|v| v != "false")
    }

    /// Return the list of extra binary names bundled in this archive artifact.
    pub fn extra_binaries(&self) -> Vec<String> {
        self.metadata
            .get("extra_binaries")
            .map(|v| {
                v.split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return the single binary name for an uploadable binary artifact.
    pub fn extra_binary(&self) -> Option<String> {
        self.metadata.get("binary").cloned()
    }

    /// Resolve the artifact's canonical file extension (including the leading
    /// dot), mirroring GoReleaser's `Artifact.Ext()` at
    /// `internal/artifact/artifact.go:442`: prefer the `ext` metadata extra
    /// when present and non-empty, fall back to parsing the filename.
    ///
    /// Stages that know their canonical extension better than filename
    /// parsing can (e.g. `srpm` knowing `.src.rpm` rather than `.rpm`)
    /// populate `metadata["ext"]` so downstream `{{ .ArtifactExt }}`
    /// renders the canonical value.
    pub fn ext(&self) -> String {
        if let Some(ext) = self.metadata.get("ext")
            && !ext.is_empty()
        {
            return ext.clone();
        }
        crate::template::extract_artifact_ext(&self.name).to_string()
    }
}

#[derive(Debug, Default)]
pub struct ArtifactRegistry {
    artifacts: Vec<Artifact>,
}

impl ArtifactRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, mut artifact: Artifact) {
        // Set canonical name from path filename if the caller hasn't provided one.
        let name = if artifact.name.is_empty() {
            let derived = artifact
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("artifact")
                .trim()
                .to_string();
            artifact.name = derived.clone();
            derived
        } else {
            artifact.name.clone()
        };

        // Relativize absolute paths to the current working directory so the
        // determinism harness produces byte-identical `artifacts.json` across
        // runs that operate in different worktrees. Mirrors GoReleaser's
        // `shouldRelPath` / `relPath` in `internal/artifact/artifact.go:529-547`.
        //
        // Without this, raw cargo binaries register paths like
        // `/tmp/anodize-determinism-12345-0/.det-tmp/target/<triple>/release/<bin>`
        // — the leading `/tmp/anodize-determinism-<pid>-<idx>` prefix differs
        // every run and drifts `dist/artifacts.json` even when the bytes of
        // every other artifact match.
        //
        // Guard against cwd being the filesystem root (`/` on Unix, `C:\` on
        // Windows): in that degenerate case every absolute path "starts with
        // cwd" but stripping the leading separator yields a path that no
        // longer resolves under the original cwd. We detect root via
        // `parent().is_none()` (works cross-platform) and skip the
        // relativization — production never runs from `/`, but a small
        // number of unit tests do (e.g. `stage-source`'s
        // `test_stage_run_does_not_depend_on_cwd`).
        if should_relativize_path(artifact.kind)
            && artifact.path.is_absolute()
            && let Ok(cwd) = std::env::current_dir()
            && cwd.parent().is_some()
            && let Ok(rel) = artifact.path.strip_prefix(&cwd)
        {
            artifact.path = rel.to_path_buf();
        }

        // Normalize path: convert to forward slashes for cross-platform consistency.
        let path_str = crate::util::normalize_path_separators(&artifact.path.to_string_lossy());
        artifact.path = PathBuf::from(path_str);

        // Warn on duplicate names for uploadable artifact types — but only when
        // the re-registration is a genuine conflict (a different on-disk path
        // for the same name). An identical re-registration (same resolved path)
        // is a benign idempotent add (e.g. cross-target `install.sh.sha256`
        // produced once per shard) and must stay silent; warning on it floods
        // the default-verbosity log with duplicate, non-actionable lines.
        if is_uploadable(artifact.kind)
            && let Some(existing) = self
                .artifacts
                .iter()
                .find(|a| is_uploadable(a.kind) && a.name == name)
            && existing.path != artifact.path
        {
            // Route through `tracing::warn!` so the subscriber-level redaction
            // layer applies and the warning is intercept-friendly for tests.
            // The formatter renders only `message`, so the actionable detail
            // (which artifact, both conflicting paths) is folded inline.
            tracing::warn!(
                "artifact '{}' already registered at '{}' but re-added from '{}'; \
                 upload may fail with a duplicate error",
                name,
                existing.path.display(),
                artifact.path.display(),
            );
        }

        self.artifacts.push(artifact);
    }

    /// Drop later duplicate-path entries that carry `target: None`.
    ///
    /// Used by the publish-only multi-shard rehydration: cross-target
    /// artifacts (source archive, install.sh, release-level metadata)
    /// have `target: None` and are produced identically by every shard's
    /// harness run. After per-shard manifests are merged into a single
    /// registry, those entries duplicate by path. `download-artifact
    /// merge-multiple` collapses the on-disk copies to one file, so the
    /// registry must follow suit or SignStage / ReleaseStage emits each
    /// entry separately and races on the same on-disk path.
    ///
    /// Per-target duplicates (`target: Some(_)`) are left untouched —
    /// those indicate a real shard-overlap bug and downstream
    /// validators must surface them.
    pub fn dedupe_targetless_duplicates(&mut self) {
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        self.artifacts.retain(|a| {
            if a.target.is_some() {
                return true;
            }
            seen.insert(a.path.clone())
        });
    }

    pub fn by_kind(&self, kind: ArtifactKind) -> Vec<&Artifact> {
        self.artifacts.iter().filter(|a| a.kind == kind).collect()
    }

    pub fn by_kind_and_crate(&self, kind: ArtifactKind, crate_name: &str) -> Vec<&Artifact> {
        self.artifacts
            .iter()
            .filter(|a| a.kind == kind && a.crate_name == crate_name)
            .collect()
    }

    pub fn by_kinds_and_crate(&self, kinds: &[ArtifactKind], crate_name: &str) -> Vec<&Artifact> {
        self.artifacts
            .iter()
            .filter(|a| kinds.contains(&a.kind) && a.crate_name == crate_name)
            .collect()
    }

    /// Return one artifact per `path` from the (Binary | UploadableBinary |
    /// UniversalBinary) set, preferring `UploadableBinary` when both kinds
    /// register the same path. UniversalBinary paths differ from their
    /// component binaries so they pass through untouched.
    ///
    /// Mirrors GoReleaser's `artifact.ByBinaryLikeArtifacts`
    /// (`internal/artifact/artifact.go:733-761`). Used by stage-sbom to
    /// avoid generating duplicate SBOMs at the same path; would be silently
    /// hand-rolled in any future stage that walks binary kinds.
    pub fn binary_like_dedup(&self) -> Vec<&Artifact> {
        let uploadable_paths: std::collections::HashSet<&std::path::Path> = self
            .artifacts
            .iter()
            .filter(|a| a.kind == ArtifactKind::UploadableBinary)
            .map(|a| a.path.as_path())
            .collect();
        self.artifacts
            .iter()
            .filter(|a| {
                matches!(
                    a.kind,
                    ArtifactKind::Binary
                        | ArtifactKind::UploadableBinary
                        | ArtifactKind::UniversalBinary
                )
            })
            .filter(|a| {
                a.kind == ArtifactKind::UploadableBinary
                    || !uploadable_paths.contains(a.path.as_path())
            })
            .collect()
    }

    pub fn all(&self) -> &[Artifact] {
        &self.artifacts
    }

    pub fn all_mut(&mut self) -> &mut [Artifact] {
        &mut self.artifacts
    }

    /// Filter artifacts by a predicate, returning matching references.
    pub fn filter<F: Fn(&Artifact) -> bool>(&self, predicate: F) -> Vec<&Artifact> {
        self.artifacts.iter().filter(|a| predicate(a)).collect()
    }

    /// Remove all artifacts whose path matches one of the given paths.
    pub fn remove_by_paths(&mut self, paths: &[std::path::PathBuf]) {
        self.artifacts.retain(|a| !paths.contains(&a.path));
    }

    /// Serialize all artifacts to a JSON value suitable for writing to artifacts.json.
    /// Normalizes all artifact paths to use forward slashes for cross-platform
    /// consistency (GoReleaser always writes forward slashes).
    ///
    /// **Determinism**: artifacts are emitted in a stable sort order keyed on
    /// `(kind, target, crate_name, name, path)` regardless of registration
    /// order. The harness caught a regression where two runs registered the
    /// same archive set in opposite orders (`linux-amd64` first vs
    /// `linux-arm64` first) because the upstream `stage-archive` grouping
    /// used `HashMap` iteration. Even with that root cause fixed in
    /// `stage-archive/src/run.rs`, every other stage that registers
    /// artifacts via `ArtifactRegistry::add` is a future regression risk;
    /// sorting here forecloses the failure mode entirely. Cost is O(N log N)
    /// on a small N (~tens of artifacts per release).
    pub fn to_artifacts_json(&self) -> anyhow::Result<serde_json::Value> {
        let mut sorted: Vec<&Artifact> = self.artifacts.iter().collect();
        sorted.sort_by(|a, b| {
            (
                a.kind.as_str(),
                a.target.as_deref().unwrap_or(""),
                a.crate_name.as_str(),
                a.name.as_str(),
                a.path.as_path(),
            )
                .cmp(&(
                    b.kind.as_str(),
                    b.target.as_deref().unwrap_or(""),
                    b.crate_name.as_str(),
                    b.name.as_str(),
                    b.path.as_path(),
                ))
        });
        let mut val = serde_json::to_value(&sorted)?;
        // Normalize backslashes in path fields to forward slashes.
        if let Some(arr) = val.as_array_mut() {
            for entry in arr {
                if let Some(path) = entry
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(crate::util::normalize_path_separators)
                {
                    entry["path"] = serde_json::Value::String(path);
                }
            }
        }
        Ok(val)
    }
}

/// Artifact kinds that should be included in size reporting.
pub fn size_reportable_kinds() -> &'static [ArtifactKind] {
    &[
        // Uploadable types (all appear in releases)
        ArtifactKind::Archive,
        ArtifactKind::SourceArchive,
        ArtifactKind::UploadableFile,
        ArtifactKind::Makeself,
        ArtifactKind::AppImage,
        ArtifactKind::LinuxPackage,
        ArtifactKind::Flatpak,
        ArtifactKind::SourceRpm,
        ArtifactKind::Sbom,
        ArtifactKind::Checksum,
        ArtifactKind::Signature,
        ArtifactKind::Certificate,
        ArtifactKind::DiskImage,
        ArtifactKind::Installer,
        ArtifactKind::MacOsPackage,
        ArtifactKind::Snap,
        ArtifactKind::PublishableSnapcraft,
        // Build outputs
        ArtifactKind::Binary,
        ArtifactKind::UploadableBinary,
        ArtifactKind::UniversalBinary,
        ArtifactKind::Library,
        ArtifactKind::Header,
        ArtifactKind::CArchive,
        ArtifactKind::CShared,
        ArtifactKind::Wasm,
    ]
}

/// Artifact kinds that are uploadable to releases/blob storage — the canonical
/// list of types that should be uploaded, checksummed, signed, and distributed.
pub fn uploadable_kinds() -> &'static [ArtifactKind] {
    &[
        ArtifactKind::Archive,
        ArtifactKind::UploadableBinary,
        ArtifactKind::SourceArchive,
        ArtifactKind::UploadableFile,
        ArtifactKind::Makeself,
        ArtifactKind::AppImage,
        ArtifactKind::LinuxPackage,
        ArtifactKind::PublishableSnapcraft,
        ArtifactKind::Flatpak,
        ArtifactKind::SourceRpm,
        ArtifactKind::Sbom,
        ArtifactKind::Checksum,
        ArtifactKind::Signature,
        ArtifactKind::Certificate,
        ArtifactKind::DiskImage,
        ArtifactKind::Installer,
        ArtifactKind::MacOsPackage,
    ]
}

/// Artifact kinds eligible for release upload. Canonical list used by the
/// GitHub release publisher, blob storage, stage-checksum, and the stage-sign
/// "all" filter.
///
/// Mirrors GoReleaser's `artifact.ReleaseUploadableTypes()` plus the four
/// installer kinds that are GR Pro features (MSI/NSIS as `Installer`, DMG as
/// `DiskImage`, PKG as `MacOsPackage`) — anodizer ships these as OSS so they
/// are first-class release artifacts here.
///
/// Kept narrower than [`uploadable_kinds`]: snap-store-bound kinds
/// ([`ArtifactKind::Snap`], [`ArtifactKind::PublishableSnapcraft`]) and raw
/// build outputs ([`ArtifactKind::Binary`], [`ArtifactKind::UniversalBinary`])
/// don't end up in the GitHub release, so they don't appear here either.
pub fn release_uploadable_kinds() -> &'static [ArtifactKind] {
    &[
        ArtifactKind::Archive,
        ArtifactKind::UploadableBinary,
        ArtifactKind::UploadableFile,
        ArtifactKind::SourceArchive,
        ArtifactKind::Makeself,
        ArtifactKind::AppImage,
        ArtifactKind::LinuxPackage,
        ArtifactKind::Flatpak,
        ArtifactKind::SourceRpm,
        ArtifactKind::Installer,
        ArtifactKind::DiskImage,
        ArtifactKind::MacOsPackage,
        ArtifactKind::Sbom,
        ArtifactKind::Checksum,
        ArtifactKind::Signature,
        ArtifactKind::Certificate,
    ]
}

/// Check if an artifact kind is uploadable.
fn is_uploadable(kind: ArtifactKind) -> bool {
    uploadable_kinds().contains(&kind)
}

/// Should the `add()` path normaliser convert an absolute path into a path
/// relative to the current working directory? Mirrors GoReleaser's
/// `shouldRelPath` in `internal/artifact/artifact.go:540-547`: Docker image
/// "paths" are actually image refs (e.g. `repo/name:tag`) and must pass
/// through untouched. Every other kind is a real on-disk file whose absolute
/// path would otherwise leak the (per-run) worktree prefix into
/// `dist/artifacts.json`.
fn should_relativize_path(kind: ArtifactKind) -> bool {
    !matches!(
        kind,
        ArtifactKind::DockerImage
            | ArtifactKind::DockerImageV2
            | ArtifactKind::PublishableDockerImage
            | ArtifactKind::DockerManifest
            | ArtifactKind::DockerDigest
    )
}

/// Return `true` for signature/certificate artifacts produced by the
/// `binary_signs:` stage.  These are intermediate per-binary outputs
/// (e.g. `anodizer_linux_amd64` without a `.sig` extension) that must not
/// appear as GitHub release assets.
pub fn is_binary_sign_output(artifact: &Artifact) -> bool {
    artifact
        .metadata
        .get("binary_sign")
        .is_some_and(|v| v == "true")
}

/// Filter an artifact by the `id` metadata field.
///
/// Matches GoReleaser's `artifact.ByID` semantic:
/// - When `ids` is `None` or empty, every artifact passes.
/// - Artifact kinds `Checksum`, `SourceArchive`, `UploadableFile`, `Metadata`
///   always pass regardless of filter (these are emitted for every release).
/// - For all other kinds, the artifact's `metadata["id"]` must match one of
///   the supplied ids. An artifact missing an `id` metadata value does not
///   match a non-empty filter.
///
/// Upstream reference: `goreleaser/internal/artifact/artifact.go::ByID`.
pub fn matches_id_filter(artifact: &Artifact, ids: Option<&[String]>) -> bool {
    let Some(id_list) = ids else { return true };
    if id_list.is_empty() {
        return true;
    }
    if matches!(
        artifact.kind,
        ArtifactKind::Checksum
            | ArtifactKind::SourceArchive
            | ArtifactKind::UploadableFile
            | ArtifactKind::Metadata
    ) {
        return true;
    }
    let artifact_id = artifact
        .metadata
        .get("id")
        .map(|s| s.as_str())
        .unwrap_or("");
    id_list.iter().any(|id| id == artifact_id)
}

/// Format a byte count into a human-readable string (e.g. "4.2 MB").
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Populate artifact sizes and print a formatted size table.
///
/// Filters artifacts to [`size_reportable_kinds`] (matching GoReleaser's
/// `reportsizes` pipe), stores the file size in each artifact's `size` field,
/// and prints a human-readable table.
pub fn print_size_report(registry: &mut ArtifactRegistry, log: &crate::log::StageLogger) {
    let reportable = size_reportable_kinds();
    let mut entries: Vec<(String, u64)> = Vec::new();
    let mut total: u64 = 0;

    for artifact in registry.all_mut() {
        if !reportable.contains(&artifact.kind) {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&artifact.path) {
            let size = meta.len();
            artifact.size = Some(size);
            let name = artifact
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| artifact.path.display().to_string());
            entries.push((name, size));
            total += size;
        }
    }

    if entries.is_empty() {
        return;
    }

    let max_name_len = entries.iter().map(|(n, _)| n.len()).max().unwrap_or(0);

    log.status("");
    log.status("Artifact Sizes:");
    for (name, size) in &entries {
        log.status(&format!(
            "  {:<width$}  {}",
            name,
            format_size(*size),
            width = max_name_len
        ));
    }
    log.status(&format!(
        "  {:<width$}  {}",
        "Total:",
        format_size(total),
        width = max_name_len
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_add_and_query_artifacts() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/cfgd"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "cfgd".to_string(),
            metadata: Default::default(),
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/cfgd.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "cfgd".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let binaries = registry.by_kind(ArtifactKind::Binary);
        assert_eq!(binaries.len(), 1);

        let archives = registry.by_kind_and_crate(ArtifactKind::Archive, "cfgd");
        assert_eq!(archives.len(), 1);
    }

    #[test]
    fn test_empty_query() {
        let registry = ArtifactRegistry::new();
        assert!(registry.by_kind(ArtifactKind::Binary).is_empty());
    }

    /// Multi-shard rehydration appends each shard's artifacts manifest
    /// into one registry. Cross-target artifacts (source archive,
    /// install.sh, metadata.json — `target: None`) appear N times
    /// (once per shard). `dedupe_targetless_duplicates` must collapse
    /// them to one entry per path while leaving per-target entries
    /// intact.
    #[test]
    fn dedupe_targetless_duplicates_collapses_cross_shard_dups() {
        let mut registry = ArtifactRegistry::new();
        // Three shards each register the same cross-target source archive.
        for _ in 0..3 {
            registry.add(Artifact {
                kind: ArtifactKind::SourceArchive,
                name: "anodizer-0.3.0-source.tar.gz".to_string(),
                path: PathBuf::from("dist/anodizer-0.3.0-source.tar.gz"),
                target: None,
                crate_name: "anodizer".to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }
        // Plus a couple of per-target archives that are NOT duplicates
        // (same crate, different target → different path expected, but
        // we use the same path here to exercise the negative case:
        // dedupe must leave target-Some duplicates alone for the
        // downstream overlap-detection check).
        for triple in &["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
            registry.add(Artifact {
                kind: ArtifactKind::Archive,
                name: format!("anodizer-0.3.0-{}.tar.gz", triple),
                path: PathBuf::from(format!("dist/anodizer-0.3.0-{}.tar.gz", triple)),
                target: Some((*triple).to_string()),
                crate_name: "anodizer".to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        registry.dedupe_targetless_duplicates();

        // Source archive collapsed from 3 → 1 entry.
        let sources: Vec<_> = registry.by_kind(ArtifactKind::SourceArchive);
        assert_eq!(
            sources.len(),
            1,
            "cross-shard target-None duplicates must collapse to 1 entry"
        );
        // Per-target archives untouched.
        assert_eq!(registry.by_kind(ArtifactKind::Archive).len(), 2);
    }

    /// Companion: dedupe must NOT touch per-target duplicates (target:
    /// Some) since those signal real matrix overlap and must be caught
    /// by the downstream `detect_duplicate_artifact_paths` validator.
    #[test]
    fn dedupe_targetless_duplicates_leaves_per_target_duplicates_intact() {
        let mut registry = ArtifactRegistry::new();
        for _ in 0..3 {
            registry.add(Artifact {
                kind: ArtifactKind::Archive,
                name: "anodizer-x86_64.tar.gz".to_string(),
                path: PathBuf::from("dist/anodizer-x86_64.tar.gz"),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "anodizer".to_string(),
                metadata: HashMap::new(),
                size: None,
            });
        }

        registry.dedupe_targetless_duplicates();

        assert_eq!(
            registry.by_kind(ArtifactKind::Archive).len(),
            3,
            "per-target duplicates must remain so detect_duplicate_artifact_paths can flag them"
        );
    }

    #[test]
    fn test_by_kinds_and_crate() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "bin".to_string(),
            path: PathBuf::from("bin"),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::UniversalBinary,
            name: "ubin".to_string(),
            path: PathBuf::from("ubin"),
            target: None,
            crate_name: "app".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Header,
            name: "hdr".to_string(),
            path: PathBuf::from("hdr"),
            target: None,
            crate_name: "other".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let results = registry.by_kinds_and_crate(
            &[ArtifactKind::Binary, ArtifactKind::UniversalBinary],
            "app",
        );
        assert_eq!(results.len(), 2);

        // Header belongs to "other" crate, not "app"
        let results = registry.by_kinds_and_crate(&[ArtifactKind::Header], "app");
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_to_artifacts_json_empty() {
        let registry = ArtifactRegistry::new();
        let json = registry.to_artifacts_json().unwrap();
        assert!(json.is_array());
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_to_artifacts_json_with_artifacts() {
        let mut registry = ArtifactRegistry::new();
        let mut meta = HashMap::new();
        meta.insert("format".to_string(), "tar.gz".to_string());
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp-1.0.0-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta,
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Checksum,
            name: String::new(),
            path: PathBuf::from("dist/myapp_1.0.0_checksums.txt"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let json = registry.to_artifacts_json().unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);

        // First artifact
        let first = &arr[0];
        assert_eq!(first["kind"], "archive");
        assert_eq!(first["path"], "dist/myapp-1.0.0-linux-amd64.tar.gz");
        assert_eq!(first["target"], "x86_64-unknown-linux-gnu");
        assert_eq!(first["crate_name"], "myapp");
        assert_eq!(first["metadata"]["format"], "tar.gz");

        // Second artifact
        let second = &arr[1];
        assert_eq!(second["kind"], "checksum");
        assert!(second["target"].is_null());
    }

    /// Regression for the determinism harness drift on `dist/artifacts.json`.
    /// Two harness runs use different worktrees (e.g.
    /// `/tmp/anodize-determinism-11193-0` vs `…-22847-0`) and CARGO_TARGET_DIR
    /// is an absolute per-worktree path; `Artifact.path` for raw cargo binaries
    /// is therefore absolute. Without the `add()`-time relativization, the
    /// worktree prefix would land in `artifacts.json` and the two runs would
    /// disagree on that byte sequence even when every other artifact matches.
    /// Mirrors GoReleaser's `shouldRelPath` / `relPath`
    /// (`internal/artifact/artifact.go:529-560`).
    #[test]
    #[serial_test::serial]
    fn to_artifacts_json_strips_absolute_worktree_prefix() {
        let cwd_guard = tempfile::tempdir().unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd_guard.path()).unwrap();
        // current_dir() returns a canonicalized path on most platforms; mirror
        // that so strip_prefix matches what add() will compute internally.
        let canonical_cwd = std::env::current_dir().unwrap();
        let abs = canonical_cwd
            .join("dist")
            .join("anodize-1.0.0-linux-amd64.tar.gz");

        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: abs,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "anodize".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let json = registry.to_artifacts_json().unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(
            arr[0]["path"], "dist/anodize-1.0.0-linux-amd64.tar.gz",
            "absolute worktree prefix must be stripped at add() time so two \
             determinism-harness runs at different worktree paths produce \
             byte-identical artifacts.json"
        );

        std::env::set_current_dir(original_cwd).unwrap();
    }

    /// Regression for determinism drift on `dist/artifacts.json`: two
    /// runs produced byte-different `artifacts.json` even though the set
    /// of artifacts was identical — the upstream `stage-archive`
    /// registered per-target archives in `HashMap` iteration order, which
    /// is randomised per process. The diff was archive entries in
    /// opposite positions (`linux-arm64` before `linux-amd64` vs. the
    /// reverse).
    ///
    /// `to_artifacts_json` now sorts on (kind, target, crate_name, name,
    /// path) before emitting, so even if a future stage registers artifacts
    /// in non-deterministic order the JSON output is byte-identical.
    #[test]
    fn to_artifacts_json_output_is_order_insensitive() {
        // Build registry A: arm64 archive first, then amd64.
        let mut reg_a = ArtifactRegistry::new();
        reg_a.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/anodize-1.0.0-linux-arm64.tar.gz"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "anodize".to_string(),
            metadata: Default::default(),
            size: Some(15_000_000),
        });
        reg_a.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/anodize-1.0.0-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "anodize".to_string(),
            metadata: Default::default(),
            size: Some(18_000_000),
        });

        // Build registry B: amd64 archive first, then arm64 (opposite order).
        let mut reg_b = ArtifactRegistry::new();
        reg_b.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/anodize-1.0.0-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "anodize".to_string(),
            metadata: Default::default(),
            size: Some(18_000_000),
        });
        reg_b.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/anodize-1.0.0-linux-arm64.tar.gz"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "anodize".to_string(),
            metadata: Default::default(),
            size: Some(15_000_000),
        });

        let json_a = serde_json::to_string_pretty(&reg_a.to_artifacts_json().unwrap()).unwrap();
        let json_b = serde_json::to_string_pretty(&reg_b.to_artifacts_json().unwrap()).unwrap();

        assert_eq!(
            json_a, json_b,
            "two registries with the same artifacts in different insertion \
             orders must produce byte-identical artifacts.json — otherwise \
             the determinism harness will surface per-run drift in dist/"
        );
    }

    /// Docker image "paths" are image refs (`repo/name:tag`), not on-disk
    /// files. The `add()` path normaliser must NOT touch them — stripping a
    /// `/` prefix off `repo/name:tag` would corrupt downstream stages that
    /// `docker push` the value verbatim. Mirrors `shouldRelPath`'s
    /// docker-kind carve-out.
    #[test]
    #[serial_test::serial]
    fn to_artifacts_json_preserves_docker_image_refs() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: "myorg/myimage:v1.2.3".to_string(),
            path: PathBuf::from("/myorg/myimage:v1.2.3"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let json = registry.to_artifacts_json().unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(
            arr[0]["path"], "/myorg/myimage:v1.2.3",
            "docker image refs are pass-through and must not be relativized"
        );
    }

    #[test]
    fn to_artifacts_json_drops_content_hash_keys() {
        let mut metadata = HashMap::new();
        metadata.insert("format".into(), "deb".into());
        metadata.insert("id".into(), "default".into());
        // Content hashes vary between runs for non-deterministic
        // artifacts (.deb / .rpm / .msi ...); they belong in the
        // `.sha256` sidecar, not in this manifest.
        metadata.insert("Checksum".into(), "sha256:abc".into());
        metadata.insert("sha256".into(), "abc".into());
        metadata.insert("blake3".into(), "xyz".into());

        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::LinuxPackage,
            name: "pkg.deb".to_string(),
            path: PathBuf::from("dist/pkg.deb"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata,
            size: None,
        });

        let json = registry.to_artifacts_json().unwrap();
        let meta = &json.as_array().unwrap()[0]["metadata"];
        assert_eq!(meta["format"], "deb");
        assert_eq!(meta["id"], "default");
        assert!(
            meta.get("Checksum").is_none(),
            "Checksum (content-hash) must be filtered from artifacts.json: {meta:?}"
        );
        assert!(meta.get("sha256").is_none(), "sha256 must be filtered");
        assert!(meta.get("blake3").is_none(), "blake3 must be filtered");
    }

    #[test]
    fn test_metadata_json_is_valid_json_string() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let json = registry.to_artifacts_json().unwrap();
        let serialized = serde_json::to_string_pretty(&json).unwrap();
        // Should be parseable back
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed, json);
    }

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn test_format_size_kilobytes() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(10240), "10.0 KB");
    }

    #[test]
    fn test_format_size_megabytes() {
        assert_eq!(format_size(1048576), "1.0 MB");
        assert_eq!(format_size(4404019), "4.2 MB");
    }

    #[test]
    fn test_format_size_gigabytes() {
        assert_eq!(format_size(1073741824), "1.0 GB");
        assert_eq!(format_size(2147483648), "2.0 GB");
    }

    #[test]
    fn test_artifact_kind_serializes_to_snake_case() {
        let json = serde_json::to_value(ArtifactKind::DockerImage).unwrap();
        assert_eq!(json, "docker_image");
        let json = serde_json::to_value(ArtifactKind::LinuxPackage).unwrap();
        assert_eq!(json, "linux_package");
        let json = serde_json::to_value(ArtifactKind::Binary).unwrap();
        assert_eq!(json, "binary");
    }

    #[test]
    fn test_artifact_kind_new_variants_serialize() {
        assert_eq!(
            serde_json::to_value(ArtifactKind::UploadableBinary).unwrap(),
            "uploadable_binary"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::UniversalBinary).unwrap(),
            "universal_binary"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::Header).unwrap(),
            "header"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::CArchive).unwrap(),
            "c_archive"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::CShared).unwrap(),
            "c_shared"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::Makeself).unwrap(),
            "makeself"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::DockerImageV2).unwrap(),
            "docker_image_v2"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::PublishableDockerImage).unwrap(),
            "publishable_docker_image"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::PublishableSnapcraft).unwrap(),
            "publishable_snapcraft"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::SourceRpm).unwrap(),
            "source_rpm"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::BrewFormula).unwrap(),
            "brew_formula"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::BrewCask).unwrap(),
            "brew_cask"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::Nixpkg).unwrap(),
            "nixpkg"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::ScoopManifest).unwrap(),
            "scoop_manifest"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::PublishableChocolatey).unwrap(),
            "publishable_chocolatey"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::WingetInstaller).unwrap(),
            "winget_installer"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::WingetDefaultLocale).unwrap(),
            "winget_default_locale"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::WingetVersion).unwrap(),
            "winget_version"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::PkgBuild).unwrap(),
            "pkg_build"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::SrcInfo).unwrap(),
            "src_info"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::SourcePkgBuild).unwrap(),
            "source_pkg_build"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::SourceSrcInfo).unwrap(),
            "source_src_info"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::KrewPluginManifest).unwrap(),
            "krew_plugin_manifest"
        );
        assert_eq!(
            serde_json::to_value(ArtifactKind::UploadableFile).unwrap(),
            "uploadable_file"
        );
    }

    #[test]
    fn test_artifact_kind_library_and_wasm() {
        let json = serde_json::to_value(ArtifactKind::Library).unwrap();
        assert_eq!(json, "library");
        let json = serde_json::to_value(ArtifactKind::Wasm).unwrap();
        assert_eq!(json, "wasm");
    }

    #[test]
    fn test_artifact_kind_as_str_library_wasm() {
        assert_eq!(ArtifactKind::Library.as_str(), "library");
        assert_eq!(ArtifactKind::Wasm.as_str(), "wasm");
    }

    #[test]
    fn test_artifact_kind_parse_roundtrip_all_variants() {
        let all_variants = [
            ArtifactKind::Binary,
            ArtifactKind::UploadableBinary,
            ArtifactKind::UniversalBinary,
            ArtifactKind::Library,
            ArtifactKind::Header,
            ArtifactKind::CArchive,
            ArtifactKind::CShared,
            ArtifactKind::Wasm,
            ArtifactKind::Archive,
            ArtifactKind::SourceArchive,
            ArtifactKind::Makeself,
            ArtifactKind::LinuxPackage,
            ArtifactKind::Snap,
            ArtifactKind::PublishableSnapcraft,
            ArtifactKind::Flatpak,
            ArtifactKind::SourceRpm,
            ArtifactKind::DiskImage,
            ArtifactKind::Installer,
            ArtifactKind::MacOsPackage,
            ArtifactKind::DockerImage,
            ArtifactKind::DockerImageV2,
            ArtifactKind::PublishableDockerImage,
            ArtifactKind::DockerManifest,
            ArtifactKind::BrewFormula,
            ArtifactKind::BrewCask,
            ArtifactKind::Nixpkg,
            ArtifactKind::ScoopManifest,
            ArtifactKind::PublishableChocolatey,
            ArtifactKind::WingetInstaller,
            ArtifactKind::WingetDefaultLocale,
            ArtifactKind::WingetVersion,
            ArtifactKind::PkgBuild,
            ArtifactKind::SrcInfo,
            ArtifactKind::SourcePkgBuild,
            ArtifactKind::SourceSrcInfo,
            ArtifactKind::KrewPluginManifest,
            ArtifactKind::Checksum,
            ArtifactKind::Signature,
            ArtifactKind::Certificate,
            ArtifactKind::Sbom,
            ArtifactKind::Metadata,
            ArtifactKind::UploadableFile,
        ];
        for variant in &all_variants {
            let s = variant.as_str();
            let parsed =
                ArtifactKind::parse(s).unwrap_or_else(|| panic!("parse({:?}) returned None", s));
            assert_eq!(*variant, parsed, "roundtrip failed for {:?}", s);
        }
        assert_eq!(all_variants.len(), 42, "update test when adding variants");
    }

    #[test]
    fn test_query_by_library_and_wasm_kinds() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Library,
            name: String::new(),
            path: PathBuf::from("target/libmylib.so"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "mylib".to_string(),
            metadata: Default::default(),
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Wasm,
            name: String::new(),
            path: PathBuf::from("target/mylib.wasm"),
            target: Some("wasm32-unknown-unknown".to_string()),
            crate_name: "mylib".to_string(),
            metadata: Default::default(),
            size: None,
        });

        assert_eq!(registry.by_kind(ArtifactKind::Library).len(), 1);
        assert_eq!(registry.by_kind(ArtifactKind::Wasm).len(), 1);
        assert_eq!(
            registry
                .by_kind_and_crate(ArtifactKind::Wasm, "mylib")
                .len(),
            1
        );
    }

    #[test]
    fn test_size_reportable_kinds_includes_releasable_and_binaries() {
        let kinds = size_reportable_kinds();
        // Uploadable types
        assert!(kinds.contains(&ArtifactKind::Archive));
        assert!(kinds.contains(&ArtifactKind::SourceArchive));
        assert!(kinds.contains(&ArtifactKind::UploadableFile));
        assert!(kinds.contains(&ArtifactKind::Makeself));
        assert!(kinds.contains(&ArtifactKind::LinuxPackage));
        assert!(kinds.contains(&ArtifactKind::Flatpak));
        assert!(kinds.contains(&ArtifactKind::SourceRpm));
        assert!(kinds.contains(&ArtifactKind::Sbom));
        assert!(kinds.contains(&ArtifactKind::Checksum));
        assert!(kinds.contains(&ArtifactKind::Signature));
        assert!(kinds.contains(&ArtifactKind::Certificate));
        assert!(kinds.contains(&ArtifactKind::DiskImage));
        assert!(kinds.contains(&ArtifactKind::Installer));
        assert!(kinds.contains(&ArtifactKind::MacOsPackage));
        assert!(kinds.contains(&ArtifactKind::Snap));
        // Build outputs
        assert!(kinds.contains(&ArtifactKind::Binary));
        assert!(kinds.contains(&ArtifactKind::UniversalBinary));
        assert!(kinds.contains(&ArtifactKind::Library));
        assert!(kinds.contains(&ArtifactKind::Header));
        assert!(kinds.contains(&ArtifactKind::CArchive));
        assert!(kinds.contains(&ArtifactKind::CShared));
        assert!(kinds.contains(&ArtifactKind::Wasm));
    }

    #[test]
    fn test_size_reportable_kinds_excludes_non_releasable() {
        let kinds = size_reportable_kinds();
        assert!(!kinds.contains(&ArtifactKind::DockerImage));
        assert!(!kinds.contains(&ArtifactKind::DockerManifest));
        assert!(!kinds.contains(&ArtifactKind::Metadata));
        assert!(!kinds.contains(&ArtifactKind::BrewFormula));
        assert!(!kinds.contains(&ArtifactKind::ScoopManifest));
    }

    #[test]
    fn test_print_size_report_filters_and_stores_size() {
        use std::io::Write;

        let dir = std::env::temp_dir().join("anodizer_test_size_report");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Create real files with known sizes
        let archive_path = dir.join("app.tar.gz");
        let mut f = std::fs::File::create(&archive_path).unwrap();
        f.write_all(&[0u8; 2048]).unwrap();

        let binary_path = dir.join("app");
        let mut f = std::fs::File::create(&binary_path).unwrap();
        f.write_all(&[0u8; 4096]).unwrap();

        let docker_path = dir.join("docker-image");
        let mut f = std::fs::File::create(&docker_path).unwrap();
        f.write_all(&[0u8; 8192]).unwrap();

        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: archive_path.clone(),
            target: None,
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        });
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: binary_path.clone(),
            target: None,
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        });
        // DockerImage should be excluded from size reporting
        registry.add(Artifact {
            kind: ArtifactKind::DockerImage,
            name: String::new(),
            path: docker_path.clone(),
            target: None,
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let log = crate::log::StageLogger::new("test", crate::log::Verbosity::Normal);
        print_size_report(&mut registry, &log);

        // Archive and Binary should have size populated
        let archive = &registry.all()[0];
        assert_eq!(archive.kind, ArtifactKind::Archive);
        assert_eq!(archive.size, Some(2048));

        let binary = &registry.all()[1];
        assert_eq!(binary.kind, ArtifactKind::Binary);
        assert_eq!(binary.size, Some(4096));

        // DockerImage should NOT have size populated
        let docker = &registry.all()[2];
        assert_eq!(docker.kind, ArtifactKind::DockerImage);
        assert_eq!(docker.size, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_size_field_defaults_to_none() {
        let registry = ArtifactRegistry::new();
        // Artifact's size is None when freshly constructed
        let mut reg = ArtifactRegistry::new();
        reg.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/nonexistent/binary"),
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });
        assert_eq!(reg.all()[0].size, None);
        drop(registry);
    }

    #[test]
    fn test_size_field_not_serialized_when_none() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });
        let json = registry.to_artifacts_json().unwrap();
        let first = &json.as_array().unwrap()[0];
        // size should not appear in JSON when None
        assert!(first.get("size").is_none());
    }

    #[test]
    fn test_size_field_serialized_when_some() {
        let mut registry = ArtifactRegistry::new();
        registry.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: Some(12345),
        });
        let json = registry.to_artifacts_json().unwrap();
        let first = &json.as_array().unwrap()[0];
        assert_eq!(first["size"], 12345);
    }

    #[test]
    fn release_uploadable_kinds_matches_canonical_set() {
        // Pins the cross-linked artifact set used by stage-checksum,
        // stage-release upload, blob storage, and stage-sign "all" filter.
        // Mirrors GoReleaser's `artifact.ReleaseUploadableTypes()` plus the
        // four installer kinds anodizer ships as OSS:
        //   - Installer       <- GR Pro: MSI / NSIS
        //   - DiskImage       <- GR Pro: DMG
        //   - MacOsPackage    <- GR Pro: PKG
        // A regression that drops any of these silently breaks downstream
        // upload/checksum/sign behavior.
        let kinds = release_uploadable_kinds();
        let expected = [
            ArtifactKind::Archive,
            ArtifactKind::UploadableBinary,
            ArtifactKind::UploadableFile,
            ArtifactKind::SourceArchive,
            ArtifactKind::Makeself,
            ArtifactKind::AppImage,
            ArtifactKind::LinuxPackage,
            ArtifactKind::Flatpak,
            ArtifactKind::SourceRpm,
            ArtifactKind::Installer,
            ArtifactKind::DiskImage,
            ArtifactKind::MacOsPackage,
            ArtifactKind::Sbom,
            ArtifactKind::Checksum,
            ArtifactKind::Signature,
            ArtifactKind::Certificate,
        ];
        assert_eq!(kinds, &expected);
    }

    #[test]
    fn artifact_ext_prefers_metadata_when_present() {
        // GoReleaser parity: `Artifact.Ext()` reads `ExtraExt` from extras
        // (`internal/artifact/artifact.go:442`), not the filename. An SRPM
        // artifact registers `metadata["ext"] = ".src.rpm"` so downstream
        // `{{ .ArtifactExt }}` resolves to `.src.rpm`, not the
        // last-dot-suffix `.rpm` the filename would produce.
        let mut metadata = HashMap::new();
        metadata.insert("ext".to_string(), ".src.rpm".to_string());
        let art = Artifact {
            kind: ArtifactKind::SourceRpm,
            name: "myapp-1.0.0-1.fc42.src.rpm".to_string(),
            path: PathBuf::from("dist/myapp-1.0.0-1.fc42.src.rpm"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata,
            size: None,
        };
        assert_eq!(art.ext(), ".src.rpm");
    }

    #[test]
    fn artifact_ext_falls_back_to_filename_when_metadata_missing() {
        let art = Artifact {
            kind: ArtifactKind::Archive,
            name: "myapp-1.0.0-linux-amd64.tar.gz".to_string(),
            path: PathBuf::from("dist/myapp-1.0.0-linux-amd64.tar.gz"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        assert_eq!(art.ext(), ".tar.gz");
    }

    #[test]
    fn artifact_ext_falls_back_when_metadata_ext_is_empty() {
        let mut metadata = HashMap::new();
        metadata.insert("ext".to_string(), String::new());
        let art = Artifact {
            kind: ArtifactKind::Archive,
            name: "myapp.zip".to_string(),
            path: PathBuf::from("dist/myapp.zip"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata,
            size: None,
        };
        assert_eq!(art.ext(), ".zip");
    }

    #[test]
    fn release_uploadable_kinds_excludes_snap_store_and_raw_build_outputs() {
        // Negative pin: snap-store-bound kinds and raw build outputs must
        // never appear in the release-upload set. Snap files are pushed to
        // the snap store (not GitHub releases); raw Binary / UniversalBinary
        // are wrapped as UploadableBinary or bundled into Archive before
        // upload. A regression that adds any of these would put files in
        // checksums.txt that aren't in the GitHub release.
        let kinds = release_uploadable_kinds();
        for excluded in [
            ArtifactKind::Snap,
            ArtifactKind::PublishableSnapcraft,
            ArtifactKind::Binary,
            ArtifactKind::UniversalBinary,
        ] {
            assert!(
                !kinds.contains(&excluded),
                "{:?} must not be in release_uploadable_kinds()",
                excluded
            );
        }
    }

    /// Shared buffer writer that captures `tracing` output into a `Vec<u8>`.
    #[derive(Clone, Default)]
    struct BufferWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl BufferWriter {
        fn captured(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
        type Writer = BufferWriterGuard<'a>;
        fn make_writer(&'a self) -> Self::Writer {
            BufferWriterGuard(self.0.lock().unwrap())
        }
    }

    struct BufferWriterGuard<'a>(std::sync::MutexGuard<'a, Vec<u8>>);
    impl std::io::Write for BufferWriterGuard<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `body` under a WARN-level capturing subscriber and return the
    /// emitted text so assertions can inspect duplicate-registration warnings.
    fn capture_warnings<F: FnOnce()>(body: F) -> String {
        let buf = BufferWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_max_level(tracing::Level::WARN)
            .without_time()
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, body);
        buf.captured()
    }

    fn upload_artifact(kind: ArtifactKind, name: &str, path: &str) -> Artifact {
        Artifact {
            kind,
            name: name.to_string(),
            path: PathBuf::from(path),
            target: None,
            crate_name: "anodizer".to_string(),
            metadata: Default::default(),
            size: None,
        }
    }

    #[test]
    fn identical_reregistration_is_silent() {
        let captured = capture_warnings(|| {
            let mut registry = ArtifactRegistry::new();
            // Same name AND same resolved path, registered four times — the
            // benign cross-shard `install.sh.sha256` case.
            for _ in 0..4 {
                registry.add(upload_artifact(
                    ArtifactKind::Checksum,
                    "install.sh.sha256",
                    "dist/install.sh.sha256",
                ));
            }
        });
        assert!(
            !captured.contains("already registered"),
            "identical re-registration must not warn, got: {captured:?}"
        );
    }

    #[test]
    fn conflicting_reregistration_still_warns() {
        let captured = capture_warnings(|| {
            let mut registry = ArtifactRegistry::new();
            // Same name but a DIFFERENT path — a genuine upload-collision risk.
            registry.add(upload_artifact(
                ArtifactKind::Archive,
                "app.tar.gz",
                "dist/app.tar.gz",
            ));
            registry.add(upload_artifact(
                ArtifactKind::Archive,
                "app.tar.gz",
                "dist/other/app.tar.gz",
            ));
        });
        assert!(
            captured.contains("already registered"),
            "conflicting re-registration must still warn, got: {captured:?}"
        );
    }
}
