//! Podman backend integration coverage.
//!
//! These tests drive the public surface of `anodizer-stage-docker`'s
//! podman path end-to-end at the command-construction level, mirroring
//! how the docker stage assembles argv before spawning `podman build`.
//! They exercise:
//!
//! - Linux-only enforcement on `resolve_backend(Some("podman"))`.
//! - Buildx-only flag rejection under `use: podman` (the full
//!   `BUILDX_ONLY_FLAGS` set).
//! - Argv shape of `build_docker_v2_command` with `backend = Some("podman")`
//!   — confirms buildx-only switches (`--push`, `--load`, `--attest=*`) are
//!   omitted while podman-compatible flags (`--iidfile`, `--build-arg`,
//!   `--label`, `--platform`, `--tag`) survive.
//!
//! A "real" `podman manifest push` test would require a live registry and
//! a `podman` binary on `PATH`; both are out of scope for a unit-test
//! harness. The argv-level coverage here pins the spec the surface MUST
//! adhere to, so any future regression that silently sneaks a buildx-only
//! flag through fails CI on every OS.

use anodizer_stage_docker::{
    build_podman_push_commands, enforce_podman_linux_only, resolve_backend,
    validate_podman_flag_compat,
};
// Argv-shape assertions only run on linux (podman is linux-only), so the
// builder surface they exercise is in scope solely for that target.
#[cfg(target_os = "linux")]
use anodizer_stage_docker::{DockerV2Spec, build_docker_v2_command};

#[cfg(target_os = "linux")]
#[test]
fn podman_resolves_to_podman_build_on_linux() {
    let (bin, sub) = resolve_backend(Some("podman"), false).expect("linux host resolves podman");
    assert_eq!(bin, "podman");
    assert_eq!(sub, vec!["build"]);
}

#[cfg(not(target_os = "linux"))]
#[test]
fn podman_rejected_on_non_linux_with_clear_error() {
    let err = resolve_backend(Some("podman"), false).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("Linux only"),
        "non-linux host must reject podman, got: {msg}"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn enforce_podman_linux_only_errors_on_non_linux() {
    let err = enforce_podman_linux_only().unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("Linux only"), "got: {msg}");
}

#[cfg(target_os = "linux")]
#[test]
fn enforce_podman_linux_only_ok_on_linux() {
    enforce_podman_linux_only().expect("linux host passes");
}

#[test]
fn buildx_only_flags_rejected_for_podman_path() {
    for flag in [
        "--rewrite-timestamp",
        "--sbom",
        "--sbom=true",
        "--provenance=false",
        "--attest=type=sbom",
        "--output=type=oci,dest=/tmp/x.tar",
        "--cache-from=type=gha",
        "--cache-to=type=gha,mode=max",
    ] {
        let err = validate_podman_flag_compat(&[flag.to_string()])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("buildx-only"),
            "flag {flag} must be rejected, got: {err}"
        );
    }
}

#[test]
fn podman_compatible_flags_pass_validation() {
    validate_podman_flag_compat(&[
        "--build-arg=FOO=bar".to_string(),
        "--label=org.opencontainers.image.title=demo".to_string(),
        "--platform=linux/amd64".to_string(),
        "--tag=ghcr.io/owner/app:v1".to_string(),
        "--no-cache".to_string(),
        "--pull-always".to_string(),
    ])
    .expect("non-buildx-only flags must pass under podman");
}

#[cfg(target_os = "linux")]
#[test]
fn podman_v2_command_shape_matches_spec() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64"],
        image_tags: &["ghcr.io/owner/app:v1".to_string()],
        build_args: &[("VERSION".to_string(), "1.0.0".to_string())],
        annotations: &[],
        labels: &[(
            "org.opencontainers.image.title".to_string(),
            "demo".to_string(),
        )],
        flags: &["--no-cache".to_string()],
        sbom: false,
        push: true,
        load: true,
        backend: Some("podman"),
    })
    .expect("podman spec valid on linux");

    assert_eq!(cmd[0], "podman");
    assert_eq!(cmd[1], "build");
    assert!(cmd.contains(&"--no-cache".to_string()));
    assert!(cmd.contains(&"--build-arg".to_string()));
    assert!(cmd.contains(&"--label".to_string()));
    assert!(cmd.contains(&"--platform=linux/amd64".to_string()));
    assert!(
        cmd.iter().any(|a| a.starts_with("--iidfile=")),
        "podman build retains --iidfile for digest capture"
    );
    for forbidden in ["--push", "--load", "--attest=type=sbom"] {
        assert!(
            !cmd.iter().any(|a| a == forbidden),
            "podman command must not contain buildx-only {forbidden}: {cmd:?}"
        );
    }

    // Single-platform podman names the target with `--tag`, never `--manifest`.
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--tag" && w[1] == "ghcr.io/owner/app:v1"),
        "single-platform podman must use --tag: {cmd:?}"
    );
    assert!(
        !cmd.iter().any(|a| a == "--manifest"),
        "single-platform podman must NOT use --manifest: {cmd:?}"
    );
}

/// Multi-platform podman MUST name the build target with `--manifest <name>`
/// (NOT `--tag`). Per the podman-build docs, a multi-platform `--tag` build
/// does not assemble a local manifest list, so the subsequent
/// `podman manifest push --all` would publish nothing valid. This test fails
/// before the `--manifest` fix lands (the build emitted `--tag`).
#[cfg(target_os = "linux")]
#[test]
fn podman_multi_platform_build_uses_manifest_not_tag() {
    let cmd = build_docker_v2_command(&DockerV2Spec {
        staging_dir: "/tmp/staging",
        platforms: &["linux/amd64", "linux/arm64"],
        image_tags: &["ghcr.io/owner/app:v1".to_string()],
        build_args: &[],
        annotations: &[],
        labels: &[],
        flags: &[],
        sbom: false,
        push: true,
        load: false,
        backend: Some("podman"),
    })
    .expect("podman multi-platform spec valid on linux");

    assert_eq!(cmd[0], "podman");
    assert_eq!(cmd[1], "build");
    assert!(
        cmd.contains(&"--platform=linux/amd64,linux/arm64".to_string()),
        "both platforms must be passed: {cmd:?}"
    );
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--manifest" && w[1] == "ghcr.io/owner/app:v1"),
        "multi-platform podman must name the target with --manifest: {cmd:?}"
    );
    assert!(
        !cmd.iter().any(|a| a == "--tag"),
        "multi-platform podman must NOT use --tag (does not build a manifest list): {cmd:?}"
    );
    // Multi-platform `podman build` rejects --iidfile (errors when --platform
    // is given more than once), so it must be suppressed.
    assert!(
        !cmd.iter().any(|a| a.starts_with("--iidfile")),
        "multi-platform podman must NOT pass --iidfile: {cmd:?}"
    );
}

/// Push verb depends on arity: single-platform → `podman push <tag>`,
/// multi-platform → `podman manifest push --all <tag>` (the `--all` pushes the
/// list's per-arch contents, which a downstream manifest resolve depends on).
#[test]
fn podman_push_verb_depends_on_platform_arity() {
    let tags = vec!["ghcr.io/owner/app:v1".to_string()];

    let single = build_podman_push_commands(&tags, false);
    assert_eq!(
        single,
        vec![vec![
            "podman".to_string(),
            "push".to_string(),
            "ghcr.io/owner/app:v1".to_string(),
        ]],
        "single-platform podman publishes with plain `podman push`"
    );

    let multi = build_podman_push_commands(&tags, true);
    assert_eq!(
        multi,
        vec![vec![
            "podman".to_string(),
            "manifest".to_string(),
            "push".to_string(),
            "--all".to_string(),
            "ghcr.io/owner/app:v1".to_string(),
        ]],
        "multi-platform podman publishes with `podman manifest push --all`"
    );
}
