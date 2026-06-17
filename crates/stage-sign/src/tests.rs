use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::SignConfig;
use anodizer_core::stage::Stage;
use anodizer_core::test_helpers::TestContextBuilder;

use super::helpers::{
    collapse_doubled_digest, pin_image_ref_to_digest, prepare_stdin_from, resolve_sign_args,
    resolve_signature_path, should_sign_artifact,
};
use super::process::{ArtifactFilter, process_sign_configs};
use super::{DockerSignStage, SignStage};

/// Readability alias for the kind-filter predicate at call sites.
fn should_sign(kind: ArtifactKind, filter: &str) -> anyhow::Result<bool> {
    should_sign_artifact(kind, filter)
}

/// Metadata marking a Checksum artifact as the combined `checksums.txt`,
/// used by stage-level fixtures that register a combined checksum file.
fn combined_meta() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::from([(
        anodizer_core::artifact::COMBINED_CHECKSUM_META.to_string(),
        anodizer_core::artifact::COMBINED_CHECKSUM_VALUE.to_string(),
    )])
}

/// Return a shell command + args that writes `content_expr` to `dest_file`.
/// On Unix: sh -c "echo $VAR > file"
/// On Windows: cmd.exe /C "echo %VAR% > file"
fn shell_echo_to_file(env_var: &str, dest_file: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "cmd.exe".to_string(),
            vec![
                "/C".to_string(),
                format!("echo %{}% > {}", env_var, dest_file),
            ],
        )
    } else {
        (
            "sh".to_string(),
            vec![
                "-c".to_string(),
                format!("echo ${} > {}", env_var, dest_file),
            ],
        )
    }
}

/// Return a shell command + args that writes a literal string to `dest_file`.
fn shell_echo_literal_to_file(literal: &str, dest_file: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "cmd.exe".to_string(),
            vec![
                "/C".to_string(),
                format!("echo {} > {}", literal, dest_file),
            ],
        )
    } else {
        (
            "sh".to_string(),
            vec![
                "-c".to_string(),
                format!("echo \"{}\" > {}", literal, dest_file),
            ],
        )
    }
}

/// Return (cmd, args) for a simple echo command (no shell).
fn echo_command() -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "cmd.exe".to_string(),
            vec!["/C".to_string(), "echo".to_string()],
        )
    } else {
        ("echo".to_string(), vec![])
    }
}

#[test]
fn test_resolve_sign_args() {
    let args = vec![
        "--output".to_string(),
        "{{ .Signature }}".to_string(),
        "--detach-sign".to_string(),
        "{{ .Artifact }}".to_string(),
    ];
    let resolved = resolve_sign_args(&args, "/tmp/file.tar.gz", "/tmp/file.tar.gz.sig", None);
    assert_eq!(resolved[1], "/tmp/file.tar.gz.sig");
    assert_eq!(resolved[3], "/tmp/file.tar.gz");
}

#[test]
fn pin_image_ref_to_digest_appends_digest() {
    // A bare tag becomes a digest-pinned reference cosign can sign safely.
    assert_eq!(
        pin_image_ref_to_digest("ghcr.io/org/app:0.9.0", "sha256:abc123"),
        "ghcr.io/org/app:0.9.0@sha256:abc123"
    );
}

#[test]
fn pin_image_ref_to_digest_is_idempotent() {
    // Feeding an already-pinned ref must not double the digest.
    assert_eq!(
        pin_image_ref_to_digest("ghcr.io/org/app:0.9.0@sha256:abc123", "sha256:abc123"),
        "ghcr.io/org/app:0.9.0@sha256:abc123"
    );
    // Re-pinning swaps to the freshly-resolved digest (the build stage's
    // value is authoritative).
    assert_eq!(
        pin_image_ref_to_digest("ghcr.io/org/app:0.9.0@sha256:stale", "sha256:fresh"),
        "ghcr.io/org/app:0.9.0@sha256:fresh"
    );
}

#[test]
fn pin_image_ref_to_digest_without_digest_returns_bare_ref() {
    // No digest captured → leave the ref unpinned (caller warns); never
    // fabricate a digest.
    assert_eq!(
        pin_image_ref_to_digest("ghcr.io/org/app:latest", ""),
        "ghcr.io/org/app:latest"
    );
}

#[test]
fn pin_image_ref_to_digest_handles_port_registry_and_no_tag() {
    // A `host:port/repo:tag` reference: the PORT colon must not be mistaken
    // for a tag/digest boundary — strip_digest_suffix splits on the last `@`
    // (absent here), so the whole ref survives and the digest appends after.
    assert_eq!(
        pin_image_ref_to_digest("localhost:5000/org/app:0.9.0", "sha256:abc"),
        "localhost:5000/org/app:0.9.0@sha256:abc"
    );
    // A digest-only reference (no tag) pins cleanly.
    assert_eq!(
        pin_image_ref_to_digest("ghcr.io/org/app", "sha256:abc"),
        "ghcr.io/org/app@sha256:abc"
    );
    // Port registry that was already pinned re-pins without confusing the
    // port colon for the digest delimiter.
    assert_eq!(
        pin_image_ref_to_digest("localhost:5000/org/app:0.9.0@sha256:old", "sha256:new"),
        "localhost:5000/org/app:0.9.0@sha256:new"
    );
}

#[test]
fn collapse_doubled_digest_removes_one_pin() {
    // The historical default `{{ .Artifact }}@{{ .Digest }}` with a pinned
    // Artifact yields a doubled pin; collapse to a single valid reference.
    assert_eq!(
        collapse_doubled_digest("ghcr.io/org/app:0.9.0@sha256:abc@sha256:abc"),
        "ghcr.io/org/app:0.9.0@sha256:abc"
    );
    // A single pin is untouched.
    assert_eq!(
        collapse_doubled_digest("ghcr.io/org/app:0.9.0@sha256:abc"),
        "ghcr.io/org/app:0.9.0@sha256:abc"
    );
    // Two DIFFERENT trailing tokens are left alone (not a self-doubling).
    assert_eq!(
        collapse_doubled_digest("ghcr.io/org/app:0.9.0@sha256:abc@sha256:def"),
        "ghcr.io/org/app:0.9.0@sha256:abc@sha256:def"
    );
}

#[test]
fn test_filter_artifacts_checksum() {
    // GoReleaser parity (internal/pipe/sign/sign.go:93-94):
    // `artifacts: checksum` -> artifact.ByType(artifact.Checksum) — EVERY
    // Checksum, the combined `checksums.txt` AND each per-artifact split
    // `.sha256` sidecar. A checksum's metadata is irrelevant to the match.
    assert!(
        should_sign(ArtifactKind::Checksum, "checksum").unwrap(),
        "every Checksum kind is signed by artifacts: checksum (GoReleaser parity)"
    );
    assert!(!should_sign(ArtifactKind::Archive, "checksum").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "checksum").unwrap());
    assert!(!should_sign(ArtifactKind::Signature, "checksum").unwrap());
}

#[test]
fn test_filter_artifacts_all() {
    // "all" matches anodizer_core::artifact::signable_subject_kinds() (the
    // PRIMARY subjects) PLUS every Checksum (GoReleaser parity:
    // ReleaseUploadableTypes() minus only Signature/Certificate;
    // internal/pipe/sign/sign.go:103-108). Installer-family kinds (MSI/NSIS as
    // Installer, DMG as DiskImage, PKG as MacOsPackage) are primary so they are
    // signed alongside archives. Dedicated filters (`installer`, `diskimage`,
    // `macos_package`) remain available for finer-grain selection.
    //
    // Every Checksum IS signed by `all` — combined `checksums.txt` AND split
    // `.sha256` sidecars alike (→ one `X.sha256.sig` each). This cannot recurse
    // into `X.sha256.sig.sha256`: the checksum stage's subject set is primary-
    // only and `refresh_combined_checksums` skips derived sidecars, so the
    // produced `.sig` is never re-hashed.
    assert!(
        should_sign(ArtifactKind::Checksum, "all").unwrap(),
        "every Checksum is signed by artifacts: all (GoReleaser parity)"
    );
    assert!(should_sign(ArtifactKind::Archive, "all").unwrap());
    assert!(should_sign(ArtifactKind::UploadableBinary, "all").unwrap());
    assert!(should_sign(ArtifactKind::LinuxPackage, "all").unwrap());
    assert!(should_sign(ArtifactKind::SourceArchive, "all").unwrap());
    assert!(should_sign(ArtifactKind::Makeself, "all").unwrap());
    assert!(should_sign(ArtifactKind::Flatpak, "all").unwrap());
    assert!(should_sign(ArtifactKind::Sbom, "all").unwrap());
    assert!(should_sign(ArtifactKind::SourceRpm, "all").unwrap());
    assert!(should_sign(ArtifactKind::UploadableFile, "all").unwrap());
    assert!(should_sign(ArtifactKind::Installer, "all").unwrap());
    assert!(should_sign(ArtifactKind::DiskImage, "all").unwrap());
    assert!(should_sign(ArtifactKind::MacOsPackage, "all").unwrap());

    // Signature + Certificate are excluded from the `all` filter (the
    // 87a55ea / #6509). Although both kinds are otherwise release-uploadable,
    // signing them on a re-run of the stage would produce `.sig.sig` /
    // `.pem.sig` chains and corrupt checksums. See
    // `re_sign_idempotency_does_not_chain_signatures` below.
    assert!(!should_sign(ArtifactKind::Signature, "all").unwrap());
    assert!(!should_sign(ArtifactKind::Certificate, "all").unwrap());

    // Kinds that are not primary subjects — users must opt in via the
    // dedicated `binary` / `snap` filters.
    assert!(!should_sign(ArtifactKind::Binary, "all").unwrap());
    assert!(!should_sign(ArtifactKind::Snap, "all").unwrap());

    // Internal / metadata types — never signed.
    assert!(!should_sign(ArtifactKind::DockerImage, "all").unwrap());
    assert!(!should_sign(ArtifactKind::DockerManifest, "all").unwrap());
    assert!(!should_sign(ArtifactKind::BrewFormula, "all").unwrap());
    assert!(!should_sign(ArtifactKind::Metadata, "all").unwrap());
}

#[test]
fn test_filter_artifacts_any_alias() {
    // "any" is an alias for "all"
    assert!(should_sign(ArtifactKind::Archive, "any").unwrap());
    assert!(should_sign(ArtifactKind::UploadableBinary, "any").unwrap());
    // Signature + Certificate excluded from `any` (alias of `all`) — see
    // `re_sign_idempotency_does_not_chain_signatures`.
    assert!(!should_sign(ArtifactKind::Signature, "any").unwrap());
    assert!(!should_sign(ArtifactKind::Certificate, "any").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "any").unwrap());
    assert!(!should_sign(ArtifactKind::DockerImage, "any").unwrap());
}

#[test]
fn test_filter_artifacts_none() {
    assert!(!should_sign(ArtifactKind::Checksum, "none").unwrap());
}

#[test]
fn all_and_checksum_filters_sign_every_checksum_kind_without_recursion() {
    // GoReleaser parity: BOTH `artifacts: all` and `artifacts: checksum` sign
    // EVERY Checksum — the combined `checksums.txt` AND each per-artifact split
    // `.sha256` sidecar (internal/pipe/sign/sign.go:93-94, 103-108) — yielding
    // one legitimate `X.sha256.sig` each. `all` additionally signs primaries
    // (the archive); `checksum` does not. NEITHER produces a forbidden
    // `(.sha256|.sig)` re-derivation chain: the produced `.sig` is never
    // re-checksummed (checksum input is primary-only) nor re-signed (Signature
    // is not in the sign set).
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::has_recursive_sidecar_chain;

    let gpg_sign = |filter: &str| SignConfig {
        id: Some("gpg".to_string()),
        cmd: Some("gpg".to_string()),
        args: Some(vec![
            "--output".to_string(),
            "{{ .Signature }}".to_string(),
            "--detach-sign".to_string(),
            "{{ .Artifact }}".to_string(),
        ]),
        artifacts: Some(filter.to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };

    // (filter, archive should be signed?)
    for (filter, archive_signed) in [("all", true), ("checksum", false)] {
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![gpg_sign(filter)])
            .build();

        let add = |ctx: &mut anodizer_core::context::Context,
                   kind: ArtifactKind,
                   name: &str,
                   meta: std::collections::HashMap<String, String>| {
            ctx.artifacts.add(Artifact {
                kind,
                name: name.to_string(),
                path: std::path::PathBuf::from(format!("/tmp/{name}")),
                target: None,
                crate_name: "myapp".to_string(),
                metadata: meta,
                size: None,
            });
        };

        add(
            &mut ctx,
            ArtifactKind::Archive,
            "myapp.tar.gz",
            Default::default(),
        );
        // Combined checksums file.
        add(
            &mut ctx,
            ArtifactKind::Checksum,
            "myapp_checksums.txt",
            combined_meta(),
        );
        // Split per-artifact checksum sidecar.
        add(
            &mut ctx,
            ArtifactKind::Checksum,
            "myapp.tar.gz.sha256",
            Default::default(),
        );

        SignStage.run(&mut ctx).unwrap();

        let sig_names: Vec<String> = ctx
            .artifacts
            .by_kind(ArtifactKind::Signature)
            .into_iter()
            .map(|a| a.name.clone())
            .collect();

        // Both the combined file AND the split sidecar are signed (GR parity).
        assert!(
            sig_names.iter().any(|n| n == "myapp_checksums.txt.sig"),
            "[{filter}] combined checksums file must be signed; got {sig_names:?}"
        );
        assert!(
            sig_names.iter().any(|n| n == "myapp.tar.gz.sha256.sig"),
            "[{filter}] split checksum sidecar must be signed (GR parity); got {sig_names:?}"
        );
        // The primary archive is signed only under `all`, never under `checksum`.
        assert_eq!(
            sig_names.iter().any(|n| n == "myapp.tar.gz.sig"),
            archive_signed,
            "[{filter}] archive signed == {archive_signed}; got {sig_names:?}"
        );

        // No registered artifact (subjects or produced signatures) may carry a
        // forbidden recursive chain. `myapp.tar.gz.sha256.sig` (a sig of a
        // checksum) is the GR-legit second level and is NOT a violation.
        for a in ctx.artifacts.all() {
            assert!(
                !has_recursive_sidecar_chain(&a.name),
                "[{filter}] forbidden recursive chain registered: {} (kind={:?})",
                a.name,
                a.kind
            );
        }
    }
}

/// Regression:
/// signing a previously-signed dist must not chain `*.sig.sig` /
/// `*.pem.sig` files. The `all` and `any` filters explicitly skip the
/// `Signature` and `Certificate` kinds even though both are otherwise
/// release-uploadable (signatures DO get uploaded to GitHub releases —
/// they're excluded from re-signing, not from upload).
#[test]
fn re_sign_idempotency_does_not_chain_signatures() {
    // The bug: on a re-run of the sign stage, prior `.sig` / `.pem`
    // artifacts already in the registry would match `artifacts: all` and
    // be fed back through the signing command, producing nested
    // signatures and corrupting `checksums.txt` (since checksums.txt.sig
    // would itself be signed).
    for filter in ["all", "any"] {
        assert!(
            !should_sign(ArtifactKind::Signature, filter).unwrap(),
            "Signature must be excluded from `{filter}` filter (#6509)"
        );
        assert!(
            !should_sign(ArtifactKind::Certificate, filter).unwrap(),
            "Certificate must be excluded from `{filter}` filter (#6509)"
        );
    }

    // Sanity: a normal release-uploadable kind (Archive) is still signed.
    assert!(should_sign(ArtifactKind::Archive, "all").unwrap());
    assert!(should_sign(ArtifactKind::Archive, "any").unwrap());
}

#[test]
fn test_filter_artifacts_archive() {
    assert!(should_sign(ArtifactKind::Archive, "archive").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "archive").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "archive").unwrap());
    assert!(!should_sign(ArtifactKind::LinuxPackage, "archive").unwrap());
}

#[test]
fn test_filter_artifacts_source() {
    // "source" matches SourceArchive (not Archive)
    assert!(should_sign(ArtifactKind::SourceArchive, "source").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "source").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "source").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "source").unwrap());
}

#[test]
fn test_filter_artifacts_binary() {
    assert!(should_sign(ArtifactKind::Binary, "binary").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "binary").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "binary").unwrap());
    assert!(!should_sign(ArtifactKind::LinuxPackage, "binary").unwrap());
}

#[test]
fn test_filter_artifacts_package() {
    assert!(should_sign(ArtifactKind::LinuxPackage, "package").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "package").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "package").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "package").unwrap());
}

#[test]
fn test_filter_artifacts_installer() {
    assert!(should_sign(ArtifactKind::Installer, "installer").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "installer").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "installer").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "installer").unwrap());
    assert!(!should_sign(ArtifactKind::LinuxPackage, "installer").unwrap());
}

#[test]
fn test_filter_artifacts_diskimage() {
    assert!(should_sign(ArtifactKind::DiskImage, "diskimage").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "diskimage").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "diskimage").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "diskimage").unwrap());
    assert!(!should_sign(ArtifactKind::Installer, "diskimage").unwrap());
}

#[test]
fn test_filter_artifacts_sbom() {
    assert!(should_sign(ArtifactKind::Sbom, "sbom").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "sbom").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "sbom").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "sbom").unwrap());
    assert!(!should_sign(ArtifactKind::LinuxPackage, "sbom").unwrap());
}

#[test]
fn test_filter_artifacts_snap() {
    assert!(should_sign(ArtifactKind::Snap, "snap").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "snap").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "snap").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "snap").unwrap());
    assert!(!should_sign(ArtifactKind::LinuxPackage, "snap").unwrap());
}

#[test]
fn test_filter_artifacts_macos_package() {
    assert!(should_sign(ArtifactKind::MacOsPackage, "macos_package").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "macos_package").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "macos_package").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "macos_package").unwrap());
    assert!(!should_sign(ArtifactKind::Installer, "macos_package").unwrap());
}

#[test]
fn test_stage_skips_without_sign_config() {
    let mut ctx = TestContextBuilder::new().build();
    let stage = SignStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_stage_skips_with_empty_signs() {
    let mut ctx = TestContextBuilder::new().signs(vec![]).build();
    let stage = SignStage;
    assert!(stage.run(&mut ctx).is_ok());
}

// -----------------------------------------------------------------------
// Additional behavior tests — config fields actually do things
// -----------------------------------------------------------------------

#[test]
fn test_multiple_sign_configs_run_independently() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    // Two sign configs targeting different artifact types
    let signs = vec![
        SignConfig {
            id: Some("gpg".to_string()),
            cmd: Some("echo".to_string()),
            args: Some(vec!["signing-archive".to_string()]),
            artifacts: Some("archive".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        },
        SignConfig {
            id: Some("cosign".to_string()),
            cmd: Some("echo".to_string()),
            args: Some(vec!["signing-checksum".to_string()]),
            artifacts: Some("checksum".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: None,
            certificate: None,
            output: None,
            if_condition: None,
        },
    ];

    let mut ctx = TestContextBuilder::new().dry_run(true).signs(signs).build();

    // Add artifacts of both types
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/app.tar.gz"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = SignStage;
    // Both configs should run independently without interfering
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_artifacts_filter_selects_correct_kinds() {
    // "all" = signable_subject_kinds() (primary subjects) PLUS every Checksum
    // (GoReleaser parity). Installer-family kinds (MSI/NSIS as Installer, DMG as
    // DiskImage, PKG as MacOsPackage) are all primary, so all three are signed
    // under "all".
    assert!(should_sign(ArtifactKind::Archive, "all").unwrap());
    assert!(should_sign(ArtifactKind::UploadableBinary, "all").unwrap());
    // Checksum IS signed under `all` (GR signs checksums); one `X.sha256.sig`.
    assert!(should_sign(ArtifactKind::Checksum, "all").unwrap());
    assert!(should_sign(ArtifactKind::LinuxPackage, "all").unwrap());
    assert!(should_sign(ArtifactKind::Sbom, "all").unwrap());
    assert!(should_sign(ArtifactKind::Installer, "all").unwrap());
    assert!(should_sign(ArtifactKind::DiskImage, "all").unwrap());
    assert!(should_sign(ArtifactKind::MacOsPackage, "all").unwrap());
    // Signature + Certificate + Metadata are excluded from `all` to prevent
    // recursive signing (never sign a sig).
    assert!(!should_sign(ArtifactKind::Signature, "all").unwrap());
    assert!(!should_sign(ArtifactKind::Certificate, "all").unwrap());

    // Kinds outside the primary subjects — use dedicated `binary` /
    // `snap` filters to opt in.
    assert!(!should_sign(ArtifactKind::Binary, "all").unwrap());
    assert!(!should_sign(ArtifactKind::Snap, "all").unwrap());

    // "all" does NOT match internal/non-uploadable types
    assert!(!should_sign(ArtifactKind::DockerImage, "all").unwrap());
    assert!(!should_sign(ArtifactKind::DockerManifest, "all").unwrap());
    assert!(!should_sign(ArtifactKind::BrewFormula, "all").unwrap());
    assert!(!should_sign(ArtifactKind::Metadata, "all").unwrap());
    assert!(!should_sign(ArtifactKind::ScoopManifest, "all").unwrap());
    assert!(!should_sign(ArtifactKind::KrewPluginManifest, "all").unwrap());

    // "none" matches nothing
    assert!(!should_sign(ArtifactKind::Archive, "none").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "none").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "none").unwrap());
    assert!(!should_sign(ArtifactKind::LinuxPackage, "none").unwrap());
    assert!(!should_sign(ArtifactKind::Installer, "none").unwrap());
    assert!(!should_sign(ArtifactKind::DiskImage, "none").unwrap());
    assert!(!should_sign(ArtifactKind::Sbom, "none").unwrap());
    assert!(!should_sign(ArtifactKind::Snap, "none").unwrap());
    assert!(!should_sign(ArtifactKind::MacOsPackage, "none").unwrap());

    // "archive" only matches Archive
    assert!(should_sign(ArtifactKind::Archive, "archive").unwrap());
    assert!(!should_sign(ArtifactKind::Binary, "archive").unwrap());
    assert!(!should_sign(ArtifactKind::Checksum, "archive").unwrap());

    // "binary" only matches Binary
    assert!(should_sign(ArtifactKind::Binary, "binary").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "binary").unwrap());

    // "package" only matches LinuxPackage
    assert!(should_sign(ArtifactKind::LinuxPackage, "package").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "package").unwrap());

    // "installer" only matches Installer
    assert!(should_sign(ArtifactKind::Installer, "installer").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "installer").unwrap());

    // "diskimage" only matches DiskImage
    assert!(should_sign(ArtifactKind::DiskImage, "diskimage").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "diskimage").unwrap());

    // "sbom" only matches Sbom
    assert!(should_sign(ArtifactKind::Sbom, "sbom").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "sbom").unwrap());

    // "snap" only matches Snap
    assert!(should_sign(ArtifactKind::Snap, "snap").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "snap").unwrap());

    // "macos_package" only matches MacOsPackage
    assert!(should_sign(ArtifactKind::MacOsPackage, "macos_package").unwrap());
    assert!(!should_sign(ArtifactKind::Archive, "macos_package").unwrap());

    // Unknown filter returns an error
    assert!(should_sign(ArtifactKind::Checksum, "unknown-value").is_err());
}

#[test]
fn test_ids_filter_restricts_signed_artifacts() {
    // Verify the ids filter logic directly by testing should_sign_artifact
    // combined with the ids-based metadata check that the stage performs.
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let sign_cfg = SignConfig {
        id: Some("gpg".to_string()),
        cmd: Some("echo".to_string()),
        args: Some(vec!["sign".to_string()]),
        artifacts: Some("archive".to_string()),
        ids: Some(vec!["linux-release".to_string()]),
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };

    let filter = sign_cfg.artifacts.as_deref().unwrap_or("none");

    // Build test artifacts
    let matching_artifact = Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/linux.tar.gz"),
        target: None,
        crate_name: "test".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), "linux-release".to_string());
            m
        },
        size: None,
    };

    let non_matching_artifact = Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/darwin.tar.gz"),
        target: None,
        crate_name: "test".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), "darwin-release".to_string());
            m
        },
        size: None,
    };

    let no_id_artifact = Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/other.tar.gz"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    };

    let wrong_kind_artifact = Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/binary"),
        target: None,
        crate_name: "test".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), "linux-release".to_string());
            m
        },
        size: None,
    };

    // Replicate the stage's filtering logic:
    // 1. should_sign_artifact(kind, filter) must be true
    // 2. If ids is set, artifact metadata "id" or "name" must match
    let ids = &sign_cfg.ids;
    let should_sign = |a: &Artifact| -> bool {
        if !should_sign_artifact(a.kind, filter).unwrap() {
            return false;
        }
        if let Some(id_list) = ids {
            let matches_id = a
                .metadata
                .get("id")
                .map(|id| id_list.contains(id))
                .unwrap_or(false);
            let matches_name = a
                .metadata
                .get("name")
                .map(|name| id_list.contains(name))
                .unwrap_or(false);
            return matches_id || matches_name;
        }
        true
    };

    assert!(
        should_sign(&matching_artifact),
        "archive with matching id 'linux-release' should be signed"
    );
    assert!(
        !should_sign(&non_matching_artifact),
        "archive with non-matching id 'darwin-release' should NOT be signed"
    );
    assert!(
        !should_sign(&no_id_artifact),
        "archive with no id metadata should NOT be signed when ids filter is set"
    );
    assert!(
        !should_sign(&wrong_kind_artifact),
        "binary with matching id should NOT be signed when filter is 'archive'"
    );

    // Also run through the stage in dry-run to confirm it completes
    let mut ctx = TestContextBuilder::new()
        .dry_run(true)
        .signs(vec![sign_cfg])
        .build();
    ctx.artifacts.add(matching_artifact);
    ctx.artifacts.add(non_matching_artifact);
    ctx.artifacts.add(no_id_artifact);
    ctx.artifacts.add(wrong_kind_artifact);

    let stage = SignStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_dry_run_logs_without_executing() {
    // The critical assertion: a nonexistent binary in dry-run mode must NOT
    // cause an error. If the stage tried to actually execute the binary,
    // it would fail because /nonexistent/gpg does not exist.
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let signs = vec![SignConfig {
        id: Some("gpg".to_string()),
        cmd: Some("/nonexistent/binary/that/does/not/exist".to_string()),
        args: Some(vec![
            "--output".to_string(),
            "{{ .Signature }}".to_string(),
            "--detach-sign".to_string(),
            "{{ .Artifact }}".to_string(),
        ]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    }];

    let mut ctx = TestContextBuilder::new()
        .dry_run(true)
        .signs(signs.clone())
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = SignStage;
    // This MUST succeed. If dry-run mode were broken and tried to spawn
    // the nonexistent binary, it would return an error.
    let result = stage.run(&mut ctx);
    assert!(
        result.is_ok(),
        "dry-run must not execute the signing binary; got error: {:?}",
        result.err()
    );

    // Now verify that WITHOUT dry-run, the same config WOULD fail,
    // proving that dry-run is what prevents execution.
    let mut ctx_no_dry = TestContextBuilder::new()
        .dry_run(false)
        .signs(signs)
        .build();

    ctx_no_dry.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: combined_meta(),
        size: None,
    });

    let result_no_dry = stage.run(&mut ctx_no_dry);
    assert!(
        result_no_dry.is_err(),
        "without dry-run, a nonexistent binary should cause an error"
    );
}

#[test]
fn test_template_variables_in_args_resolve_correctly() {
    let args = vec![
        "--output".to_string(),
        "{{ .Signature }}".to_string(),
        "--detach-sign".to_string(),
        "{{ .Artifact }}".to_string(),
        "--extra={{ .Artifact }}.meta".to_string(),
    ];

    let resolved = resolve_sign_args(&args, "/tmp/file.tar.gz", "/tmp/file.tar.gz.sig", None);
    assert_eq!(resolved[0], "--output");
    assert_eq!(resolved[1], "/tmp/file.tar.gz.sig");
    assert_eq!(resolved[2], "--detach-sign");
    assert_eq!(resolved[3], "/tmp/file.tar.gz");
    assert_eq!(resolved[4], "--extra=/tmp/file.tar.gz.meta");
}

#[test]
fn test_sign_none_filter_skips_entirely() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let signs = vec![SignConfig {
        id: Some("skip".to_string()),
        cmd: Some("false".to_string()), // Would fail if executed
        args: None,
        artifacts: Some("none".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    }];

    let mut ctx = TestContextBuilder::new().signs(signs).build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/file.tar.gz"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = SignStage;
    // "none" filter should skip without executing any command
    assert!(stage.run(&mut ctx).is_ok());

    // SkipMemento should record the (sign, skip, "artifacts: none") tuple
    // so the end-of-pipeline summary can surface it.
    let events = ctx.skip_memento.snapshot();
    assert_eq!(events.len(), 1, "expected one recorded skip");
    assert_eq!(events[0].stage, "sign");
    assert_eq!(events[0].label, "skip");
    assert_eq!(events[0].reason, "artifacts: none");
}

#[test]
fn test_sign_if_false_records_skip_memento() {
    // A sign config with `if: "false"` must not execute AND must leave a
    // memento entry so operators can tell an intentionally-disabled sign
    // config apart from a misconfigured one in the pipeline summary.
    let signs = vec![SignConfig {
        id: Some("gated".to_string()),
        cmd: Some("false".to_string()),
        args: None,
        artifacts: Some("archive".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: Some("false".to_string()),
    }];

    let mut ctx = TestContextBuilder::new().signs(signs).build();
    let stage = SignStage;
    assert!(stage.run(&mut ctx).is_ok());

    let events = ctx.skip_memento.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].stage, "sign");
    assert_eq!(events[0].label, "gated");
    assert!(
        events[0].reason.contains("`if` condition evaluated falsy"),
        "unexpected reason: {}",
        events[0].reason
    );
}

#[test]
fn test_sign_positional_label_when_id_missing() {
    // A sign config without an id should get a positional label of the
    // form `<stage-label>[N]` in the skip summary so users can still
    // find it in their config.
    let signs = vec![SignConfig {
        id: None,
        cmd: Some("false".to_string()),
        args: None,
        artifacts: Some("none".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    }];

    let mut ctx = TestContextBuilder::new().signs(signs).build();
    let stage = SignStage;
    assert!(stage.run(&mut ctx).is_ok());

    let events = ctx.skip_memento.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].label, "sign[0]");
}

// ---- Error path tests: missing tools / bad inputs ----

#[test]
fn test_missing_signing_binary_errors_with_command_name() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let signs = vec![SignConfig {
        id: Some("test".to_string()),
        cmd: Some("/nonexistent/path/to/gpg-that-does-not-exist".to_string()),
        args: Some(vec![
            "--output".to_string(),
            "{{ .Signature }}".to_string(),
            "--detach-sign".to_string(),
            "{{ .Artifact }}".to_string(),
        ]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    }];

    let mut ctx = TestContextBuilder::new()
        .dry_run(false)
        .signs(signs)
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: combined_meta(),
        size: None,
    });

    let stage = SignStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err(), "missing signing binary should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("gpg-that-does-not-exist") || err.contains("spawn"),
        "error should mention the missing command, got: {err}"
    );
}

#[test]
fn test_signing_command_nonzero_exit_errors_with_details() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let signs = vec![SignConfig {
        id: Some("test".to_string()),
        cmd: Some("false".to_string()), // always exits with code 1
        args: Some(vec![]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    }];

    let mut ctx = TestContextBuilder::new()
        .dry_run(false)
        .signs(signs)
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/test.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: combined_meta(),
        size: None,
    });

    let stage = SignStage;
    let result = stage.run(&mut ctx);
    assert!(
        result.is_err(),
        "signing command returning non-zero should fail"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("non-zero") || err.contains("false"),
        "error should mention non-zero exit or command name, got: {err}"
    );
}

#[test]
fn test_resolve_sign_args_no_placeholders() {
    let args = vec!["--armor".to_string(), "--verbose".to_string()];
    let resolved = resolve_sign_args(&args, "/tmp/file", "/tmp/file.sig", None);
    assert_eq!(
        resolved, args,
        "args without placeholders should be unchanged"
    );
}

#[test]
fn test_resolve_sign_args_both_placeholders_in_single_arg() {
    let args = vec!["{{ .Artifact }}:{{ .Signature }}".to_string()];
    let resolved = resolve_sign_args(&args, "/tmp/f", "/tmp/f.sig", None);
    assert_eq!(resolved[0], "/tmp/f:/tmp/f.sig");
}

#[test]
fn test_stdin_file_missing_errors_with_path() {
    let sign_cfg = SignConfig {
        id: None,
        cmd: None,
        args: None,
        artifacts: None,
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: Some("/nonexistent/stdin_file.txt".to_string()),
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };

    let result = prepare_stdin_from(
        sign_cfg.stdin.as_deref(),
        sign_cfg.stdin_file.as_deref(),
        "sign",
    );
    assert!(
        result.is_err(),
        "missing stdin_file should produce an error"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("/nonexistent/stdin_file.txt") || err.contains("stdin_file"),
        "error should mention the missing stdin_file path, got: {err}"
    );
}

// ---- new field tests: env, certificate, docker sign ids/stdin ----

#[test]
fn test_sign_env_config_parsing() {
    let yaml = r#"
cmd: "cosign"
env:
  - COSIGN_EXPERIMENTAL=1
  - MY_KEY=my_value
"#;
    let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let env = cfg.env.unwrap();
    assert_eq!(env, vec!["COSIGN_EXPERIMENTAL=1", "MY_KEY=my_value"]);
}

#[test]
fn test_sign_certificate_config_parsing() {
    let yaml = r#"
cmd: "cosign"
certificate: "{{ .ProjectName }}-{{ .Tag }}.pem"
"#;
    let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(
        cfg.certificate.as_deref(),
        Some("{{ .ProjectName }}-{{ .Tag }}.pem")
    );
}

#[test]
fn test_docker_sign_ids_config_parsing() {
    let yaml = r#"
cmd: "cosign"
ids:
  - "my-docker-image"
  - "another-image"
"#;
    let cfg: anodizer_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let ids = cfg.ids.unwrap();
    assert_eq!(ids, vec!["my-docker-image", "another-image"]);
}

#[test]
fn test_docker_sign_stdin_config_parsing() {
    let yaml = r#"
cmd: "cosign"
stdin: "my-password"
"#;
    let cfg: anodizer_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.stdin.as_deref(), Some("my-password"));
}

#[test]
fn test_docker_sign_stdin_file_config_parsing() {
    let yaml = r#"
cmd: "cosign"
stdin_file: "/path/to/password"
"#;
    let cfg: anodizer_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.stdin_file.as_deref(), Some("/path/to/password"));
}

#[test]
fn test_sign_env_vars_passed_to_command() {
    // Verify that custom env vars reach the signing command.
    // Use `sh -c` to write the env var value to a file so we can verify it.
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let tmp = tempfile::TempDir::new().unwrap();
    let marker_path = tmp.path().join("env_check.txt");
    let marker_str = marker_path.to_string_lossy().to_string();

    let (cmd, args) = shell_echo_to_file("ANODIZER_TEST_SIGN_ENV", &marker_str);
    let signs = vec![SignConfig {
        id: Some("test-env".to_string()),
        cmd: Some(cmd),
        args: Some(args),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: Some(vec!["ANODIZER_TEST_SIGN_ENV=hello_from_sign".to_string()]),
        certificate: None,
        output: None,
        if_condition: None,
    }];

    // Create a real artifact file so the command runs
    let artifact_path = tmp.path().join("checksums.sha256");
    std::fs::write(&artifact_path, b"checksum content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .dry_run(false)
        .signs(signs)
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: artifact_path,
        target: None,
        crate_name: "test".to_string(),
        metadata: combined_meta(),
        size: None,
    });

    let stage = SignStage;
    let result = stage.run(&mut ctx);
    assert!(
        result.is_ok(),
        "sign with custom env vars should succeed; got: {:?}",
        result.err()
    );

    // Verify the env var was actually passed to the child process
    let env_output = std::fs::read_to_string(&marker_path).unwrap_or_else(|e| {
        panic!("marker file should exist — env var was written by signing command: {e}")
    });
    assert_eq!(
        env_output.trim(),
        "hello_from_sign",
        "ANODIZER_TEST_SIGN_ENV should have been passed to the signing command"
    );
}

#[test]
fn test_docker_sign_ids_filter() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    let docker_signs = vec![DockerSignConfig {
        cmd: Some("echo".to_string()),
        args: Some(vec!["sign".to_string(), "{{ .Artifact }}".to_string()]),
        artifacts: Some("all".to_string()),
        ids: Some(vec!["prod-image".to_string()]),
        stdin: None,
        stdin_file: None,
        id: None,
        env: None,
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    }];

    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.config.docker_signs = Some(docker_signs);

    // Add docker images: one matching, one not
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        path: std::path::PathBuf::from("ghcr.io/myorg/prod:latest"),
        target: None,
        crate_name: "test".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), "prod-image".to_string());
            m
        },
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        path: std::path::PathBuf::from("ghcr.io/myorg/dev:latest"),
        target: None,
        crate_name: "test".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), "dev-image".to_string());
            m
        },
        size: None,
    });

    let stage = SignStage;
    // Should succeed (dry-run). The ids filter restricts to prod-image only.
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_certificate_template_resolves_in_args() {
    // Test that {{ .Certificate }} placeholder in args gets resolved
    let args = vec![
        "sign".to_string(),
        "--certificate".to_string(),
        "{{ .Certificate }}".to_string(),
        "{{ .Artifact }}".to_string(),
    ];
    let resolved = resolve_sign_args(
        &args,
        "/tmp/app.tar.gz",
        "/tmp/app.tar.gz.sig",
        Some("/tmp/app.pem"),
    );
    assert_eq!(resolved[2], "/tmp/app.pem");
    assert_eq!(resolved[3], "/tmp/app.tar.gz");
}

#[test]
fn test_certificate_template_none_clears_placeholder() {
    // When certificate is None, {{ .Certificate }} is replaced with empty string
    // to prevent it from being fed to Tera and causing spurious warnings.
    let args = vec!["--cert={{ .Certificate }}".to_string()];
    let resolved = resolve_sign_args(&args, "/tmp/f", "/tmp/f.sig", None);
    assert_eq!(
        resolved[0], "--cert=",
        "placeholder should be replaced with empty string when certificate is None"
    );
}

#[test]
fn test_sign_with_certificate_dry_run() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let signs = vec![SignConfig {
        id: Some("cosign".to_string()),
        cmd: Some("cosign".to_string()),
        args: Some(vec![
            "sign-blob".to_string(),
            "--certificate".to_string(),
            "{{ .Certificate }}".to_string(),
            "--output-signature".to_string(),
            "{{ .Signature }}".to_string(),
            "{{ .Artifact }}".to_string(),
        ]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: Some("{{ .Artifact }}.pem".to_string()),
        output: None,
        if_condition: None,
    }];

    let mut ctx = TestContextBuilder::new().dry_run(true).signs(signs).build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = SignStage;
    assert!(
        stage.run(&mut ctx).is_ok(),
        "dry-run with certificate template should succeed"
    );
}

#[test]
fn test_prepare_stdin_from_content() {
    let (_, data) = prepare_stdin_from(Some("my-password"), None, "docker-sign").unwrap();
    assert!(data.is_some());
    assert_eq!(data.unwrap(), b"my-password");
}

#[test]
fn test_prepare_stdin_from_file_missing() {
    let result = prepare_stdin_from(None, Some("/nonexistent/docker_stdin.txt"), "docker-sign");
    assert!(result.is_err());
}

#[test]
fn test_prepare_stdin_from_inherit() {
    let (_, data) = prepare_stdin_from(None, None, "docker-sign").unwrap();
    assert!(data.is_none());
}

#[test]
fn test_sign_stage_registers_signature_artifacts_dry_run() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let signs = vec![SignConfig {
        id: Some("gpg".to_string()),
        cmd: Some("gpg".to_string()),
        args: Some(vec![
            "--output".to_string(),
            "{{ .Signature }}".to_string(),
            "--detach-sign".to_string(),
            "{{ .Artifact }}".to_string(),
        ]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    }];

    let mut ctx = TestContextBuilder::new().dry_run(true).signs(signs).build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: combined_meta(),
        size: None,
    });

    let stage = SignStage;
    stage.run(&mut ctx).unwrap();

    // The signature artifact should be registered even in dry-run mode.
    let sig_artifacts = ctx.artifacts.by_kind(ArtifactKind::Signature);
    assert_eq!(
        sig_artifacts.len(),
        1,
        "should register one signature artifact"
    );
    let sig = &sig_artifacts[0];
    assert_eq!(sig.metadata.get("type").unwrap(), "Signature");
    assert_eq!(sig.crate_name, "myapp");
}

#[test]
fn test_sign_stage_registers_certificate_artifacts_dry_run() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let signs = vec![SignConfig {
        id: Some("cosign".to_string()),
        cmd: Some("cosign".to_string()),
        args: Some(vec!["sign-blob".to_string(), "{{ .Artifact }}".to_string()]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: Some("{{ .Artifact }}.pem".to_string()),
        output: None,
        if_condition: None,
    }];

    let mut ctx = TestContextBuilder::new().dry_run(true).signs(signs).build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "myapp".to_string(),
        metadata: combined_meta(),
        size: None,
    });

    let stage = SignStage;
    stage.run(&mut ctx).unwrap();

    // Should register both a signature and a certificate artifact.
    let sig_artifacts = ctx.artifacts.by_kind(ArtifactKind::Signature);
    assert_eq!(
        sig_artifacts.len(),
        1,
        "should register one Signature artifact"
    );
    let cert_artifacts = ctx.artifacts.by_kind(ArtifactKind::Certificate);
    assert_eq!(
        cert_artifacts.len(),
        1,
        "should register one Certificate artifact"
    );
}

#[test]
fn test_docker_sign_id_config_parsing() {
    let yaml = r#"
id: "my-docker-signer"
cmd: "cosign"
"#;
    let cfg: anodizer_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.id.as_deref(), Some("my-docker-signer"));
}

#[test]
fn test_docker_sign_env_config_parsing() {
    let yaml = r#"
cmd: "cosign"
env:
  - COSIGN_EXPERIMENTAL=1
  - REGISTRY_TOKEN=secret
"#;
    let cfg: anodizer_core::config::DockerSignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    let env = cfg.env.unwrap();
    assert_eq!(env, vec!["COSIGN_EXPERIMENTAL=1", "REGISTRY_TOKEN=secret"]);
}

#[test]
fn test_docker_sign_env_vars_passed_to_command() {
    // Verify that custom env vars reach the docker signing command.
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    let tmp = tempfile::TempDir::new().unwrap();
    let marker_path = tmp.path().join("docker_env_check.txt");
    let marker_str = marker_path.to_string_lossy().to_string();

    let (cmd, args) = shell_echo_to_file("ANODIZER_TEST_DOCKER_ENV", &marker_str);
    let docker_signs = vec![DockerSignConfig {
        id: Some("test-env".to_string()),
        cmd: Some(cmd),
        args: Some(args),
        artifacts: Some("all".to_string()),
        ids: None,
        stdin: None,
        stdin_file: None,
        env: Some(vec!["ANODIZER_TEST_DOCKER_ENV=docker_hello".to_string()]),
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    }];

    let mut ctx = TestContextBuilder::new().dry_run(false).build();
    ctx.config.docker_signs = Some(docker_signs);

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        path: std::path::PathBuf::from("ghcr.io/test/app:latest"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = DockerSignStage;
    stage.run(&mut ctx).unwrap();

    let env_output = std::fs::read_to_string(&marker_path).unwrap();
    assert_eq!(
        env_output.trim(),
        "docker_hello",
        "ANODIZER_TEST_DOCKER_ENV should have been passed to the docker signing command"
    );
}

/// Run `DockerSignStage` with a `docker_signs` config whose command writes a
/// marker file when executed, under the given options, and assert the marker
/// was NEVER written — proving the (irreversible) cosign signature push was
/// skipped. The non-invocation oracle for the operator-selection gate.
fn assert_docker_sign_deselected_not_run(opts: anodizer_core::context::ContextOptions) {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    let tmp = tempfile::TempDir::new().unwrap();
    let marker_path = tmp.path().join("docker_sign_ran.txt");
    let marker_str = marker_path.to_string_lossy().to_string();
    let (cmd, args) = shell_echo_to_file("ANODIZER_TEST_DOCKER_SIGN", &marker_str);

    let docker_signs = vec![DockerSignConfig {
        id: Some("deselect-probe".to_string()),
        cmd: Some(cmd),
        args: Some(args),
        artifacts: Some("all".to_string()),
        ids: None,
        stdin: None,
        stdin_file: None,
        env: Some(vec!["ANODIZER_TEST_DOCKER_SIGN=ran".to_string()]),
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    }];

    // dry_run is false: only the deselect gate (not the dry-run guard) may
    // prevent the command from running, so a missing marker isolates the gate.
    let mut ctx = TestContextBuilder::new().dry_run(false).build();
    ctx.config.docker_signs = Some(docker_signs);
    ctx.options.skip_stages = opts.skip_stages;
    ctx.options.publisher_allowlist = opts.publisher_allowlist;
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        path: std::path::PathBuf::from("ghcr.io/test/app:latest"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    DockerSignStage
        .run(&mut ctx)
        .expect("deselected docker-sign must short-circuit to Ok");
    assert!(
        !marker_path.exists(),
        "deselected docker-sign must NOT run the signing command"
    );
}

#[test]
fn docker_sign_deselected_by_skip_not_run() {
    assert_docker_sign_deselected_not_run(anodizer_core::context::ContextOptions {
        skip_stages: vec!["docker-sign".to_string()],
        ..Default::default()
    });
}

#[test]
fn docker_sign_deselected_by_allowlist_not_run() {
    assert_docker_sign_deselected_not_run(anodizer_core::context::ContextOptions {
        publisher_allowlist: vec!["cargo".to_string()],
        ..Default::default()
    });
}

#[test]
fn docker_sign_deselected_skip_wins_over_allowlist() {
    assert_docker_sign_deselected_not_run(anodizer_core::context::ContextOptions {
        skip_stages: vec!["docker-sign".to_string()],
        publisher_allowlist: vec!["docker-sign".to_string()],
        ..Default::default()
    });
}

#[test]
fn docker_sign_in_allowlist_is_not_deselected() {
    // `--publishers docker-sign`: docker-sign IS selected, so the gate must
    // NOT fire and the command runs (marker written), proving the path entered.
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    let tmp = tempfile::TempDir::new().unwrap();
    let marker_path = tmp.path().join("docker_sign_ran.txt");
    let marker_str = marker_path.to_string_lossy().to_string();
    let (cmd, args) = shell_echo_to_file("ANODIZER_TEST_DOCKER_SIGN", &marker_str);
    let docker_signs = vec![DockerSignConfig {
        id: Some("selected-probe".to_string()),
        cmd: Some(cmd),
        args: Some(args),
        artifacts: Some("all".to_string()),
        ids: None,
        stdin: None,
        stdin_file: None,
        env: Some(vec!["ANODIZER_TEST_DOCKER_SIGN=ran".to_string()]),
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    }];
    let mut ctx = TestContextBuilder::new().dry_run(false).build();
    ctx.config.docker_signs = Some(docker_signs);
    ctx.options.publisher_allowlist = vec!["docker-sign".to_string()];
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        path: std::path::PathBuf::from("ghcr.io/test/app:latest"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });
    DockerSignStage.run(&mut ctx).unwrap();
    assert!(
        marker_path.exists(),
        "selected docker-sign must run the signing command"
    );
}

// -----------------------------------------------------------------------
// Sign stage parity — output, if, binary_signs, docker vars
// -----------------------------------------------------------------------

#[test]
fn test_sign_config_output_field_parsing() {
    let yaml = r#"
cmd: "gpg"
output: true
"#;
    let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(cfg.output.unwrap().as_bool());
}

#[test]
fn test_sign_config_if_field_parsing() {
    let yaml = r#"
cmd: "gpg"
if: "{{ IsSnapshot }}"
"#;
    let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.if_condition.as_deref(), Some("{{ IsSnapshot }}"));
}

#[test]
fn test_sign_config_output_and_if_together() {
    let yaml = r#"
cmd: "cosign"
output: true
if: "{{ IsSnapshot }}"
artifacts: all
"#;
    let cfg: SignConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(cfg.output.unwrap().as_bool());
    assert_eq!(cfg.if_condition.as_deref(), Some("{{ IsSnapshot }}"));
    assert_eq!(cfg.artifacts.as_deref(), Some("all"));
}

#[test]
fn test_sign_config_output_defaults_to_none() {
    let cfg = SignConfig::default();
    assert!(cfg.output.is_none());
    assert!(cfg.if_condition.is_none());
}

#[test]
fn test_binary_signs_config_parsing() {
    // `artifacts: binary` is the canonical value — broader
    // filters like `all` would silently match nothing.
    let yaml = r#"
project_name: test
binary_signs:
  - cmd: gpg
    artifacts: binary
  - cmd: cosign
    args:
      - sign-blob
crates: []
"#;
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.binary_signs.len(), 2);
    assert_eq!(config.binary_signs[0].cmd.as_deref(), Some("gpg"));
    assert_eq!(config.binary_signs[1].cmd.as_deref(), Some("cosign"));
}

#[test]
fn test_binary_signs_single_object() {
    let yaml = r#"
project_name: test
binary_signs:
  cmd: gpg
  artifacts: binary
crates: []
"#;
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.binary_signs.len(), 1);
    assert_eq!(config.binary_signs[0].cmd.as_deref(), Some("gpg"));
}

#[test]
fn test_binary_signs_defaults_to_empty() {
    let yaml = "project_name: test\ncrates: []";
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.binary_signs.is_empty());
}

#[test]
fn test_if_condition_false_skips_sign() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    // Sign config with if: "false" — should be skipped entirely.
    // If not skipped, the nonexistent binary would cause an error.
    let signs = vec![SignConfig {
        id: Some("skipped".to_string()),
        cmd: Some("/nonexistent/sign-tool".to_string()),
        args: Some(vec!["sign".to_string()]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: Some("false".to_string()),
    }];

    let mut ctx = TestContextBuilder::new()
        .dry_run(false)
        .signs(signs)
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = SignStage;
    assert!(
        stage.run(&mut ctx).is_ok(),
        "if condition 'false' should skip the sign config"
    );
}

#[test]
fn test_if_condition_true_proceeds() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    // Sign config with if: "true" — should proceed normally.
    // Uses "echo" which always succeeds.
    let signs = vec![SignConfig {
        id: Some("active".to_string()),
        cmd: Some("echo".to_string()),
        args: Some(vec!["signing".to_string()]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: Some("true".to_string()),
    }];

    let mut ctx = TestContextBuilder::new().dry_run(true).signs(signs).build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: combined_meta(),
        size: None,
    });

    let stage = SignStage;
    assert!(
        stage.run(&mut ctx).is_ok(),
        "if condition 'true' should proceed with sign config"
    );

    // Verify the signature artifact was registered (proves the config was not skipped)
    let sig_artifacts = ctx.artifacts.by_kind(ArtifactKind::Signature);
    assert!(
        !sig_artifacts.is_empty(),
        "sign config with if='true' should register signature artifacts"
    );
}

#[test]
fn test_if_condition_template_renders_to_empty_skips_sign() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    // A template that RENDERS to an empty / whitespace-only string must skip
    // the sign config (rendered "" / "false" / "0" / "no"
    // are falsy). NOTE: an EMPTY LITERAL `if: ""` is a separate no-op-gate
    // case (covered by config-tests) — this test exercises the
    // template-that-renders-empty path, which is the actual runtime
    // gate behavior the operator-facing skip surfaces.
    let signs = vec![SignConfig {
        id: Some("skipped".to_string()),
        cmd: Some("/nonexistent/sign-tool".to_string()),
        args: Some(vec!["sign".to_string()]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        // Render expands to an empty string (UndefinedSymbol unset; the
        // {{ ... }} braces render as nothing in Tera's strict mode would
        // error — use a literal " " that trims to empty instead so we exercise
        // the trimmed-empty branch).
        if_condition: Some(" ".to_string()),
    }];

    let mut ctx = TestContextBuilder::new()
        .dry_run(false)
        .signs(signs)
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = SignStage;
    assert!(
        stage.run(&mut ctx).is_ok(),
        "trim-empty rendered `if:` must skip the sign config",
    );
    let events = ctx.skip_memento.snapshot();
    assert!(
        events
            .iter()
            .any(|e| e.stage == "sign" && e.label == "skipped"),
        "expected a sign skip memento for the gated config: {events:?}",
    );
}

/// Build a single-checksum-artifact context whose env carries (or omits) the
/// harness marker, run a `signs` config, and return the recorded skip labels.
///
/// The keyless-cosign harness skip is decided at the TOP of the per-config
/// loop, before any spawn AND before the dry-run gate — so the skip memento is
/// authoritative under either mode. `dry_run` is passed through so the
/// negative-control cases (which must NOT skip) can be exercised without
/// actually spawning real `cosign` (whose keyless `sign-blob` triggers a
/// network OAuth device flow that hangs ~300s).
fn run_signs_capture_skips(
    sign: SignConfig,
    harness: bool,
    dry_run: bool,
) -> (anyhow::Result<()>, Vec<(String, String)>) {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let tmp = tempfile::TempDir::new().unwrap();
    let artifact_path = tmp.path().join("checksums.sha256");
    std::fs::write(&artifact_path, b"checksum content").unwrap();

    let mut builder = TestContextBuilder::new().dry_run(dry_run).signs(vec![sign]);
    if harness {
        builder = builder.env("ANODIZER_IN_DETERMINISM_HARNESS", "1");
    } else {
        builder = builder.sealed_env();
    }
    let mut ctx = builder.build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: artifact_path,
        target: None,
        crate_name: "test".to_string(),
        metadata: combined_meta(),
        size: None,
    });

    let result = SignStage.run(&mut ctx);
    let skips = ctx
        .skip_memento
        .snapshot()
        .iter()
        .map(|e| (e.stage.clone(), e.label.clone()))
        .collect();
    (result, skips)
}

/// A keyless cosign config (no `--key`) cannot sign inside the determinism
/// harness: cosign needs ambient OIDC (Fulcio/Rekor) the harness strips for
/// hermeticity, and `--key` is environment-bound to `COSIGN_KEY` so it would
/// crash opening the ephemeral PEM contents as a key-file path. Under the
/// harness it MUST be skipped (recorded, never spawned).
#[test]
fn keyless_cosign_is_skipped_under_harness() {
    let sign = SignConfig {
        id: Some("cosign-keyless".to_string()),
        cmd: Some("cosign".to_string()),
        args: Some(vec![
            "sign-blob".to_string(),
            "--bundle=cosign.bundle".to_string(),
            "--yes".to_string(),
            "{{ Artifact }}".to_string(),
        ]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };
    // dry_run=false: prove the config never spawns real cosign even in the
    // live path (the production failure mode); a non-skip here would error on
    // cosign's Fulcio OAuth flow.
    let (result, skips) = run_signs_capture_skips(sign, true, false);
    assert!(
        result.is_ok(),
        "keyless cosign under the harness must skip cleanly, not error: {result:?}"
    );
    assert!(
        skips
            .iter()
            .any(|(stage, label)| stage == "sign" && label == "cosign-keyless"),
        "keyless cosign must be recorded as skipped under the harness: {skips:?}"
    );
}

/// A cosign config WITH an explicit `--key` (e.g. `--key=env://COSIGN_KEY`)
/// signs with the harness's ephemeral key, so it must still RUN under the
/// harness — not be swept up by the keyless skip.
#[test]
fn keyed_cosign_is_not_skipped_under_harness() {
    let sign = SignConfig {
        id: Some("cosign-keyed".to_string()),
        cmd: Some("cosign".to_string()),
        args: Some(vec![
            "sign-blob".to_string(),
            "--key=env://COSIGN_KEY".to_string(),
            "--bundle=cosign.bundle".to_string(),
            "--yes".to_string(),
            "{{ Artifact }}".to_string(),
        ]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };
    let (_result, skips) = run_signs_capture_skips(sign, true, true);
    assert!(
        !skips
            .iter()
            .any(|(stage, label)| stage == "sign" && label == "cosign-keyed"),
        "a `--key`-bearing cosign config must still run under the harness: {skips:?}"
    );
}

/// Outside the harness (production), a keyless cosign config is UNAFFECTED —
/// it runs normally against ambient OIDC.
#[test]
fn keyless_cosign_is_not_skipped_outside_harness() {
    let sign = SignConfig {
        id: Some("cosign-keyless".to_string()),
        cmd: Some("cosign".to_string()),
        args: Some(vec![
            "sign-blob".to_string(),
            "--bundle=cosign.bundle".to_string(),
            "--yes".to_string(),
            "{{ Artifact }}".to_string(),
        ]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };
    let (_result, skips) = run_signs_capture_skips(sign, false, true);
    assert!(
        !skips
            .iter()
            .any(|(stage, label)| stage == "sign" && label == "cosign-keyless"),
        "keyless cosign outside the harness must NOT be skipped: {skips:?}"
    );
}

/// The keyless-cosign harness skip keys on `cmd == cosign`; a non-cosign
/// signing tool (gpg) under the harness is unaffected.
#[test]
fn gpg_is_not_skipped_under_harness() {
    let sign = SignConfig {
        id: Some("gpg-sign".to_string()),
        cmd: Some("gpg".to_string()),
        args: Some(vec![
            "--detach-sign".to_string(),
            "{{ Artifact }}".to_string(),
        ]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };
    let (_result, skips) = run_signs_capture_skips(sign, true, true);
    assert!(
        !skips
            .iter()
            .any(|(stage, label)| stage == "sign" && label == "gpg-sign"),
        "a gpg config under the harness must not be swept by the cosign skip: {skips:?}"
    );
}

#[test]
fn test_if_condition_snapshot_template() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    // When snapshot mode is active, IsSnapshot = "true".
    // This sign config with if: "{{ IsSnapshot }}" should only run
    // when in snapshot mode.
    let signs = vec![SignConfig {
        id: Some("snapshot-only".to_string()),
        cmd: Some("/nonexistent/sign-tool".to_string()),
        args: Some(vec!["sign".to_string()]),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: Some("{{ IsSnapshot }}".to_string()),
    }];

    // Non-snapshot mode: IsSnapshot = "false" → should skip
    let mut ctx = TestContextBuilder::new()
        .snapshot(false)
        .dry_run(false)
        .signs(signs.clone())
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = SignStage;
    assert!(
        stage.run(&mut ctx).is_ok(),
        "non-snapshot should skip sign config with if={{ IsSnapshot }}"
    );

    // Snapshot mode: IsSnapshot = "true" → should proceed (but uses
    // nonexistent binary, so it will error — prove it tries to run).
    let mut ctx_snap = TestContextBuilder::new()
        .snapshot(true)
        .dry_run(false)
        .signs(signs)
        .build();

    ctx_snap.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: combined_meta(),
        size: None,
    });

    let result = stage.run(&mut ctx_snap);
    assert!(
        result.is_err(),
        "snapshot mode should attempt to run the sign command (and fail with nonexistent binary)"
    );
}

#[test]
fn test_binary_signs_only_signs_binaries() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let binary_signs = vec![SignConfig {
        id: Some("binary-gpg".to_string()),
        cmd: Some("echo".to_string()),
        args: Some(vec!["signing-binary".to_string()]),
        // Even if artifacts says "all", binary_signs should only sign binaries
        artifacts: Some("all".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    }];

    let mut ctx = TestContextBuilder::new()
        .dry_run(true)
        .binary_signs(binary_signs)
        .build();

    // Add a binary and an archive artifact
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/myapp"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/myapp.tar.gz"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = SignStage;
    stage.run(&mut ctx).unwrap();

    // Only the binary should have generated a signature artifact
    let sig_artifacts = ctx.artifacts.by_kind(ArtifactKind::Signature);
    assert_eq!(
        sig_artifacts.len(),
        1,
        "binary_signs should only sign Binary artifacts, not Archive or Checksum"
    );
}

#[test]
fn test_binary_signs_if_condition_works() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    // binary_signs with if: "false" should be skipped
    let binary_signs = vec![SignConfig {
        id: Some("skipped".to_string()),
        cmd: Some("/nonexistent/sign-tool".to_string()),
        args: Some(vec!["sign".to_string()]),
        artifacts: Some("all".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: Some("false".to_string()),
    }];

    let mut ctx = TestContextBuilder::new()
        .dry_run(false)
        .binary_signs(binary_signs)
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/myapp"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = SignStage;
    assert!(
        stage.run(&mut ctx).is_ok(),
        "binary_signs with if=false should be skipped"
    );
}

#[test]
fn test_docker_sign_digest_and_artifact_id_template_vars() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    let tmp = tempfile::TempDir::new().unwrap();
    let marker_path = tmp.path().join("docker_vars.txt");
    let marker_str = marker_path.to_string_lossy().to_string();

    // Use a shell to capture template-resolved variables
    let (cmd, args) = shell_echo_literal_to_file(
        "digest={{ digest }} artifactID={{ artifactID }}",
        &marker_str,
    );
    let docker_signs = vec![DockerSignConfig {
        id: Some("test-vars".to_string()),
        cmd: Some(cmd),
        args: Some(args),
        artifacts: Some("all".to_string()),
        ids: None,
        stdin: None,
        stdin_file: None,
        env: None,
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    }];

    let mut ctx = TestContextBuilder::new().dry_run(false).build();
    ctx.config.docker_signs = Some(docker_signs);

    // Add a docker image with digest and id metadata
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("digest".to_string(), "sha256:abc123def456".to_string());
    metadata.insert("id".to_string(), "my-docker-image".to_string());

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        path: std::path::PathBuf::from("ghcr.io/myorg/app:latest"),
        target: None,
        crate_name: "test".to_string(),
        metadata,
        size: None,
    });

    let stage = DockerSignStage;
    stage.run(&mut ctx).unwrap();

    let output = std::fs::read_to_string(&marker_path).unwrap();
    assert!(
        output.contains("digest=sha256:abc123def456"),
        "digest template var should resolve from metadata, got: {}",
        output.trim()
    );
    assert!(
        output.contains("artifactID=my-docker-image"),
        "artifactID template var should resolve from metadata, got: {}",
        output.trim()
    );
}

/// The reference handed to cosign must be the digest-pinned
/// `<repo>:<tag>@sha256:<digest>`, never a bare tag — signing a tag is a
/// TOCTOU hole (the tag can move between build and sign). Regression for the
/// v0.9.0 log where cosign warned it was handed `ghcr.io/...:0.9.0` (a tag).
/// Exercises BOTH a bare `{{ .Artifact }}` args template and the historical
/// `{{ .Artifact }}@{{ .Digest }}` default; each must yield exactly one pin.
#[test]
fn test_docker_sign_signs_by_digest_not_tag() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    for (label, args_template) in [
        ("bare-artifact", "{{ .Artifact }}"),
        ("artifact-at-digest", "{{ .Artifact }}@{{ .Digest }}"),
    ] {
        let tmp = tempfile::TempDir::new().unwrap();
        let marker_path = tmp.path().join("ref.txt");
        let marker_str = marker_path.to_string_lossy().to_string();

        // Capture the reference cosign receives. The ref template is a
        // STANDALONE arg (`$1`) — exactly how a real `cosign sign <ref>`
        // call shapes it — so the per-arg digest-collapse applies. A shell
        // captures `$1` to the marker file.
        let (cmd, args) = if cfg!(windows) {
            // A real `.bat` is required: `cmd.exe /C "echo %1>marker"` never
            // binds `%1` to a positional arg (`%1` is a parameter reference
            // only inside a batch FILE), so the literal `%1` leaked into the
            // marker. Write the batch into this iteration's `tmp` and let
            // `Command::new(<bat>).arg(<ref>)` bind it. Leading-redirect form
            // (`>"file" echo %~1`) — NOT `echo %1>file` — because a digit
            // immediately before `>` (the `1>`) is parsed as a stdout-stream
            // redirect, corrupting the capture; redirect-first sidesteps that.
            // `%~1` strips any surrounding quotes. The ref has no spaces or
            // `% !` so Windows arg-escaping and env/delayed expansion are moot.
            let bat = tmp.path().join("capture.bat");
            std::fs::write(&bat, format!(">\"{marker_str}\" echo %~1\r\n"))
                .expect("write capture.bat");
            (
                bat.to_string_lossy().to_string(),
                vec![args_template.to_string()],
            )
        } else {
            (
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    format!("printf '%s' \"$1\" > {}", marker_str),
                    "sh".to_string(),
                    args_template.to_string(),
                ],
            )
        };
        let docker_signs = vec![DockerSignConfig {
            id: Some("digest-ref".to_string()),
            cmd: Some(cmd),
            args: Some(args),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        }];

        let mut ctx = TestContextBuilder::new().dry_run(false).build();
        ctx.config.docker_signs = Some(docker_signs);

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("tag".to_string(), "ghcr.io/myorg/app:0.9.0".to_string());
        metadata.insert("digest".to_string(), "sha256:282ea8edeadbeef".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::DockerImageV2,
            name: "ghcr.io/myorg/app:0.9.0".to_string(),
            path: std::path::PathBuf::from("ghcr.io/myorg/app:0.9.0"),
            target: None,
            crate_name: "test".to_string(),
            metadata,
            size: None,
        });

        let stage = DockerSignStage;
        stage.run(&mut ctx).unwrap();

        let signed_ref = std::fs::read_to_string(&marker_path).unwrap();
        let signed_ref = signed_ref.trim();
        assert_eq!(
            signed_ref, "ghcr.io/myorg/app:0.9.0@sha256:282ea8edeadbeef",
            "[{label}] cosign must receive the digest-pinned ref, got: {signed_ref}"
        );
        // Exactly one digest pin — no doubling, no bare tag.
        assert_eq!(
            signed_ref.matches("@sha256:").count(),
            1,
            "[{label}] exactly one digest pin expected: {signed_ref}"
        );
    }
}

/// Missing-digest path (safety-critical): when the build stage recorded NO
/// digest, the publisher must NOT fabricate one — it signs the bare tag
/// loudly (and warns elsewhere). This captures the rendered cosign reference
/// and pins that exactly one image is signed by its bare tag, with ZERO
/// `@sha256:` pins, so a future change cannot silently invent a digest.
#[cfg(unix)]
#[test]
fn test_docker_sign_without_digest_signs_bare_tag_never_fabricates() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    let tmp = tempfile::TempDir::new().unwrap();
    let marker_path = tmp.path().join("ref.txt");
    let marker_str = marker_path.to_string_lossy().to_string();

    // Capture the standalone `{{ .Artifact }}` reference ($1) the sign command
    // receives, exactly as `cosign sign <ref>` would shape it.
    let (cmd, args) = (
        "sh".to_string(),
        vec![
            "-c".to_string(),
            format!("printf '%s' \"$1\" > {marker_str}"),
            "sh".to_string(),
            "{{ .Artifact }}".to_string(),
        ],
    );
    let docker_signs = vec![DockerSignConfig {
        id: Some("test-no-meta".to_string()),
        cmd: Some(cmd),
        args: Some(args),
        artifacts: Some("all".to_string()),
        ids: None,
        stdin: None,
        stdin_file: None,
        env: None,
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    }];

    // Run for real (not dry-run) so the ref is actually rendered + captured.
    let mut ctx = TestContextBuilder::new().dry_run(false).build();
    ctx.config.docker_signs = Some(docker_signs);

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        // No digest metadata — the safety-critical path.
        path: std::path::PathBuf::from("ghcr.io/myorg/app:latest"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = DockerSignStage;
    assert!(
        stage.run(&mut ctx).is_ok(),
        "docker sign without digest metadata must still run"
    );

    let signed_ref = std::fs::read_to_string(&marker_path).unwrap();
    let signed_ref = signed_ref.trim();
    assert_eq!(
        signed_ref, "ghcr.io/myorg/app:latest",
        "missing digest must sign the bare tag, got: {signed_ref}"
    );
    assert_eq!(
        signed_ref.matches("@sha256:").count(),
        0,
        "must NOT fabricate a digest when none was recorded: {signed_ref}"
    );
}

#[test]
fn test_output_capture_with_real_command() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    // Use echo to produce stdout; with output: true it should be captured
    let (cmd, mut base_args) = echo_command();
    base_args.push("hello-from-sign".to_string());
    let signs = vec![SignConfig {
        id: Some("test-output".to_string()),
        cmd: Some(cmd),
        args: Some(base_args),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: Some(anodizer_core::config::StringOrBool::Bool(true)),
        if_condition: None,
    }];

    let mut ctx = TestContextBuilder::new()
        .dry_run(false)
        .signs(signs)
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Checksum,
        name: String::new(),
        path: std::path::PathBuf::from("/tmp/checksums.sha256"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = SignStage;
    // The command succeeds; output capture should not cause errors
    assert!(
        stage.run(&mut ctx).is_ok(),
        "sign with output: true and a real command should succeed"
    );
}

// -----------------------------------------------------------------------
// binary_signs architecture-aware signature template
// -----------------------------------------------------------------------

/// Regression: DEFAULT_BINARY_SIGNATURE_TEMPLATE must produce `<artifact>.sig`
/// for anodize's flat layout where binaries are already named with the platform
/// suffix (e.g. `myapp_linux_amd64`). The old template appended Os/Arch
/// again, producing `myapp_linux_amd64_linux_amd64` with no `.sig` extension.
#[test]
fn test_binary_signature_no_duplicate_suffix_has_dot_sig() {
    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    // Template vars that would be set during binary-sign processing.
    ctx.template_vars_mut().set("Os", "linux");
    ctx.template_vars_mut().set("Arch", "amd64");
    ctx.template_vars_mut().set("Arm", "");
    ctx.template_vars_mut().set("Amd64", "");
    ctx.template_vars_mut().set("Mips", "");

    let sign_cfg = SignConfig::default();
    let _log = ctx.logger("test");

    // The artifact path already contains the platform suffix (anodize flat layout).
    let result = resolve_signature_path(
        &sign_cfg,
        "/dist/myapp_linux_amd64",
        &ctx,
        SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
    )
    .unwrap();

    // Must end with .sig
    assert!(
        result.ends_with(".sig"),
        "binary signature must end with .sig; got '{result}'"
    );
    // Must NOT contain duplicate platform suffix
    assert!(
        !result.contains("linux_amd64_linux_amd64"),
        "binary signature must not contain duplicate platform suffix; got '{result}'"
    );
    // Expected canonical form
    assert_eq!(result, "/dist/myapp_linux_amd64.sig");
}

#[test]
fn test_default_binary_signature_template_is_simple_dot_sig() {
    // The default template for binary_signs must be identical to the plain
    // DEFAULT_SIGNATURE_TEMPLATE because anodize's binary names already
    // encode the platform suffix — no Os/Arch duplication needed.
    assert_eq!(
        SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
        "{{ .Artifact }}.sig",
        "DEFAULT_BINARY_SIGNATURE_TEMPLATE must equal '{{ .Artifact }}.sig'"
    );
}

#[test]
fn test_binary_signs_signature_default_adds_dot_sig() {
    // With anodize's flat layout the binary name already contains the platform
    // suffix. The default template must append only ".sig".
    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.template_vars_mut().set("Os", "linux");
    ctx.template_vars_mut().set("Arch", "amd64");
    ctx.template_vars_mut().set("Arm", "");
    ctx.template_vars_mut().set("Amd64", "");
    ctx.template_vars_mut().set("Mips", "");

    let sign_cfg = SignConfig {
        id: None,
        artifacts: None,
        cmd: None,
        args: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        ids: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };
    let _log = ctx.logger("test");
    // artifact_path already contains the platform suffix (anodize flat layout)
    let result = resolve_signature_path(
        &sign_cfg,
        "/dist/myapp_linux_amd64",
        &ctx,
        SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
    )
    .unwrap();
    assert_eq!(result, "/dist/myapp_linux_amd64.sig");
}

#[test]
fn test_binary_signs_signature_arm_artifact_gets_dot_sig() {
    // ARM binary already has platform suffix in its name; default template appends .sig only.
    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.template_vars_mut().set("Os", "linux");
    ctx.template_vars_mut().set("Arch", "arm");
    ctx.template_vars_mut().set("Arm", "6");
    ctx.template_vars_mut().set("Amd64", "");
    ctx.template_vars_mut().set("Mips", "");

    let sign_cfg = SignConfig {
        id: None,
        artifacts: None,
        cmd: None,
        args: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        ids: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };
    let _log = ctx.logger("test");
    // artifact_path already contains the platform suffix (anodize flat layout)
    let result = resolve_signature_path(
        &sign_cfg,
        "/dist/myapp_linux_armv6",
        &ctx,
        SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
    )
    .unwrap();
    assert_eq!(result, "/dist/myapp_linux_armv6.sig");
}

#[test]
fn test_binary_signs_signature_amd64v2_artifact_gets_dot_sig() {
    // amd64v2 binary already has platform+level suffix; default template appends .sig only.
    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.template_vars_mut().set("Os", "linux");
    ctx.template_vars_mut().set("Arch", "amd64");
    ctx.template_vars_mut().set("Arm", "");
    ctx.template_vars_mut().set("Amd64", "v2");
    ctx.template_vars_mut().set("Mips", "");

    let sign_cfg = SignConfig {
        id: None,
        artifacts: None,
        cmd: None,
        args: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        ids: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };
    let _log = ctx.logger("test");
    // artifact_path already contains the platform+level suffix (anodize flat layout)
    let result = resolve_signature_path(
        &sign_cfg,
        "/dist/myapp_linux_amd64v2",
        &ctx,
        SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE,
    )
    .unwrap();
    assert_eq!(result, "/dist/myapp_linux_amd64v2.sig");
}

#[test]
fn test_normal_signs_uses_simple_default() {
    let ctx = TestContextBuilder::new().dry_run(true).build();
    let sign_cfg = SignConfig {
        id: None,
        artifacts: None,
        cmd: None,
        args: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        ids: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };
    let _log = ctx.logger("test");
    // Normal signs default → simple {{ .Artifact }}.sig.
    let result = resolve_signature_path(
        &sign_cfg,
        "/dist/myapp.tar.gz",
        &ctx,
        SignConfig::DEFAULT_SIGNATURE_TEMPLATE,
    )
    .unwrap();
    assert_eq!(result, "/dist/myapp.tar.gz.sig");
}

// -----------------------------------------------------------------------
// DockerImageV2 in docker_signs filters
// -----------------------------------------------------------------------

#[test]
fn test_docker_signs_default_filter_selects_v2() {
    // When docker_signs artifacts is "" (default), only DockerImageV2 should match.
    // This verifies the code path — full integration tested via stage.run() above.
    use anodizer_core::artifact::Artifact;

    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: "legacy".to_string(),
        path: std::path::PathBuf::from("ghcr.io/owner/app:v1"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImageV2,
        name: "v2".to_string(),
        path: std::path::PathBuf::from("ghcr.io/owner/app:v2"),
        target: None,
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    // Default filter "" should return only DockerImageV2
    let v2_only = ctx.artifacts.by_kind(ArtifactKind::DockerImageV2);
    assert_eq!(v2_only.len(), 1);
    assert_eq!(v2_only[0].name, "v2");

    // "images" filter should return both DockerImage and DockerImageV2
    let mut images = ctx.artifacts.by_kind(ArtifactKind::DockerImage);
    images.extend(ctx.artifacts.by_kind(ArtifactKind::DockerImageV2));
    assert_eq!(images.len(), 2);
}

// -----------------------------------------------------------------------
// Integration: binary_signs with target triple through process_sign_configs
// -----------------------------------------------------------------------

#[test]
fn test_binary_signs_sets_os_arch_from_target_triple() {
    use anodizer_core::artifact::Artifact;

    let binary_sign_cfg = SignConfig {
        id: None,
        artifacts: Some("binary".to_string()),
        cmd: Some("true".to_string()),
        args: Some(vec![]),
        signature: None,
        stdin: None,
        stdin_file: None,
        ids: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };
    let mut ctx = TestContextBuilder::new()
        .binary_signs(vec![binary_sign_cfg])
        .dry_run(true)
        .build();

    // Add a binary artifact with a linux/amd64 target
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: "myapp".to_string(),
        path: std::path::PathBuf::from("/dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let log = ctx.logger("binary-sign");
    let binary_sign_configs = ctx.config.binary_signs.clone();
    let result = process_sign_configs(
        &binary_sign_configs,
        &mut ctx,
        &log,
        ArtifactFilter::BinaryOnly,
        "binary-sign",
    );
    assert!(result.is_ok());

    // Verify a signature artifact was registered; the name is derived directly
    // from the artifact path (flat layout — no platform suffix appended again).
    let sigs: Vec<_> = ctx.artifacts.by_kind(ArtifactKind::Signature);
    assert_eq!(sigs.len(), 1);
    assert!(
        sigs[0].name.ends_with(".sig"),
        "signature name must end with .sig: got '{}'",
        sigs[0].name
    );
    assert!(
        !sigs[0].name.contains("linux_amd64_linux_amd64"),
        "signature name must not contain duplicate platform suffix: got '{}'",
        sigs[0].name
    );

    // Template vars should be cleaned up after processing
    let os_val = ctx.render_template("{{ Os }}").unwrap_or_default();
    assert_eq!(
        os_val, "",
        "Os template var should be cleared after binary_signs"
    );
}

#[test]
fn test_binary_signs_arm_target_splits_arch_correctly() {
    use anodizer_core::artifact::Artifact;

    let binary_sign_cfg = SignConfig {
        id: None,
        artifacts: Some("binary".to_string()),
        cmd: Some("true".to_string()),
        args: Some(vec![]),
        signature: None,
        stdin: None,
        stdin_file: None,
        ids: None,
        env: None,
        certificate: None,
        output: None,
        if_condition: None,
    };
    let mut ctx = TestContextBuilder::new()
        .binary_signs(vec![binary_sign_cfg])
        .dry_run(true)
        .build();

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: "myapp".to_string(),
        path: std::path::PathBuf::from("/dist/myapp"),
        target: Some("armv7-unknown-linux-gnueabihf".to_string()),
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let log = ctx.logger("binary-sign");
    let binary_sign_configs = ctx.config.binary_signs.clone();
    let result = process_sign_configs(
        &binary_sign_configs,
        &mut ctx,
        &log,
        ArtifactFilter::BinaryOnly,
        "binary-sign",
    );
    assert!(result.is_ok());

    let sigs: Vec<_> = ctx.artifacts.by_kind(ArtifactKind::Signature);
    assert_eq!(sigs.len(), 1);
    // With flat layout: no platform suffix appended — just .sig
    assert!(
        sigs[0].name.ends_with(".sig"),
        "signature name must end with .sig: got '{}'",
        sigs[0].name
    );
    assert!(
        !sigs[0].name.contains("armv7v7"),
        "signature name must NOT contain armv7v7 double-suffix: got '{}'",
        sigs[0].name
    );
}

#[test]
fn test_binary_signs_register_target_qualified_names_per_target() {
    use anodizer_core::artifact::Artifact;

    // Per-target binaries share a basename and differ only by directory
    // (the preserved-bin layout), so without target qualification every
    // target's signature would register under the same name.
    let binary_sign_cfg = SignConfig {
        id: None,
        artifacts: Some("binary".to_string()),
        cmd: Some("true".to_string()),
        args: Some(vec![]),
        signature: None,
        stdin: None,
        stdin_file: None,
        ids: None,
        env: None,
        certificate: Some("{{ .Artifact }}.pem".to_string()),
        output: None,
        if_condition: None,
    };
    let mut ctx = TestContextBuilder::new()
        .binary_signs(vec![binary_sign_cfg])
        .dry_run(true)
        .build();

    let binary = |triple: &str, basename: &str| Artifact {
        kind: ArtifactKind::Binary,
        name: basename.to_string(),
        path: std::path::PathBuf::from(format!("/dist/_preserved-bin/{triple}/{basename}")),
        target: Some(triple.to_string()),
        crate_name: "test".to_string(),
        metadata: Default::default(),
        size: None,
    };
    ctx.artifacts
        .add(binary("x86_64-unknown-linux-gnu", "anodizer"));
    ctx.artifacts
        .add(binary("aarch64-apple-darwin", "anodizer"));
    ctx.artifacts
        .add(binary("x86_64-pc-windows-msvc", "anodizer.exe"));

    let log = ctx.logger("binary-sign");
    let binary_sign_configs = ctx.config.binary_signs.clone();
    process_sign_configs(
        &binary_sign_configs,
        &mut ctx,
        &log,
        ArtifactFilter::BinaryOnly,
        "binary-sign",
    )
    .expect("binary-sign run");

    let mut sig_names: Vec<String> = ctx
        .artifacts
        .by_kind(ArtifactKind::Signature)
        .iter()
        .map(|a| a.name.clone())
        .collect();
    sig_names.sort();
    assert_eq!(
        sig_names,
        vec![
            "anodizer-aarch64-apple-darwin.sig",
            "anodizer-x86_64-unknown-linux-gnu.sig",
            "anodizer.exe-x86_64-pc-windows-msvc.sig",
        ],
        "each target's signature must register under a distinct name"
    );

    let mut cert_names: Vec<String> = ctx
        .artifacts
        .by_kind(ArtifactKind::Certificate)
        .iter()
        .map(|a| a.name.clone())
        .collect();
    cert_names.sort();
    assert_eq!(
        cert_names,
        vec![
            "anodizer-aarch64-apple-darwin.pem",
            "anodizer-x86_64-unknown-linux-gnu.pem",
            "anodizer.exe-x86_64-pc-windows-msvc.pem",
        ],
        "each target's certificate must register under a distinct name"
    );

    for kind in [ArtifactKind::Signature, ArtifactKind::Certificate] {
        for art in ctx.artifacts.by_kind(kind) {
            assert!(
                art.target.is_some(),
                "{kind:?} '{}' must carry its subject binary's target",
                art.name
            );
            assert!(
                art.name.contains(art.target.as_deref().unwrap_or_default()),
                "{kind:?} name '{}' must embed its target '{}'",
                art.name,
                art.target.as_deref().unwrap_or_default()
            );
            // The on-disk path keeps the bare basename: only the registry
            // name is qualified, the signer wrote next to the binary.
            assert!(
                art.path.to_string_lossy().contains("_preserved-bin"),
                "{kind:?} path '{}' must stay inside the per-target directory",
                art.path.display()
            );
        }
    }
}

// -----------------------------------------------------------------------
// Gap E: Docker sign ID defaults to "default"
// -----------------------------------------------------------------------

#[test]
fn test_docker_sign_id_defaults_to_default() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    // Config with no explicit id — should default to "default".
    let docker_signs = vec![DockerSignConfig {
        id: None,
        cmd: Some("echo".to_string()),
        args: Some(vec!["sign".to_string(), "{{ .Artifact }}".to_string()]),
        artifacts: Some("all".to_string()),
        ids: None,
        stdin: None,
        stdin_file: None,
        env: None,
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    }];

    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.config.docker_signs = Some(docker_signs);

    // Add a docker image so the sign loop has something to process
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        path: std::path::PathBuf::from("ghcr.io/myorg/app:latest"),
        target: None,
        crate_name: "test".to_string(),
        metadata: std::collections::HashMap::new(),
        size: None,
    });

    let stage = SignStage;
    // Dry-run should succeed and the log should contain the default id.
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_docker_sign_explicit_id_preserved() {
    use anodizer_core::config::DockerSignConfig;

    let cfg = DockerSignConfig {
        id: Some("my-signer".to_string()),
        cmd: None,
        args: None,
        artifacts: None,
        ids: None,
        stdin: None,
        stdin_file: None,
        env: None,
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    };

    let sign_id = cfg.id.as_deref().unwrap_or("default");
    assert_eq!(sign_id, "my-signer");
}

#[test]
fn test_docker_sign_none_id_defaults() {
    use anodizer_core::config::DockerSignConfig;

    let cfg = DockerSignConfig {
        id: None,
        cmd: None,
        args: None,
        artifacts: None,
        ids: None,
        stdin: None,
        stdin_file: None,
        env: None,
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    };

    let sign_id = cfg.id.as_deref().unwrap_or("default");
    assert_eq!(sign_id, "default");
}

// -----------------------------------------------------------------------
// Bug 1: "all" filter only matches release-uploadable types
// -----------------------------------------------------------------------

#[test]
fn test_all_filter_excludes_internal_types() {
    // Internal types that should NOT be signed by the "all" filter
    assert!(!should_sign(ArtifactKind::DockerImage, "all").unwrap());
    assert!(!should_sign(ArtifactKind::DockerImageV2, "all").unwrap());
    assert!(!should_sign(ArtifactKind::DockerManifest, "all").unwrap());
    assert!(!should_sign(ArtifactKind::BrewFormula, "all").unwrap());
    assert!(!should_sign(ArtifactKind::ScoopManifest, "all").unwrap());
    assert!(!should_sign(ArtifactKind::Metadata, "all").unwrap());
    assert!(!should_sign(ArtifactKind::Nixpkg, "all").unwrap());
    assert!(!should_sign(ArtifactKind::KrewPluginManifest, "all").unwrap());
    assert!(!should_sign(ArtifactKind::WingetInstaller, "all").unwrap());
    assert!(!should_sign(ArtifactKind::PkgBuild, "all").unwrap());
    assert!(!should_sign(ArtifactKind::PublishableSnapcraft, "all").unwrap());
    assert!(!should_sign(ArtifactKind::PublishableDockerImage, "all").unwrap());
}

#[test]
fn test_all_filter_includes_primary_subject_types() {
    // "all" = anodizer_core::artifact::signable_subject_kinds() (primary
    // subjects). Mapping: MSI/NSIS (Installer), DMG (DiskImage), and
    // PKG (MacOsPackage) are all primary subjects so they get signed and
    // uploaded alongside archives; anodizer treats them as first-class.
    assert!(should_sign(ArtifactKind::Archive, "all").unwrap());
    assert!(should_sign(ArtifactKind::UploadableBinary, "all").unwrap());
    assert!(should_sign(ArtifactKind::LinuxPackage, "all").unwrap());
    assert!(should_sign(ArtifactKind::SourceArchive, "all").unwrap());
    assert!(should_sign(ArtifactKind::Makeself, "all").unwrap());
    assert!(should_sign(ArtifactKind::Flatpak, "all").unwrap());
    assert!(should_sign(ArtifactKind::SourceRpm, "all").unwrap());
    assert!(should_sign(ArtifactKind::Installer, "all").unwrap());
    assert!(should_sign(ArtifactKind::DiskImage, "all").unwrap());
    assert!(should_sign(ArtifactKind::MacOsPackage, "all").unwrap());
    assert!(should_sign(ArtifactKind::Sbom, "all").unwrap());
    // Checksum IS signed under `all` (GoReleaser parity); one `X.sha256.sig`.
    assert!(should_sign(ArtifactKind::Checksum, "all").unwrap());
    assert!(should_sign(ArtifactKind::UploadableFile, "all").unwrap());
    // Signature + Certificate + Metadata are excluded from `all` so re-running
    // sign on a partially-built dist does not produce sig.sig chains (never
    // sign a sig).
    assert!(!should_sign(ArtifactKind::Signature, "all").unwrap());
    assert!(!should_sign(ArtifactKind::Certificate, "all").unwrap());

    // These are NOT primary subjects — use the dedicated
    // `binary` / `snap` filters to opt in.
    assert!(!should_sign(ArtifactKind::Binary, "all").unwrap());
    assert!(!should_sign(ArtifactKind::UniversalBinary, "all").unwrap());
    assert!(!should_sign(ArtifactKind::Snap, "all").unwrap());
}

// -----------------------------------------------------------------------
// Bug 4: Docker sign IDs must be unique
// -----------------------------------------------------------------------

#[test]
fn test_docker_sign_duplicate_ids_rejected() {
    use anodizer_core::config::DockerSignConfig;

    let docker_signs = vec![
        DockerSignConfig {
            id: Some("signer".to_string()),
            cmd: Some("echo".to_string()),
            args: Some(vec!["sign".to_string()]),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        },
        DockerSignConfig {
            id: Some("signer".to_string()), // duplicate!
            cmd: Some("echo".to_string()),
            args: Some(vec!["sign".to_string()]),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        },
    ];

    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.config.docker_signs = Some(docker_signs);

    let stage = DockerSignStage;
    let result = stage.run(&mut ctx);
    assert!(
        result.is_err(),
        "duplicate docker_signs IDs should be rejected"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("docker_signs") && err.contains("signer"),
        "error should mention docker_signs and the duplicate ID, got: {err}"
    );
}

#[test]
fn test_docker_sign_duplicate_default_ids_rejected() {
    use anodizer_core::config::DockerSignConfig;

    // Two configs with no explicit id — both default to "default"
    let docker_signs = vec![
        DockerSignConfig {
            id: None,
            cmd: Some("echo".to_string()),
            args: Some(vec!["sign".to_string()]),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        },
        DockerSignConfig {
            id: None,
            cmd: Some("echo".to_string()),
            args: Some(vec!["sign".to_string()]),
            artifacts: Some("all".to_string()),
            ids: None,
            stdin: None,
            stdin_file: None,
            env: None,
            output: None,
            if_condition: None,
            signature: None,
            certificate: None,
        },
    ];

    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.config.docker_signs = Some(docker_signs);

    let stage = DockerSignStage;
    let result = stage.run(&mut ctx);
    assert!(
        result.is_err(),
        "duplicate default docker_signs IDs should be rejected"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("default"),
        "error should mention the 'default' ID, got: {err}"
    );
}

// -----------------------------------------------------------------------
// Bug 5: Docker sign Digest variable uses correct casing
// -----------------------------------------------------------------------

#[test]
fn test_docker_sign_digest_go_compat_syntax() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    let tmp = tempfile::TempDir::new().unwrap();
    let marker_path = tmp.path().join("docker_digest_case.txt");
    let marker_str = marker_path.to_string_lossy().to_string();

    // Use Go-compat syntax {{ .Digest }} which gets preprocessed to {{ Digest }}
    let (cmd, args) = shell_echo_literal_to_file("{{ Digest }}", &marker_str);
    let docker_signs = vec![DockerSignConfig {
        id: Some("test-digest-case".to_string()),
        cmd: Some(cmd),
        args: Some(args),
        artifacts: Some("all".to_string()),
        ids: None,
        stdin: None,
        stdin_file: None,
        env: None,
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    }];

    let mut ctx = TestContextBuilder::new().dry_run(false).build();
    ctx.config.docker_signs = Some(docker_signs);

    let mut metadata = std::collections::HashMap::new();
    metadata.insert("digest".to_string(), "sha256:deadbeef".to_string());

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        path: std::path::PathBuf::from("ghcr.io/myorg/app:latest"),
        target: None,
        crate_name: "test".to_string(),
        metadata,
        size: None,
    });

    let stage = DockerSignStage;
    stage.run(&mut ctx).unwrap();

    let output = std::fs::read_to_string(&marker_path).unwrap();
    assert_eq!(
        output.trim(),
        "sha256:deadbeef",
        "PascalCase Digest template var should resolve correctly, got: {}",
        output.trim()
    );
}

// ---------------------------------------------------------------------------
// SOURCE_DATE_EPOCH byte-stability regression
// ---------------------------------------------------------------------------
//
// stage-sign delegates to an external signer (`cosign`, `gpg`) via
// `Command::new`. There are no `Utc::now()` / `SystemTime::now()`
// callsites in stage-sign — the SDE goes in as an env var on the
// `Command`. Byte-stable signature output requires the signer itself to
// honor SDE (and, for GPG, `--faked-system-time`). These two
// signer-dependent tests are gated as `#[ignore]` because:
//
//   1. cosign's signature output is intentionally non-deterministic by
//      default (random nonce); deterministic-signing mode requires
//      `--key-ref` with a specific KMS configuration the test harness
//      cannot provision.
//   2. gpg's `--faked-system-time` flag requires a preflight check that
//      fails fast on gpg < 2.0.10 (no `--faked-system-time` support);
//      without it, a test that pins gpg byte-stability is flaky on
//      hosts where the flag isn't supported.
//
// Both tests remain in the suite as documentation of the contract; they
// will be un-ignored once the preflight gpg check + cosign-KMS fixture
// are wired up.

#[test]
#[ignore = "cosign deterministic-signing requires KMS key fixture"]
fn cosign_signature_byte_stable_for_same_sde() {
    // Sketch:
    //   - Skip if `cosign` not on PATH.
    //   - Set SOURCE_DATE_EPOCH=1715000000 on two separate sign invocations.
    //   - Assert the two `.sig` outputs are byte-identical.
    //
    // Blocked on: deterministic-signing KMS fixture (preflight will
    // surface whether the host's cosign supports `--key-ref kms://`).
}

#[test]
#[ignore = "requires gpg --faked-system-time preflight"]
fn gpg_signature_byte_stable_for_same_sde() {
    // Sketch:
    //   - Skip if `gpg --version` doesn't print 2.x+.
    //   - Sign the same payload twice with the same `--faked-system-time`.
    //   - Assert byte-equal `.sig` outputs.
    //
    // Blocked on: a preflight that fails fast on gpg < 2.0.10 (no
    // `--faked-system-time` support).
}

#[test]
fn docker_sign_zero_match_ids_filter_warns_loudly() {
    // A docker_signs config whose ids filter eliminates every selected
    // docker artifact silently signs nothing — the stage must warn.
    use anodizer_core::artifact::Artifact;
    use anodizer_core::config::DockerSignConfig;

    let docker_signs = vec![DockerSignConfig {
        cmd: Some("echo".to_string()),
        args: Some(vec!["sign".to_string(), "{{ .Artifact }}".to_string()]),
        artifacts: Some("all".to_string()),
        ids: Some(vec!["no-such-id".to_string()]),
        ..Default::default()
    }];

    let mut ctx = TestContextBuilder::new().dry_run(true).build();
    ctx.config.docker_signs = Some(docker_signs);
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::DockerImage,
        name: String::new(),
        path: std::path::PathBuf::from("ghcr.io/myorg/prod:latest"),
        target: None,
        crate_name: "test".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), "prod-image".to_string());
            m
        },
        size: None,
    });
    let capture = anodizer_core::log::LogCapture::new();
    ctx.with_log_capture(capture.clone());

    DockerSignStage.run(&mut ctx).expect("docker-sign run");

    assert!(
        capture
            .warn_messages()
            .iter()
            .any(|m| m.contains("matched no docker artifacts")),
        "zero-match docker ids filter must warn: {:?}",
        capture.all_messages()
    );
}
