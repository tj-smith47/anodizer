//! SLSA build-provenance / attestation stage for anodizer.
//!
//! Runs between [`ChecksumStage`] and [`SignStage`] in the release pipeline so
//! every selected artifact already carries the sha256 that `stage-checksum`
//! computed, and so the `emit`-mode in-toto statement (written as an
//! `UploadableFile` artifact) is signed by the existing `signs:` loop and
//! uploaded by `stage-release` — no new signing path.
//!
//! Two modes (see [`anodizer_core::config::AttestationMode`]):
//!
//! - **`subjects`** (default): writes `dist/attestation-subjects.json` — a
//!   `[{ "name", "digest": { "sha256" } }]` array that `anodizer-action`
//!   feeds to `actions/attest-build-provenance` (the OIDC path). anodizer does
//!   not attest itself in this mode.
//! - **`emit`**: writes `dist/attestation.intoto.jsonl` — an in-toto v1
//!   statement carrying an SLSA provenance v1 predicate over the same
//!   subjects, registered as an `UploadableFile` so the sign + release stages
//!   handle it like any other sidecar.
//!
//! Subject digests are DERIVED, never hand-listed: each subject's sha256 is
//! reused from the artifact's `sha256` metadata (written by `stage-checksum`)
//! when present, and only computed via [`anodizer_core::hashing::sha256_file`]
//! when absent. Subjects are filtered by the configured artifact KINDS
//! (`archive` / `binary` / `checksum`).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{AttestationArtifactKind, AttestationConfig, AttestationMode};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anodizer_core::stage::Stage;

// ---------------------------------------------------------------------------
// Subject model
// ---------------------------------------------------------------------------

/// A single attestation subject: an artifact filename plus its sha256 digest.
///
/// Serializes to the shape both the subjects manifest and the in-toto
/// statement's `subject[]` array require:
/// `{ "name": "<file>", "digest": { "sha256": "<hex>" } }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subject {
    pub name: String,
    pub digest: SubjectDigest,
}

/// The digest map for a [`Subject`]. Only sha256 is carried — it is the
/// digest `actions/attest-build-provenance` keys on and the one
/// `stage-checksum` always computes by default.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubjectDigest {
    pub sha256: String,
}

// ---------------------------------------------------------------------------
// in-toto v1 statement / SLSA provenance v1 predicate
// ---------------------------------------------------------------------------

const IN_TOTO_STATEMENT_TYPE: &str = "https://in-toto.io/Statement/v1";
const SLSA_PROVENANCE_PREDICATE_TYPE: &str = "https://slsa.dev/provenance/v1";
const ANODIZER_BUILD_TYPE: &str = "https://anodizer.dev/release/v1";
const ANODIZER_BUILDER_ID: &str = "https://anodizer.dev";

/// An in-toto v1 statement carrying an SLSA provenance v1 predicate.
///
/// Deliberately omits the optional `metadata.startedOn` / `finishedOn`
/// timestamps so the statement is byte-deterministic across release retries
/// (anodizer's reproducible-build contract): the same tag + same subjects
/// produce the same statement bytes, so a re-uploaded asset never trips
/// GitHub's `already_exists` size-mismatch check.
#[derive(Debug, Clone, Serialize)]
pub struct InTotoStatement {
    #[serde(rename = "_type")]
    pub _type: String,
    pub subject: Vec<Subject>,
    #[serde(rename = "predicateType")]
    pub predicate_type: String,
    pub predicate: SlsaProvenance,
}

/// The SLSA provenance v1 predicate body.
#[derive(Debug, Clone, Serialize)]
pub struct SlsaProvenance {
    #[serde(rename = "buildDefinition")]
    pub build_definition: BuildDefinition,
    #[serde(rename = "runDetails")]
    pub run_details: RunDetails,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildDefinition {
    #[serde(rename = "buildType")]
    pub build_type: String,
    #[serde(rename = "externalParameters")]
    pub external_parameters: ExternalParameters,
    #[serde(rename = "internalParameters")]
    pub internal_parameters: serde_json::Value,
    #[serde(rename = "resolvedDependencies")]
    pub resolved_dependencies: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExternalParameters {
    pub tag: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunDetails {
    pub builder: Builder,
    pub metadata: RunMetadata,
}

#[derive(Debug, Clone, Serialize)]
pub struct Builder {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunMetadata {
    #[serde(rename = "invocationId")]
    pub invocation_id: String,
}

impl InTotoStatement {
    /// Build a statement over `subjects` for the given release `tag` / `version`.
    pub fn new(subjects: Vec<Subject>, tag: &str, version: &str) -> Self {
        InTotoStatement {
            _type: IN_TOTO_STATEMENT_TYPE.to_string(),
            subject: subjects,
            predicate_type: SLSA_PROVENANCE_PREDICATE_TYPE.to_string(),
            predicate: SlsaProvenance {
                build_definition: BuildDefinition {
                    build_type: ANODIZER_BUILD_TYPE.to_string(),
                    external_parameters: ExternalParameters {
                        tag: tag.to_string(),
                        version: version.to_string(),
                    },
                    internal_parameters: serde_json::json!({}),
                    resolved_dependencies: Vec::new(),
                },
                run_details: RunDetails {
                    builder: Builder {
                        id: ANODIZER_BUILDER_ID.to_string(),
                    },
                    metadata: RunMetadata {
                        // The tag is a deterministic stand-in for the build
                        // invocation: every re-run of the same tag shares the
                        // same id, so the statement stays byte-identical (no
                        // wall-clock / random run id that would break the
                        // reproducible-release contract).
                        invocation_id: tag.to_string(),
                    },
                },
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Kind mapping
// ---------------------------------------------------------------------------

/// Map a configured [`AttestationArtifactKind`] to the concrete
/// [`ArtifactKind`]s it selects from the registry.
///
/// Between them the variants reach every release-uploadable kind (the
/// `release_uploadable_kinds()` set minus signatures/certificates), so an
/// explicit `artifacts:` selection can name anything that lands on the release.
fn concrete_kinds(kind: AttestationArtifactKind) -> &'static [ArtifactKind] {
    match kind {
        // archive: packaged + self-extracting archives + AppImages (a single
        // self-contained, runnable Linux bundle, same shape as makeself).
        AttestationArtifactKind::Archive => &[
            ArtifactKind::Archive,
            ArtifactKind::Makeself,
            ArtifactKind::AppImage,
        ],
        // binary: uploadable raw binaries (the bare-binary release-asset kind,
        // NOT the intermediate build output).
        AttestationArtifactKind::Binary => &[ArtifactKind::UploadableBinary],
        // checksum: checksum files + split sidecars.
        AttestationArtifactKind::Checksum => &[ArtifactKind::Checksum],
        // package: Linux packages (.deb/.rpm/.apk) + source RPMs.
        AttestationArtifactKind::Package => &[ArtifactKind::LinuxPackage, ArtifactKind::SourceRpm],
        // source: the source-archive tarball.
        AttestationArtifactKind::Source => &[ArtifactKind::SourceArchive],
        // sbom: generated SBOM documents.
        AttestationArtifactKind::Sbom => &[ArtifactKind::Sbom],
        // installer: Windows MSI/NSIS, macOS DMG, macOS PKG.
        AttestationArtifactKind::Installer => &[
            ArtifactKind::Installer,
            ArtifactKind::DiskImage,
            ArtifactKind::MacOsPackage,
        ],
    }
}

/// The concrete [`ArtifactKind`]s attested when `artifacts:` is omitted: the
/// canonical release-uploadable set minus the integrity outputs that are
/// either signatures over other artifacts (`Signature`/`Certificate`) or the
/// attestation outputs themselves. Derived from `release_uploadable_kinds()`
/// rather than hand-curated so a kind added to the release surface is attested
/// by default rather than silently dropped.
fn default_attestable_kinds() -> Vec<ArtifactKind> {
    anodizer_core::artifact::release_uploadable_kinds()
        .iter()
        .copied()
        .filter(|k| !matches!(k, ArtifactKind::Signature | ArtifactKind::Certificate))
        .collect()
}

// ---------------------------------------------------------------------------
// Subject derivation
// ---------------------------------------------------------------------------

/// Resolve an artifact's sha256: reuse the digest `stage-checksum` propagated
/// into `metadata["sha256"]` when present; otherwise hash the file on disk.
///
/// Rule #11 / derive-don't-duplicate: the manifest digest is the SAME sha256
/// the checksum stage already computed, never a re-derived or hand-listed one.
fn resolve_sha256(artifact: &Artifact) -> Result<String> {
    if let Some(existing) = artifact.metadata.get("sha256") {
        return Ok(existing.clone());
    }
    anodizer_core::hashing::sha256_file(&artifact.path).with_context(|| {
        format!(
            "attest: hashing {} (no sha256 in artifact metadata)",
            artifact.path.display()
        )
    })
}

/// An artifact is one of attestation's OWN outputs (the subjects manifest or an
/// emit-mode in-toto statement). Excluded from the subject set so attestation
/// never attests itself (self-reference / recursion across re-runs).
fn is_attestation_output(artifact: &Artifact) -> bool {
    artifact.metadata.contains_key("attestation_subjects")
        || artifact.metadata.contains_key("attestation_statement")
}

/// Collect attestation subjects for one crate, in a deterministic order.
///
/// Resolves the concrete [`ArtifactKind`] set — the explicit `artifacts:`
/// selection mapped via [`concrete_kinds`], or the full
/// [`default_attestable_kinds`] when omitted — and derives a [`Subject`] per
/// matching artifact. Skips binary-sign intermediates (never release assets)
/// and attestation's own outputs (no self-attestation). De-duplicates by name
/// so a kind that registers the same file twice doesn't produce a duplicate
/// subject. Sorted by name for byte-stable output.
fn collect_subjects(
    ctx: &Context,
    crate_name: &str,
    selected: Option<&[AttestationArtifactKind]>,
) -> Result<Vec<Subject>> {
    let kinds: Vec<ArtifactKind> = match selected {
        Some(sel) => sel
            .iter()
            .flat_map(|s| concrete_kinds(*s))
            .copied()
            .collect(),
        None => default_attestable_kinds(),
    };

    let mut subjects: Vec<Subject> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for kind in kinds {
        for artifact in ctx.artifacts.by_kind_and_crate(kind, crate_name) {
            if anodizer_core::artifact::is_binary_sign_output(artifact) {
                continue;
            }
            if is_attestation_output(artifact) {
                continue;
            }
            if !seen.insert(artifact.name.clone()) {
                continue;
            }
            let sha256 = resolve_sha256(artifact)?;
            subjects.push(Subject {
                name: artifact.name.clone(),
                digest: SubjectDigest { sha256 },
            });
        }
    }

    subjects.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(subjects)
}

// ---------------------------------------------------------------------------
// Output naming (per-crate clobber avoidance)
// ---------------------------------------------------------------------------

/// Resolve the output filename for a crate's attestation output.
///
/// In single-crate / workspace-lockstep mode (`multi_crate == false`) the bare
/// name is used. In workspace per-crate mode every published crate runs the
/// pipeline, so the filename is prefixed with the crate name to avoid clobber
/// (`<crate>.attestation-subjects.json` / `<crate>.attestation.intoto.jsonl`).
fn output_name(base: &str, crate_name: &str, multi_crate: bool) -> String {
    if multi_crate {
        format!("{crate_name}.{base}")
    } else {
        base.to_string()
    }
}

/// Human-readable description of the kind selection for the empty-match warn:
/// the explicit list, or `all release artifacts` when `artifacts:` is omitted.
fn describe_selection(selected: Option<&[AttestationArtifactKind]>) -> String {
    match selected {
        None => "all release artifacts".to_string(),
        Some(sel) => sel
            .iter()
            .map(|k| {
                serde_json::to_value(k)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| format!("{k:?}"))
            })
            .collect::<Vec<_>>()
            .join(", "),
    }
}

// ---------------------------------------------------------------------------
// AttestStage
// ---------------------------------------------------------------------------

/// Pipeline stage emitting SLSA build-provenance for binaries + archives.
///
/// No-op unless `attestations.enabled` is true. See the module docs for the
/// two modes and the no-new-signing-path design.
pub struct AttestStage;

impl Stage for AttestStage {
    fn name(&self) -> &str {
        "attest"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("attest");

        let Some(cfg) = ctx.config.attestations.clone() else {
            return Ok(());
        };
        if !cfg.enabled {
            log.verbose("attestations disabled; skipping");
            return Ok(());
        }

        let skip = cfg.skip.clone();
        if ctx.skip_with_log(&skip, &log, "attest")? {
            return Ok(());
        }

        let mode = cfg.resolved_mode();
        let selected = cfg.resolved_artifacts();
        let dist = ctx.config.dist.clone();
        let dry_run = ctx.is_dry_run();

        // Per-crate output naming only kicks in when more than one published
        // crate runs through this stage in the same invocation (workspace
        // per-crate mode). Determined from the crate set the run targets.
        let selected_crates = ctx.options.selected_crates.clone();
        let crates: Vec<String> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected_crates.is_empty() || selected_crates.contains(&c.name))
            .map(|c| c.name.clone())
            .collect();
        let multi_crate = crates.len() > 1;

        let tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();
        let version = ctx.version();

        let selected_desc = describe_selection(selected.as_deref());

        let mut new_artifacts: Vec<Artifact> = Vec::new();

        for crate_name in &crates {
            let subjects = collect_subjects(ctx, crate_name, selected.as_deref())?;
            if subjects.is_empty() {
                // Enabled but nothing matched: surface a warn (not a silent
                // verbose line) so a misconfigured filter doesn't ship a green
                // run with zero attestation output. Mirrors the empty-match
                // warn convention in stage-archive / stage-nfpm.
                log.warn(&format!(
                    "attestations enabled but no artifacts matched for crate \
                     {crate_name} (selected kinds: {selected_desc})"
                ));
                continue;
            }

            match mode {
                AttestationMode::Subjects => {
                    let name = output_name(
                        AttestationConfig::SUBJECTS_MANIFEST_NAME,
                        crate_name,
                        multi_crate,
                    );
                    let path = dist.join(&name);
                    let bytes = serialize_subjects_manifest(&subjects)?;
                    write_output(&path, &bytes, dry_run, &log)?;
                    log.status(&format!(
                        "wrote attestation subjects manifest: {name} ({} subjects)",
                        subjects.len()
                    ));
                    // Metadata kind: the manifest is consumed by
                    // anodizer-action, not uploaded as a release asset (the
                    // Action mints the GitHub-trusted attestation from it).
                    new_artifacts.push(manifest_artifact(path, name, crate_name));
                }
                AttestationMode::Emit => {
                    let name =
                        output_name(AttestationConfig::STATEMENT_NAME, crate_name, multi_crate);
                    let path = dist.join(&name);
                    let stmt = InTotoStatement::new(subjects.clone(), &tag, &version);
                    let bytes = serialize_statement(&stmt)?;
                    write_output(&path, &bytes, dry_run, &log)?;
                    log.status(&format!(
                        "wrote in-toto SLSA provenance statement: {name} ({} subjects)",
                        subjects.len()
                    ));
                    // UploadableFile so the existing `signs:` loop signs it and
                    // stage-release uploads it as a release asset — no new
                    // signing or upload path is introduced here.
                    new_artifacts.push(statement_artifact(path, name, crate_name));
                }
            }
        }

        for a in new_artifacts {
            ctx.artifacts.add(a);
        }
        Ok(())
    }
}

/// Serialize the subjects manifest as a pretty JSON array. Sorted-by-name
/// subjects make the bytes deterministic.
fn serialize_subjects_manifest(subjects: &[Subject]) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(subjects).context("attest: serialize subjects")?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Serialize the in-toto statement as a single JSON line (`.jsonl`).
fn serialize_statement(stmt: &InTotoStatement) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(stmt).context("attest: serialize in-toto statement")?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Write `bytes` to `path`, creating parent dirs. No-op in dry-run.
fn write_output(path: &Path, bytes: &[u8], dry_run: bool, log: &StageLogger) -> Result<()> {
    if dry_run {
        log.verbose(&format!("(dry-run) would write {}", path.display()));
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("attest: create parent {}", parent.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("attest: write {}", path.display()))
}

/// Build the registry entry for a subjects manifest. Tagged `Metadata` so it
/// is NOT uploaded as a release asset (the Action consumes it from `dist/`).
fn manifest_artifact(path: PathBuf, name: String, crate_name: &str) -> Artifact {
    Artifact {
        kind: ArtifactKind::Metadata,
        path,
        name,
        target: None,
        crate_name: crate_name.to_string(),
        metadata: std::collections::HashMap::from([(
            "attestation_subjects".to_string(),
            "true".to_string(),
        )]),
        size: None,
    }
}

/// Build the registry entry for an emit-mode in-toto statement. Tagged
/// `UploadableFile` so the existing sign + release stages handle it.
fn statement_artifact(path: PathBuf, name: String, crate_name: &str) -> Artifact {
    Artifact {
        kind: ArtifactKind::UploadableFile,
        path,
        name,
        target: None,
        crate_name: crate_name.to_string(),
        metadata: std::collections::HashMap::from([(
            "attestation_statement".to_string(),
            "true".to_string(),
        )]),
        size: None,
    }
}

#[cfg(test)]
mod tests;
