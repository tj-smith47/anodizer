#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    InstallScript,

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
            ArtifactKind::InstallScript => "install_script",
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
            "install_script" => Some(ArtifactKind::InstallScript),
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

// Hand-written so the serialized wire form is EXACTLY `as_str()`, which
// `parse()` round-trips. A derived `rename_all = "snake_case"` diverges for
// variants like `MacOsPackage` (→ `mac_os_package`) and `AppImage`
// (→ `app_image`) that `parse()` does not accept, silently breaking the
// publish-only artifact-manifest loader.
impl serde::Serialize for ArtifactKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
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
        ArtifactKind::InstallScript,
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
        ArtifactKind::InstallScript,
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
/// The release-uploadable artifact kinds plus the four
/// installer kinds (MSI/NSIS as `Installer`, DMG as
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
        ArtifactKind::InstallScript,
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

/// PRIMARY artifact kinds: the real, distributable build outputs that MAY be
/// inputs (subjects) to the checksum and sign stages.
///
/// Deliberately EXCLUDES every DERIVED sidecar kind ([`ArtifactKind::Checksum`],
/// [`ArtifactKind::Signature`], [`ArtifactKind::Certificate`],
/// [`ArtifactKind::Metadata`]). A sidecar is the *output* of checksumming or
/// signing — feeding it back in as a subject is what produces the pathological
/// recursive chains (a checksum-of-a-signature-of-a-checksum:
/// `X.sha256.sig.sha256`). By construction those kinds are not in this list, so
/// no checksum or sign stage can ever take one as input.
///
/// [`ArtifactKind::Sbom`] IS primary: a primary artifact's SBOM (`X.cdx.json`)
/// may be checksummed once (→ `X.cdx.json.sha256`) and signed once, matching
/// GoReleaser (which lists SBOMs in `checksums.txt`). The HARD invariant is
/// only that a Checksum/Signature/Certificate/Metadata is NEVER an input — an
/// SBOM, being a first-class catalog artifact rather than a checksum/signature
/// of something, is safe to checksum/sign once with no recursion.
pub fn primary_subject_kinds() -> &'static [ArtifactKind] {
    &[
        ArtifactKind::Archive,
        ArtifactKind::UploadableBinary,
        ArtifactKind::SourceArchive,
        ArtifactKind::UploadableFile,
        ArtifactKind::Makeself,
        ArtifactKind::InstallScript,
        ArtifactKind::AppImage,
        ArtifactKind::LinuxPackage,
        ArtifactKind::Flatpak,
        ArtifactKind::SourceRpm,
        ArtifactKind::Installer,
        ArtifactKind::DiskImage,
        ArtifactKind::MacOsPackage,
        ArtifactKind::Sbom,
    ]
}

/// The kinds the checksum stage may take as subjects. Identical to
/// [`primary_subject_kinds`] — a primary artifact (including its SBOM) is
/// checksummed exactly once; a derived sidecar never is.
pub fn checksummable_subject_kinds() -> &'static [ArtifactKind] {
    primary_subject_kinds()
}

/// The kinds the sign stage's `artifacts: all` / `artifacts: any` filter may
/// take as subjects. Identical to [`primary_subject_kinds`] — a derived sidecar
/// (Checksum/Signature/Certificate/Metadata) is never signed, so the stage can
/// never produce a `.sig.sig` / `.sha256.sig` chain from `artifacts: all`.
pub fn signable_subject_kinds() -> &'static [ArtifactKind] {
    primary_subject_kinds()
}

/// `true` when `kind` is a DERIVED sidecar — the OUTPUT of checksumming,
/// signing, or run metadata, and therefore something that must NEVER be an
/// input (subject) to the checksum or sign stages.
///
/// What makes the recursive `(.sha256|.sig)`-chain bug unrepresentable is the
/// ALLOW-LIST taxonomy — [`checksummable_subject_kinds`] /
/// [`signable_subject_kinds`] enumerate only primary kinds, so a derived
/// sidecar can never be selected as a subject in the first place. This
/// predicate is the COMPLEMENTARY deny-side filter: it is used where a stage
/// iterates the whole registry rather than an allow-listed kind set —
/// `refresh_combined_checksums` skips every artifact for which this returns
/// `true` so a freshly-produced `.sig` is not re-hashed into the combined file.
pub fn is_derived_sidecar_kind(kind: ArtifactKind) -> bool {
    matches!(
        kind,
        ArtifactKind::Checksum
            | ArtifactKind::Signature
            | ArtifactKind::Certificate
            | ArtifactKind::Metadata
    )
}

/// Check if an artifact kind is uploadable.
pub(super) fn is_uploadable(kind: ArtifactKind) -> bool {
    uploadable_kinds().contains(&kind)
}
