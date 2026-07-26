use super::*;
use std::collections::HashMap;
use std::path::PathBuf;

/// Every `ArtifactKind` variant, in one list that drives the round-trip
/// coverage below. The `const _` guard that follows is an exhaustive match
/// with no `_` arm, sitting immediately after this list: adding a variant
/// fails to compile until it is added to BOTH, and the two lists live
/// together so they move together. `#[non_exhaustive]` does not block
/// exhaustive matching within the defining crate, so this compiles here.
/// (Stable Rust cannot enumerate variants without an explicit list; the
/// adjacent exhaustive match is the strongest available poka-yoke.)
fn all_artifact_kinds() -> Vec<ArtifactKind> {
    use ArtifactKind::*;
    let all = vec![
        Binary,
        UploadableBinary,
        UniversalBinary,
        Library,
        Header,
        CArchive,
        CShared,
        Wasm,
        Archive,
        SourceArchive,
        Makeself,
        AppImage,
        InstallScript,
        LinuxPackage,
        Snap,
        PublishableSnapcraft,
        Flatpak,
        SourceRpm,
        DiskImage,
        Installer,
        MacOsPackage,
        DockerImage,
        DockerImageV2,
        PublishableDockerImage,
        DockerManifest,
        DockerDigest,
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
        Checksum,
        Signature,
        Certificate,
        Sbom,
        Metadata,
        UploadableFile,
    ];
    // Compile-time exhaustiveness guard: coercing a non-capturing closure
    // to a `fn` pointer in `const` context forces the match below to be
    // type-checked. A new variant breaks the (no-`_`) match and fails the
    // build until it is also added to `all` above. `const _` is never
    // dead-code-linted, so this needs no `#[allow]`.
    const _: fn(ArtifactKind) = |k| match k {
        ArtifactKind::Binary
        | ArtifactKind::UploadableBinary
        | ArtifactKind::UniversalBinary
        | ArtifactKind::Library
        | ArtifactKind::Header
        | ArtifactKind::CArchive
        | ArtifactKind::CShared
        | ArtifactKind::Wasm
        | ArtifactKind::Archive
        | ArtifactKind::SourceArchive
        | ArtifactKind::Makeself
        | ArtifactKind::AppImage
        | ArtifactKind::InstallScript
        | ArtifactKind::LinuxPackage
        | ArtifactKind::Snap
        | ArtifactKind::PublishableSnapcraft
        | ArtifactKind::Flatpak
        | ArtifactKind::SourceRpm
        | ArtifactKind::DiskImage
        | ArtifactKind::Installer
        | ArtifactKind::MacOsPackage
        | ArtifactKind::DockerImage
        | ArtifactKind::DockerImageV2
        | ArtifactKind::PublishableDockerImage
        | ArtifactKind::DockerManifest
        | ArtifactKind::DockerDigest
        | ArtifactKind::BrewFormula
        | ArtifactKind::BrewCask
        | ArtifactKind::Nixpkg
        | ArtifactKind::ScoopManifest
        | ArtifactKind::PublishableChocolatey
        | ArtifactKind::WingetInstaller
        | ArtifactKind::WingetDefaultLocale
        | ArtifactKind::WingetVersion
        | ArtifactKind::PkgBuild
        | ArtifactKind::SrcInfo
        | ArtifactKind::SourcePkgBuild
        | ArtifactKind::SourceSrcInfo
        | ArtifactKind::KrewPluginManifest
        | ArtifactKind::Checksum
        | ArtifactKind::Signature
        | ArtifactKind::Certificate
        | ArtifactKind::Sbom
        | ArtifactKind::Metadata
        | ArtifactKind::UploadableFile => {}
    };
    all
}

#[test]
fn artifact_kind_serde_parse_roundtrip() {
    for k in all_artifact_kinds() {
        // `parse()` must accept the canonical `as_str()` spelling.
        assert_eq!(
            ArtifactKind::parse(k.as_str()),
            Some(k),
            "parse(as_str()) must round-trip for {k:?}",
        );
        // serde serialization must equal `as_str()` exactly, so a kind
        // serialized into artifacts.json re-parses cleanly on load.
        assert_eq!(
            serde_json::to_value(k).unwrap(),
            serde_json::Value::String(k.as_str().to_string()),
            "serde serialization must equal as_str() for {k:?}",
        );
    }
}

fn derived(kind: ArtifactKind, meta: &[(&str, &str)]) -> Artifact {
    Artifact {
        kind,
        name: "x".to_string(),
        path: PathBuf::from("x"),
        target: None,
        crate_name: "app".to_string(),
        metadata: meta
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        size: None,
    }
}

fn named(name: &str) -> Artifact {
    Artifact {
        kind: ArtifactKind::Archive,
        name: name.to_string(),
        path: PathBuf::from(name),
        target: None,
        crate_name: "app".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    }
}

#[test]
fn exclude_none_keeps_everything() {
    let a = named("app_1.0.0_x86_64.tar.gz");
    assert!(passes_exclude_filter(&a, None));
}

#[test]
fn exclude_empty_keeps_everything() {
    let a = named("checksums.txt.sig");
    let globs: Vec<String> = vec![];
    assert!(passes_exclude_filter(&a, Some(&globs)));
}

#[test]
fn exclude_suffix_glob_drops_match_keeps_archive() {
    let globs = vec!["*.sig".to_string()];
    assert!(
        !passes_exclude_filter(&named("checksums.txt.sig"), Some(&globs)),
        "*.sig must drop the signature sidecar"
    );
    assert!(
        passes_exclude_filter(&named("app_1.0.0_x86_64.tar.gz"), Some(&globs)),
        "*.sig must keep the archive"
    );
}

#[test]
fn exclude_multi_glob_drops_any_match() {
    let globs = vec![
        "*.sha256".to_string(),
        "*.sig".to_string(),
        "*.cdx.json".to_string(),
    ];
    assert!(!passes_exclude_filter(
        &named("app.tar.gz.sha256"),
        Some(&globs)
    ));
    assert!(!passes_exclude_filter(
        &named("app.tar.gz.sig"),
        Some(&globs)
    ));
    assert!(!passes_exclude_filter(&named("app.cdx.json"), Some(&globs)));
    assert!(passes_exclude_filter(
        &named("app_1.0.0_x86_64.tar.gz"),
        Some(&globs)
    ));
}

#[test]
fn exclude_no_match_keeps_artifact() {
    let globs = vec!["*.sig".to_string(), "*.deb".to_string()];
    assert!(passes_exclude_filter(
        &named("app_1.0.0_x86_64.tar.gz"),
        Some(&globs)
    ));
}

#[test]
fn exclude_invalid_glob_is_ignored_not_panic() {
    // An unparseable pattern (unclosed `[`) must NOT crash the release and
    // must NOT match — it is skipped, so the artifact is kept.
    let globs = vec!["[".to_string()];
    assert!(
        passes_exclude_filter(&named("app.tar.gz"), Some(&globs)),
        "an invalid glob is skipped (treated as non-matching), keeping the artifact"
    );
}

#[test]
fn exclude_invalid_glob_alongside_valid_still_filters() {
    // A valid glob in the same list still does its job even when a sibling
    // glob is malformed.
    let globs = vec!["[".to_string(), "*.sig".to_string()];
    assert!(!passes_exclude_filter(&named("a.sig"), Some(&globs)));
    assert!(passes_exclude_filter(&named("a.tar.gz"), Some(&globs)));
}

#[test]
fn exclude_eliminated_all_flags_empty_result() {
    let globs = vec!["*".to_string()];
    // Non-empty candidate set reduced to zero by a non-empty exclude.
    assert!(exclude_filter_eliminated_all(Some(&globs), 5, 0));
    // Zero result but the candidate set was already empty: not the filter's
    // fault.
    assert!(!exclude_filter_eliminated_all(Some(&globs), 0, 0));
    // Non-zero result: nothing to warn about.
    assert!(!exclude_filter_eliminated_all(Some(&globs), 5, 3));
    // No / empty exclude never trips the warning.
    assert!(!exclude_filter_eliminated_all(None, 5, 0));
    let empty: Vec<String> = vec![];
    assert!(!exclude_filter_eliminated_all(Some(&empty), 5, 0));
}

#[test]
fn id_filter_signature_inherits_included_subject_verdict() {
    let sig = derived(
        ArtifactKind::Signature,
        &[("subject_kind", "archive"), ("id", "keep")],
    );
    let ids = vec!["keep".to_string()];
    assert!(matches_id_filter(&sig, Some(&ids)));
}

#[test]
fn id_filter_signature_inherits_excluded_subject_verdict() {
    let sig = derived(
        ArtifactKind::Signature,
        &[("subject_kind", "archive"), ("id", "drop")],
    );
    let ids = vec!["keep".to_string()];
    assert!(!matches_id_filter(&sig, Some(&ids)));
}

#[test]
fn id_filter_signature_of_checksum_always_passes() {
    // Checksum subjects always upload, so their signatures do too — even
    // though neither carries a build id.
    let sig = derived(ArtifactKind::Signature, &[("subject_kind", "checksum")]);
    let ids = vec!["keep".to_string()];
    assert!(matches_id_filter(&sig, Some(&ids)));
}

#[test]
fn id_filter_derived_without_subject_record_passes() {
    // Project-wide `artifacts: any` SBOMs and pre-subject_kind artifacts
    // (merge mode) have no recorded subject; dropping them silently is
    // worse than uploading an extra asset.
    let sbom = derived(ArtifactKind::Sbom, &[("sbom_id", "default")]);
    let ids = vec!["keep".to_string()];
    assert!(matches_id_filter(&sbom, Some(&ids)));
}

#[test]
fn subject_verdict_record_is_transitive_for_derived_subjects() {
    // Ordinary subject: record = own kind + build id.
    let archive = derived(ArtifactKind::Archive, &[("id", "keep")]);
    assert_eq!(
        subject_verdict_record(archive.kind, &archive.metadata),
        (Some("archive".to_string()), Some("keep".to_string()))
    );
    // Derived subject WITH a record: the record is copied, not the
    // subject's own kind — a sig of an SBOM of an archive answers to
    // the archive.
    let sbom = derived(
        ArtifactKind::Sbom,
        &[("subject_kind", "archive"), ("id", "keep")],
    );
    assert_eq!(
        subject_verdict_record(sbom.kind, &sbom.metadata),
        (Some("archive".to_string()), Some("keep".to_string()))
    );
    // Derived subject WITHOUT a record (project-wide `any` SBOM): the
    // absence propagates, inheriting the always-pass verdict.
    let any_sbom = derived(ArtifactKind::Sbom, &[("sbom_id", "default")]);
    assert_eq!(
        subject_verdict_record(any_sbom.kind, &any_sbom.metadata),
        (None, None)
    );
}

#[test]
fn id_filter_record_naming_derived_kind_passes() {
    // Only artifacts written before transitive recording can carry a
    // record that names a derived kind; it holds no terminal verdict,
    // and dropping a signature silently is worse than uploading one.
    let sig = derived(ArtifactKind::Signature, &[("subject_kind", "sbom")]);
    let ids = vec!["keep".to_string()];
    assert!(matches_id_filter(&sig, Some(&ids)));
}

#[test]
fn id_filter_sbom_inherits_subject_verdict() {
    let kept = derived(
        ArtifactKind::Sbom,
        &[("subject_kind", "archive"), ("id", "keep")],
    );
    let dropped = derived(
        ArtifactKind::Sbom,
        &[("subject_kind", "archive"), ("id", "drop")],
    );
    let ids = vec!["keep".to_string()];
    assert!(matches_id_filter(&kept, Some(&ids)));
    assert!(!matches_id_filter(&dropped, Some(&ids)));
}

#[test]
fn primary_subject_set_excludes_every_derived_sidecar() {
    // The HARD invariant: no Checksum/Signature/Certificate/Metadata is
    // ever a checksum or sign subject. If any leak in, the recursive
    // X.sha256.sig.sha256 chain becomes representable again.
    for set in [
        primary_subject_kinds(),
        checksummable_subject_kinds(),
        signable_subject_kinds(),
    ] {
        for &k in set {
            assert!(
                !is_derived_sidecar_kind(k),
                "derived sidecar {k:?} must never be a checksum/sign subject"
            );
        }
    }
}

#[test]
fn derived_sidecar_classification() {
    for k in [
        ArtifactKind::Checksum,
        ArtifactKind::Signature,
        ArtifactKind::Certificate,
        ArtifactKind::Metadata,
    ] {
        assert!(is_derived_sidecar_kind(k), "{k:?} is a derived sidecar");
    }
    // SBOM is a first-class catalog artifact, not a derived sidecar: it may
    // be checksummed/signed once without recursion.
    for k in [
        ArtifactKind::Sbom,
        ArtifactKind::Archive,
        ArtifactKind::UploadableBinary,
    ] {
        assert!(
            !is_derived_sidecar_kind(k),
            "{k:?} is NOT a derived sidecar"
        );
        assert!(primary_subject_kinds().contains(&k));
    }
}

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
fn add_collapses_same_path_same_kind_into_one_entry() {
    let mut registry = ArtifactRegistry::new();
    registry.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: "install.sh.sha256".to_string(),
        path: PathBuf::from("dist/install.sh.sha256"),
        target: None,
        crate_name: "anodizer".to_string(),
        metadata: HashMap::from([("algorithm".to_string(), "sha256".to_string())]),
        size: Some(64),
    });
    // Same path, no size yet, richer metadata: an idempotent re-add.
    registry.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: "install.sh.sha256".to_string(),
        path: PathBuf::from("dist/install.sh.sha256"),
        target: None,
        crate_name: "anodizer".to_string(),
        metadata: HashMap::from([
            ("algorithm".to_string(), "sha256".to_string()),
            ("ChecksumOf".to_string(), "dist/install.sh".to_string()),
        ]),
        size: None,
    });

    let checksums = registry.by_kind(ArtifactKind::Checksum);
    assert_eq!(
        checksums.len(),
        1,
        "a same-path+kind re-add must collapse to one entry, not duplicate"
    );
    let only = checksums[0];
    assert_eq!(
        only.size,
        Some(64),
        "a non-None size must survive an idempotent re-add"
    );
    assert_eq!(
        only.metadata.get("ChecksumOf").map(String::as_str),
        Some("dist/install.sh"),
        "the re-add's richer metadata must be merged in"
    );
    assert_eq!(
        only.metadata.get("algorithm").map(String::as_str),
        Some("sha256")
    );
}

#[test]
fn add_keeps_distinct_paths_and_distinct_kinds() {
    let mut registry = ArtifactRegistry::new();
    registry.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: "a.sha256".to_string(),
        path: PathBuf::from("dist/a.sha256"),
        target: None,
        crate_name: "anodizer".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    registry.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: "b.sha256".to_string(),
        path: PathBuf::from("dist/b.sha256"),
        target: None,
        crate_name: "anodizer".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    assert_eq!(
        registry.by_kind(ArtifactKind::Checksum).len(),
        2,
        "distinct paths must both remain"
    );
    // Same path, different (sidecar) kind: not collapsed.
    registry.add(Artifact {
        kind: ArtifactKind::Signature,
        name: "a.sha256".to_string(),
        path: PathBuf::from("dist/a.sha256"),
        target: None,
        crate_name: "anodizer".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    assert_eq!(
        registry.all().len(),
        3,
        "same path with a different kind must NOT be collapsed"
    );

    // PRIMARY kind, same path twice: collapse is sidecar-only, so a
    // duplicate primary artifact must NOT be swallowed — it stays a
    // distinct entry for `detect_duplicate_paths` to flag as a real
    // shard-overlap emission bug.
    for _ in 0..2 {
        registry.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "app.tar.gz".to_string(),
            path: PathBuf::from("dist/app.tar.gz"),
            target: None,
            crate_name: "anodizer".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
    }
    assert_eq!(
        registry.by_kind(ArtifactKind::Archive).len(),
        2,
        "a same-path PRIMARY kind must NOT collapse — the duplicate must \
         survive for the path guard to catch"
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
/// Normalizes the artifact path relative to the working directory.
#[test]
#[serial_test::serial]
fn to_artifacts_json_strips_absolute_worktree_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    // RAII: restores the original cwd on drop even if the body panics, so a
    // peer test reading the process-global cwd never observes the swap.
    let _cwd = crate::test_helpers::CwdGuard::new(tmp.path()).unwrap();
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
    // The release-uploadable artifact kinds plus the
    // four installer kinds anodizer ships as OSS:
    //   - Installer       <- MSI / NSIS
    //   - DiskImage       <- DMG
    //   - MacOsPackage    <- PKG
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
    ];
    assert_eq!(kinds, &expected);
}

#[test]
fn artifact_ext_prefers_metadata_when_present() {
    // `Artifact.Ext()` reads the `ext` extra,
    // not the filename. An SRPM
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

#[test]
fn contains_path_kind_matches_on_path_and_kind_only() {
    let mut registry = ArtifactRegistry::new();
    registry.add(upload_artifact(
        ArtifactKind::UploadableFile,
        "attestation.intoto.jsonl",
        "dist/attestation.intoto.jsonl",
    ));

    // Exact (path, kind): present.
    assert!(registry.contains_path_kind(
        std::path::Path::new("dist/attestation.intoto.jsonl"),
        ArtifactKind::UploadableFile,
    ));
    // Same path, different kind: a real distinct asset — not present, so a
    // re-registration there is never wrongly skipped.
    assert!(!registry.contains_path_kind(
        std::path::Path::new("dist/attestation.intoto.jsonl"),
        ArtifactKind::Metadata,
    ));
    // Different path, same kind: a genuine conflict the skip must NOT
    // swallow — falls through to add()'s duplicate-name warning.
    assert!(!registry.contains_path_kind(
        std::path::Path::new("dist/other/attestation.intoto.jsonl"),
        ArtifactKind::UploadableFile,
    ));
}

#[test]
fn contains_path_kind_normalizes_separators_like_add() {
    let mut registry = ArtifactRegistry::new();
    registry.add(upload_artifact(
        ArtifactKind::UploadableFile,
        "stmt.jsonl",
        r"dist\stmt.jsonl",
    ));
    // add() stored the forward-slash form; a backslash query must still hit.
    assert!(registry.contains_path_kind(
        std::path::Path::new(r"dist\stmt.jsonl"),
        ArtifactKind::UploadableFile,
    ));
}

#[test]
fn third_registration_warns_even_when_it_matches_the_first_path() {
    let captured = capture_warnings(|| {
        let mut registry = ArtifactRegistry::new();
        // Path A, then a conflicting path B, then path A again. The
        // third add re-uses A — a first-match-only check would compare
        // against A, see equal paths, and miss the live conflict with B.
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
        registry.add(upload_artifact(
            ArtifactKind::Archive,
            "app.tar.gz",
            "dist/app.tar.gz",
        ));
    });
    // Two conflict warnings expected: the B-vs-A add, and the final
    // A-add that still conflicts with the registered B entry.
    let hits = captured.matches("already registered").count();
    assert!(
        hits >= 2,
        "third add must still warn against the differing-path entry; \
         got {hits} warning(s): {captured:?}"
    );
}
