use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    AttestationArtifactKind, AttestationConfig, AttestationMode, CrateConfig,
};
use anodizer_core::stage::Stage;
use anodizer_core::test_helpers::TestContextBuilder;

use super::*;

fn crate_cfg(name: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    }
}

/// Register an on-disk artifact with the given kind/name/crate. Returns its path.
fn add_artifact(
    ctx: &mut anodizer_core::context::Context,
    dist: &std::path::Path,
    kind: ArtifactKind,
    name: &str,
    crate_name: &str,
    bytes: &[u8],
) -> PathBuf {
    let path = dist.join(name);
    fs::write(&path, bytes).unwrap();
    ctx.artifacts.add(Artifact {
        kind,
        name: name.to_string(),
        path: path.clone(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: crate_name.to_string(),
        metadata: Default::default(),
        size: None,
    });
    path
}

fn attest_config(
    mode: AttestationMode,
    artifacts: Option<Vec<AttestationArtifactKind>>,
) -> AttestationConfig {
    AttestationConfig {
        enabled: true,
        mode: Some(mode),
        artifacts,
        skip: None,
    }
}

// ---------------------------------------------------------------------------
// subjects mode
// ---------------------------------------------------------------------------

/// subjects mode writes `dist/attestation-subjects.json` whose digests are the
/// SAME sha256 stage-checksum computed (rule #11: derive, don't recompute
/// independently). Asserts the manifest digest equals the artifact's `.sha256`
/// sidecar produced by the real ChecksumStage.
#[test]
fn subjects_manifest_digest_equals_checksum_sidecar() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![crate_cfg("myapp")])
        .build();
    ctx.config.attestations = Some(attest_config(AttestationMode::Subjects, None));

    let archive = add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Archive,
        "myapp-1.0.0-linux-amd64.tar.gz",
        "myapp",
        b"the archive bytes",
    );

    // Run the REAL checksum stage so `sha256` lands in artifact metadata and a
    // combined `checksums.txt` exists, then attest.
    anodizer_stage_checksum::ChecksumStage
        .run(&mut ctx)
        .unwrap();
    AttestStage.run(&mut ctx).unwrap();

    let manifest_path = dist.join(AttestationConfig::SUBJECTS_MANIFEST_NAME);
    assert!(manifest_path.exists(), "subjects manifest must exist");

    let subjects: Vec<Subject> =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();

    // The archive subject's digest must equal the independently-computed
    // sha256 of the same file on disk (which is what the checksum stage wrote).
    let expected = anodizer_core::hashing::sha256_file(&archive).unwrap();
    let archive_subject = subjects
        .iter()
        .find(|s| s.name == "myapp-1.0.0-linux-amd64.tar.gz")
        .expect("archive subject present");
    assert_eq!(
        archive_subject.digest.sha256, expected,
        "manifest digest must be the reused checksum-stage sha256, not a divergent value"
    );

    // The combined checksums.txt is selected by the default `checksum` kind, so
    // it appears as a subject too (reuse path, not duplication).
    assert!(
        subjects
            .iter()
            .any(|s| s.name == "myapp_1.0.0_checksums.txt"),
        "checksum file should be a subject under the default artifacts filter"
    );
}

/// The `artifacts:` filter selects only the configured kinds. With
/// `artifacts: [archive]`, a registered binary and checksum must NOT appear.
#[test]
fn subjects_artifacts_filter_includes_only_selected_kinds() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![crate_cfg("myapp")])
        .build();
    ctx.config.attestations = Some(attest_config(
        AttestationMode::Subjects,
        Some(vec![AttestationArtifactKind::Archive]),
    ));

    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Archive,
        "app.tar.gz",
        "myapp",
        b"archive",
    );
    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::UploadableBinary,
        "app-binary",
        "myapp",
        b"binary",
    );
    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Checksum,
        "checksums.txt",
        "myapp",
        b"hash  app.tar.gz\n",
    );

    AttestStage.run(&mut ctx).unwrap();

    let subjects: Vec<Subject> = serde_json::from_slice(
        &fs::read(dist.join(AttestationConfig::SUBJECTS_MANIFEST_NAME)).unwrap(),
    )
    .unwrap();
    let names: Vec<&str> = subjects.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["app.tar.gz"], "only archive kind selected");
}

/// Each selected kind maps correctly: binary and checksum included when
/// configured.
#[test]
fn subjects_filter_includes_binary_and_checksum_when_selected() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![crate_cfg("myapp")])
        .build();
    ctx.config.attestations = Some(attest_config(
        AttestationMode::Subjects,
        Some(vec![
            AttestationArtifactKind::Binary,
            AttestationArtifactKind::Checksum,
        ]),
    ));

    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Archive,
        "app.tar.gz",
        "myapp",
        b"archive",
    );
    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::UploadableBinary,
        "app-binary",
        "myapp",
        b"binary",
    );
    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Checksum,
        "checksums.txt",
        "myapp",
        b"hash  app.tar.gz\n",
    );

    AttestStage.run(&mut ctx).unwrap();

    let subjects: Vec<Subject> = serde_json::from_slice(
        &fs::read(dist.join(AttestationConfig::SUBJECTS_MANIFEST_NAME)).unwrap(),
    )
    .unwrap();
    let names: std::collections::BTreeSet<&str> =
        subjects.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        ["app-binary", "checksums.txt"].into_iter().collect(),
        "binary + checksum selected, archive excluded"
    );
}

/// Default mode (no `mode:` set) is subjects — it writes the manifest, not the
/// in-toto statement.
#[test]
fn default_mode_writes_subjects_manifest_not_statement() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![crate_cfg("myapp")])
        .build();
    // enabled but mode unset → defaults to subjects.
    ctx.config.attestations = Some(AttestationConfig {
        enabled: true,
        mode: None,
        artifacts: None,
        skip: None,
    });

    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Archive,
        "app.tar.gz",
        "myapp",
        b"archive",
    );

    AttestStage.run(&mut ctx).unwrap();

    assert!(
        dist.join(AttestationConfig::SUBJECTS_MANIFEST_NAME)
            .exists(),
        "default mode must produce the subjects manifest"
    );
    assert!(
        !dist.join(AttestationConfig::STATEMENT_NAME).exists(),
        "default mode must NOT produce an in-toto statement"
    );
}

/// `enabled: false` (and a missing block) are no-ops: no files, no artifacts.
#[test]
fn disabled_is_noop() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![crate_cfg("myapp")])
        .build();
    ctx.config.attestations = Some(AttestationConfig {
        enabled: false,
        ..Default::default()
    });

    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Archive,
        "app.tar.gz",
        "myapp",
        b"archive",
    );

    AttestStage.run(&mut ctx).unwrap();

    assert!(
        !dist
            .join(AttestationConfig::SUBJECTS_MANIFEST_NAME)
            .exists()
    );
    assert!(
        ctx.artifacts.by_kind(ArtifactKind::Metadata).is_empty(),
        "disabled attestation must not register any manifest artifact"
    );
}

/// A missing `attestations:` block is a no-op.
#[test]
fn missing_block_is_noop() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![crate_cfg("myapp")])
        .build();

    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Archive,
        "app.tar.gz",
        "myapp",
        b"archive",
    );

    AttestStage.run(&mut ctx).unwrap();
    assert!(
        !dist
            .join(AttestationConfig::SUBJECTS_MANIFEST_NAME)
            .exists()
    );
}

// ---------------------------------------------------------------------------
// emit mode
// ---------------------------------------------------------------------------

/// emit mode writes a valid in-toto v1 statement: correct `_type`,
/// `predicateType = slsa provenance v1`, and `subject[].digest.sha256`
/// matching the artifact's actual sha256. The statement is registered as an
/// `UploadableFile` so the existing sign + release stages handle it.
#[test]
fn emit_writes_valid_intoto_statement_with_matching_digests() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.2.3")
        .dist(dist.clone())
        .crates(vec![crate_cfg("myapp")])
        .build();
    ctx.config.attestations = Some(attest_config(
        AttestationMode::Emit,
        Some(vec![AttestationArtifactKind::Archive]),
    ));

    let archive = add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Archive,
        "app.tar.gz",
        "myapp",
        b"the archive bytes for emit",
    );

    AttestStage.run(&mut ctx).unwrap();

    let stmt_path = dist.join(AttestationConfig::STATEMENT_NAME);
    assert!(stmt_path.exists(), "in-toto statement must exist");

    let v: serde_json::Value =
        serde_json::from_slice(&fs::read(&stmt_path).unwrap()).expect("statement is valid JSON");

    assert_eq!(v["_type"], "https://in-toto.io/Statement/v1");
    assert_eq!(v["predicateType"], "https://slsa.dev/provenance/v1");
    assert_eq!(
        v["predicate"]["buildDefinition"]["externalParameters"]["tag"],
        "v1.2.3"
    );

    let subjects = v["subject"].as_array().expect("subject array");
    assert_eq!(subjects.len(), 1);
    let expected = anodizer_core::hashing::sha256_file(&archive).unwrap();
    assert_eq!(subjects[0]["name"], "app.tar.gz");
    assert_eq!(subjects[0]["digest"]["sha256"], expected);

    // The statement is an UploadableFile so the existing `signs:` loop signs
    // it and stage-release uploads it — no new signing/upload path.
    let uploadable: Vec<_> = ctx
        .artifacts
        .by_kind(ArtifactKind::UploadableFile)
        .into_iter()
        .filter(|a| a.metadata.get("attestation_statement").map(String::as_str) == Some("true"))
        .collect();
    assert_eq!(
        uploadable.len(),
        1,
        "emit-mode statement must register as exactly one UploadableFile artifact"
    );
}

/// The emit-mode statement is signed by the EXISTING sign stage with no new
/// signing path: a `signs: [{artifacts: all}]` config produces a `.sig` for
/// the statement when AttestStage runs before SignStage. Uses `cmd: cp` so no
/// real cosign/gpg is needed — proves only that the statement is fed to the
/// generic `signs:` loop as an uploadable artifact.
#[test]
fn emit_statement_is_signed_by_existing_sign_stage() {
    use anodizer_core::signing::SignConfig;

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myapp")
        .tag("v1.2.3")
        .dist(dist.clone())
        // The generic `signs:` loop, with a stand-in command that writes the
        // signature output. `{{ .Artifact }}` / `{{ .Signature }}` are the
        // sign-arg placeholders; `cp` copies the artifact to its `.sig` path.
        .signs(vec![SignConfig {
            artifacts: Some("all".to_string()),
            cmd: Some("cp".to_string()),
            args: Some(vec![
                "{{ .Artifact }}".to_string(),
                "{{ .Signature }}".to_string(),
            ]),
            ..Default::default()
        }])
        .crates(vec![crate_cfg("myapp")])
        .build();
    ctx.config.attestations = Some(attest_config(
        AttestationMode::Emit,
        Some(vec![AttestationArtifactKind::Archive]),
    ));

    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Archive,
        "app.tar.gz",
        "myapp",
        b"archive bytes",
    );

    // Pipeline order: Attest BEFORE Sign, so the statement is in the registry
    // when the existing sign loop runs.
    AttestStage.run(&mut ctx).unwrap();
    anodizer_stage_sign::SignStage.run(&mut ctx).unwrap();

    let sig = dist.join(format!("{}.sig", AttestationConfig::STATEMENT_NAME));
    assert!(
        sig.exists(),
        "the existing sign stage must produce a .sig for the in-toto statement: {:?}",
        sig
    );
}

// ---------------------------------------------------------------------------
// workspace per-crate (no clobber)
// ---------------------------------------------------------------------------

/// In workspace per-crate mode (multiple published crates in one run), each
/// crate's subjects manifest is written under a crate-prefixed name so they
/// don't clobber, and each covers only its own crate's artifacts.
#[test]
fn workspace_per_crate_manifests_do_not_clobber() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    fs::create_dir_all(&dist).unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("ws")
        .tag("v1.0.0")
        .dist(dist.clone())
        .crates(vec![crate_cfg("alpha"), crate_cfg("beta")])
        .build();
    ctx.config.attestations = Some(attest_config(
        AttestationMode::Subjects,
        Some(vec![AttestationArtifactKind::Archive]),
    ));

    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Archive,
        "alpha.tar.gz",
        "alpha",
        b"alpha archive",
    );
    add_artifact(
        &mut ctx,
        &dist,
        ArtifactKind::Archive,
        "beta.tar.gz",
        "beta",
        b"beta archive",
    );

    AttestStage.run(&mut ctx).unwrap();

    // Bare name must NOT be used in multi-crate mode.
    assert!(
        !dist
            .join(AttestationConfig::SUBJECTS_MANIFEST_NAME)
            .exists(),
        "multi-crate mode must use crate-prefixed manifest names"
    );

    let alpha_manifest = dist.join(format!(
        "alpha.{}",
        AttestationConfig::SUBJECTS_MANIFEST_NAME
    ));
    let beta_manifest = dist.join(format!(
        "beta.{}",
        AttestationConfig::SUBJECTS_MANIFEST_NAME
    ));
    assert!(alpha_manifest.exists(), "alpha manifest present");
    assert!(beta_manifest.exists(), "beta manifest present");

    let alpha_subjects: Vec<Subject> =
        serde_json::from_slice(&fs::read(&alpha_manifest).unwrap()).unwrap();
    let beta_subjects: Vec<Subject> =
        serde_json::from_slice(&fs::read(&beta_manifest).unwrap()).unwrap();

    assert_eq!(
        alpha_subjects.iter().map(|s| &s.name).collect::<Vec<_>>(),
        vec!["alpha.tar.gz"],
        "alpha manifest covers only alpha's artifacts"
    );
    assert_eq!(
        beta_subjects.iter().map(|s| &s.name).collect::<Vec<_>>(),
        vec!["beta.tar.gz"],
        "beta manifest covers only beta's artifacts"
    );
}

/// The in-toto statement is byte-deterministic for the same inputs (no clock
/// reads): two builds of the same tag + subjects produce identical bytes.
#[test]
fn statement_is_byte_deterministic() {
    let subjects = vec![Subject {
        name: "app.tar.gz".to_string(),
        digest: SubjectDigest {
            sha256: "abc123".to_string(),
        },
    }];
    let a =
        serialize_statement(&InTotoStatement::new(subjects.clone(), "v1.0.0", "1.0.0")).unwrap();
    let b = serialize_statement(&InTotoStatement::new(subjects, "v1.0.0", "1.0.0")).unwrap();
    assert_eq!(a, b, "statement bytes must be deterministic");
}
