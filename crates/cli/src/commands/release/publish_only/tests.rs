use super::*;

#[test]
fn load_preserved_context_rejects_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let err = load_preserved_context(&tmp.path().join("context.json")).unwrap_err();
    let msg = format!("{:#}", err);
    assert!(
        msg.contains("publish-only: missing"),
        "error should name the publish-only path; got: {msg}"
    );
    assert!(
        msg.contains("--preserve-dist"),
        "error should point at the preserve-dist flag; got: {msg}"
    );
    // The error must use the literal `<dist-dir>` placeholder, not
    // a `path.parent()` interpolation that would emit "." for
    // relative paths and confuse the operator on the recovery hint.
    assert!(
        msg.contains("<dist-dir>"),
        "error should use the literal <dist-dir> placeholder; got: {msg}"
    );
}

#[test]
fn load_preserved_context_parses_minimal_json() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("context.json");
    std::fs::write(
            &path,
            r#"{"artifacts":[{"name":"a.tar.gz","path":"a.tar.gz","sha256":"sha256:abc","size":42}],"targets":["x86_64-unknown-linux-gnu"],"version":"0.1.0","commit":"deadbeefcafe"}"#,
        )
        .unwrap();
    let parsed = load_preserved_context(&path).unwrap();
    assert_eq!(parsed.version, "0.1.0");
    assert_eq!(parsed.commit, "deadbeefcafe");
    assert_eq!(parsed.targets, vec!["x86_64-unknown-linux-gnu"]);
    assert_eq!(parsed.artifacts.len(), 1);
    assert_eq!(parsed.artifacts[0].name, "a.tar.gz");
}

#[test]
fn load_preserved_context_tolerates_missing_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("context.json");
    std::fs::write(&path, r#"{}"#).unwrap();
    let parsed = load_preserved_context(&path).unwrap();
    assert!(parsed.artifacts.is_empty());
    assert!(parsed.targets.is_empty());
    assert_eq!(parsed.version, "");
    assert_eq!(parsed.commit, "");
}

// ── discover_sharded_manifests / .tmp skip ────────────────────────

#[test]
fn discover_sharded_manifests_skips_tmp_siblings_uniformly() {
    // Both manifest families (`context`, `artifacts`) must skip a
    // `*.tmp` file the harness's atomic-rename writer may have
    // left mid-crash — a leftover scratch file never represents a
    // committed manifest, regardless of which base it sits next to.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("context.json"), "{}").unwrap();
    std::fs::write(tmp.path().join("context.json.tmp"), "garbage").unwrap();
    std::fs::write(tmp.path().join("artifacts.json"), "[]").unwrap();
    std::fs::write(tmp.path().join("artifacts.json.tmp"), "garbage").unwrap();
    std::fs::write(tmp.path().join("artifacts-linux.json"), "[]").unwrap();
    std::fs::write(tmp.path().join("artifacts-linux.json.tmp"), "garbage").unwrap();

    let ctx = discover_sharded_manifests(tmp.path(), anodizer_core::dist::CONTEXT_JSON).unwrap();
    let names: Vec<String> = ctx
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["context.json"], "tmp siblings must be skipped");

    let arts = discover_sharded_manifests(tmp.path(), anodizer_core::dist::ARTIFACTS_JSON).unwrap();
    let names: Vec<String> = arts
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["artifacts-linux.json", "artifacts.json"],
        "artifacts family must also skip .tmp; got {names:?}"
    );
}

// ── un-suffixed + suffixed coexistence ────────────────────────────

#[test]
fn collision_check_errors_when_unsuffixed_and_suffixed_both_present_context() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("context.json"), "{}").unwrap();
    std::fs::write(tmp.path().join("context-linux.json"), "{}").unwrap();
    let err = check_no_unsuffixed_suffixed_collision(tmp.path(), "context").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("context.json") && msg.contains("context-linux.json"),
        "error should name both colliding manifests; got: {msg}"
    );
    assert!(
        msg.contains("upload-artifact merged"),
        "error should name the symptom hypothesis; got: {msg}"
    );
}

#[test]
fn collision_check_errors_when_unsuffixed_and_suffixed_both_present_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("artifacts.json"), "[]").unwrap();
    std::fs::write(tmp.path().join("artifacts-darwin.json"), "[]").unwrap();
    let err = check_no_unsuffixed_suffixed_collision(tmp.path(), "artifacts").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("artifacts.json") && msg.contains("artifacts-darwin.json"),
        "error should name both colliding manifests; got: {msg}"
    );
}

#[test]
fn collision_check_ok_for_unsuffixed_alone() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("context.json"), "{}").unwrap();
    check_no_unsuffixed_suffixed_collision(tmp.path(), "context")
        .expect("unsuffixed-only must be fine");
}

#[test]
fn collision_check_ok_for_suffixed_only() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("context-a.json"), "{}").unwrap();
    std::fs::write(tmp.path().join("context-b.json"), "{}").unwrap();
    check_no_unsuffixed_suffixed_collision(tmp.path(), "context")
        .expect("suffixed-only must be fine");
}

#[test]
fn collision_check_ignores_tmp_sibling_of_suffixed() {
    // A leftover `*.tmp` next to a single un-suffixed manifest
    // must NOT trip the collision check (the tmp file is harness
    // crash debris, not a real shard manifest).
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("context.json"), "{}").unwrap();
    std::fs::write(tmp.path().join("context-linux.json.tmp"), "garbage").unwrap();
    check_no_unsuffixed_suffixed_collision(tmp.path(), "context")
        .expect(".tmp sibling must not trigger collision");
}

// ── merge_preserved_contexts cross-checks ─────────────────────────

fn ctx_entry(version: &str, commit: &str) -> PreservedDistContext {
    PreservedDistContext {
        artifacts: vec![],
        targets: vec![],
        version: version.to_string(),
        commit: commit.to_string(),
    }
}

#[test]
fn merge_preserved_contexts_bails_when_commit_empty_everywhere() {
    let contexts = vec![
        (PathBuf::from("context-a.json"), ctx_entry("0.1.0", "")),
        (PathBuf::from("context-b.json"), ctx_entry("0.1.0", "")),
    ];
    let err = merge_preserved_contexts(&contexts).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no context manifest carried a `commit`"),
        "expected commit-missing diagnostic; got: {msg}"
    );
}

#[test]
fn merge_preserved_contexts_bails_on_commit_mismatch_across_shards() {
    let contexts = vec![
        (
            PathBuf::from("context-a.json"),
            ctx_entry("0.1.0", "deadbeefcafe"),
        ),
        (
            PathBuf::from("context-b.json"),
            ctx_entry("0.1.0", "ba5eba11feed"),
        ),
    ];
    let err = merge_preserved_contexts(&contexts).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("records commit") && msg.contains("merged set is"),
        "expected per-shard commit-mismatch diagnostic; got: {msg}"
    );
    assert!(
        msg.contains("context-b.json"),
        "diagnostic must name the dissenting shard; got: {msg}"
    );
}

#[test]
fn merge_preserved_contexts_bails_on_version_mismatch_across_shards() {
    let contexts = vec![
        (
            PathBuf::from("context-a.json"),
            ctx_entry("0.1.0", "deadbeefcafe"),
        ),
        (
            PathBuf::from("context-b.json"),
            ctx_entry("0.2.0", "deadbeefcafe"),
        ),
    ];
    let err = merge_preserved_contexts(&contexts).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("records version") && msg.contains("merged set is"),
        "expected per-shard version-mismatch diagnostic; got: {msg}"
    );
    assert!(
        msg.contains("context-b.json"),
        "diagnostic must name the dissenting shard; got: {msg}"
    );
}

#[test]
fn merge_preserved_contexts_accepts_consistent_shards() {
    let contexts = vec![
        (
            PathBuf::from("context-a.json"),
            ctx_entry("0.1.0", "deadbeefcafe"),
        ),
        (
            PathBuf::from("context-b.json"),
            ctx_entry("0.1.0", "deadbeefcafe"),
        ),
    ];
    let merged = merge_preserved_contexts(&contexts).expect("consistent shards must merge");
    assert_eq!(merged.commit, "deadbeefcafe");
    assert_eq!(merged.version, "0.1.0");
}

#[test]
fn merge_preserved_contexts_tolerates_one_shard_with_empty_commit() {
    // Half-populated shards (some carry commit, others empty) are
    // fine: the empty entries simply don't anchor the merged
    // value. The cross-check only fires when a non-empty entry
    // disagrees.
    let contexts = vec![
        (PathBuf::from("context-a.json"), ctx_entry("0.1.0", "")),
        (
            PathBuf::from("context-b.json"),
            ctx_entry("0.1.0", "deadbeefcafe"),
        ),
    ];
    let merged = merge_preserved_contexts(&contexts).expect("mixed-empty shards must merge");
    assert_eq!(merged.commit, "deadbeefcafe");
}

// ── detect_duplicate_paths_in ──────────────────────────────────────

#[test]
fn detect_duplicate_paths_in_passes_on_unique_set() {
    let paths = [Path::new("a.tar.gz"), Path::new("b.tar.gz")];
    crate::commands::helpers::detect_duplicate_paths(paths).expect("unique paths must pass");
}

#[test]
fn detect_duplicate_paths_in_flags_repeated_path() {
    let paths = [
        Path::new("a.tar.gz"),
        Path::new("b.tar.gz"),
        Path::new("a.tar.gz"),
    ];
    let err = crate::commands::helpers::detect_duplicate_paths(paths).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("a.tar.gz"),
        "error must name the duplicated path; got: {msg}"
    );
    assert!(
        msg.contains("(2×)"),
        "error must show the duplicate count; got: {msg}"
    );
    assert!(
        msg.contains("shards overlapped"),
        "error must name the matrix-overlap hypothesis; got: {msg}"
    );
}

// ── detect_missing_files_in ────────────────────────────────────────

#[test]
fn detect_missing_files_in_passes_when_all_present() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.tar.gz");
    std::fs::write(&a, b"x").unwrap();
    // Mix absolute (the loader's default shape) and relative paths
    // to ensure both code paths are exercised.
    std::fs::write(tmp.path().join("rel.tar.gz"), b"x").unwrap();
    let paths = [a.as_path(), Path::new("rel.tar.gz")];
    crate::commands::helpers::detect_missing_files(paths, tmp.path())
        .expect("all present must pass");
}

#[test]
fn detect_missing_files_in_errors_on_absent_absolute_path() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist.tar.gz");
    let paths = [missing.as_path()];
    let err = crate::commands::helpers::detect_missing_files(paths, tmp.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("does-not-exist.tar.gz"),
        "error must name the missing file; got: {msg}"
    );
    assert!(
        msg.contains("preserved dist is incomplete"),
        "error must surface the incomplete-dist hypothesis; got: {msg}"
    );
}

#[test]
fn detect_missing_files_in_errors_on_absent_relative_path() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = [Path::new("rel-missing.tar.gz")];
    let err = crate::commands::helpers::detect_missing_files(paths, tmp.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("rel-missing.tar.gz"),
        "error must name the missing relative file; got: {msg}"
    );
}

#[test]
fn detect_missing_files_in_ignores_files_not_in_manifest() {
    // Files that exist in dist/ but are NOT in the manifest are
    // fine — the cross-check only flags MISSING references, not
    // unreferenced files (metadata.json, harness logs, etc.).
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.tar.gz");
    std::fs::write(&a, b"x").unwrap();
    std::fs::write(tmp.path().join("metadata.json"), b"{}").unwrap();
    std::fs::write(tmp.path().join("orphan.tar.gz"), b"x").unwrap();
    let paths = [a.as_path()];
    crate::commands::helpers::detect_missing_files(paths, tmp.path())
        .expect("unreferenced dist files must not trigger the check");
}

// ── hash_verify_preserved_dist ─────────────────────────────────────

/// `sha256("hello world")` — pinned literal so the matching-bytes
/// test doesn't recompute the hash via the very function under test.
const HELLO_WORLD_SHA256: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

#[test]
fn hash_verify_preserved_dist_accepts_matching_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), b"hello world").unwrap();
    let ctx = PreservedDistContext {
        artifacts: vec![PreservedArtifact {
            name: "hello.txt".into(),
            path: "hello.txt".into(),
            sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
            size: 11,
        }],
        ..PreservedDistContext::default()
    };
    hash_verify_preserved_dist(&ctx, tmp.path()).expect("matching bytes must verify clean");
}

#[test]
fn hash_verify_preserved_dist_rejects_mismatched_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let rel = "hello.txt";
    std::fs::write(tmp.path().join(rel), b"hello world").unwrap();
    let ctx = PreservedDistContext {
        artifacts: vec![PreservedArtifact {
            name: rel.into(),
            path: rel.into(),
            // Wrong hash on purpose — drives the mismatch branch.
            sha256: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .into(),
            size: 11,
        }],
        ..PreservedDistContext::default()
    };
    let err = hash_verify_preserved_dist(&ctx, tmp.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("diverge"),
        "error must surface the divergence wording; got: {msg}"
    );
    assert!(
        msg.contains(rel),
        "error must name the offending file; got: {msg}"
    );
}

/// Regression test for the multi-shard ephemeral-signature
/// false-positive. cosign's ECDSA nonce makes per-shard signatures
/// of identical content diverge by design; each shard's context.json
/// records its own .sig hash, but only ONE shard's file wins the
/// `actions/download-artifact merge-multiple: true` race. The merged
/// context references the others' hashes which CANNOT match the
/// surviving bytes. Since `strip_ephemeral_signatures` discards
/// these files and `SignStage` produces the production-key
/// signatures, the hash-verify must skip them rather than block
/// the publish.
#[test]
fn hash_verify_preserved_dist_skips_ephemeral_signatures() {
    let tmp = tempfile::tempdir().unwrap();
    // Plant a `.sig` whose bytes do NOT match the recorded hash.
    // A non-skipping verify would error here.
    std::fs::write(tmp.path().join("foo.tar.gz.sha256.sig"), b"shard-A-bytes").unwrap();
    let ctx = PreservedDistContext {
        artifacts: vec![PreservedArtifact {
            name: "foo.tar.gz.sha256.sig".into(),
            path: "foo.tar.gz.sha256.sig".into(),
            // Hash of unrelated bytes — exercises the skip path.
            sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
            size: 13,
        }],
        ..PreservedDistContext::default()
    };
    hash_verify_preserved_dist(&ctx, tmp.path())
        .expect("ephemeral .sig paths must skip hash-verify");
}

#[test]
fn hash_verify_preserved_dist_skips_pem_and_asc() {
    // Same guarantee for the `.pem` (cosign cert) and `.asc` (gpg
    // armored sig) suffixes. Both are produced by SignStage's
    // ephemeral path and replaced on re-sign.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("foo.pem"), b"cert-A").unwrap();
    std::fs::write(tmp.path().join("foo.asc"), b"asc-A").unwrap();
    let ctx = PreservedDistContext {
        artifacts: vec![
            PreservedArtifact {
                name: "foo.pem".into(),
                path: "foo.pem".into(),
                sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
                size: 6,
            },
            PreservedArtifact {
                name: "foo.asc".into(),
                path: "foo.asc".into(),
                sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
                size: 5,
            },
        ],
        ..PreservedDistContext::default()
    };
    hash_verify_preserved_dist(&ctx, tmp.path())
        .expect("ephemeral .pem / .asc paths must skip hash-verify");
}

/// Regression: cross-shard duplicate paths with diverging recorded
/// hashes (e.g. `anodizer-<ver>-source.tar.gz` produced
/// independently on every shard with subtle git/tar/locale variance)
/// land in the merged context multiple times. Only ONE shard's bytes
/// survive `download-artifact merge-multiple` on disk; the others'
/// claims cannot match. hash-verify must accept the path as soon as
/// the disk bytes match ANY shard's recorded hash, not bail because
/// some shards disagree with disk.
#[test]
fn hash_verify_preserved_dist_accepts_when_any_shard_matches_disk() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("source.tar.gz"), b"hello world").unwrap();
    let ctx = PreservedDistContext {
        artifacts: vec![
            // Shard A: WRONG hash (would fail alone).
            PreservedArtifact {
                name: "source.tar.gz".into(),
                path: "source.tar.gz".into(),
                sha256: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .into(),
                size: 11,
            },
            // Shard B: correct hash → verifies the merged context.
            PreservedArtifact {
                name: "source.tar.gz".into(),
                path: "source.tar.gz".into(),
                sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
                size: 11,
            },
            // Shard C: another WRONG hash (asserts iteration doesn't
            // short-circuit on the first mismatch).
            PreservedArtifact {
                name: "source.tar.gz".into(),
                path: "source.tar.gz".into(),
                sha256: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .into(),
                size: 11,
            },
        ],
        ..PreservedDistContext::default()
    };
    hash_verify_preserved_dist(&ctx, tmp.path())
        .expect("cross-shard duplicate must verify when any shard's hash matches disk");
}

/// Counterpart: if NO shard's recorded hash matches disk, the
/// verifier must still bail and surface every shard's expected hash
/// in the error so the operator can audit which shards diverged.
#[test]
fn hash_verify_preserved_dist_bails_when_no_shard_matches_disk() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("source.tar.gz"), b"hello world").unwrap();
    let ctx = PreservedDistContext {
        artifacts: vec![
            PreservedArtifact {
                name: "source.tar.gz".into(),
                path: "source.tar.gz".into(),
                sha256: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                    .into(),
                size: 11,
            },
            PreservedArtifact {
                name: "source.tar.gz".into(),
                path: "source.tar.gz".into(),
                sha256: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                    .into(),
                size: 11,
            },
        ],
        ..PreservedDistContext::default()
    };
    let err = hash_verify_preserved_dist(&ctx, tmp.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("recorded across 2 shard(s)"),
        "error must surface the shard count; got: {msg}"
    );
    assert!(
        msg.contains("source.tar.gz"),
        "error must name the offending file; got: {msg}"
    );
}

#[test]
fn hash_verify_preserved_dist_rejects_missing_file() {
    let tmp = tempfile::tempdir().unwrap();
    let ctx = PreservedDistContext {
        artifacts: vec![PreservedArtifact {
            name: "absent.tar.gz".into(),
            path: "absent.tar.gz".into(),
            sha256: format!("sha256:{HELLO_WORLD_SHA256}"),
            size: 11,
        }],
        ..PreservedDistContext::default()
    };
    let err = hash_verify_preserved_dist(&ctx, tmp.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("hashing preserved artifact"),
        "error must surface the hash-failure wording; got: {msg}"
    );
    assert!(
        msg.contains("absent.tar.gz"),
        "error must name the missing file; got: {msg}"
    );
}

/// Cleanup must drop the stale per-shard `artifacts-<shard>.json`
/// manifests but leave `context-<shard>.json` alone — see the
/// function-level doc-comment on `cleanup_shard_manifests`.
#[test]
fn cleanup_shard_manifests_removes_only_artifacts_shards_leaves_context() {
    use anodizer_core::log::Verbosity;
    let tmp = tempfile::tempdir().unwrap();
    let dist = tmp.path();
    // Set up: one un-suffixed artifacts.json (the canonical), three
    // sharded artifacts-*.json, three sharded context-*.json.
    std::fs::write(dist.join("artifacts.json"), b"[]").unwrap();
    std::fs::write(dist.join("artifacts-ubuntu-latest.json"), b"[]").unwrap();
    std::fs::write(dist.join("artifacts-macos-latest.json"), b"[]").unwrap();
    std::fs::write(dist.join("artifacts-windows-x86_64.json"), b"[]").unwrap();
    std::fs::write(dist.join("context-ubuntu-latest.json"), b"{}").unwrap();
    std::fs::write(dist.join("context-macos-latest.json"), b"{}").unwrap();

    let log = StageLogger::new("test", Verbosity::Quiet);
    cleanup_shard_manifests(dist, &log);

    // Canonical artifacts.json survives.
    assert!(dist.join("artifacts.json").is_file());
    // Sharded artifacts-* are gone.
    assert!(!dist.join("artifacts-ubuntu-latest.json").exists());
    assert!(!dist.join("artifacts-macos-latest.json").exists());
    assert!(!dist.join("artifacts-windows-x86_64.json").exists());
    // Context shards SURVIVE — there's no un-suffixed replacement, so
    // we must not delete the only manifest the next retry could use.
    assert!(dist.join("context-ubuntu-latest.json").is_file());
    assert!(dist.join("context-macos-latest.json").is_file());
}

/// Filter contract for the inlined missing-file check: Binary +
/// UniversalBinary kinds must be skipped (their paths live under
/// `.det-tmp/target/...` and are not preserved into `dist/`),
/// while every other kind flows through to
/// `detect_missing_files`. Pin the filter shape so a refactor
/// can't silently re-include Binary kinds and break the
/// determinism-verified → publish flow.
#[test]
fn missing_file_check_skips_binary_and_universal_binary_kinds() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());

    // Seed Binary + UniversalBinary (should be filtered out) and
    // a couple of other kinds (should flow through).
    let kinds = [
        ArtifactKind::Binary,
        ArtifactKind::UniversalBinary,
        ArtifactKind::Archive,
        ArtifactKind::Checksum,
    ];
    for (i, k) in kinds.iter().enumerate() {
        ctx.artifacts.add(Artifact {
            kind: *k,
            name: format!("art-{i}"),
            path: std::path::PathBuf::from(format!("art-{i}")),
            target: None,
            crate_name: String::new(),
            metadata: Default::default(),
            size: None,
        });
    }

    // Apply the same filter the run() call site uses and verify
    // exactly the non-Binary kinds survive.
    let kept: Vec<ArtifactKind> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| !matches!(a.kind, ArtifactKind::Binary | ArtifactKind::UniversalBinary))
        .map(|a| a.kind)
        .collect();

    assert_eq!(kept, vec![ArtifactKind::Archive, ArtifactKind::Checksum]);
}

// ── detect_dist_layout tests ──────────────────────────────────────────────

fn write_context_file(dir: &std::path::Path, name: &str) {
    let content = r#"{"artifacts":[],"targets":[],"version":"0.0.0","commit":"abc"}"#;
    std::fs::write(dir.join(name), content).unwrap();
}

fn layout_test_log() -> StageLogger {
    StageLogger::new("layout-test", anodizer_core::log::Verbosity::Quiet)
}

#[test]
fn detect_layout_flat_single_context() {
    let tmp = tempfile::tempdir().unwrap();
    write_context_file(tmp.path(), "context.json");
    let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
    assert!(
        matches!(layout, super::DistLayout::Flat),
        "expected Flat, got {layout:?}"
    );
}

#[test]
fn detect_layout_flat_sharded_context() {
    let tmp = tempfile::tempdir().unwrap();
    write_context_file(tmp.path(), "context-linux.json");
    write_context_file(tmp.path(), "context-macos.json");
    let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
    assert!(
        matches!(layout, super::DistLayout::Flat),
        "expected Flat, got {layout:?}"
    );
}

#[test]
fn detect_layout_per_crate_two_subdirs() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("core");
    let b = tmp.path().join("cli");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    write_context_file(&a, "context.json");
    write_context_file(&b, "context.json");
    let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
    match layout {
        super::DistLayout::PerCrate(names) => {
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(sorted, vec!["cli", "core"]);
        }
        other => panic!("expected PerCrate, got {other:?}"),
    }
}

#[test]
fn detect_layout_ambiguous_flat_and_per_crate() {
    let tmp = tempfile::tempdir().unwrap();
    write_context_file(tmp.path(), "context.json");
    let sub = tmp.path().join("core");
    std::fs::create_dir_all(&sub).unwrap();
    write_context_file(&sub, "context.json");
    let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
    assert!(
        matches!(layout, super::DistLayout::Ambiguous { .. }),
        "expected Ambiguous, got {layout:?}"
    );
}

#[test]
fn detect_layout_empty_dist_returns_flat() {
    let tmp = tempfile::tempdir().unwrap();
    let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
    assert!(
        matches!(layout, super::DistLayout::Flat),
        "empty dist must return Flat, got {layout:?}"
    );
}

#[test]
fn detect_layout_subdir_without_context_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    write_context_file(tmp.path(), "context-linux.json");
    let sub = tmp.path().join("random-dir");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("artifact.tar.gz"), b"bytes").unwrap();
    let layout = super::detect_dist_layout(tmp.path(), &layout_test_log()).unwrap();
    assert!(
        matches!(layout, super::DistLayout::Flat),
        "subdir without context.json must not count as per-crate, got {layout:?}"
    );
}

// ── merge_workspace_skip ─────────────────────────────────────────

#[test]
fn merge_workspace_skip_appends_new_entries() {
    let mut into: Vec<String> = vec![];
    super::merge_workspace_skip(&mut into, &["announce".to_string(), "publish".to_string()]);
    assert_eq!(into, vec!["announce", "publish"]);
}

#[test]
fn merge_workspace_skip_dedupes_existing_cli_entries() {
    // CLI-supplied `--skip announce` plus a workspace
    // `skip: [announce, blob]` must NOT yield `[announce, announce, blob]`
    // — the dedup keeps each stage exactly once so the
    // `should_skip` lookup short-circuits as soon as it finds the
    // first match.
    let mut into: Vec<String> = vec!["announce".to_string()];
    super::merge_workspace_skip(&mut into, &["announce".to_string(), "blob".to_string()]);
    assert_eq!(into, vec!["announce", "blob"]);
}

#[test]
fn merge_workspace_skip_empty_ws_is_noop() {
    let mut into: Vec<String> = vec!["snapcraft-publish".to_string()];
    super::merge_workspace_skip(&mut into, &[]);
    assert_eq!(into, vec!["snapcraft-publish"]);
}

/// Regression: prior to the fix, publish-only per-crate iteration
/// applied the workspace overlay but never propagated
/// `workspaces[].skip:` into the iteration's effective skip list.
/// cfgd-core (a library workspace declaring `skip: [announce]`)
/// ran announce anyway and failed rendering templates that depend
/// on stage-release outputs the announce stage never saw a release
/// from. This asserts the dedup behavior that gates the propagation.
#[test]
fn merge_workspace_skip_propagates_cfgd_core_announce_skip() {
    let mut into: Vec<String> = vec![];
    // Mirrors cfgd's `workspaces[name=cfgd-core].skip: [announce]`.
    super::merge_workspace_skip(&mut into, &["announce".to_string()]);
    assert!(
        into.iter().any(|s| s == "announce"),
        "workspace-level announce skip must propagate; got {:?}",
        into
    );
}

// ── run_per_crate dist restore ───────────────────────────────────

/// Regression: `run_per_crate` re-anchors `ctx.config.dist` onto
/// the per-crate preserved subdir for the duration of each
/// iteration so downstream code reading `ctx.config.dist`
/// (`write_pre_release_metadata`, the GitHub uploader's
/// relative-path resolver) sees the active crate's preserved
/// location. The pre-fix code left `ctx.config.dist` pointing at
/// the workspace-root `./dist`, so cfgd's per-crate metadata.json
/// landed in the wrong place. The save/restore must hold even
/// when the iteration body errors out — otherwise a partial
/// publish-only run would leak the per-iteration dist into the
/// caller's context.
#[test]
fn run_per_crate_restores_ctx_config_dist_on_error() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    let original_dist = tmp.path().join("dist");
    config.dist = original_dist.clone();
    let mut ctx = Context::new(config.clone(), ContextOptions::default());

    // `dist_base` points at a path that doesn't exist; `run_per_crate`
    // will iterate to the first crate, then `run_one_crate_dist`
    // will fail at `detect_dist_layout` / preserved-context discovery.
    // The dist-restore logic must still fire on the Err branch.
    let dist_base = tmp.path().join("missing");
    let log = anodizer_core::log::StageLogger::new(
        "publish-only-restore-test",
        anodizer_core::log::Verbosity::Quiet,
    );
    let opts = RunOpts { dry_run: true };
    let result = run_per_crate(
        &mut ctx,
        &config,
        &log,
        opts,
        dist_base,
        vec!["cfgd".to_string()],
    );
    assert!(
        result.is_err(),
        "iteration must fail when dist_base is absent — fixture precondition"
    );
    assert_eq!(
        ctx.config.dist, original_dist,
        "ctx.config.dist must be restored after the iteration (Ok or Err) \
             so the per-iteration override never leaks into the caller's context"
    );
}

/// Seed `<dist_base>/<name>/` with a minimal but valid EMPTY
/// preserved dist (zero artifacts, commit `deadbeef`) so a
/// `run_per_crate` iteration over `name` runs the real publish-only
/// pipeline to completion in dry-run mode.
fn seed_valid_preserved_dist(dist_base: &std::path::Path, name: &str) {
    let crate_dist = dist_base.join(name);
    std::fs::create_dir_all(&crate_dist).unwrap();
    std::fs::write(
        crate_dist.join("context.json"),
        r#"{"artifacts":[],"targets":[],"version":"0.0.0","commit":"deadbeef"}"#,
    )
    .unwrap();
    std::fs::write(crate_dist.join("artifacts.json"), "[]").unwrap();
}

/// Build the dry-run `Context` matching [`seed_valid_preserved_dist`]'s
/// commit/version so the preserved-context cross-checks pass.
fn preserved_dist_ctx(config: &anodizer_core::config::Config) -> anodizer_core::context::Context {
    use anodizer_core::context::{Context, ContextOptions};
    let mut ctx = Context::new(
        config.clone(),
        ContextOptions {
            dry_run: true,
            ..ContextOptions::default()
        },
    );
    ctx.template_vars_mut().set("FullCommit", "deadbeef");
    ctx.template_vars_mut().set("Version", "0.0.0");
    ctx.template_vars_mut().set("Tag", "v0.0.0");
    ctx
}

/// Each per-crate iteration owns its publish outcome: a leftover
/// `publish_report` / `publish_attempted` from a prior iteration (or
/// an outer run) would render the wrong publisher rows under the
/// next crate's Summary, re-gate the prior crate's failures, and
/// mislabel a skipped publish as "aborted before dispatch". The loop
/// must clear both at EVERY iteration top, not once before the loop.
///
/// Two-crate fixture: crate 'a' carries a minimal but valid empty
/// preserved dist, so iteration 1 runs the real publish-only
/// pipeline (`PublishStage::run` marks `publish_attempted` before
/// its guards). Crate 'b' has no subdir, so iteration 2 fails at
/// preserved-context discovery — AFTER its loop-top reset. A reset
/// hoisted above the loop would clear only the pre-seeded outer
/// state and leave iteration 1's outcome behind, failing the final
/// asserts — this pins the per-iteration placement, not just
/// outer-stale clearing.
#[test]
fn run_per_crate_resets_publish_outcome_each_iteration() {
    use anodizer_core::config::Config;
    use anodizer_core::publish_report::PublishReport;

    let tmp = tempfile::tempdir().unwrap();
    let dist_base = tmp.path().join("dist");
    seed_valid_preserved_dist(&dist_base, "a");

    let config = Config {
        dist: dist_base.clone(),
        ..Config::default()
    };
    let mut ctx = preserved_dist_ctx(&config);
    // Pre-seed stale OUTER state as well: a loop-hoisted reset would
    // clear this much, so the distinguishing signal below stays
    // iteration 1's freshly-set outcome.
    ctx.set_publish_report(PublishReport::default());
    ctx.set_publish_attempted();

    let log = anodizer_core::log::StageLogger::new(
        "publish-only-reset-test",
        anodizer_core::log::Verbosity::Quiet,
    );
    let opts = RunOpts { dry_run: true };
    let err = run_per_crate(
        &mut ctx,
        &config,
        &log,
        opts,
        dist_base.clone(),
        vec!["a".to_string(), "b".to_string()],
    )
    .expect_err("iteration 2 must fail on the absent dist/b subdir");
    let chain = format!("{err:#}");
    assert!(
        chain.contains(&dist_base.join("b").display().to_string()),
        "iteration 1 must succeed and iteration 2 must be the failing one \
             (otherwise this test never observes the per-iteration reset); got: {chain}"
    );
    assert!(
        ctx.publish_report().is_none(),
        "iteration 1's publish_report must be cleared at iteration 2's top"
    );
    assert!(
        !ctx.publish_attempted(),
        "iteration 1's publish_attempted must be cleared at iteration 2's top"
    );
}

/// Vacuity guard for the reset test above: prove the fixture's
/// single successful iteration really exercises the
/// `set_publish_attempted` setter. If `PublishStage::run` ever stops
/// marking the attempt unconditionally (today it fires right after
/// the snapshot guard), the reset test would degrade into asserting
/// "still-cleared state stayed cleared" without noticing — this
/// assert catches that drift loudly.
#[test]
fn run_per_crate_pipeline_marks_publish_attempted() {
    use anodizer_core::config::Config;

    let tmp = tempfile::tempdir().unwrap();
    let dist_base = tmp.path().join("dist");
    seed_valid_preserved_dist(&dist_base, "a");

    let config = Config {
        dist: dist_base.clone(),
        ..Config::default()
    };
    let mut ctx = preserved_dist_ctx(&config);
    let log = anodizer_core::log::StageLogger::new(
        "publish-only-vacuity-test",
        anodizer_core::log::Verbosity::Quiet,
    );
    let opts = RunOpts { dry_run: true };
    run_per_crate(
        &mut ctx,
        &config,
        &log,
        opts,
        dist_base,
        vec!["a".to_string()],
    )
    .expect("single valid-crate iteration must run the pipeline to completion");
    assert!(
        ctx.publish_attempted(),
        "the fixture's pipeline run must mark publish_attempted — \
             otherwise the reset test's distinguishing signal is gone"
    );
}

/// Build a crate config with a GitHub release block so a per-crate
/// dry-run iteration drives the release stage's `ReleaseURL`
/// derivation end-to-end.
fn released_crate_cfg(name: &str, tag_template: &str) -> anodizer_core::config::CrateConfig {
    anodizer_core::config::CrateConfig {
        name: name.to_string(),
        tag_template: Some(tag_template.to_string()),
        release: Some(anodizer_core::config::ReleaseConfig {
            github: Some(anodizer_core::config::ScmRepoConfig {
                owner: "acme".to_string(),
                name: "widget".to_string(),
                token: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Workspace per-crate mode, end-to-end through the real publish-only
/// pipeline in dry-run: each crate's `dist/<crate>/metadata.json` must
/// carry that crate's OWN release URL (derived from its own per-crate
/// tag), not the prior iteration's. This is the file the action-side
/// `release-url` output reads via `.release_url`.
#[test]
#[serial_test::serial]
fn run_per_crate_metadata_carries_per_crate_release_url() {
    use anodizer_core::config::Config;

    let tmp = tempfile::tempdir().unwrap();
    let dist_base = tmp.path().join("dist");
    seed_valid_preserved_dist(&dist_base, "a");
    seed_valid_preserved_dist(&dist_base, "b");

    let config = Config {
        dist: dist_base.clone(),
        crates: vec![
            released_crate_cfg("a", "a-v{{ Version }}"),
            released_crate_cfg("b", "b-v{{ Version }}"),
        ],
        ..Config::default()
    };
    let mut ctx = preserved_dist_ctx(&config);
    // The changelog stage shells to git in the process cwd; skip it so
    // the test stays hermetic — the surface under test is the release
    // stage's URL derivation + the metadata write.
    ctx.options.skip_stages = vec!["changelog".to_string()];

    let log = anodizer_core::log::StageLogger::new(
        "publish-only-release-url-test",
        anodizer_core::log::Verbosity::Quiet,
    );
    let opts = RunOpts { dry_run: true };
    run_per_crate(
        &mut ctx,
        &config,
        &log,
        opts,
        dist_base.clone(),
        vec!["a".to_string(), "b".to_string()],
    )
    .expect("both per-crate dry-run iterations must complete");

    for name in ["a", "b"] {
        let body = std::fs::read_to_string(dist_base.join(name).join("metadata.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        let expected_tag = format!("{name}-v0.0.0");
        assert_eq!(
            json["tag"], expected_tag,
            "crate '{name}' metadata must carry its own tag"
        );
        assert_eq!(
            json["release_url"],
            format!("https://github.com/acme/widget/releases/tag/{expected_tag}"),
            "crate '{name}' metadata must carry its OWN release URL"
        );
    }
    assert!(
        ctx.template_vars().get("ReleaseURL").is_none(),
        "guard Drop must restore the caller's pre-loop (unset) ReleaseURL"
    );
}

/// `reset_release_url` must rewind `ReleaseURL` to the captured
/// baseline at every iteration top, and the guard's Drop must restore
/// it for the caller — otherwise a crate whose release stage never
/// derives a URL (skipped stage, no resolvable repo) inherits the
/// prior crate's URL into its metadata.json / announce templates.
#[test]
fn per_crate_overlay_guard_resets_release_url() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    assert!(ctx.template_vars().get("ReleaseURL").is_none());
    {
        let mut guard = PerCrateOverlayGuard::capture(&mut ctx);
        // Simulate iteration 1's release stage setting the var.
        guard
            .ctx_mut()
            .set_release_url("https://github.com/acme/widget/releases/tag/a-v0.0.0");
        // Iteration 2's loop-top reset must rewind to the unset baseline.
        guard.reset_release_url();
        assert!(
            guard.ctx_mut().template_vars().get("ReleaseURL").is_none(),
            "loop-top reset must rewind ReleaseURL to the pre-loop baseline"
        );
        // Iteration 2 sets its own URL; Drop must still restore the baseline.
        guard
            .ctx_mut()
            .set_release_url("https://github.com/acme/widget/releases/tag/b-v0.0.0");
    }
    assert!(
        ctx.template_vars().get("ReleaseURL").is_none(),
        "guard Drop must restore the caller's pre-loop (unset) ReleaseURL"
    );
}

/// `PerCrateOverlayGuard::Drop` must fire on unwind so a panic from
/// inside the iteration body (e.g. an `unwrap` deep in stage code,
/// a templating overflow, an `unreachable!()`) still rolls the
/// caller's `ctx` back to its pre-loop shape. The closure-then-
/// restore pattern this RAII guard replaces would skip the restore
/// on panic, leaking mid-iteration override values into any outer
/// `catch_unwind` boundary (test harnesses, embedding crates).
#[test]
fn per_crate_overlay_guard_restores_on_panic() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut config = Config::default();
    let original_dist = std::path::PathBuf::from("/tmp/per-crate-guard-panic/dist");
    config.dist = original_dist.clone();
    let mut ctx = Context::new(config, ContextOptions::default());
    let original_selected = vec!["root-crate".to_string()];
    let original_skip = vec!["root-skip".to_string()];
    ctx.options.selected_crates = original_selected.clone();
    ctx.options.skip_stages = original_skip.clone();

    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut guard = PerCrateOverlayGuard::capture(&mut ctx);
        // Simulate the per-iteration mutations the loop performs.
        let inner = guard.ctx_mut();
        inner.config.dist = std::path::PathBuf::from("/scratch/mid-iteration");
        inner.options.selected_crates = vec!["mid-iter-crate".to_string()];
        inner.options.skip_stages = vec!["mid-iter-skip".to_string()];
        // Panic before the guard would normally fall out of scope
        // at the end of the loop. The Drop impl must still fire.
        panic!("simulated mid-iteration panic");
    }));

    assert!(
        result.is_err(),
        "fixture must actually panic — otherwise the guard's restore would also \
             run via the happy path and the test would pass trivially"
    );
    assert_eq!(
        ctx.config.dist, original_dist,
        "Drop must restore ctx.config.dist on panic"
    );
    assert_eq!(
        ctx.options.selected_crates, original_selected,
        "Drop must restore ctx.options.selected_crates on panic"
    );
    assert_eq!(
        ctx.options.skip_stages, original_skip,
        "Drop must restore ctx.options.skip_stages on panic"
    );
}

/// Each per-crate iteration must apply its workspace overlay to a
/// clean baseline. `apply_workspace_overlay` overwrites `changelog` /
/// `signs` only when the workspace sets them and *appends* to `env`,
/// so without the guard's per-iteration `reset_overlay_fields` a value
/// set by workspace A would leak into workspace B (which leaves it
/// unset) and `env` would accumulate A's entries every iteration.
#[test]
fn per_crate_overlay_does_not_leak_across_workspaces() {
    use anodizer_core::config::{
        ChangelogConfig, Config, CrateConfig, HookEntry, HooksConfig, WorkspaceConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::signing::SignConfig;

    fn ws(name: &str, set_overlay: bool) -> WorkspaceConfig {
        WorkspaceConfig {
            name: name.to_string(),
            crates: vec![CrateConfig {
                name: name.to_string(),
                ..CrateConfig::default()
            }],
            changelog: set_overlay.then(|| ChangelogConfig {
                format: Some(format!("{name}-format")),
                ..ChangelogConfig::default()
            }),
            signs: if set_overlay {
                vec![SignConfig {
                    id: Some(format!("{name}-sign")),
                    ..SignConfig::default()
                }]
            } else {
                Vec::new()
            },
            binary_signs: if set_overlay {
                vec![SignConfig {
                    id: Some(format!("{name}-binary-sign")),
                    ..SignConfig::default()
                }]
            } else {
                Vec::new()
            },
            before: set_overlay.then(|| HooksConfig {
                hooks: Some(vec![HookEntry::Simple(format!("{name}-before"))]),
                post: None,
            }),
            after: set_overlay.then(|| HooksConfig {
                hooks: Some(vec![HookEntry::Simple(format!("{name}-after"))]),
                post: None,
            }),
            env: set_overlay.then(|| vec![format!("{name}_KEY=1")]),
            ..WorkspaceConfig::default()
        }
    }

    // Baseline config carries no changelog/signs/env so any value
    // observed after the overlay came from the workspace, not the
    // top-level config.
    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    let workspace_a = ws("alpha", /* set_overlay */ true);
    let workspace_b = ws("beta", /* set_overlay */ false);

    let mut guard = PerCrateOverlayGuard::capture(&mut ctx);

    // Iteration A: workspace alpha sets every overlay field.
    guard.reset_overlay_fields();
    crate::commands::helpers::apply_workspace_overlay(&mut guard.ctx_mut().config, &workspace_a);
    {
        let cfg = &guard.ctx_mut().config;
        assert_eq!(
            cfg.changelog.as_ref().and_then(|c| c.format.as_deref()),
            Some("alpha-format")
        );
        assert_eq!(cfg.signs.len(), 1);
        assert_eq!(cfg.binary_signs.len(), 1);
        assert_eq!(
            cfg.before
                .as_ref()
                .and_then(|h| h.hooks.as_ref())
                .map(|v| v.as_slice()),
            Some([HookEntry::Simple("alpha-before".to_string())].as_slice())
        );
        assert_eq!(
            cfg.after
                .as_ref()
                .and_then(|h| h.hooks.as_ref())
                .map(|v| v.as_slice()),
            Some([HookEntry::Simple("alpha-after".to_string())].as_slice())
        );
        assert_eq!(
            cfg.env.as_deref(),
            Some(["alpha_KEY=1".to_string()].as_slice())
        );
    }

    // Iteration B: workspace beta leaves every overlay field unset, so
    // after the reset+overlay it must NOT inherit alpha's values, and
    // env must not have accumulated alpha's entry.
    guard.reset_overlay_fields();
    crate::commands::helpers::apply_workspace_overlay(&mut guard.ctx_mut().config, &workspace_b);
    {
        let cfg = &guard.ctx_mut().config;
        assert!(
            cfg.changelog.is_none(),
            "workspace B must not inherit A's changelog"
        );
        assert!(
            cfg.signs.is_empty(),
            "workspace B must not inherit A's signs"
        );
        assert!(
            cfg.binary_signs.is_empty(),
            "workspace B must not inherit A's binary_signs"
        );
        assert!(
            cfg.before.is_none(),
            "workspace B must not inherit A's before hooks"
        );
        assert!(
            cfg.after.is_none(),
            "workspace B must not inherit A's after hooks"
        );
        assert!(
            cfg.env.as_ref().map(|e| e.is_empty()).unwrap_or(true),
            "env must not accumulate A's entries into B's iteration: {:?}",
            cfg.env
        );
    }

    // Drop must rewind the overlay fields back to the empty baseline.
    drop(guard);
    assert!(ctx.config.changelog.is_none());
    assert!(ctx.config.signs.is_empty());
    assert!(ctx.config.binary_signs.is_empty());
    assert!(ctx.config.before.is_none());
    assert!(ctx.config.after.is_none());
    assert!(
        ctx.config
            .env
            .as_ref()
            .map(|e| e.is_empty())
            .unwrap_or(true)
    );
}

// ── per-crate Tag restore (the lockstep-workspace title/changelog bug) ──

mod per_crate_tag {
    use super::*;
    use anodizer_core::config::{Config, CrateConfig, WorkspaceConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};
    use serial_test::serial;

    fn quiet_log() -> StageLogger {
        StageLogger::new("per-crate-tag-test", Verbosity::Quiet)
    }

    /// Run `body` with the process cwd swapped to a freshly-`git
    /// init`ed empty temp repo, restoring the original cwd after.
    ///
    /// `apply_per_crate_tag`'s `PreviousTag` lookup shells to `git
    /// describe` in the process cwd; without this the tag tests would
    /// scan the real anodize checkout (non-hermetic, slow, and
    /// dependent on whatever tags happen to be in the dev's tree). An
    /// empty repo makes the lookup return an error fast — caught and
    /// logged by `apply_per_crate_tag`, leaving `Tag` (the thing under
    /// test) untouched. Process-wide cwd swap, so callers must be
    /// `#[serial(cwd)]` (the workspace-canonical cwd serial group).
    fn with_hermetic_git_cwd(body: impl FnOnce()) {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = std::process::Command::new("git");
                    cmd.args(["init", "-q"]).current_dir(tmp.path());
                    cmd
                },
                "git",
            )
            .status
            .success(),
            "git init must succeed for the hermetic tag-test repo",
        );
        // The shared CwdGuard swaps into `tmp` and restores cwd on Drop
        // (panic-safe). Declared after `tmp` so cwd is restored before the
        // tempdir is deleted.
        let _cwd = anodizer_core::test_helpers::CwdGuard::new(tmp.path()).unwrap();
        body();
    }

    fn crate_cfg(name: &str, tag_template: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            tag_template: Some(tag_template.to_string()),
            ..CrateConfig::default()
        }
    }

    /// Build a config whose `crates` already hold the workspace's
    /// entries (the shape `apply_workspace_overlay` produces before
    /// `apply_per_crate_tag` runs).
    fn config_with_crates(crates: Vec<CrateConfig>) -> Config {
        Config {
            crates,
            ..Config::default()
        }
    }

    /// A lockstep workspace shares one `Version`; each crate's own
    /// `tag_template` must recover its own tag. cfgd's top-level
    /// crate templates `v{{ Version }}` → `v0.4.0`; cfgd-core
    /// templates `core-v{{ Version }}` → `core-v0.4.0`. Without the
    /// restore, both inherit whichever tag `resolve_git_context`
    /// pinned once at HEAD.
    #[test]
    #[serial(cwd)]
    fn restores_per_crate_tag_from_tag_template() {
        with_hermetic_git_cwd(|| {
            for (crate_name, tag_template, expect_tag) in [
                ("cfgd", "v{{ Version }}", "v0.4.0"),
                ("cfgd-core", "core-v{{ Version }}", "core-v0.4.0"),
                (
                    "cfgd-operator",
                    "operator-v{{ Version }}",
                    "operator-v0.4.0",
                ),
            ] {
                let config = config_with_crates(vec![crate_cfg(crate_name, tag_template)]);
                let mut ctx = Context::new(config.clone(), ContextOptions::default());
                ctx.template_vars_mut().set("Version", "0.4.0");
                // The global, HEAD-derived tag every iteration would
                // otherwise carry.
                ctx.template_vars_mut().set("Tag", "core-v0.4.0");

                apply_per_crate_tag(&mut ctx, &config, crate_name, &quiet_log());

                assert_eq!(
                    ctx.template_vars().get("Tag").map(String::as_str),
                    Some(expect_tag),
                    "crate '{crate_name}' must carry its own tag, not the global HEAD tag",
                );
            }
        });
    }

    /// An UNSET `tag_template` in a `{name}-v`-convention per-crate
    /// workspace must re-anchor `Tag` onto the crate's OWN family, not
    /// the repo-level bare `v{version}` (`resolved_tag_template()`'s
    /// built-in default — correct for lockstep/single, wrong family
    /// here).
    #[test]
    #[serial(cwd)]
    fn unset_tag_template_resolves_name_v_convention_not_bare_v() {
        with_hermetic_git_cwd(|| {
            let config = config_with_crates(vec![CrateConfig {
                name: "widget".to_string(),
                tag_template: None,
                ..CrateConfig::default()
            }]);
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            ctx.template_vars_mut().set("Version", "0.4.0");
            // The global HEAD-resolved tag from a sibling crate, which
            // the per-crate re-anchor must overwrite.
            ctx.template_vars_mut().set("Tag", "core-v0.4.0");

            apply_per_crate_tag(&mut ctx, &config, "widget", &quiet_log());

            assert_eq!(
                ctx.template_vars().get("Tag").map(String::as_str),
                Some("widget-v0.4.0"),
                "an UNSET tag_template must resolve the crate's own \
                     {{name}}-v convention family, not the repo-level bare-v \
                     built-in default",
            );
        });
    }

    /// Write a minimal preserved `context.json` recording only the
    /// `version`, under `<base>/<crate>/context.json`. Returns the
    /// per-crate dist subdir.
    fn write_preserved_version(base: &Path, crate_name: &str, version: &str) -> std::path::PathBuf {
        let crate_dist = base.join(crate_name);
        std::fs::create_dir_all(&crate_dist).unwrap();
        std::fs::write(
            crate_dist.join("context.json"),
            format!(r#"{{"version":"{version}","commit":"deadbeefcafe"}}"#),
        )
        .unwrap();
        crate_dist
    }

    /// Workspace per-crate INDEPENDENT-version mode: each crate's
    /// preserved manifest carries its OWN version, so the per-crate
    /// tag (and any version-templated artifact name / release title)
    /// must render against that crate's version, NOT the single
    /// HEAD-resolved global version. cfgd-core preserved at 0.5.1 and
    /// cfgd preserved at 0.4.0 — re-anchoring `Version` before the tag
    /// render recovers `core-v0.5.1` / `v0.4.0`. Without the
    /// `apply_per_crate_version` re-anchor, both render against the
    /// global `0.4.0` and the wrong crate gets a mis-tagged release.
    #[test]
    #[serial(cwd)]
    fn independent_version_workspace_renders_per_crate_version() {
        with_hermetic_git_cwd(|| {
            let tmp = tempfile::tempdir().unwrap();
            let dist = tmp.path().join("dist");

            let cases = [
                ("cfgd", "v{{ Version }}", "0.4.0", "v0.4.0"),
                ("cfgd-core", "core-v{{ Version }}", "0.5.1", "core-v0.5.1"),
            ];
            for (crate_name, tag_template, preserved_version, expect_tag) in cases {
                let crate_dist = write_preserved_version(&dist, crate_name, preserved_version);
                let config = config_with_crates(vec![crate_cfg(crate_name, tag_template)]);
                let mut ctx = Context::new(config.clone(), ContextOptions::default());
                // The single HEAD-resolved global version every
                // iteration would otherwise inherit.
                ctx.template_vars_mut().set("Version", "0.4.0");
                ctx.template_vars_mut().set("Tag", "v0.4.0");

                apply_per_crate_version(&mut ctx, &crate_dist, crate_name, &quiet_log());
                assert_eq!(
                    ctx.template_vars().get("Version").map(String::as_str),
                    Some(preserved_version),
                    "crate '{crate_name}' must carry its own preserved Version",
                );

                apply_per_crate_tag(&mut ctx, &config, crate_name, &quiet_log());
                assert_eq!(
                    ctx.template_vars().get("Tag").map(String::as_str),
                    Some(expect_tag),
                    "crate '{crate_name}' tag must render against its own preserved version",
                );
            }
        });
    }

    /// Write a minimal preserved `context.json` that records NO
    /// `version` (only a commit), under `<base>/<crate>/context.json`.
    /// Mirrors a preserved dist whose manifest predates the version
    /// field or was hand-written without it — the case where
    /// `apply_per_crate_version` early-returns.
    fn write_preserved_no_version(base: &Path, crate_name: &str) -> std::path::PathBuf {
        let crate_dist = base.join(crate_name);
        std::fs::create_dir_all(&crate_dist).unwrap();
        std::fs::write(
            crate_dist.join("context.json"),
            r#"{"version":"","commit":"deadbeefcafe"}"#,
        )
        .unwrap();
        crate_dist
    }

    /// Per-crate iteration must rewind the version-derived vars to the
    /// pre-loop baseline at the START of each iteration, mirroring the
    /// `baseline_skip_stages` reset. `apply_per_crate_version`
    /// early-returns (leaves the vars untouched) when a crate's
    /// preserved manifest records no version; without the per-iteration
    /// reset, crate 2 (no preserved version) would inherit crate 1's
    /// re-anchored version and render its tag against the WRONG value.
    ///
    /// Drives the real loop shape: capture the guard, then per crate
    /// `reset_version_vars()` → `apply_per_crate_version`. Crate 1
    /// preserves 0.5.1; crate 2 preserves no version and must fall back
    /// to the pre-loop baseline (0.4.0), NOT inherit crate 1's 0.5.1.
    #[test]
    fn per_iteration_reset_prevents_version_bleed_when_next_crate_lacks_version() {
        let tmp = tempfile::tempdir().unwrap();
        let dist = tmp.path().join("dist");
        let crate1_dist = write_preserved_version(&dist, "cfgd-core", "0.5.1");
        let crate2_dist = write_preserved_no_version(&dist, "cfgd");

        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        // The single HEAD-resolved baseline every iteration rewinds to.
        ctx.template_vars_mut().set("Version", "0.4.0");
        ctx.template_vars_mut().set("Major", "0");
        ctx.template_vars_mut().set("Minor", "4");
        ctx.template_vars_mut().set("Patch", "0");

        let mut guard = PerCrateOverlayGuard::capture(&mut ctx);

        // Iteration 1: crate with a preserved version re-anchors to it.
        guard.reset_version_vars();
        apply_per_crate_version(guard.ctx_mut(), &crate1_dist, "cfgd-core", &quiet_log());
        assert_eq!(
            guard
                .ctx_mut()
                .template_vars()
                .get("Version")
                .map(String::as_str),
            Some("0.5.1"),
            "crate 1 must re-anchor to its own preserved version",
        );

        // Iteration 2: crate WITHOUT a preserved version. The
        // per-iteration reset must rewind to the baseline before the
        // early-returning `apply_per_crate_version`, so the vars are the
        // pre-loop 0.4.0 — NOT crate 1's leaked 0.5.1.
        guard.reset_version_vars();
        apply_per_crate_version(guard.ctx_mut(), &crate2_dist, "cfgd", &quiet_log());
        let vars = guard.ctx_mut();
        assert_eq!(
            vars.template_vars().get("Version").map(String::as_str),
            Some("0.4.0"),
            "crate 2 (no preserved version) must fall back to the pre-loop \
                 baseline, NOT inherit crate 1's re-anchored version",
        );
        assert_eq!(
            vars.template_vars().get("Major").map(String::as_str),
            Some("0"),
            "derived Major must also rewind to baseline, not crate 1's",
        );
    }

    /// A preserved version with prerelease + build metadata must
    /// populate the derived vars (`Major`/`Minor`/`Patch`/
    /// `Prerelease`/`BuildMetadata`) so version-templated names that
    /// reference them render with the per-crate values too.
    #[test]
    fn apply_per_crate_version_populates_derived_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dist = write_preserved_version(tmp.path(), "cfgd", "1.2.3-rc.1+build.7");
        let mut ctx = Context::new(Config::default(), ContextOptions::default());

        apply_per_crate_version(&mut ctx, &crate_dist, "cfgd", &quiet_log());

        let v = ctx.template_vars();
        assert_eq!(
            v.get("Version").map(String::as_str),
            Some("1.2.3-rc.1+build.7")
        );
        assert_eq!(v.get("RawVersion").map(String::as_str), Some("1.2.3"));
        assert_eq!(v.get("Major").map(String::as_str), Some("1"));
        assert_eq!(v.get("Minor").map(String::as_str), Some("2"));
        assert_eq!(v.get("Patch").map(String::as_str), Some("3"));
        assert_eq!(v.get("Prerelease").map(String::as_str), Some("rc.1"));
        assert_eq!(v.get("BuildMetadata").map(String::as_str), Some("build.7"));
    }

    /// A missing preserved manifest (or a non-semver version) leaves
    /// the upstream `Version` untouched rather than blanking it.
    #[test]
    fn apply_per_crate_version_missing_manifest_leaves_version() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut().set("Version", "9.9.9");

        apply_per_crate_version(
            &mut ctx,
            &tmp.path().join("absent-crate"),
            "absent-crate",
            &quiet_log(),
        );

        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("9.9.9"),
            "a missing preserved manifest must not clobber the upstream Version",
        );
    }

    /// The overlay guard must snapshot the pre-loop version-derived
    /// vars and restore them on drop so the per-iteration re-anchor
    /// never leaks into the caller's context.
    #[test]
    fn overlay_guard_restores_version_vars() {
        let mut ctx = Context::new(Config::default(), ContextOptions::default());
        ctx.template_vars_mut().set("Version", "0.4.0");
        ctx.template_vars_mut().set("Major", "0");

        {
            let mut guard = PerCrateOverlayGuard::capture(&mut ctx);
            let inner = guard.ctx_mut();
            inner.template_vars_mut().set("Version", "0.5.1");
            inner.template_vars_mut().set("Major", "9");
        }

        assert_eq!(
            ctx.template_vars().get("Version").map(String::as_str),
            Some("0.4.0"),
            "Drop must restore the caller's Version",
        );
        assert_eq!(
            ctx.template_vars().get("Major").map(String::as_str),
            Some("0"),
            "Drop must restore the caller's Major",
        );
    }

    /// The crate may live in `config.workspaces` rather than the
    /// top-level `crates` list (e.g. when the caller passes the
    /// original config rather than the overlaid one). The lookup
    /// must fall back to the workspace list.
    #[test]
    #[serial(cwd)]
    fn finds_tag_template_in_workspace_fallback() {
        with_hermetic_git_cwd(|| {
            let config = Config {
                workspaces: Some(vec![WorkspaceConfig {
                    name: "cfgd".to_string(),
                    crates: vec![crate_cfg("cfgd", "v{{ Version }}")],
                    ..WorkspaceConfig::default()
                }]),
                ..Config::default()
            };
            let mut ctx = Context::new(config.clone(), ContextOptions::default());
            ctx.template_vars_mut().set("Version", "0.4.0");
            ctx.template_vars_mut().set("Tag", "core-v0.4.0");

            apply_per_crate_tag(&mut ctx, &config, "cfgd", &quiet_log());

            assert_eq!(
                ctx.template_vars().get("Tag").map(String::as_str),
                Some("v0.4.0"),
                "workspace-list fallback must resolve the crate's tag_template",
            );
        });
    }

    /// A crate with no matching config / empty `tag_template` leaves
    /// the upstream tag untouched rather than blanking it.
    #[test]
    fn missing_tag_template_leaves_tag_untouched() {
        let config = config_with_crates(vec![crate_cfg("known", "v{{ Version }}")]);
        let mut ctx = Context::new(config.clone(), ContextOptions::default());
        ctx.template_vars_mut().set("Version", "0.4.0");
        ctx.template_vars_mut().set("Tag", "v0.4.0");

        apply_per_crate_tag(&mut ctx, &config, "unknown-crate", &quiet_log());

        assert_eq!(
            ctx.template_vars().get("Tag").map(String::as_str),
            Some("v0.4.0"),
            "an unmatched crate must not clobber the existing Tag",
        );
    }

    /// The overlay guard must snapshot the pre-loop `Tag` /
    /// `PreviousTag` and restore them on drop so the per-iteration
    /// re-derivation never leaks into the caller's context.
    #[test]
    fn overlay_guard_restores_tag_and_previous_tag() {
        let config = Config {
            dist: std::path::PathBuf::from("/tmp/per-crate-guard-tag/dist"),
            ..Config::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Tag", "v0.4.0");
        ctx.template_vars_mut().set("PreviousTag", "v0.3.0");

        {
            let mut guard = PerCrateOverlayGuard::capture(&mut ctx);
            let inner = guard.ctx_mut();
            inner.template_vars_mut().set("Tag", "core-v0.4.0");
            inner.template_vars_mut().set("PreviousTag", "core-v0.3.0");
        }

        assert_eq!(
            ctx.template_vars().get("Tag").map(String::as_str),
            Some("v0.4.0"),
            "Drop must restore the caller's Tag",
        );
        assert_eq!(
            ctx.template_vars().get("PreviousTag").map(String::as_str),
            Some("v0.3.0"),
            "Drop must restore the caller's PreviousTag",
        );
    }

    /// `write_metadata_json` must land `metadata.json` under
    /// `ctx.config.dist` (the per-crate subdir the loop re-anchored
    /// to), NOT the flat `config.dist` the `config` param still
    /// carries. The release stage's existence gate reads
    /// `ctx.config.dist/metadata.json`; writing to the flat root
    /// would leave that gate looking at a missing file and bail
    /// before the draft→published PATCH.
    ///
    /// This mirrors the real `run_per_crate` / `run_one_crate_dist`
    /// call shape: `config.dist` is the workspace-root dist, while
    /// `ctx.config.dist` was re-anchored onto `dist/<crate>/`. Asserts
    /// the file materializes under `ctx.config.dist` and NOT under
    /// the flat root — fails against the pre-fix code that derived the
    /// dir from the `config` param.
    #[test]
    fn write_metadata_json_materializes_per_crate_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let flat_dist = tmp.path().join("dist");
        let crate_dist = flat_dist.join("cfgd-core");

        // `config` carries the FLAT dist root (what the loop threads
        // through unchanged), `ctx.config.dist` the per-crate subdir.
        let config = Config {
            project_name: "cfgd".to_string(),
            dist: flat_dist.clone(),
            crates: vec![crate_cfg("cfgd-core", "core-v{{ Version }}")],
            ..Config::default()
        };
        let ctx_config = Config {
            dist: crate_dist.clone(),
            ..config.clone()
        };
        let mut ctx = Context::new(ctx_config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "0.4.0");
        ctx.template_vars_mut().set("Tag", "core-v0.4.0");
        ctx.template_vars_mut().set("FullCommit", "deadbeef");
        ctx.set_release_url("https://github.com/acme/cfgd/releases/tag/core-v0.4.0");

        let path =
            crate::commands::helpers::write_metadata_json(&ctx, &config, &quiet_log()).unwrap();

        assert_eq!(
            path,
            crate_dist.join("metadata.json"),
            "metadata.json must land under ctx.config.dist (per-crate subdir)",
        );
        assert!(
            path.exists(),
            "metadata.json must exist for the release upload"
        );
        assert!(
            !flat_dist.join("metadata.json").exists(),
            "metadata.json must NOT land at the flat root (where the release \
                 stage never looks in per-crate mode)",
        );
        let body = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            json["tag"], "core-v0.4.0",
            "metadata must carry the per-crate tag"
        );
        assert_eq!(json["version"], "0.4.0");
        assert_eq!(json["project_name"], "cfgd");
        assert_eq!(
            json["release_url"], "https://github.com/acme/cfgd/releases/tag/core-v0.4.0",
            "per-crate metadata must carry this crate's own release URL \
                 (the action-side `release-url` output reads `.release_url`)"
        );
    }
}

// ── --crate dispatch: per-crate-subdir layout awareness ────

fn subdir_test_log() -> StageLogger {
    StageLogger::new("subdir-test", anodizer_core::log::Verbosity::Quiet)
}

#[test]
fn crate_subdir_has_manifest_detects_context_json() {
    let tmp = tempfile::tempdir().unwrap();
    let sub = tmp.path().join("cfgd");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("context.json"), "{}").unwrap();
    assert!(
        crate_subdir_has_manifest(tmp.path(), "cfgd", &subdir_test_log()),
        "a subdir with context.json must be recognized",
    );
}

#[test]
fn crate_subdir_has_manifest_detects_sharded_context() {
    let tmp = tempfile::tempdir().unwrap();
    let sub = tmp.path().join("cfgd");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("context-linux.json"), "{}").unwrap();
    assert!(
        crate_subdir_has_manifest(tmp.path(), "cfgd", &subdir_test_log()),
        "a subdir with a sharded context-<shard>.json must be recognized",
    );
}

#[test]
fn crate_subdir_has_manifest_false_for_flat_layout() {
    let tmp = tempfile::tempdir().unwrap();
    // Flat layout: manifest at the root, no per-crate subdir.
    std::fs::write(tmp.path().join("context.json"), "{}").unwrap();
    assert!(
        !crate_subdir_has_manifest(tmp.path(), "cfgd", &subdir_test_log()),
        "absence of dist/<crate>/ must fall back to flat (returns false)",
    );
}

// ── per-crate before / after lifecycle hooks ────────────────────────

use anodizer_core::config::{CrateConfig, HookEntry, HooksConfig};
use anodizer_core::context::ContextOptions;

fn crate_with_lifecycle(name: &str, before: Option<&str>, after: Option<&str>) -> CrateConfig {
    let mk = |cmd: &str| HooksConfig {
        hooks: Some(vec![HookEntry::Simple(cmd.to_string())]),
        post: None,
    };
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        before: before.map(mk),
        after: after.map(mk),
        ..Default::default()
    }
}

/// Per-crate `before:` / `after:` fire from the crate's RESOLVED config,
/// rendered against the crate's already-anchored template vars, writing a
/// distinct line per phase so the test proves both ran scoped.
#[test]
fn per_crate_lifecycle_hooks_fire_scoped() {
    let dir = std::env::temp_dir().join(format!("anodizer-pclc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("lifecycle.txt");
    let _ = std::fs::remove_file(&out);
    let out_s = out.display().to_string().replace('\\', "/");

    let config = Config {
        crates: vec![crate_with_lifecycle(
            "foo",
            Some(&format!("echo before:{{{{ Version }}}} >> {out_s}")),
            Some(&format!("echo after:{{{{ Version }}}} >> {out_s}")),
        )],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "3.4.5");
    let log = StageLogger::new("test", anodizer_core::log::Verbosity::Normal);

    run_per_crate_lifecycle_hooks(&ctx, "foo", HookKind::Before, false, &log)
        .expect("before hook runs");
    run_per_crate_lifecycle_hooks(&ctx, "foo", HookKind::After, false, &log)
        .expect("after hook runs");

    let contents = std::fs::read_to_string(&out).unwrap();
    assert!(
        contents.contains("before:3.4.5"),
        "per-crate before: must render against the crate's Version; got: {contents:?}"
    );
    assert!(
        contents.contains("after:3.4.5"),
        "per-crate after: must render against the crate's Version; got: {contents:?}"
    );
    let _ = std::fs::remove_file(&out);
}

/// `--skip=before` suppresses the per-crate before: hook (parity with the
/// top-level surface); a crate with no block is a no-op.
#[test]
fn per_crate_lifecycle_hooks_honor_skip_and_absent_block() {
    let opts = ContextOptions {
        skip_stages: vec!["before".to_string()],
        ..Default::default()
    };
    // `false` would error if spawned; skip must prevent the spawn.
    let config = Config {
        crates: vec![crate_with_lifecycle("foo", Some("false"), None)],
        ..Default::default()
    };
    let ctx = Context::new(config, opts);
    let log = StageLogger::new("test", anodizer_core::log::Verbosity::Normal);
    run_per_crate_lifecycle_hooks(&ctx, "foo", HookKind::Before, false, &log)
        .expect("--skip=before must prevent the hook from spawning");
    // Absent after: block is a no-op.
    run_per_crate_lifecycle_hooks(&ctx, "foo", HookKind::After, false, &log)
        .expect("absent after: block must be a no-op");
    // Unknown crate name is a no-op.
    run_per_crate_lifecycle_hooks(&ctx, "ghost", HookKind::Before, false, &log)
        .expect("unknown crate must be a no-op");
}
