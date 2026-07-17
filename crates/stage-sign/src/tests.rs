use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::SignConfig;
use anodizer_core::stage::Stage;
use anodizer_core::test_helpers::TestContextBuilder;

use super::helpers::{
    collapse_doubled_digest, pin_image_ref_to_digest, prepare_stdin_from, resolve_sign_args,
    resolve_signature_path, should_sign_artifact,
};
use super::process::{
    ArtifactFilter, COSIGN_CONSENT_ENV, ensure_cosign_consent_env, process_sign_configs,
};
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

/// Write an executable shell script to `dir/name` and return its path.
#[cfg(unix)]
fn write_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write script");
    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
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

/// Return a shell command + args that copies the signer's stdin to
/// `dest_file`, so a test can assert what content was piped in.
/// On Unix: `sh -c "cat > file"`. On Windows: `more > file` copies stdin to
/// the redirected file. `more` is used (not `findstr "^"`) because the latter
/// needs a quoted pattern arg, and Rust's Windows command-line escaping mangles
/// the embedded quotes — `more` has no quotes, matching the proven
/// `shell_echo_to_file` shape. (`more` may append trailing whitespace; callers
/// assert with `contains`.)
fn shell_stdin_capture_to_file(dest_file: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        (
            "cmd.exe".to_string(),
            vec!["/C".to_string(), format!("more > {}", dest_file)],
        )
    } else {
        (
            "sh".to_string(),
            vec!["-c".to_string(), format!("cat > {}", dest_file)],
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
        authenticode: None,
        verify: None,
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

/// `sign` is a prep stage with no `--publishers` identity, but its `signs:`
/// output (detached archive/checksum signatures) is consumed only by
/// github-release, blob, and artifactory. When a `--publishers` allowlist
/// deselects ALL THREE consumers (e.g. `--publishers npm`), the stage skips
/// the `signs:` loop so a publisher-scoped runner isn't asked to sign output
/// nothing selected will read. An EMPTY allowlist deselects nothing, so the
/// signs loop runs — and an allowlist that keeps ANY one consumer keeps it
/// running too.
#[test]
fn signs_loop_skips_only_when_every_consumer_is_deselected() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let gpg_sign = || SignConfig {
        id: Some("gpg".to_string()),
        cmd: Some("gpg".to_string()),
        args: Some(vec![
            "--output".to_string(),
            "{{ .Signature }}".to_string(),
            "--detach-sign".to_string(),
            "{{ .Artifact }}".to_string(),
        ]),
        artifacts: Some("all".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        authenticode: None,
        verify: None,
        if_condition: None,
    };

    // (allowlist, expect the `signs:` loop to run -> a signature is produced)
    let cases: [(&[&str], bool); 6] = [
        // Empty allowlist: nothing deselected, the loop runs.
        (&[], true),
        // npm-only: every consumer in `signs_consumers()` is deselected, so the
        // loop self-skips and produces no signature.
        (&["npm"], false),
        // Any single consumer still selected keeps the loop running.
        (&["github-release"], true),
        (&["blob"], true),
        (&["artifactory"], true),
        // `uploads` consumes the signs sidecars when an entry sets
        // `signature: true`; selecting it alone must keep the loop running.
        (&["uploads"], true),
    ];

    for (allowlist, expect_signed) in cases {
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![gpg_sign()])
            .publisher_allowlist(allowlist.iter().map(|s| s.to_string()).collect())
            .build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "myapp.tar.gz".to_string(),
            path: std::path::PathBuf::from("/tmp/myapp.tar.gz"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        SignStage.run(&mut ctx).unwrap();

        let produced_sig = ctx
            .artifacts
            .by_kind(ArtifactKind::Signature)
            .iter()
            .any(|a| a.name == "myapp.tar.gz.sig");
        assert_eq!(
            produced_sig, expect_signed,
            "allowlist {allowlist:?}: expected signs-loop ran == {expect_signed}, \
             but signature present == {produced_sig}"
        );
    }
}

/// A custom publisher (`config.publishers`) with `signature: true` is a fifth
/// `signs:` consumer that no static list can name. `signs_fully_deselected`
/// must keep `signs:` alive when such a publisher is *selected*, and ignore it
/// when it is `signature: false` or itself deselected.
#[test]
fn signs_gate_honors_selected_custom_signature_publisher() {
    use anodizer_core::config::PublisherConfig;
    let custom = |name: &str, sig: bool| PublisherConfig {
        name: Some(name.to_string()),
        signature: Some(sig),
        ..Default::default()
    };

    // `--publishers my-cdn` where my-cdn opts into signatures: every built-in
    // consumer is deselected, but the selected custom consumer keeps signs: on.
    let mut ctx = TestContextBuilder::new()
        .publisher_allowlist(vec!["my-cdn".to_string()])
        .build();
    ctx.config.publishers = Some(vec![custom("my-cdn", true)]);
    assert!(
        !crate::signs_fully_deselected(&ctx),
        "selected custom publisher with signature:true must keep signs: alive"
    );

    // Same target but `signature: false` → not a consumer → signs: skips.
    let mut ctx = TestContextBuilder::new()
        .publisher_allowlist(vec!["my-cdn".to_string()])
        .build();
    ctx.config.publishers = Some(vec![custom("my-cdn", false)]);
    assert!(
        crate::signs_fully_deselected(&ctx),
        "custom publisher with signature:false is not a signs: consumer"
    );

    // The signature publisher exists but is deselected (npm-only run): it won't
    // run, so it must not keep signs: alive.
    let mut ctx = TestContextBuilder::new()
        .publisher_allowlist(vec!["npm".to_string()])
        .build();
    ctx.config.publishers = Some(vec![custom("my-cdn", true)]);
    assert!(
        crate::signs_fully_deselected(&ctx),
        "a deselected custom signature publisher does not keep signs: alive"
    );
}

/// `binary_signs:` (raw-binary signing) self-skips in `--publish-only` mode:
/// its output carries the `binary_sign` marker and is filtered out of every
/// publish-time consumer, so signing in publish-only is discarded work that
/// would demand cosign/GPG material a publish-time runner does not carry. The
/// FULL build/release pipeline (`publish_only == false`) still signs — under
/// ANY `--publishers` allowlist value, including the npm-only allowlist — so
/// the main job's binary signing is never weakened. The empty allowlist (the
/// main release job's invariant) ALWAYS signs.
#[test]
fn binary_signs_loop_skips_only_in_publish_only_mode() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let cosign_sign = || SignConfig {
        id: Some("binkey".to_string()),
        cmd: Some("echo".to_string()),
        args: Some(vec![
            "--output".to_string(),
            "{{ .Signature }}".to_string(),
            "{{ .Artifact }}".to_string(),
        ]),
        artifacts: Some("binary".to_string()),
        ids: None,
        signature: None,
        stdin: None,
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        authenticode: None,
        verify: None,
        if_condition: None,
    };

    // (publish_only, allowlist, expect the `binary_signs:` loop to run)
    let cases: [(bool, &[&str], bool); 5] = [
        // FULL pipeline, empty allowlist — the main release job's invariant:
        // binaries ARE signed.
        (false, &[], true),
        // FULL pipeline, npm-only allowlist: `binary_signs:` is NOT gated on
        // the publish-time allowlist (it is a build-time concern), so it still
        // runs — a `--publishers` value never weakens binary signing.
        (false, &["npm"], true),
        // FULL pipeline, github-release allowlist: still signs.
        (false, &["github-release"], true),
        // publish-only, npm-only allowlist (the npm provenance job): the loop
        // self-skips — no consumer reads binary-sign output in publish-only.
        (true, &["npm"], false),
        // publish-only, empty allowlist: still publish-only, so it skips.
        (true, &[], false),
    ];

    for (publish_only, allowlist, expect_signed) in cases {
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .publish_only(publish_only)
            .binary_signs(vec![cosign_sign()])
            .publisher_allowlist(allowlist.iter().map(|s| s.to_string()).collect())
            .build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: std::path::PathBuf::from("/tmp/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        SignStage.run(&mut ctx).unwrap();

        let produced_sig = ctx
            .artifacts
            .by_kind(ArtifactKind::Signature)
            .iter()
            .any(|a| anodizer_core::artifact::is_binary_sign_output(a));
        assert_eq!(
            produced_sig, expect_signed,
            "publish_only={publish_only}, allowlist {allowlist:?}: expected \
             binary_signs-loop ran == {expect_signed}, but binary-sign signature \
             present == {produced_sig}"
        );
    }
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
            verify: None,
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
            authenticode: None,
            if_condition: None,
        },
        SignConfig {
            verify: None,
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
            authenticode: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
fn test_sign_stdin_is_template_rendered() {
    // Regression: `signs.stdin` must be template-expanded before it is piped
    // to the signer, so `stdin: "{{ Env.GPG_PASSPHRASE }}"` reaches gpg as the
    // passphrase VALUE, not the literal template string (matches GoReleaser's
    // `sign.go` which applies templates to `stdin`).
    use anodizer_core::artifact::{Artifact, ArtifactKind};

    let tmp = tempfile::TempDir::new().unwrap();
    let marker_path = tmp.path().join("stdin_check.txt");
    let marker_str = marker_path.to_string_lossy().to_string();

    let (cmd, args) = shell_stdin_capture_to_file(&marker_str);
    let signs = vec![SignConfig {
        id: Some("test-stdin".to_string()),
        cmd: Some(cmd),
        args: Some(args),
        artifacts: Some("checksum".to_string()),
        ids: None,
        signature: None,
        stdin: Some("{{ Env.GPG_PASSPHRASE }}".to_string()),
        stdin_file: None,
        env: None,
        certificate: None,
        output: None,
        authenticode: None,
        verify: None,
        if_condition: None,
    }];

    let artifact_path = tmp.path().join("checksums.sha256");
    std::fs::write(&artifact_path, b"checksum content").unwrap();

    let mut ctx = TestContextBuilder::new()
        .dry_run(false)
        .signs(signs)
        .build();
    // `Env.*` resolves from the template env map (checked before the
    // ProcessEnvSource fallback that supplies it in real CI); seed it
    // hermetically so the test does not depend on the process environment.
    ctx.template_vars_mut()
        .set_env("GPG_PASSPHRASE", "s3cr3t-passphrase");

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
    assert!(
        result.is_ok(),
        "sign should succeed; got: {:?}",
        result.err()
    );

    let piped = std::fs::read_to_string(&marker_path)
        .unwrap_or_else(|e| panic!("marker file should exist — stdin was piped: {e}"));
    assert!(
        piped.contains("s3cr3t-passphrase"),
        "stdin template must render to the env value; got: {piped:?}"
    );
    assert!(
        !piped.contains("{{"),
        "the literal template string must not reach the signer; got: {piped:?}"
    );
}

#[test]
fn test_docker_sign_stdin_is_template_rendered() {
    // Same regression as `test_sign_stdin_is_template_rendered`, for the
    // docker-sign call site.
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    let tmp = tempfile::TempDir::new().unwrap();
    let marker_path = tmp.path().join("docker_stdin_check.txt");
    let marker_str = marker_path.to_string_lossy().to_string();

    let (cmd, args) = shell_stdin_capture_to_file(&marker_str);
    let docker_signs = vec![DockerSignConfig {
        verify: None,
        id: Some("test-stdin".to_string()),
        cmd: Some(cmd),
        args: Some(args),
        artifacts: Some("all".to_string()),
        ids: None,
        stdin: Some("{{ Env.GPG_PASSPHRASE }}".to_string()),
        stdin_file: None,
        env: None,
        output: None,
        if_condition: None,
        signature: None,
        certificate: None,
    }];

    let mut ctx = TestContextBuilder::new().dry_run(false).build();
    ctx.template_vars_mut()
        .set_env("GPG_PASSPHRASE", "s3cr3t-passphrase");
    ctx.config.docker_signs = Some(docker_signs);

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

    DockerSignStage.run(&mut ctx).unwrap();

    let piped = std::fs::read_to_string(&marker_path)
        .unwrap_or_else(|e| panic!("marker file should exist — stdin was piped: {e}"));
    assert!(
        piped.contains("s3cr3t-passphrase"),
        "docker-sign stdin template must render to the env value; got: {piped:?}"
    );
    assert!(
        !piped.contains("{{"),
        "the literal template string must not reach the signer; got: {piped:?}"
    );
}

#[test]
fn test_docker_sign_ids_filter() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::DockerSignConfig;

    let docker_signs = vec![DockerSignConfig {
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        verify: None,
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
        verify: None,
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
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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

/// cosign invocations must carry the non-interactive consent (`COSIGN_YES`) so
/// the sigstore privacy banner / `y/N` prompt never blocks or pollutes CI
/// output. The seam exports it in the child env, idempotently, for any
/// `cosign`/`cosign-*` basename.
#[test]
fn cosign_consent_env_is_injected_for_cosign() {
    let mut env: Vec<(String, String)> = Vec::new();
    ensure_cosign_consent_env("cosign", &mut env);
    assert!(
        env.iter()
            .any(|(k, v)| k == COSIGN_CONSENT_ENV && v == "true"),
        "cosign must carry the non-interactive consent env: {env:?}"
    );

    // An absolute path and a `cosign-*` variant still resolve to the cosign
    // basename and get the consent.
    let mut env_abs: Vec<(String, String)> = Vec::new();
    ensure_cosign_consent_env("/usr/local/bin/cosign", &mut env_abs);
    assert!(env_abs.iter().any(|(k, _)| k == COSIGN_CONSENT_ENV));
}

/// A non-cosign signer (gpg, custom) must NOT have `COSIGN_YES` forced into its
/// env, and an operator who pinned `COSIGN_YES` explicitly is respected
/// (idempotent — no duplicate, value untouched).
#[test]
fn cosign_consent_env_noop_for_non_cosign_and_respects_explicit() {
    let mut gpg_env: Vec<(String, String)> = Vec::new();
    ensure_cosign_consent_env("gpg", &mut gpg_env);
    assert!(
        gpg_env.is_empty(),
        "non-cosign signer must not get the cosign consent env: {gpg_env:?}"
    );

    let mut pinned: Vec<(String, String)> =
        vec![(COSIGN_CONSENT_ENV.to_string(), "false".to_string())];
    ensure_cosign_consent_env("cosign", &mut pinned);
    assert_eq!(
        pinned.len(),
        1,
        "an explicit COSIGN_YES must not be duplicated: {pinned:?}"
    );
    assert_eq!(
        pinned[0].1, "false",
        "an operator-set COSIGN_YES value must be respected"
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        verify: None,
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
            verify: None,
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
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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
        authenticode: None,
        verify: None,
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

/// An armv7 binary under the DEFAULT signature template gets a plain `.sig`
/// name with no platform suffix at all (flat layout appends nothing).
#[test]
fn test_binary_signs_armv7_default_template_appends_only_sig_ext() {
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
        authenticode: None,
        verify: None,
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

/// Sign seeds the COMPOSITE build/installer policy on an armv7 binary:
/// `Arch="armv7"` with `Arm` empty — never the archive-policy split
/// (`Arch="arm"` + `Arm="7"`). Two probes pin both halves:
/// - the certificate template's bare `{{ Arch }}` renders `armv7`
///   (the split policy would render `arm`),
/// - the signature template's Arm-suffix idiom appends NOTHING
///   (a seeded `Arm` would render the doubled `armv7v7`).
#[test]
fn test_binary_signs_armv7_templates_render_composite_arch_and_empty_arm() {
    use anodizer_core::artifact::Artifact;

    let binary_sign_cfg = SignConfig {
        id: None,
        artifacts: Some("binary".to_string()),
        cmd: Some("true".to_string()),
        args: Some(vec![]),
        signature: Some(
            "{{ .Artifact }}.{{ Arch }}{% if Arm %}v{{ Arm }}{% endif %}.sig".to_string(),
        ),
        stdin: None,
        stdin_file: None,
        ids: None,
        env: None,
        certificate: Some("{{ .Artifact }}.{{ Arch }}.pem".to_string()),
        output: None,
        authenticode: None,
        verify: None,
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
    process_sign_configs(
        &binary_sign_configs,
        &mut ctx,
        &log,
        ArtifactFilter::BinaryOnly,
        "binary-sign",
    )
    .unwrap();

    // Registered names may be target-qualified, so pin the rendered stem
    // rather than the full basename (mirrors the amd64-variant test above).
    let sigs: Vec<_> = ctx.artifacts.by_kind(ArtifactKind::Signature);
    assert_eq!(sigs.len(), 1);
    assert!(
        sigs[0].name.starts_with("myapp.armv7") && !sigs[0].name.contains("armv7v7"),
        "signature Arm-suffix idiom must append nothing on the composite \
         policy (expected stem 'myapp.armv7'): got '{}'",
        sigs[0].name
    );

    let certs: Vec<_> = ctx.artifacts.by_kind(ArtifactKind::Certificate);
    assert_eq!(certs.len(), 1);
    assert!(
        certs[0].name.starts_with("myapp.armv7"),
        "certificate `{{{{ Arch }}}}` must render the composite 'armv7', not \
         the archive-split 'arm': got '{}'",
        certs[0].name
    );
}

/// A signature template referencing `{{ Amd64 }}` must render the binary's
/// real `amd64_variant` metadata — the key the build stage writes — not a
/// dead metadata key that silently falls back to the baseline. An untagged
/// binary renders the unified `"v1"` baseline.
#[test]
fn test_binary_signs_amd64_variant_metadata_renders_in_signature_template() {
    use anodizer_core::artifact::Artifact;

    // Registered names are target-qualified, so pin the rendered stem
    // (`myapp.<variant>`) rather than the full basename.
    for (metadata, expected) in [
        (
            std::collections::HashMap::from([("amd64_variant".to_string(), "v3".to_string())]),
            "myapp.v3",
        ),
        (Default::default(), "myapp.v1"),
    ] {
        let binary_sign_cfg = SignConfig {
            verify: None,
            id: None,
            artifacts: Some("binary".to_string()),
            cmd: Some("true".to_string()),
            args: Some(vec![]),
            signature: Some("{{ .Artifact }}.{{ Amd64 }}.sig".to_string()),
            stdin: None,
            stdin_file: None,
            ids: None,
            env: None,
            certificate: None,
            output: None,
            authenticode: None,
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
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "test".to_string(),
            metadata,
            size: None,
        });

        let log = ctx.logger("binary-sign");
        let binary_sign_configs = ctx.config.binary_signs.clone();
        process_sign_configs(
            &binary_sign_configs,
            &mut ctx,
            &log,
            ArtifactFilter::BinaryOnly,
            "binary-sign",
        )
        .unwrap();

        let sigs: Vec<_> = ctx.artifacts.by_kind(ArtifactKind::Signature);
        assert_eq!(sigs.len(), 1);
        assert!(
            sigs[0].name.starts_with(expected) && sigs[0].name.ends_with(".sig"),
            "signature name must render the binary's amd64_variant \
             (expected stem '{expected}'): got '{}'",
            sigs[0].name
        );
    }
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
        authenticode: None,
        verify: None,
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
        verify: None,
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
        verify: None,
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
        verify: None,
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
            verify: None,
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
            verify: None,
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
            verify: None,
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
            verify: None,
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
        verify: None,
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
// honor SDE (and, for GPG, `--faked-system-time`, whose support the
// preflight now probes via `<cmd> --faked-system-time=...! --version`).
//
// The gpg half is exercised live below (gated on gpg + cosign
// availability, keys provisioned ephemerally via
// `core::harness_signing`). The cosign half stays `#[ignore]`: cosign's
// signature output is intentionally non-deterministic by default
// (random nonce), and deterministic-signing mode requires `--key-ref`
// with a KMS configuration the test harness cannot provision.

#[test]
#[ignore = "cosign deterministic-signing requires KMS key fixture"]
fn cosign_signature_byte_stable_for_same_sde() {
    // Sketch:
    //   - Skip if `cosign` not on PATH.
    //   - Set SOURCE_DATE_EPOCH=1715000000 on two separate sign invocations.
    //   - Assert the two `.sig` outputs are byte-identical.
    //
    // Blocked on: deterministic-signing KMS fixture (a KMS-backed
    // `--key-ref` is the only cosign mode with a stable nonce).
}

#[test]
fn gpg_signature_byte_stable_for_same_sde() {
    use std::process::Command;

    // Key provisioning spawns BOTH gpg and cosign, so gate on both.
    for tool in ["gpg", "cosign"] {
        match anodizer_core::tool_detect::runs(tool) {
            anodizer_core::tool_detect::ToolProbe::Available => {}
            probe => {
                eprintln!("skipping gpg_signature_byte_stable_for_same_sde: {tool}={probe:?}");
                return;
            }
        }
    }
    let sde: i64 = 1_715_000_000;
    let keys = anodizer_core::harness_signing::provision_ephemeral_keys(sde)
        .expect("provision ephemeral gpg keypair");

    let tmp = tempfile::tempdir().expect("tempdir for gpg sign payload");
    let payload = tmp.path().join("payload.txt");
    std::fs::write(&payload, b"byte-stability probe payload").expect("write payload");

    let sign = |sig_name: &str| -> Vec<u8> {
        let sig = tmp.path().join(sig_name);
        let out = Command::new("gpg")
            .env("GNUPGHOME", &keys.gnupg_home)
            .args([
                "--batch",
                "--yes",
                "--local-user",
                &keys.gpg_fingerprint,
                &format!("--faked-system-time={sde}!"),
                "--output",
            ])
            .arg(&sig)
            .arg("--detach-sig")
            .arg(&payload)
            .output()
            .expect("spawn gpg --detach-sig");
        assert!(
            out.status.success(),
            "gpg sign failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::read(&sig).expect("read detached signature")
    };

    // Same payload + same pinned timestamp + EdDSA (deterministic per
    // RFC 8032) must yield byte-identical detached signatures.
    let first = sign("payload.sig.1");
    let second = sign("payload.sig.2");
    assert_eq!(
        first, second,
        "detached gpg signatures must be byte-identical under a pinned --faked-system-time"
    );
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

// ---------------------------------------------------------------------------
// Authenticode (Windows PE/MSI/DLL) signing backend
// ---------------------------------------------------------------------------

mod authenticode {
    use super::*;
    use anodizer_core::artifact::Artifact;
    use anodizer_core::config::AuthenticodeConfig;
    use anodizer_core::log::LogCapture;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use crate::helpers::{
        build_authenticode_argv, redact_password_in_argv, windows_artifact_extension_matches,
    };

    /// A sign config carrying an Authenticode block (everything else default).
    fn authenticode_sign(authenticode: AuthenticodeConfig) -> SignConfig {
        SignConfig {
            id: Some("authenticode".to_string()),
            authenticode: Some(authenticode),
            ..Default::default()
        }
    }

    fn add_artifact(
        ctx: &mut anodizer_core::context::Context,
        kind: ArtifactKind,
        rel_path: &str,
        crate_name: &str,
        target: Option<&str>,
    ) {
        let path = ctx.config.dist.join(rel_path);
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        ctx.artifacts.add(Artifact {
            kind,
            name,
            path,
            target: target.map(str::to_string),
            crate_name: crate_name.to_string(),
            metadata: HashMap::new(),
            size: None,
        });
    }

    // ---- pure argv builder ----

    #[test]
    fn osslsigncode_argv_is_exact() {
        let argv = build_authenticode_argv(
            "osslsigncode",
            "/certs/win.p12",
            Some("hunter2"),
            "http://timestamp.digicert.com",
            Some("Acme Corp"),
            Some("https://acme.example"),
            "/dist/myapp.exe",
            "/dist/myapp.exe.authenticode-tmp",
        );
        assert_eq!(
            argv,
            vec![
                "sign",
                "-pkcs12",
                "/certs/win.p12",
                "-pass",
                "hunter2",
                "-n",
                "Acme Corp",
                "-i",
                "https://acme.example",
                "-ts",
                "http://timestamp.digicert.com",
                "-in",
                "/dist/myapp.exe",
                "-out",
                "/dist/myapp.exe.authenticode-tmp",
            ]
        );
    }

    #[test]
    fn osslsigncode_argv_omits_optional_flags() {
        // No password, name, or url → those flags vanish, but -ts/-in/-out
        // remain.
        let argv = build_authenticode_argv(
            "osslsigncode",
            "/c.pfx",
            None,
            "http://timestamp.digicert.com",
            None,
            None,
            "/d/x.msi",
            "/d/x.msi.authenticode-tmp",
        );
        assert_eq!(
            argv,
            vec![
                "sign",
                "-pkcs12",
                "/c.pfx",
                "-ts",
                "http://timestamp.digicert.com",
                "-in",
                "/d/x.msi",
                "-out",
                "/d/x.msi.authenticode-tmp",
            ]
        );
    }

    #[test]
    fn signtool_argv_is_exact() {
        // signtool signs in place — `out_tmp` is ignored, no -out token.
        let argv = build_authenticode_argv(
            "signtool",
            "C:\\certs\\win.pfx",
            Some("hunter2"),
            "http://timestamp.digicert.com",
            Some("Acme Corp"),
            Some("https://acme.example"),
            "C:\\dist\\myapp.exe",
            "C:\\dist\\myapp.exe.authenticode-tmp",
        );
        assert_eq!(
            argv,
            vec![
                "sign",
                "/f",
                "C:\\certs\\win.pfx",
                "/p",
                "hunter2",
                "/fd",
                "sha256",
                "/tr",
                "http://timestamp.digicert.com",
                "/td",
                "sha256",
                "/d",
                "Acme Corp",
                "/du",
                "https://acme.example",
                "C:\\dist\\myapp.exe",
            ]
        );
        assert!(
            !argv.iter().any(|a| a == "-out" || a == "/out"),
            "signtool signs in place; no -out token"
        );
    }

    #[test]
    fn default_timestamp_url_is_the_documented_constant() {
        assert_eq!(
            AuthenticodeConfig::default().resolved_timestamp_url(),
            "http://timestamp.digicert.com"
        );
        let overridden = AuthenticodeConfig {
            timestamp_url: Some("http://timestamp.sectigo.com".to_string()),
            ..Default::default()
        };
        assert_eq!(
            overridden.resolved_timestamp_url(),
            "http://timestamp.sectigo.com"
        );
    }

    #[test]
    fn password_is_masked_in_dry_run_echo() {
        let argv = build_authenticode_argv(
            "osslsigncode",
            "/c.p12",
            Some("s3cr3t-pw"),
            "http://timestamp.digicert.com",
            None,
            None,
            "/d/app.exe",
            "/d/app.exe.authenticode-tmp",
        );
        let echo = redact_password_in_argv(&argv);
        assert!(
            !echo.contains("s3cr3t-pw"),
            "password must not leak: {echo}"
        );
        assert!(echo.contains("***"), "masked form must appear: {echo}");
        assert!(
            echo.contains("-pkcs12 /c.p12"),
            "non-secret args survive: {echo}"
        );
    }

    #[test]
    fn redact_password_in_argv_does_not_mangle_unrelated_tokens() {
        // A password equal to the `sign` subcommand token (W1): slot-level
        // masking replaces ONLY the `-pass` value, never the matching subcommand
        // or any other token that happens to equal the password string.
        let argv = build_authenticode_argv(
            "osslsigncode",
            "/c.p12",
            Some("sign"),
            "http://timestamp.digicert.com",
            None,
            None,
            "/d/sign.exe",
            "/d/sign.exe.authenticode-tmp",
        );
        let echo = redact_password_in_argv(&argv);
        assert!(
            echo.starts_with("sign -pkcs12 /c.p12 -pass ***"),
            "subcommand + cert survive, only the -pass slot is masked: {echo}"
        );
        assert!(
            echo.contains("-in /d/sign.exe"),
            "the artifact path token equal to the password is not corrupted: {echo}"
        );
        // Exactly one masked slot — the blind replace would have produced three.
        assert_eq!(
            echo.matches("***").count(),
            1,
            "only the password slot is masked: {echo}"
        );
    }

    #[test]
    fn redact_password_in_argv_masks_signtool_slash_p_slot() {
        let argv = build_authenticode_argv(
            "signtool",
            "C:\\c.pfx",
            Some("p"),
            "http://timestamp.digicert.com",
            None,
            None,
            "C:\\app.exe",
            "C:\\app.exe.authenticode-tmp",
        );
        let echo = redact_password_in_argv(&argv);
        assert!(echo.contains("/p ***"), "signtool /p slot masked: {echo}");
        assert_eq!(echo.matches("***").count(), 1, "only the /p slot: {echo}");
    }

    // ---- extension filter ----

    #[test]
    fn windows_extension_filter_selects_pe_msi_dll_only() {
        assert!(windows_artifact_extension_matches(std::path::Path::new(
            "/d/app.exe"
        )));
        assert!(windows_artifact_extension_matches(std::path::Path::new(
            "/d/app.EXE"
        )));
        assert!(windows_artifact_extension_matches(std::path::Path::new(
            "/d/inst.msi"
        )));
        assert!(windows_artifact_extension_matches(std::path::Path::new(
            "/d/lib.dll"
        )));
        // Linux ELF binary (no extension) and a tarball are rejected.
        assert!(!windows_artifact_extension_matches(std::path::Path::new(
            "/d/myapp_linux_amd64"
        )));
        assert!(!windows_artifact_extension_matches(std::path::Path::new(
            "/d/app.tar.gz"
        )));
    }

    #[test]
    fn windows_kind_prefilter_admits_binary_installer_library() {
        // The kind pre-filter admits the container kinds; extension refines.
        assert!(should_sign(ArtifactKind::Binary, "windows").unwrap());
        assert!(should_sign(ArtifactKind::Installer, "windows").unwrap());
        assert!(should_sign(ArtifactKind::Library, "windows").unwrap());
        assert!(!should_sign(ArtifactKind::Archive, "windows").unwrap());
        assert!(!should_sign(ArtifactKind::Checksum, "windows").unwrap());
    }

    // ---- end-to-end selection (dry-run) ----

    /// Run the sign stage in dry-run with an Authenticode config + a cert env
    /// var, returning the captured log lines.
    fn run_dry(ctx: &mut anodizer_core::context::Context) -> LogCapture {
        let capture = LogCapture::new();
        ctx.with_log_capture(capture.clone());
        SignStage.run(ctx).expect("sign stage run");
        capture
    }

    #[test]
    fn selects_exe_msi_dll_and_skips_non_windows() {
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![authenticode_sign(AuthenticodeConfig::default())])
            .env("WINDOWS_CERT_FILE", "/certs/win.p12")
            .env("WINDOWS_CERT_PASSWORD", "pw")
            .build();
        add_artifact(
            &mut ctx,
            ArtifactKind::Binary,
            "myapp.exe",
            "myapp",
            Some("x86_64-pc-windows-msvc"),
        );
        add_artifact(
            &mut ctx,
            ArtifactKind::Installer,
            "myapp.msi",
            "myapp",
            None,
        );
        add_artifact(&mut ctx, ArtifactKind::Library, "plugin.dll", "myapp", None);
        // Non-windows artifacts that must be skipped.
        add_artifact(
            &mut ctx,
            ArtifactKind::Binary,
            "myapp_linux_amd64",
            "myapp",
            Some("x86_64-unknown-linux-gnu"),
        );
        add_artifact(
            &mut ctx,
            ArtifactKind::Archive,
            "myapp.tar.gz",
            "myapp",
            None,
        );

        let cap = run_dry(&mut ctx);
        let msgs = cap.all_messages();
        let signed: Vec<&String> = msgs
            .iter()
            .map(|(_, m)| m)
            .filter(|m| m.starts_with("authenticode-signed "))
            .collect();
        assert!(
            signed.iter().any(|m| m.ends_with("myapp.exe")),
            "exe signed: {signed:?}"
        );
        assert!(
            signed.iter().any(|m| m.ends_with("myapp.msi")),
            "msi signed: {signed:?}"
        );
        assert!(
            signed.iter().any(|m| m.ends_with("plugin.dll")),
            "dll signed: {signed:?}"
        );
        assert!(
            !signed.iter().any(|m| m.contains("linux")),
            "linux ELF skipped: {signed:?}"
        );
        assert!(
            !signed.iter().any(|m| m.contains("tar.gz")),
            "archive skipped: {signed:?}"
        );
        assert_eq!(
            signed.len(),
            3,
            "exactly the three windows artifacts: {signed:?}"
        );
    }

    #[test]
    fn missing_cert_not_required_skips_gracefully() {
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![authenticode_sign(AuthenticodeConfig::default())])
            .sealed_env()
            .build();
        add_artifact(&mut ctx, ArtifactKind::Binary, "myapp.exe", "myapp", None);

        let before = ctx.artifacts.all().len();
        let cap = run_dry(&mut ctx);
        // No error, no signature artifact registered, artifact count unchanged.
        assert_eq!(ctx.artifacts.all().len(), before, "no new artifact on skip");
        assert_eq!(
            ctx.artifacts.by_kind(ArtifactKind::Signature).len(),
            0,
            "authenticode never registers a detached signature"
        );
        assert!(
            cap.all_messages()
                .iter()
                .any(|(_, m)| m.contains("no Authenticode cert")),
            "graceful-skip note logged: {:?}",
            cap.all_messages()
        );
    }

    #[test]
    fn missing_cert_required_hard_fails_naming_env_var() {
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![authenticode_sign(AuthenticodeConfig {
                required: Some(true),
                ..Default::default()
            })])
            .sealed_env()
            .build();
        add_artifact(&mut ctx, ArtifactKind::Binary, "myapp.exe", "myapp", None);

        let err = SignStage
            .run(&mut ctx)
            .expect_err("required cert missing must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("WINDOWS_CERT_FILE"),
            "error names the cert env var: {msg}"
        );
        assert!(
            msg.contains("required"),
            "error states the requirement: {msg}"
        );
    }

    #[test]
    fn cert_resolves_from_literal_cert_file() {
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![authenticode_sign(AuthenticodeConfig {
                cert_file: Some("/explicit/cert.pfx".to_string()),
                ..Default::default()
            })])
            .sealed_env()
            .build();
        add_artifact(&mut ctx, ArtifactKind::Binary, "myapp.exe", "myapp", None);

        let cap = run_dry(&mut ctx);
        let echo = cap
            .all_messages()
            .into_iter()
            .map(|(_, m)| m)
            .find(|m| m.contains("(dry-run) would run:"))
            .expect("dry-run echo present");
        assert!(
            echo.contains("/explicit/cert.pfx"),
            "literal cert path used: {echo}"
        );
    }

    #[test]
    fn cert_resolves_from_cert_env_var() {
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![authenticode_sign(AuthenticodeConfig {
                cert_env: Some("MY_CERT_PATH".to_string()),
                ..Default::default()
            })])
            .env("MY_CERT_PATH", "/env/cert.p12")
            .build();
        add_artifact(&mut ctx, ArtifactKind::Binary, "myapp.exe", "myapp", None);

        let cap = run_dry(&mut ctx);
        let echo = cap
            .all_messages()
            .into_iter()
            .map(|(_, m)| m)
            .find(|m| m.contains("(dry-run) would run:"))
            .expect("dry-run echo present");
        assert!(
            echo.contains("/env/cert.p12"),
            "cert path from env var used: {echo}"
        );
    }

    #[test]
    fn password_masked_in_stage_dry_run_echo() {
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![authenticode_sign(AuthenticodeConfig::default())])
            .env("WINDOWS_CERT_FILE", "/certs/win.p12")
            .env("WINDOWS_CERT_PASSWORD", "topsecret-pw")
            .build();
        add_artifact(&mut ctx, ArtifactKind::Binary, "myapp.exe", "myapp", None);

        let cap = run_dry(&mut ctx);
        let echo = cap
            .all_messages()
            .into_iter()
            .map(|(_, m)| m)
            .find(|m| m.contains("(dry-run) would run:"))
            .expect("dry-run echo present");
        assert!(
            !echo.contains("topsecret-pw"),
            "password must not appear in echo: {echo}"
        );
        assert!(
            echo.contains("***"),
            "masked password marker present: {echo}"
        );
    }

    #[test]
    fn no_detached_signature_artifact_registered() {
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![authenticode_sign(AuthenticodeConfig::default())])
            .env("WINDOWS_CERT_FILE", "/certs/win.p12")
            .build();
        add_artifact(&mut ctx, ArtifactKind::Binary, "myapp.exe", "myapp", None);
        let before = ctx.artifacts.all().len();

        run_dry(&mut ctx);

        assert_eq!(
            ctx.artifacts.all().len(),
            before,
            "authenticode mutates in place — registers no new artifact"
        );
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Signature).len(), 0);
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Certificate).len(), 0);
    }

    #[test]
    fn per_crate_mode_windows_binary_selected_and_signed() {
        // Workspace per-crate axis: the iterated artifact carries a crate_name
        // and target; the authenticode branch must still select + sign it.
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![authenticode_sign(AuthenticodeConfig::default())])
            .env("WINDOWS_CERT_FILE", "/certs/win.p12")
            .build();
        add_artifact(
            &mut ctx,
            ArtifactKind::Binary,
            "toolkit.exe",
            "toolkit-crate",
            Some("x86_64-pc-windows-msvc"),
        );

        let cap = run_dry(&mut ctx);
        assert!(
            cap.all_messages()
                .iter()
                .any(|(_, m)| m == "authenticode-signed toolkit.exe"),
            "per-crate windows binary signed: {:?}",
            cap.all_messages()
        );
    }

    // ---- preflight env requirements (W2/W3/W4) ----

    use anodizer_core::EnvRequirement;

    fn reqs_for(signs: Vec<SignConfig>) -> Vec<EnvRequirement> {
        let ctx = TestContextBuilder::new().signs(signs).build();
        crate::sign_env_requirements(&ctx)
    }

    fn declares_tool(reqs: &[EnvRequirement], tool: &str) -> bool {
        reqs.iter()
            .any(|r| matches!(r, EnvRequirement::Tool { name } if name == tool))
    }

    fn declares_env_var(reqs: &[EnvRequirement], var: &str) -> bool {
        reqs.iter().any(
            |r| matches!(r, EnvRequirement::EnvAllOf { vars } if vars.iter().any(|v| v == var)),
        )
    }

    #[test]
    fn preflight_not_required_declares_nothing() {
        // W2: a non-required authenticode config may skip when the tool/cert are
        // absent, so preflight must declare NOTHING — not even the tool.
        let reqs = reqs_for(vec![authenticode_sign(AuthenticodeConfig::default())]);
        assert!(reqs.is_empty(), "non-required declares nothing: {reqs:?}");
    }

    #[test]
    fn preflight_required_with_cert_env_requires_tool_and_cert_env_only() {
        // W3: the password env var is NEVER required (passwordless .p12 is
        // valid). W2: the tool IS required when the config will run.
        let reqs = reqs_for(vec![authenticode_sign(AuthenticodeConfig {
            required: Some(true),
            ..Default::default()
        })]);
        assert!(
            declares_tool(&reqs, AuthenticodeConfig::default().resolved_tool()),
            "required declares the tool: {reqs:?}"
        );
        assert!(
            declares_env_var(&reqs, "WINDOWS_CERT_FILE"),
            "required (no cert_file) declares the cert env var: {reqs:?}"
        );
        assert!(
            !declares_env_var(&reqs, "WINDOWS_CERT_PASSWORD"),
            "the password env var is optional and must NOT be required: {reqs:?}"
        );
    }

    #[test]
    fn preflight_required_with_cert_file_omits_cert_env() {
        // W4: a literal cert_file supplies the cert directly, so requiring the
        // cert ENV VAR would be a false-positive preflight failure.
        let reqs = reqs_for(vec![authenticode_sign(AuthenticodeConfig {
            required: Some(true),
            cert_file: Some("/explicit/cert.pfx".to_string()),
            ..Default::default()
        })]);
        assert!(
            declares_tool(&reqs, AuthenticodeConfig::default().resolved_tool()),
            "required declares the tool: {reqs:?}"
        );
        assert!(
            !declares_env_var(&reqs, "WINDOWS_CERT_FILE"),
            "cert_file present → cert env var must NOT be required: {reqs:?}"
        );
        assert!(
            !declares_env_var(&reqs, "WINDOWS_CERT_PASSWORD"),
            "password env var never required: {reqs:?}"
        );
    }

    // ---- non-dry-run rename lifecycle (S1/S2) ----

    #[cfg(unix)]
    fn run_authenticode_live(
        tool: PathBuf,
        dist: &std::path::Path,
        artifact_rel: &str,
    ) -> (anodizer_core::context::Context, anyhow::Result<()>) {
        let mut ctx = TestContextBuilder::new()
            .dist(dist.to_path_buf())
            .signs(vec![authenticode_sign(AuthenticodeConfig {
                // The fake signer is not named `signtool*`, so it takes the
                // osslsigncode (`-in`/`-out` + rename-after) lifecycle.
                tool: Some(tool.to_string_lossy().to_string()),
                cert_file: Some(dist.join("dummy.p12").to_string_lossy().to_string()),
                ..Default::default()
            })])
            .build();
        add_artifact(
            &mut ctx,
            ArtifactKind::Binary,
            artifact_rel,
            "myapp",
            Some("x86_64-pc-windows-msvc"),
        );
        let result = SignStage.run(&mut ctx);
        (ctx, result)
    }

    /// No `.authenticode-tmp` sibling remains under `dist` after a run.
    #[cfg(unix)]
    fn no_temp_litter(dist: &std::path::Path) -> bool {
        std::fs::read_dir(dist)
            .expect("read dist")
            .filter_map(Result::ok)
            .all(|e| {
                !e.file_name()
                    .to_string_lossy()
                    .contains(".authenticode-tmp")
            })
    }

    #[test]
    #[cfg(unix)]
    fn rename_after_success_replaces_original_with_signed_bytes() {
        // S1 success path: a fake signer that writes the `-out` temp and exits 0
        // must have its temp atomically renamed over the original artifact.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path();
        std::fs::write(dist.join("dummy.p12"), b"cert").expect("cert");
        std::fs::write(dist.join("myapp.exe"), b"ORIGINAL").expect("artifact");

        // osslsigncode-shaped fake: the `-out` path is the last argv element.
        let signer = write_script(
            dist,
            "fakesigner.sh",
            "#!/bin/sh\nout=\"\"\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = \"-out\" ]; then out=\"$2\"; fi\n  shift\ndone\nprintf 'SIGNED' > \"$out\"\nexit 0\n",
        );

        let (_ctx, result) = run_authenticode_live(signer, dist, "myapp.exe");
        result.expect("live authenticode sign succeeds");

        let signed = std::fs::read(dist.join("myapp.exe")).expect("read signed");
        assert_eq!(signed, b"SIGNED", "original replaced with signed bytes");
        assert!(
            no_temp_litter(dist),
            "no .authenticode-tmp remains on success"
        );
    }

    #[test]
    #[cfg(unix)]
    fn failed_sign_leaves_original_untouched_and_no_litter() {
        // S1 failure path + S2: a fake signer that writes a PARTIAL `-out` then
        // exits non-zero must (a) leave the ORIGINAL unclobbered and (b) leave
        // no `.authenticode-tmp` litter behind.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path();
        std::fs::write(dist.join("dummy.p12"), b"cert").expect("cert");
        std::fs::write(dist.join("myapp.exe"), b"ORIGINAL").expect("artifact");

        let signer = write_script(
            dist,
            "failsigner.sh",
            "#!/bin/sh\nout=\"\"\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = \"-out\" ]; then out=\"$2\"; fi\n  shift\ndone\nprintf 'PARTIAL' > \"$out\"\necho 'signer error' 1>&2\nexit 3\n",
        );

        let (_ctx, result) = run_authenticode_live(signer, dist, "myapp.exe");
        assert!(result.is_err(), "non-zero signer exit propagates an error");

        let original = std::fs::read(dist.join("myapp.exe")).expect("read original");
        assert_eq!(
            original, b"ORIGINAL",
            "original is NOT clobbered on failure"
        );
        assert!(
            no_temp_litter(dist),
            "partial .authenticode-tmp is cleaned up"
        );
    }

    #[test]
    #[cfg(unix)]
    fn cert_file_not_found_not_required_skips_in_live_mode() {
        // A configured-but-absent cert FILE is the same "no usable signing
        // material" condition as an absent cert env var, so in live (non-dry-run)
        // mode with `required: false` it must skip gracefully — NOT fall through
        // and hard-error on the signer failing to open the path.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path();
        let missing = dist.join("does-not-exist.p12");
        let mut ctx = TestContextBuilder::new()
            .dist(dist.to_path_buf())
            .signs(vec![authenticode_sign(AuthenticodeConfig {
                cert_file: Some(missing.to_string_lossy().to_string()),
                ..Default::default()
            })])
            .build();
        add_artifact(
            &mut ctx,
            ArtifactKind::Binary,
            "myapp.exe",
            "myapp",
            Some("x86_64-pc-windows-msvc"),
        );

        let before = ctx.artifacts.all().len();
        SignStage
            .run(&mut ctx)
            .expect("absent cert file (not required) skips without error");
        assert_eq!(
            ctx.artifacts.all().len(),
            before,
            "no artifact mutated on skip"
        );
    }

    #[test]
    #[cfg(unix)]
    fn cert_file_not_found_required_hard_fails_naming_path() {
        // The mirror of the skip above: `required: true` with an absent cert
        // FILE must hard-fail with a clear message naming the missing path,
        // rather than a generic non-zero-exit from the signer.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path();
        let missing = dist.join("does-not-exist.p12");
        let mut ctx = TestContextBuilder::new()
            .dist(dist.to_path_buf())
            .signs(vec![authenticode_sign(AuthenticodeConfig {
                cert_file: Some(missing.to_string_lossy().to_string()),
                required: Some(true),
                ..Default::default()
            })])
            .build();
        add_artifact(
            &mut ctx,
            ArtifactKind::Binary,
            "myapp.exe",
            "myapp",
            Some("x86_64-pc-windows-msvc"),
        );

        let err = SignStage
            .run(&mut ctx)
            .expect_err("absent cert file (required) must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does-not-exist.p12") && msg.contains("does not exist"),
            "error names the missing cert path: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn password_env_var_is_stripped_from_signer_child_env() {
        // Defense-in-depth: the cert password reaches the signer ONLY via argv
        // (`-pass`/`/p`), never as an inherited env var. `Command` does not
        // `env_clear`, so a password present in the real parent env would
        // otherwise be inherited by the child; `env_remove` strips it. Prove the
        // child cannot read the configured `password_env` from its environment
        // even though it is set in the real parent env — while signing still
        // succeeds (the signer gets the password from argv).
        //
        // A uniquely-named real env var keeps this from colliding with any other
        // test; precedent for `unsafe set_var` in tests: stage-makeself.
        const PW_ENV: &str = "ANODIZER_TEST_AUTHENTICODE_PW_STRIP";
        let tmp = tempfile::tempdir().expect("tempdir");
        let dist = tmp.path();
        std::fs::write(dist.join("dummy.p12"), b"cert").expect("cert");
        std::fs::write(dist.join("myapp.exe"), b"ORIGINAL").expect("artifact");

        // osslsigncode-shaped fake that dumps its environment to a sentinel file
        // before writing the `-out` temp.
        let env_dump = dist.join("child-env.txt");
        let signer = write_script(
            dist,
            "envdumpsigner.sh",
            &format!(
                "#!/bin/sh\nenv > \"{}\"\nout=\"\"\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = \"-out\" ]; then out=\"$2\"; fi\n  shift\ndone\nprintf 'SIGNED' > \"$out\"\nexit 0\n",
                env_dump.display()
            ),
        );

        let mut ctx = TestContextBuilder::new()
            .dist(dist.to_path_buf())
            .signs(vec![authenticode_sign(AuthenticodeConfig {
                tool: Some(signer.to_string_lossy().to_string()),
                cert_file: Some(dist.join("dummy.p12").to_string_lossy().to_string()),
                password_env: Some(PW_ENV.to_string()),
                ..Default::default()
            })])
            // ctx reads the password through the injected env source so the
            // job records `password.is_some()` and populates `env_remove`.
            .env(PW_ENV, "child-must-not-see-this")
            .build();
        add_artifact(
            &mut ctx,
            ArtifactKind::Binary,
            "myapp.exe",
            "myapp",
            Some("x86_64-pc-windows-msvc"),
        );

        // The child inherits the REAL parent env, not ctx's injected source, so
        // the var must exist in the real env for the strip to be observable.
        unsafe { std::env::set_var(PW_ENV, "child-must-not-see-this") };
        let result = SignStage.run(&mut ctx);
        unsafe { std::env::remove_var(PW_ENV) };

        result.expect("signing succeeds (password supplied via argv)");
        let child_env = std::fs::read_to_string(&env_dump).expect("read child env dump");
        assert!(
            !child_env.contains(PW_ENV),
            "password env var must be stripped from the signer's child env:\n{child_env}"
        );
        assert_eq!(
            std::fs::read(dist.join("myapp.exe")).expect("read signed"),
            b"SIGNED",
            "artifact still signed despite env strip"
        );
    }

    #[test]
    fn password_redacted_via_synthetic_key_independent_of_env_name() {
        // B1: `execute_sign_job` builds its redaction set from
        // `env + redact_extra + process-env`. The Authenticode path puts the
        // password into `redact_extra` under the synthetic guaranteed-secret key
        // `AUTHENTICODE_PASSWORD`, so `redact::string` masks it REGARDLESS of the
        // user's `password_env` name — even one with no secret suffix. This is
        // the exact composition the live spawn path feeds; proving it here
        // pins the decoupling without depending on per-thread log capture.
        //
        // Contrast the OLD behavior: the password was scrubbed only when the
        // user's `password_env` key (e.g. `WINDOWS_CERT_PASSWORD`) ended in a
        // secret suffix; a key like `MY_CERT_PW` would have leaked it.
        let pw = "leaky-secret-pw";
        let redact_extra = vec![("AUTHENTICODE_PASSWORD".to_string(), pw.to_string())];
        // No child env, but the user named the password under a NON-secret key.
        let env_pairs: Vec<(String, String)> = redact_extra.clone();
        let echoed = format!("osslsigncode: error reading -pass {pw}");
        let masked = anodizer_core::redact::string(&echoed, &env_pairs);
        assert!(
            !masked.contains(pw),
            "password masked under synthetic key regardless of env name: {masked}"
        );
        assert_eq!(
            masked,
            "osslsigncode: error reading -pass $AUTHENTICODE_PASSWORD"
        );

        // Sanity floor: the non-secret env KEY alone would NOT have masked it —
        // this is precisely the leak B1 closed.
        let leaky_env = vec![("MY_CERT_PW".to_string(), pw.to_string())];
        let unmasked = anodizer_core::redact::string(&echoed, &leaky_env);
        assert!(
            unmasked.contains(pw),
            "the non-secret env key does NOT mask — `redact_extra` is what saves us: {unmasked}"
        );
    }
}

/// Keyless cosign's first invocation on a fresh host initializes the
/// `~/.sigstore` TUF trust root under an exclusive flock; concurrent
/// first-wave workers lose that lock with `creating cached local store:
/// resource temporarily unavailable` (cfgd v0.5.0 run 28853272910, Publish
/// leg, 0/22 signed). These tests pin the two halves of the fix: the first
/// sign is serialized to warm the cache, and transient cosign failures are
/// retried across a multi-second window instead of fast-failing.
#[cfg(unix)]
mod cosign_tuf_race {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use std::path::Path;

    /// A keyless cosign sign config pointed at a stub script named `cosign`,
    /// with the stub's state directory exported in the child env.
    fn stub_signs(stub: &Path, state: &Path) -> Vec<SignConfig> {
        vec![SignConfig {
            verify: None,
            id: Some("cosign-keyless".to_string()),
            cmd: Some(stub.to_string_lossy().into_owned()),
            args: Some(vec![
                "sign-blob".to_string(),
                "--output-signature".to_string(),
                "{{ .Signature }}".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            artifacts: Some("all".to_string()),
            ids: None,
            signature: None,
            stdin: None,
            stdin_file: None,
            env: Some(vec![format!("STUB_STATE={}", state.display())]),
            certificate: None,
            output: None,
            authenticode: None,
            if_condition: None,
        }]
    }

    fn add_archives(ctx: &mut anodizer_core::context::Context, dir: &Path, n: usize) {
        for i in 0..n {
            let path = dir.join(format!("myapp-{i}.tar.gz"));
            std::fs::write(&path, format!("artifact {i}")).unwrap();
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Archive,
                name: format!("myapp-{i}.tar.gz"),
                path,
                target: None,
                crate_name: "myapp".to_string(),
                metadata: Default::default(),
                size: None,
            });
        }
    }

    fn build_ctx(stub: &Path, state: &Path) -> anodizer_core::context::Context {
        TestContextBuilder::new()
            .dry_run(false)
            .parallelism(4)
            .signs(stub_signs(stub, state))
            .sealed_env()
            .build()
    }

    /// A stub cosign that logs start/end wall-clock nanos per invocation to
    /// `$STUB_STATE/events`, sleeping 150ms in between so overlap is
    /// observable.
    fn write_events_stub(dir: &Path) -> std::path::PathBuf {
        write_script(
            dir,
            "cosign",
            concat!(
                "#!/bin/sh\n",
                "echo \"start $(date +%s%N)\" >> \"$STUB_STATE/events\"\n",
                "sleep 0.15\n",
                "echo \"end $(date +%s%N)\" >> \"$STUB_STATE/events\"\n",
                "printf sig > \"$3\"\n",
                "exit 0\n",
            ),
        )
    }

    /// Parse the events log the stub above writes into (starts, ends).
    fn read_events(state: &Path) -> (Vec<u128>, Vec<u128>) {
        let events = std::fs::read_to_string(state.join("events")).expect("events log");
        let mut starts: Vec<u128> = Vec::new();
        let mut ends: Vec<u128> = Vec::new();
        for line in events.lines() {
            let (kind, ts) = line.split_once(' ').expect("event shape");
            let ts: u128 = ts.trim().parse().expect("timestamp");
            match kind {
                "start" => starts.push(ts),
                "end" => ends.push(ts),
                other => panic!("unexpected event kind {other}"),
            }
        }
        (starts, ends)
    }

    /// The production failure, reduced: a stub cosign whose cache-init is an
    /// atomic one-shot (`mkdir` = the flock), held open for 500ms. Any
    /// instance that starts before the warm marker exists and doesn't own the
    /// lock fails exactly like cosign's concurrent TUF-init race. The stage
    /// must sign all 8 artifacts anyway.
    #[test]
    fn keyless_cosign_survives_cold_tuf_cache_with_parallel_fan_out() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let stub = write_script(
            tmp.path(),
            "cosign",
            concat!(
                "#!/bin/sh\n",
                "if [ ! -f \"$STUB_STATE/warm\" ]; then\n",
                "  if mkdir \"$STUB_STATE/lock\" 2>/dev/null; then\n",
                "    sleep 0.5\n",
                "    : > \"$STUB_STATE/warm\"\n",
                "  else\n",
                "    echo \"signing $4: getting key from Fulcio: getting CTFE public keys: creating cached local store: resource temporarily unavailable\" >&2\n",
                "    exit 1\n",
                "  fi\n",
                "fi\n",
                "printf sig > \"$3\"\n",
                "exit 0\n",
            ),
        );

        let mut ctx = build_ctx(&stub, &state);
        add_archives(&mut ctx, tmp.path(), 8);

        SignStage.run(&mut ctx).expect(
            "cold-TUF-cache race: every artifact must sign (first sign serialized, rest fanned out)",
        );
        assert_eq!(
            ctx.artifacts.by_kind(ArtifactKind::Signature).len(),
            8,
            "all 8 artifacts must carry a signature"
        );
    }

    /// Pins the serialization itself: the FIRST cosign invocation must
    /// complete before any second one starts. The stub logs start/end
    /// wall-clock nanos per invocation; without the serial warm-up, a
    /// parallelism-4 fan-out records 4 starts before the earliest end.
    #[test]
    fn keyless_cosign_first_invocation_completes_before_fan_out() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let stub = write_events_stub(tmp.path());

        let mut ctx = build_ctx(&stub, &state);
        add_archives(&mut ctx, tmp.path(), 6);
        SignStage.run(&mut ctx).expect("all stub signs succeed");

        let (starts, ends) = read_events(&state);
        assert_eq!(starts.len(), 6, "one start per artifact");
        assert_eq!(ends.len(), 6, "one end per artifact");
        let first_end = *ends.iter().min().expect("at least one end");
        let early = starts.iter().filter(|s| **s < first_end).count();
        assert_eq!(
            early, 1,
            "exactly one cosign invocation may start before the first one \
             completes (the TUF warm-up must run alone); got {early} early \
             starts"
        );
    }

    /// Populate `cache` as a warm sigstore TUF store: trusted root, one
    /// fetched target, and a timestamp expiring `expires_offset_hours` from
    /// now (negative = already expired).
    fn populate_tuf_cache(cache: &Path, expires_offset_hours: i64) {
        std::fs::create_dir_all(cache.join("targets")).unwrap();
        std::fs::write(cache.join("root.json"), "{}").unwrap();
        std::fs::write(cache.join("targets").join("rekor.pub"), "key").unwrap();
        let expires =
            (chrono::Utc::now() + chrono::Duration::hours(expires_offset_hours)).to_rfc3339();
        std::fs::write(
            cache.join("timestamp.json"),
            format!(r#"{{"signed":{{"expires":"{expires}"}}}}"#),
        )
        .unwrap();
    }

    /// A pre-populated TUF cache (root.json + a fetched target + unexpired
    /// timestamp under `TUF_ROOT`) makes cosign's init a no-op, so the
    /// serialized warm-up must be skipped: with parallelism 4 and a 150ms
    /// stub, several invocations start before the first one ends.
    #[test]
    fn warm_tuf_cache_skips_serialized_warm_up() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let stub = write_events_stub(tmp.path());

        let cache = tmp.path().join("tuf-root");
        populate_tuf_cache(&cache, 1);

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .parallelism(4)
            .signs(stub_signs(&stub, &state))
            .env("TUF_ROOT", cache.to_string_lossy())
            .sealed_env()
            .build();
        add_archives(&mut ctx, tmp.path(), 6);
        SignStage.run(&mut ctx).expect("all stub signs succeed");

        let (starts, ends) = read_events(&state);
        let first_end = *ends.iter().min().expect("at least one end");
        let early = starts.iter().filter(|s| **s < first_end).count();
        assert!(
            early > 1,
            "a warm TUF cache must fan out immediately (no solo first sign); \
             got {early} start(s) before the first completion"
        );
    }

    /// A cold cache under `TUF_ROOT` keeps the serialized warm-up AND takes
    /// the host-level advisory lock: the sentinel appears inside the cache
    /// dir and exactly one invocation runs alone.
    #[test]
    fn cold_tuf_cache_serializes_and_creates_host_lock_sentinel() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let stub = write_events_stub(tmp.path());

        let cache = tmp.path().join("tuf-root");

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .parallelism(4)
            .signs(stub_signs(&stub, &state))
            .env("TUF_ROOT", cache.to_string_lossy())
            .sealed_env()
            .build();
        add_archives(&mut ctx, tmp.path(), 6);
        SignStage.run(&mut ctx).expect("all stub signs succeed");

        assert!(
            cache.join(".anodizer-tuf-init.lock").is_file(),
            "cold init must create the advisory-lock sentinel in the cache dir"
        );
        let (starts, ends) = read_events(&state);
        let first_end = *ends.iter().min().expect("at least one end");
        let early = starts.iter().filter(|s| **s < first_end).count();
        assert_eq!(
            early, 1,
            "cold cache must keep the serialized warm-up; got {early} early starts"
        );
    }

    /// The warm probe must consult the env the cosign CHILD sees: a
    /// `TUF_ROOT` in the sign config's `env:` entries shadows the process
    /// env. Here the process env points at a cold dir while the config env
    /// points at a warm cache — the stage must fan out immediately.
    #[test]
    fn config_env_tuf_root_shadows_process_env_for_warm_probe() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let stub = write_events_stub(tmp.path());

        let warm = tmp.path().join("warm-root");
        populate_tuf_cache(&warm, 1);
        let cold = tmp.path().join("cold-root");

        let mut signs = stub_signs(&stub, &state);
        signs[0]
            .env
            .as_mut()
            .unwrap()
            .push(format!("TUF_ROOT={}", warm.display()));

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .parallelism(4)
            .signs(signs)
            .env("TUF_ROOT", cold.to_string_lossy())
            .sealed_env()
            .build();
        add_archives(&mut ctx, tmp.path(), 6);
        SignStage.run(&mut ctx).expect("all stub signs succeed");

        let (starts, ends) = read_events(&state);
        let first_end = *ends.iter().min().expect("at least one end");
        let early = starts.iter().filter(|s| **s < first_end).count();
        assert!(
            early > 1,
            "config-env TUF_ROOT points at a warm cache, so the child sees a \
             warm store and the stage must fan out; got {early} early start(s)"
        );
        assert!(
            !cold.join(".anodizer-tuf-init.lock").exists(),
            "the lock must scope to the cache the child will use, not the \
             process-env dir"
        );
    }

    /// An otherwise-populated cache whose `timestamp.json` has expired makes
    /// cosign refresh through the same locked store as a cold init, so the
    /// warm probe must classify it COLD and keep the serialized warm-up.
    #[test]
    fn expired_tuf_timestamp_keeps_serialized_warm_up() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let stub = write_events_stub(tmp.path());

        let cache = tmp.path().join("tuf-root");
        populate_tuf_cache(&cache, -1);

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .parallelism(4)
            .signs(stub_signs(&stub, &state))
            .env("TUF_ROOT", cache.to_string_lossy())
            .sealed_env()
            .build();
        add_archives(&mut ctx, tmp.path(), 6);
        SignStage.run(&mut ctx).expect("all stub signs succeed");

        let (starts, ends) = read_events(&state);
        let first_end = *ends.iter().min().expect("at least one end");
        let early = starts.iter().filter(|s| **s < first_end).count();
        assert_eq!(
            early, 1,
            "expired timestamp must be treated as cold (serialized warm-up); \
             got {early} early starts"
        );
    }

    /// The warm probe must run AFTER the host lock is granted: while a
    /// neighbor holds the init lock (mid-initialization), the stage must
    /// wait it out rather than trusting an unlocked warm reading. The
    /// holder releases after 400ms; every cosign start must land after that
    /// release.
    #[test]
    fn warm_probe_waits_for_host_init_lock() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let stub = write_events_stub(tmp.path());

        let cache = tmp.path().join("tuf-root");
        populate_tuf_cache(&cache, 1);

        // Acquire before the stage runs so there is no startup race; flock
        // excludes across separate descriptors even within one process.
        let lock = crate::tuf_cache::TufInitLock::acquire(&cache).expect("holder acquire");
        let released_at = Arc::new(AtomicU64::new(0));
        let released = Arc::clone(&released_at);
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            // Stamp BEFORE dropping: the recorded instant lower-bounds the
            // actual release, so the assertion can only under-approximate.
            released.store(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64,
                Ordering::SeqCst,
            );
            drop(lock);
        });

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .parallelism(4)
            .signs(stub_signs(&stub, &state))
            .env("TUF_ROOT", cache.to_string_lossy())
            .sealed_env()
            .build();
        add_archives(&mut ctx, tmp.path(), 4);
        SignStage.run(&mut ctx).expect("all stub signs succeed");
        holder.join().expect("holder thread");

        let (starts, _) = read_events(&state);
        let release_ns = released_at.load(Ordering::SeqCst) as u128;
        assert!(release_ns > 0, "holder must have recorded its release");
        let earliest = *starts.iter().min().expect("at least one start");
        assert!(
            earliest >= release_ns,
            "no cosign may start while another holder owns the TUF init lock \
             (earliest start {earliest} < release {release_ns})"
        );
    }

    /// A transiently-failing cosign (fails the first attempt with the
    /// flock-EAGAIN stderr, succeeds after) must be retried to success
    /// rather than aborting the stage on the first non-zero exit.
    #[test]
    fn transient_cosign_failure_is_retried_to_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let stub = write_script(
            tmp.path(),
            "cosign",
            concat!(
                "#!/bin/sh\n",
                "n=$(cat \"$STUB_STATE/attempts\" 2>/dev/null || echo 0)\n",
                "n=$((n+1))\n",
                "echo \"$n\" > \"$STUB_STATE/attempts\"\n",
                "if [ \"$n\" -lt 2 ]; then\n",
                "  echo \"signing $4: getting key from Fulcio: getting CTFE public keys: creating cached local store: resource temporarily unavailable\" >&2\n",
                "  exit 1\n",
                "fi\n",
                "printf sig > \"$3\"\n",
                "exit 0\n",
            ),
        );

        let mut ctx = build_ctx(&stub, &state);
        add_archives(&mut ctx, tmp.path(), 1);
        SignStage
            .run(&mut ctx)
            .expect("a transient cosign failure must be retried to success");

        let attempts = std::fs::read_to_string(state.join("attempts")).expect("attempts file");
        assert_eq!(
            attempts.trim(),
            "2",
            "the stub must have been retried exactly once"
        );
    }

    /// Non-cosign signers don't talk to sigstore, so their failures are
    /// deterministic: exactly one attempt, no retry, fail fast.
    #[test]
    fn non_cosign_signer_fails_fast_without_retry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let stub = write_script(
            tmp.path(),
            "fakesigner",
            concat!(
                "#!/bin/sh\n",
                "n=$(cat \"$STUB_STATE/attempts\" 2>/dev/null || echo 0)\n",
                "n=$((n+1))\n",
                "echo \"$n\" > \"$STUB_STATE/attempts\"\n",
                "echo \"fakesigner: bad key\" >&2\n",
                "exit 1\n",
            ),
        );

        let mut ctx = build_ctx(&stub, &state);
        add_archives(&mut ctx, tmp.path(), 1);
        let result = SignStage.run(&mut ctx);
        assert!(result.is_err(), "a failing non-cosign signer must error");

        let attempts = std::fs::read_to_string(state.join("attempts")).expect("attempts file");
        assert_eq!(
            attempts.trim(),
            "1",
            "non-cosign signers must fail fast with no retry"
        );
    }
}

/// Unit pins for the cosign transient-retry machinery (platform-neutral —
/// the injected sleep means no wall-clock time is served).
mod cosign_retry_policy {
    use crate::process::{
        COSIGN_TRANSIENT_RETRY, is_keyless_cosign, retry_transient, sign_retry_delay,
    };
    use anodizer_core::log::{StageLogger, Verbosity};
    use std::time::Duration;

    fn quiet_log() -> StageLogger {
        StageLogger::new("sign", Verbosity::Quiet)
    }

    /// The backoff schedule must outlive a multi-second TUF contention
    /// window: every delay sits inside its ±20% jitter envelope, and the
    /// nominal spread before the final attempt is at least 15s (the whole
    /// point of the fix — 3 fast tries in ~2.5s lost every round). Asserted on
    /// the pure schedule helper so the envelope is checked without serving the
    /// multi-second sleeps.
    #[test]
    fn cosign_retry_delay_schedule_spans_the_contention_window() {
        let mut nominal_total = Duration::ZERO;
        let mut served_total = Duration::ZERO;
        for i in 0..COSIGN_TRANSIENT_RETRY.max_attempts - 1 {
            let next_attempt = i + 2;
            let nominal = COSIGN_TRANSIENT_RETRY.delay_for(next_attempt);
            let actual = sign_retry_delay(&COSIGN_TRANSIENT_RETRY, next_attempt);
            nominal_total += nominal;
            served_total += actual;
            assert!(
                actual >= nominal * 4 / 5 && actual <= nominal * 6 / 5,
                "delay {i} = {actual:?} outside the ±20% jitter envelope of {nominal:?}"
            );
        }
        assert!(
            nominal_total >= Duration::from_secs(15),
            "nominal retry spread must be ≥15s to outlive the TUF contention \
             window; got {nominal_total:?}"
        );
        assert!(
            served_total >= Duration::from_secs(12),
            "even fully jitter-shrunk (×0.8), the served spread stays \
             multi-second; got {served_total:?}"
        );
    }

    /// Every attempt in the policy is spent before the error surfaces. A tiny
    /// policy keeps the served backoff to a few ms while still driving the
    /// full ladder through the shared engine.
    #[test]
    fn cosign_retry_exhausts_every_attempt() {
        let log = quiet_log();
        let policy = anodizer_core::retry::RetryPolicy {
            max_attempts: COSIGN_TRANSIENT_RETRY.max_attempts,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(3),
        };
        let attempts = std::cell::Cell::new(0u32);
        let result = retry_transient(&policy, &log, "myapp.tar.gz", &mut || {
            attempts.set(attempts.get() + 1);
            anyhow::bail!("creating cached local store: resource temporarily unavailable")
        });
        assert!(result.is_err(), "exhausted retries must surface the error");
        assert_eq!(
            attempts.get(),
            COSIGN_TRANSIENT_RETRY.max_attempts,
            "every attempt in the policy must be spent"
        );
    }

    /// A missing signer binary (spawn `NotFound`) cannot heal — it must
    /// fail on the first attempt without retrying, even through anyhow
    /// context wrapping. A single attempt implies zero backoff.
    #[test]
    fn missing_binary_fast_fails_without_retry() {
        let log = quiet_log();
        let attempts = std::cell::Cell::new(0u32);

        let result = retry_transient(&COSIGN_TRANSIENT_RETRY, &log, "myapp.tar.gz", &mut || {
            attempts.set(attempts.get() + 1);
            Err(anyhow::Error::from(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "No such file or directory",
            ))
            .context("sign: failed to spawn 'cosign' for myapp.tar.gz"))
        });

        assert!(result.is_err());
        assert_eq!(attempts.get(), 1, "NotFound must not be retried");
    }

    /// First-attempt success returns immediately (a single attempt, no backoff).
    #[test]
    fn success_on_first_attempt_never_sleeps() {
        let log = quiet_log();
        let attempts = std::cell::Cell::new(0u32);
        let result = retry_transient(&COSIGN_TRANSIENT_RETRY, &log, "myapp.tar.gz", &mut || {
            attempts.set(attempts.get() + 1);
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(attempts.get(), 1);
    }

    /// The keyless discriminator drives the serial TUF warm-up: bare and
    /// path-qualified `cosign` without `--key` warm; keyed cosign and
    /// non-cosign signers must not.
    #[test]
    fn keyless_cosign_classifier() {
        let keyless = vec!["sign-blob".to_string(), "artifact.tar.gz".to_string()];
        assert!(is_keyless_cosign("cosign", &keyless));
        assert!(is_keyless_cosign("/usr/local/bin/cosign", &keyless));

        let keyed_eq = vec![
            "sign-blob".to_string(),
            "--key=env://COSIGN_KEY".to_string(),
        ];
        assert!(!is_keyless_cosign("cosign", &keyed_eq));
        let keyed_split = vec![
            "sign-blob".to_string(),
            "--key".to_string(),
            "cosign.key".to_string(),
        ];
        assert!(!is_keyless_cosign("cosign", &keyed_split));

        assert!(!is_keyless_cosign("gpg", &keyless));
        assert!(!is_keyless_cosign("/usr/bin/gpg", &keyless));
    }

    /// A deterministic signer failure (flag typo, unparseable key,
    /// identity mismatch) is identical on every re-run — it must fail on
    /// the first attempt (no retry, hence no backoff) instead of burning the
    /// ladder.
    #[test]
    fn deterministic_failure_fast_fails_without_retry() {
        for stderr in [
            "unknown flag: --keyy",
            "error: unknown command \"sing\" for \"cosign\"",
            "error: unsupported pem type: CERTIFICATE",
            "error: parsing private key: invalid pem block",
            "none of the expected identities matched what was in the certificate",
        ] {
            let log = quiet_log();
            let attempts = std::cell::Cell::new(0u32);
            let result =
                retry_transient(&COSIGN_TRANSIENT_RETRY, &log, "myapp.tar.gz", &mut || {
                    attempts.set(attempts.get() + 1);
                    Err(anyhow::anyhow!("{stderr}")
                        .context("sign: 'cosign' failed for myapp.tar.gz"))
                });
            assert!(result.is_err());
            assert_eq!(attempts.get(), 1, "{stderr:?} must not be retried");
        }
    }

    /// The classifier is fail-open: TUF/flock/network phrasings — and any
    /// unrecognized error — stay transient so retry keeps protecting the
    /// ambiguous class.
    #[test]
    fn deterministic_classifier_leaves_transient_class_alone() {
        use crate::process::is_deterministic_sign_failure;
        for transient in [
            "creating cached local store: resource temporarily unavailable",
            "getting Fulcio SCT: context deadline exceeded",
            "rekor entry: 503 Service Unavailable",
            // Cold/racing ~/.sigstore TUF-cache read — an ENOENT that heals
            // on retry; must never be classified deterministic by phrasing.
            "open /home/runner/.sigstore/root/targets/rekor.pub: no such file or directory",
            "open /missing/cosign.key: no such file or directory",
            "some brand-new phrasing nobody has seen",
        ] {
            assert!(
                !is_deterministic_sign_failure(&anyhow::anyhow!("{transient}")),
                "{transient:?} must classify as transient"
            );
        }
        assert!(is_deterministic_sign_failure(
            &anyhow::anyhow!("unknown flag: --keyy").context("sign: 'cosign' failed")
        ));
    }
}

/// Stage-level gating for post-sign verification: after each successful
/// sign, the matching verifier must run (gpg `--verify`, cosign
/// `verify-blob`), keyed cosign must derive the public key first, and every
/// skip gate (dry-run, `verify.enabled: false`, underivable keyless
/// identity) must suppress the verifier without failing the stage.
#[cfg(unix)]
mod post_sign_verification {
    use super::write_script;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{SignConfig, SignVerifyConfig};
    use anodizer_core::stage::Stage;
    use anodizer_core::test_helpers::TestContextBuilder;

    use crate::SignStage;

    /// Recording stub: appends each invocation's argv to `$STATE/calls`,
    /// creates the file named by `--output <p>` / `--output-signature <p>` /
    /// `--bundle=<p>` / `--outfile <p>` so downstream reads succeed, exits 0.
    fn recording_stub(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        write_script(
            dir,
            name,
            concat!(
                "#!/bin/sh\n",
                "echo \"$@\" >> \"$STATE/calls\"\n",
                "prev=\"\"\n",
                "for a in \"$@\"; do\n",
                "  case \"$prev\" in --output|--output-signature|--outfile) : > \"$a\" ;; esac\n",
                "  case \"$a\" in --bundle=*) : > \"${a#--bundle=}\" ;; esac\n",
                "  prev=\"$a\"\n",
                "done\n",
                "exit 0\n",
            ),
        )
    }

    fn add_archive(ctx: &mut anodizer_core::context::Context, dir: &std::path::Path) {
        let path = dir.join("app.tar.gz");
        std::fs::write(&path, b"bytes").expect("artifact bytes");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: "app.tar.gz".to_string(),
            path,
            target: None,
            crate_name: "test".to_string(),
            metadata: Default::default(),
            size: None,
        });
    }

    fn calls(state: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(state.join("calls"))
            .expect("calls log")
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn gpg_sign_config(stub: &std::path::Path, state: &std::path::Path) -> SignConfig {
        SignConfig {
            cmd: Some(stub.to_string_lossy().to_string()),
            artifacts: Some("archive".to_string()),
            args: Some(vec![
                "--output".to_string(),
                "{{ .Signature }}".to_string(),
                "--detach-sig".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            env: Some(vec![format!("STATE={}", state.display())]),
            ..Default::default()
        }
    }

    #[test]
    fn gpg_signature_is_verified_after_signing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = recording_stub(tmp.path(), "gpg");

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(vec![gpg_sign_config(&stub, &state)])
            .sealed_env()
            .build();
        add_archive(&mut ctx, tmp.path());
        SignStage.run(&mut ctx).expect("sign + verify succeed");

        let calls = calls(&state);
        assert_eq!(calls.len(), 2, "one sign + one verify: {calls:?}");
        assert!(
            calls[0].starts_with("--output"),
            "sign argv first: {calls:?}"
        );
        let artifact = tmp.path().join("app.tar.gz");
        assert_eq!(
            calls[1],
            format!("--verify {}.sig {}", artifact.display(), artifact.display()),
            "gpg verify argv"
        );
    }

    #[test]
    fn verify_disabled_by_config_skips_verification() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = recording_stub(tmp.path(), "gpg");

        let mut cfg = gpg_sign_config(&stub, &state);
        cfg.verify = Some(SignVerifyConfig {
            enabled: Some(false),
            ..Default::default()
        });
        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(vec![cfg])
            .sealed_env()
            .build();
        add_archive(&mut ctx, tmp.path());
        SignStage.run(&mut ctx).expect("sign succeeds");

        let calls = calls(&state);
        assert_eq!(calls.len(), 1, "sign only, no verify: {calls:?}");
    }

    #[test]
    fn dry_run_never_spawns_signer_or_verifier() {
        // The cmd path does not exist; dry-run must neither sign, nor derive
        // a public key, nor verify.
        let cfg = SignConfig {
            cmd: Some("/nonexistent/gpg".to_string()),
            artifacts: Some("archive".to_string()),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut ctx = TestContextBuilder::new()
            .dry_run(true)
            .signs(vec![cfg])
            .sealed_env()
            .build();
        add_archive(&mut ctx, tmp.path());
        SignStage
            .run(&mut ctx)
            .expect("dry-run must not spawn anything");
    }

    #[test]
    fn keyed_cosign_derives_pubkey_then_verifies() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = recording_stub(tmp.path(), "cosign");

        let cfg = SignConfig {
            cmd: Some(stub.to_string_lossy().to_string()),
            artifacts: Some("archive".to_string()),
            args: Some(vec![
                "sign-blob".to_string(),
                "--key=env://COSIGN_KEY".to_string(),
                "--output-signature".to_string(),
                "{{ .Signature }}".to_string(),
                "{{ .Artifact }}".to_string(),
            ]),
            env: Some(vec![format!("STATE={}", state.display())]),
            ..Default::default()
        };
        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(vec![cfg])
            .sealed_env()
            .build();
        add_archive(&mut ctx, tmp.path());
        SignStage.run(&mut ctx).expect("sign + verify succeed");

        let calls = calls(&state);
        assert_eq!(
            calls.len(),
            3,
            "public-key derivation + sign + verify: {calls:?}"
        );
        assert!(
            calls[0].starts_with("public-key --key=env://COSIGN_KEY --outfile "),
            "derivation runs first: {calls:?}"
        );
        assert!(calls[1].starts_with("sign-blob"), "then sign: {calls:?}");
        let artifact = tmp.path().join("app.tar.gz");
        assert!(
            calls[2].starts_with("verify-blob --key ")
                && calls[2].ends_with(&format!(
                    "--signature {}.sig {}",
                    artifact.display(),
                    artifact.display()
                )),
            "then verify against the derived pubkey: {calls:?}"
        );
    }

    fn keyless_bundle_config(stub: &std::path::Path, state: &std::path::Path) -> SignConfig {
        SignConfig {
            cmd: Some(stub.to_string_lossy().to_string()),
            artifacts: Some("archive".to_string()),
            args: Some(vec![
                "sign-blob".to_string(),
                "--bundle={{ Signature }}".to_string(),
                "--yes".to_string(),
                "{{ Artifact }}".to_string(),
            ]),
            env: Some(vec![format!("STATE={}", state.display())]),
            ..Default::default()
        }
    }

    #[test]
    fn keyless_missing_identity_signs_but_skips_verification() {
        // Sealed env: no GitHub Actions OIDC context and no configured
        // identity/issuer — the honest outcome is sign-without-verify, not a
        // failure and not a silent no-op of the whole config.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = recording_stub(tmp.path(), "cosign");

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(vec![keyless_bundle_config(&stub, &state)])
            .sealed_env()
            .build();
        add_archive(&mut ctx, tmp.path());
        SignStage.run(&mut ctx).expect("sign succeeds");

        let calls = calls(&state);
        assert_eq!(calls.len(), 1, "sign only, honest verify skip: {calls:?}");
        assert!(calls[0].starts_with("sign-blob"), "{calls:?}");
    }

    #[test]
    fn keyless_verifies_with_identity_derived_from_github_actions_env() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = recording_stub(tmp.path(), "cosign");

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(vec![keyless_bundle_config(&stub, &state)])
            .env("GITHUB_ACTIONS", "true")
            .env("GITHUB_SERVER_URL", "https://github.com")
            .env(
                "GITHUB_WORKFLOW_REF",
                "acme/app/.github/workflows/release.yml@refs/tags/v1.0.0",
            )
            .build();
        add_archive(&mut ctx, tmp.path());
        SignStage.run(&mut ctx).expect("sign + verify succeed");

        let calls = calls(&state);
        assert_eq!(calls.len(), 2, "one sign + one verify: {calls:?}");
        let artifact = tmp.path().join("app.tar.gz");
        let expected = format!(
            "verify-blob --bundle {art}.sig --certificate-identity \
             https://github.com/acme/app/.github/workflows/release.yml@refs/tags/v1.0.0 \
             --certificate-oidc-issuer https://token.actions.githubusercontent.com {art}",
            art = artifact.display()
        );
        assert_eq!(
            calls[1], expected,
            "keyless verify argv derives the workflow identity"
        );
    }

    #[test]
    fn failed_verification_fails_the_stage_without_retry() {
        // A verifier that reports a deterministic bad signature must fail
        // the stage after exactly one verify attempt (sign attempt + verify
        // attempt = 2 stub calls total, despite the cosign retry ladder).
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).expect("state dir");
        let stub = write_script(
            tmp.path(),
            "gpg",
            concat!(
                "#!/bin/sh\n",
                "echo \"$@\" >> \"$STATE/calls\"\n",
                "if [ \"$1\" = \"--verify\" ]; then\n",
                "  echo 'gpg: BAD signature from \"Test\"' >&2\n",
                "  exit 1\n",
                "fi\n",
                "prev=\"\"\n",
                "for a in \"$@\"; do\n",
                "  case \"$prev\" in --output) : > \"$a\" ;; esac\n",
                "  prev=\"$a\"\n",
                "done\n",
                "exit 0\n",
            ),
        );

        let mut ctx = TestContextBuilder::new()
            .dry_run(false)
            .signs(vec![gpg_sign_config(&stub, &state)])
            .sealed_env()
            .build();
        add_archive(&mut ctx, tmp.path());
        let result = SignStage.run(&mut ctx);
        assert!(result.is_err(), "a bad signature must fail the stage");
        assert!(
            format!("{:#}", result.unwrap_err()).contains("signature verification failed"),
            "error names the verification"
        );

        let calls = calls(&state);
        assert_eq!(calls.len(), 2, "one sign + one verify, no retry: {calls:?}");
    }
}
