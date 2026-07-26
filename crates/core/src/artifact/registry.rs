use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;

use super::kind::{ArtifactKind, is_derived_sidecar_kind, is_uploadable};

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
    /// dot): prefer the `ext` metadata extra
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

        artifact.path = resolve_stored_path(&artifact.path, artifact.kind);

        // Idempotent re-add collapse, scoped to DERIVED SIDECAR kinds only.
        // The publish-only pipeline re-runs sign→checksum→attest over a
        // rehydrated dist, re-registering each artifact's checksum/signature/
        // certificate/subjects-manifest at its existing (path, kind); without
        // this merge those byte-stable re-adds would duplicate into
        // `artifacts.json`. The narrowing is load-bearing: for a PRIMARY kind
        // (Archive/Binary/LinuxPackage/…) a same-path duplicate is a real
        // emission bug (e.g. two determinism shards overlapping a target) that
        // MUST fall through to the warning + `detect_duplicate_paths` guard, not
        // be silently swallowed.
        if is_derived_sidecar_kind(artifact.kind)
            && let Some(existing) = self
                .artifacts
                .iter_mut()
                .find(|a| a.kind == artifact.kind && a.path == artifact.path)
        {
            for (key, value) in artifact.metadata {
                existing.metadata.insert(key, value);
            }
            existing.size = existing.size.or(artifact.size);
            return;
        }

        // Warn on duplicate names for uploadable artifact types — but only when
        // the re-registration is a genuine conflict (a different on-disk path
        // for the same name). An identical re-registration (same resolved path)
        // is a benign idempotent add (e.g. cross-target `install.sh.sha256`
        // produced once per shard) and must stay silent; warning on it floods
        // the default-verbosity log with duplicate, non-actionable lines.
        // Scan for ANY prior same-name entry whose path differs (not just the
        // first match): a third registration at a new path must still warn
        // even when an earlier same-name entry happens to share the new path.
        if is_uploadable(artifact.kind)
            && let Some(existing) = self
                .artifacts
                .iter()
                .find(|a| is_uploadable(a.kind) && a.name == name && a.path != artifact.path)
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

    /// `true` when an artifact at the same `(path, kind)` is already
    /// registered. Re-running stages (the publish-only sign→checksum→attest
    /// re-run over a rehydrated dist) guard a re-registration of a
    /// non-collapsing kind (e.g. [`ArtifactKind::UploadableFile`]) with this so
    /// a byte-stable re-emit is a no-op instead of a duplicate. `path` is run
    /// through the SAME relativize + separator-normalize transform
    /// [`add`](Self::add) applies before storing, so the comparison sees the
    /// stored form regardless of how the caller spells the path.
    pub fn contains_path_kind(&self, path: &std::path::Path, kind: ArtifactKind) -> bool {
        let resolved = resolve_stored_path(path, kind);
        self.artifacts
            .iter()
            .any(|a| a.kind == kind && a.path == resolved)
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
    /// Selects binary-like artifacts. Used by stage-sbom to
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
    /// consistency (always forward slashes).
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

/// Canonicalize an artifact path to the form `add` stores: relativize an
/// absolute path under the cwd, then normalize separators to forward slashes.
///
/// Relativization keeps `dist/artifacts.json` byte-identical across determinism
/// runs in different worktrees — raw cargo binaries otherwise register paths
/// like `/tmp/anodize-determinism-<pid>-<idx>/.det-tmp/target/.../<bin>` whose
/// leading prefix differs every run. The cwd-is-root guard (`parent().is_none()`,
/// cross-platform) skips relativization in the degenerate case where every
/// absolute path "starts with" `/` (or `C:\`) but stripping the separator
/// yields a path that no longer resolves under the original cwd — production
/// never runs from root, but a few unit tests do.
fn resolve_stored_path(path: &std::path::Path, kind: ArtifactKind) -> PathBuf {
    let mut path = path.to_path_buf();
    if should_relativize_path(kind)
        && path.is_absolute()
        && let Ok(cwd) = std::env::current_dir()
        && cwd.parent().is_some()
        && let Ok(rel) = path.strip_prefix(&cwd)
    {
        path = rel.to_path_buf();
    }
    PathBuf::from(crate::util::normalize_path_separators(
        &path.to_string_lossy(),
    ))
}

/// Should the `add()` path normaliser convert an absolute path into a path
/// relative to the current working directory? Docker image
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
