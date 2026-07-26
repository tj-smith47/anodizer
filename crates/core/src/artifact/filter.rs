use std::collections::HashMap;

use super::kind::ArtifactKind;
use super::registry::Artifact;

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

/// Metadata key recording the artifact kind a derived artifact (signature,
/// certificate, SBOM) was produced FROM. Written at registration by the
/// sign / SBOM stages; read by [`matches_id_filter`] so a derived artifact
/// inherits its subject's id-filter verdict.
pub const SUBJECT_KIND_META: &str = "subject_kind";

/// Metadata key marking a [`ArtifactKind::Checksum`] artifact as the COMBINED
/// `checksums.txt` (value [`COMBINED_CHECKSUM_VALUE`], i.e. `"true"`), as
/// opposed to a per-artifact split sidecar. Written by the checksum stage when
/// it emits the combined file; read by the sign stage's `checksum` / `all`
/// filters, which sign the combined file but never a split sidecar (signing a
/// split `.sha256` is what produced the recursive `.sha256.sig` chains).
pub const COMBINED_CHECKSUM_META: &str = "combined";

/// Sentinel value stored under [`COMBINED_CHECKSUM_META`].
pub const COMBINED_CHECKSUM_VALUE: &str = "true";

/// `true` when `artifact` is a COMBINED checksum sidecar (`checksums.txt`):
/// [`ArtifactKind::Checksum`] carrying the [`COMBINED_CHECKSUM_META`] marker,
/// as opposed to a per-artifact split `.sha256` sidecar.
///
/// This is the SINGLE definition shared by the two sides of a load-bearing
/// invariant. `refresh_combined_checksums` (stage-checksum) rewrites exactly
/// these artifacts at release-upload time to fold in PUBLISH-TIME artifacts
/// (docker `.digest` files registered by the publish leg's docker push), so
/// their published bytes are a function of WHICH PUBLISHERS RAN in the leg
/// that uploaded them — not of the produced dist alone. The pre-submitter
/// verify-release gate consumes the same predicate to exempt exactly these
/// artifacts from cross-leg byte comparison: a leg that did not itself upload
/// the release assets (e.g. the split-topology OIDC job publishing only
/// npm/pypi/cargo) recomputes different — equally correct — combined-checksum
/// bytes and must not treat the difference as corruption. Keeping selection
/// and exemption on one function means a change to what the refresher rewrites
/// automatically changes what the gate exempts; the two cannot drift apart.
pub fn is_combined_checksum_artifact(artifact: &Artifact) -> bool {
    matches!(artifact.kind, ArtifactKind::Checksum)
        && artifact
            .metadata
            .get(COMBINED_CHECKSUM_META)
            .map(String::as_str)
            == Some(COMBINED_CHECKSUM_VALUE)
}

/// Metadata key recording the on-disk packaging format of an artifact whose
/// [`ArtifactKind`] alone is ambiguous. The load-bearing case is the macOS
/// `.app` bundle, registered as [`ArtifactKind::Installer`] (shared with
/// `.msi`/`.exe`) but distinguished by [`FORMAT_APPBUNDLE`] because it is a
/// DIRECTORY, not a file. Read by [`is_directory_bundle_artifact`] and by the
/// `pkg`/`dmg` stages, which wrap the `.app` rather than emit it as a release
/// asset.
pub const FORMAT_META: &str = "format";

/// [`FORMAT_META`] value for a macOS `.app` bundle — a DIRECTORY tree, never a
/// single file.
pub const FORMAT_APPBUNDLE: &str = "appbundle";

/// `true` when `artifact` is a packaging BUNDLE that lives on disk as a
/// DIRECTORY rather than a single file — currently the macOS `.app` bundle
/// (registered as [`ArtifactKind::Installer`] with `format = appbundle`).
///
/// A directory can never be sha256'd, cosign-blob-signed, or uploaded as a
/// release asset (each opens the path as a file and dies with
/// `Is a directory`). GoReleaser parity: the raw `.app` is never a
/// checksum/sign/upload subject — it is wrapped into a `.dmg`/`.pkg` (both
/// FILES, kept as subjects) or archived. This is the SINGLE place that rule is
/// classified, so the subject-collection boundaries (checksum, sign, release
/// upload) all share one definition rather than scattering `path.is_dir()`
/// runtime probes — which would also misbehave under dry-run, where the
/// directory has not been materialized yet.
pub fn is_directory_bundle_artifact(artifact: &Artifact) -> bool {
    matches!(artifact.kind, ArtifactKind::Installer)
        && artifact.metadata.get(FORMAT_META).map(String::as_str) == Some(FORMAT_APPBUNDLE)
}

/// Artifact kinds the `ids:` filter always keeps — these are emitted for
/// every release, not per-build, so a build-id filter has nothing to say
/// about them.
const ID_FILTER_ALWAYS_PASS: [ArtifactKind; 5] = [
    ArtifactKind::Checksum,
    ArtifactKind::SourceArchive,
    ArtifactKind::UploadableFile,
    ArtifactKind::InstallScript,
    ArtifactKind::Metadata,
];

/// `true` when `kind` is a DERIVED artifact kind — one produced FROM another
/// artifact (a signature/certificate of it, or an SBOM cataloging it) whose
/// `ids:` upload verdict must follow that subject's.
fn is_derived_kind(kind: ArtifactKind) -> bool {
    matches!(
        kind,
        ArtifactKind::Signature | ArtifactKind::Certificate | ArtifactKind::Sbom
    )
}

/// The id-filter verdict record a DERIVED artifact (signature, certificate,
/// SBOM) must carry to inherit its subject's upload verdict, as
/// `(subject_kind, inherited_id)` metadata values.
///
/// For an ordinary subject the record is the subject's own kind plus its
/// build id. For a subject that is ITSELF derived (e.g. signing an SBOM),
/// the record is copied transitively from the subject's own record — a
/// signature of an SBOM of an archive answers to the archive, and a
/// signature of a subject-less SBOM (project-wide `artifacts: any`) carries
/// no record, inheriting the always-pass verdict. Recording the derived
/// subject's KIND instead would strand the chain: `subject_kind: "sbom"`
/// with no id would be dropped by [`matches_id_filter`] even though the
/// SBOM itself uploads.
pub fn subject_verdict_record(
    subject_kind: ArtifactKind,
    subject_metadata: &HashMap<String, String>,
) -> (Option<String>, Option<String>) {
    if is_derived_kind(subject_kind) {
        (
            subject_metadata.get(SUBJECT_KIND_META).cloned(),
            subject_metadata.get("id").cloned(),
        )
    } else {
        (
            Some(subject_kind.as_str().to_string()),
            subject_metadata.get("id").cloned(),
        )
    }
}

/// Filter an artifact by the `id` metadata field.
///
/// Artifact `id`-filter semantic:
/// - When `ids` is `None` or empty, every artifact passes.
/// - Artifact kinds `Checksum`, `SourceArchive`, `UploadableFile`, `Metadata`
///   always pass regardless of filter (these are emitted for every release).
/// - Derived kinds (`Signature`, `Certificate`, `Sbom`) inherit their
///   SUBJECT's verdict: a signature uploads iff the artifact it signs
///   uploads. The subject's terminal kind and build id are read from the
///   metadata record written at registration via [`subject_verdict_record`]
///   (transitive for derived-of-derived chains). A derived artifact with no
///   recorded subject (a project-wide `artifacts: any` SBOM or anything
///   derived from it, or an artifact loaded from a pre-`subject_kind`
///   metadata.json in merge mode) always passes — silently dropping a
///   signature is worse than uploading an extra one. A record naming a
///   derived kind (only possible for artifacts written before transitive
///   recording) passes for the same reason: it carries no terminal verdict.
/// - For all other kinds, the artifact's `metadata["id"]` must match one of
///   the supplied ids. An artifact missing an `id` metadata value does not
///   match a non-empty filter.
pub fn matches_id_filter(artifact: &Artifact, ids: Option<&[String]>) -> bool {
    let Some(id_list) = ids else { return true };
    if id_list.is_empty() {
        return true;
    }
    if is_derived_kind(artifact.kind) {
        match artifact.metadata.get(SUBJECT_KIND_META) {
            None => return true,
            Some(subject) => {
                if ID_FILTER_ALWAYS_PASS.iter().any(|k| k.as_str() == subject) {
                    return true;
                }
                if [
                    ArtifactKind::Signature,
                    ArtifactKind::Certificate,
                    ArtifactKind::Sbom,
                ]
                .iter()
                .any(|k| k.as_str() == subject)
                {
                    return true;
                }
                // Fall through: judge by the inherited subject id below.
            }
        }
    } else if ID_FILTER_ALWAYS_PASS.contains(&artifact.kind) {
        return true;
    }
    let artifact_id = artifact
        .metadata
        .get("id")
        .map(|s| s.as_str())
        .unwrap_or("");
    id_list.iter().any(|id| id == artifact_id)
}

/// `true` when a non-empty `ids:` filter reduced a non-empty candidate set
/// to zero — the signal for stages to warn that the FILTER (not the artifact
/// set) is why a config matched nothing. Without the warning a typo'd build
/// id silently no-ops the config.
pub fn ids_filter_eliminated_all(
    ids: Option<&[String]>,
    pre_filter: usize,
    post_filter: usize,
) -> bool {
    post_filter == 0 && pre_filter > 0 && ids.is_some_and(|i| !i.is_empty())
}

/// `true` when `artifact` should be KEPT for an upload destination — i.e. NO
/// `exclude:` glob matches its file name. Globs match `artifact.name` (the
/// asset filename), so `["*.sha256", "*.sig", "*.cdx.json"]` keeps heavy
/// checksum / signature / SBOM sidecars off a mirror while archives still
/// upload. A `None`/empty list keeps everything.
///
/// An unparseable glob is treated as "does not match" (it is skipped, never
/// crashing a release). Surface malformed globs at config-validation time
/// (see `validate_exclude_globs`) so a typo is rejected before it can silently
/// drop assets.
pub fn passes_exclude_filter(artifact: &Artifact, exclude: Option<&[String]>) -> bool {
    name_passes_exclude_filter(artifact.name(), exclude)
}

/// Name-level companion to [`passes_exclude_filter`]: `true` when `name` is
/// kept (no `exclude:` glob matches it). Lets call sites that hold only a
/// resolved asset name (e.g. config-derived signature/SBOM expectations in the
/// `verify-release` gate) apply the SAME exclude semantics the upload path
/// uses, so an intentionally-excluded sidecar is not reported as missing.
/// An unparseable glob is skipped (treated as non-matching); a `None`/empty
/// list keeps everything.
pub fn name_passes_exclude_filter(name: &str, exclude: Option<&[String]>) -> bool {
    let Some(globs) = exclude else { return true };
    if globs.is_empty() {
        return true;
    }
    !globs.iter().any(|g| {
        glob::Pattern::new(g)
            .map(|pat| pat.matches(name))
            .unwrap_or(false)
    })
}

/// `true` when a non-empty `exclude:` filter reduced a non-empty candidate set
/// to zero — the signal for stages to warn that the FILTER (not the artifact
/// set) is why a destination would upload nothing. Without the warning a
/// typo'd glob (e.g. `*` instead of `*.sig`) silently drops every asset.
pub fn exclude_filter_eliminated_all(
    exclude: Option<&[String]>,
    pre_filter: usize,
    post_filter: usize,
) -> bool {
    post_filter == 0 && pre_filter > 0 && exclude.is_some_and(|e| !e.is_empty())
}
