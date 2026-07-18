use super::*;
#[cfg(unix)]
use anodizer_core::artifact::{Artifact, ArtifactKind};
#[cfg(unix)]
use anodizer_core::config::SbomConfig;
#[cfg(unix)]
use anodizer_core::context::Context;
#[cfg(unix)]
use anodizer_core::stage::Stage;
#[cfg(unix)]
use anodizer_core::test_helpers::TestContextBuilder;
#[cfg(unix)]
use std::path::{Path, PathBuf};

/// Regression:
/// when `documents:` contains a glob pattern that matches multiple
/// files, each match must be registered as its own SBOM artifact
/// using the matched filename — NOT the unexpanded glob pattern.
///
/// Before the fix, `documents: ["*.spdx.json"]` produced (at most)
/// one artifact whose `name` was the literal `*.spdx.json`, since
/// the path was passed through `dist.join(...).file_name()` without
/// glob expansion. Downstream stages (checksum, release-upload,
/// signing) would then fail to find the file on disk.
#[cfg(unix)]
#[test]
fn sbom_documents_glob_expands_to_matched_filenames() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let dist = tmpdir.path().to_path_buf();

    // Pre-create two files matching the glob, plus one that does
    // not, to assert filtering precision.
    std::fs::write(dist.join("alpha.spdx.json"), b"{\"a\":1}").unwrap();
    std::fs::write(dist.join("beta.spdx.json"), b"{\"b\":1}").unwrap();
    std::fs::write(dist.join("ignored.json"), b"{\"x\":1}").unwrap();

    let mut ctx = TestContextBuilder::new()
        .project_name("myproj")
        .dist(dist.clone())
        .add_sbom(SbomConfig {
            id: Some("globbed".into()),
            cmd: Some("true".into()),
            args: Some(vec![]),
            documents: Some(vec!["*.spdx.json".into()]),
            artifacts: Some("any".into()),
            env: Some(vec![]),
            ..Default::default()
        })
        .build();

    SbomStage.run(&mut ctx).expect("sbom stage");

    let names: std::collections::BTreeSet<String> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.kind == ArtifactKind::Sbom)
        .map(|a| a.name.clone())
        .collect();

    let expected: std::collections::BTreeSet<String> = ["alpha.spdx.json", "beta.spdx.json"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(
        names, expected,
        "SBOM artifact names must be the glob-matched filenames, \
             not the literal `*.spdx.json` pattern (GR 292203e)"
    );

    // Each matched file must register a distinct on-disk path.
    let paths: std::collections::BTreeSet<PathBuf> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.kind == ArtifactKind::Sbom)
        .map(|a| a.path.clone())
        .collect();
    assert_eq!(paths.len(), 2, "expected 2 distinct SBOM paths");
    for p in &paths {
        assert!(p.exists(), "registered SBOM path must exist: {:?}", p);
    }
}

// -----------------------------------------------------------------------
// SOURCE_DATE_EPOCH wiring regression
// -----------------------------------------------------------------------
//
// These tests pin the contract that the CycloneDX `metadata.timestamp`
// field is derived from the run's SOURCE_DATE_EPOCH (via
// `ctx.determinism.sde`), not wall-clock `Utc::now()`. Without this
// wiring, two pipeline retries of the same release tag emit different
// SBOM bytes and the second upload fails with GitHub ReleaseAsset
// `already_exists` (size mismatch).

/// `generate_cyclonedx` is byte-stable for the same `timestamp` input
/// across repeated calls. Trivially true for a pure function, but
/// pinned so a future refactor that introduces clock reads inside the
/// generator (e.g. via `chrono::Utc::now()` in a helper) regresses
/// the test.
#[test]
fn cyclonedx_output_byte_stable_for_same_timestamp() {
    let pkgs = vec![CargoPackage {
        name: "anyhow".into(),
        version: "1.0.0".into(),
        source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
    }];
    // RFC3339 derived from SDE 1_715_000_000 = 2024-05-06T12:53:20+00:00.
    let ts = "2024-05-06T12:53:20+00:00";
    let a = generate_cyclonedx("myproj", "1.2.3", ts, &pkgs).unwrap();
    let b = generate_cyclonedx("myproj", "1.2.3", ts, &pkgs).unwrap();
    let a_bytes = serde_json::to_vec_pretty(&a).unwrap();
    let b_bytes = serde_json::to_vec_pretty(&b).unwrap();
    assert_eq!(
        a_bytes, b_bytes,
        "CycloneDX output must be byte-identical for the same SDE-derived timestamp"
    );
}

/// Pins the SDE-to-RFC3339 conversion that `run_sbom_builtin` uses on
/// `ctx.determinism.sde`. If this conversion drifts (e.g. UTC vs
/// local TZ, seconds vs millis), the SBOM `metadata.timestamp` field
/// changes and breaks retry idempotency.
#[test]
fn sbom_metadata_timestamp_honors_sde() {
    let sde: i64 = 1_715_000_000;
    let dt = chrono::DateTime::<chrono::Utc>::from_timestamp(sde, 0)
        .expect("SDE 1_715_000_000 is in range");
    let derived = dt.to_rfc3339();
    // The exact RFC3339 form chrono emits for this SDE — pinned so a
    // future chrono version that flips +00:00 -> Z (or vice-versa)
    // breaks this test instead of silently breaking SBOM byte
    // stability.
    assert_eq!(derived, "2024-05-06T12:53:20+00:00");

    // The generated SBOM embeds exactly that string in metadata.timestamp.
    let pkgs: Vec<CargoPackage> = vec![];
    let sbom = generate_cyclonedx("p", "0", &derived, &pkgs).unwrap();
    let embedded = sbom
        .get("metadata")
        .and_then(|m| m.get("timestamp"))
        .and_then(|t| t.as_str())
        .expect("metadata.timestamp present");
    assert_eq!(embedded, "2024-05-06T12:53:20+00:00");
}

/// Different SDEs produce different metadata timestamps (sanity: the
/// timestamp is not pinned to a constant). Pair test for
/// `sbom_metadata_timestamp_honors_sde`.
#[test]
fn sbom_metadata_timestamp_varies_with_sde() {
    let pkgs: Vec<CargoPackage> = vec![];
    let t1 = chrono::DateTime::<chrono::Utc>::from_timestamp(1_715_000_000, 0)
        .unwrap()
        .to_rfc3339();
    let t2 = chrono::DateTime::<chrono::Utc>::from_timestamp(1_716_000_000, 0)
        .unwrap()
        .to_rfc3339();
    assert_ne!(t1, t2);
    let s1 = generate_cyclonedx("p", "0", &t1, &pkgs).unwrap();
    let s2 = generate_cyclonedx("p", "0", &t2, &pkgs).unwrap();
    assert_ne!(
        serde_json::to_vec(&s1).unwrap(),
        serde_json::to_vec(&s2).unwrap(),
        "different SDEs must produce different SBOM bytes"
    );
}

// -----------------------------------------------------------------------
// External-command (subprocess) path — driven by the fake-tool harness.
// -----------------------------------------------------------------------
//
// The `cmd:` field is configurable, so each test points it straight at a
// stub installed via `FakeToolDir` (no PATH mutation, no `#[serial]`). The
// stub records its argv (`tools.calls`), can create output files
// (`.creates`), exit non-zero (`.exit`/`.stderr`), or run an arbitrary
// `sh` body (`.script`) so env propagation and per-arg syft semantics are
// observable.

#[cfg(unix)]
use anodizer_core::test_helpers::fake_tool::FakeToolDir;
#[cfg(unix)]
use std::collections::HashMap;

/// Build a `Context` with `dist` set to a fresh tempdir and a single SBOM
/// config pointed at `cmd`. Returns `(ctx, tmpdir)`; the tmpdir guard must
/// outlive the stage run.
#[cfg(unix)]
fn external_ctx(cmd: PathBuf, cfg: SbomConfig) -> (Context, tempfile::TempDir) {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let dist = tmpdir.path().to_path_buf();
    let cfg = SbomConfig {
        cmd: Some(cmd.to_string_lossy().into_owned()),
        ..cfg
    };
    let ctx = TestContextBuilder::new()
        .project_name("myproj")
        .tag("v1.0.0")
        .dist(dist)
        .add_sbom(cfg)
        .build();
    (ctx, tmpdir)
}

/// Register a Binary artifact in `dist` so `artifacts: binary` configs have
/// something to catalog. Returns the on-disk binary path.
#[cfg(unix)]
fn add_binary(ctx: &mut Context, dist: &Path, name: &str, target: &str) -> PathBuf {
    let path = dist.join(name);
    std::fs::write(&path, b"\x7fELF fake").unwrap();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: name.to_string(),
        path: path.clone(),
        target: Some(target.to_string()),
        crate_name: "myproj".to_string(),
        metadata: HashMap::new(),
        size: None,
    });
    path
}

/// Happy path: the stage shells out to the configured tool, the tool writes
/// the rendered document, and that file is registered as an Sbom artifact.
/// Asserts the tool was invoked exactly once with the rendered argv (the
/// `$document`/`$artifact` placeholders resolved).
#[cfg(unix)]
#[test]
fn external_cmd_success_registers_output() {
    let tools = FakeToolDir::new();
    // syft-style: write the path named in the `spdx-json=PATH` arg.
    tools
        .tool("syft")
        .script(
            "for a in \"$@\"; do case \"$a\" in *=*) echo '{\"k\":1}' > \"${a#*=}\";; esac; done",
        )
        .install();

    let (mut ctx, _tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("syftcfg".into()),
            artifacts: Some("any".into()),
            documents: Some(vec!["bom.spdx.json".into()]),
            args: Some(vec![
                "scan".into(),
                "--output".into(),
                "spdx-json=$document".into(),
            ]),
            env: Some(vec![]),
            ..Default::default()
        },
    );

    SbomStage.run(&mut ctx).expect("sbom stage");

    // Tool invoked once with the rendered argv ($document -> bom.spdx.json).
    assert_eq!(tools.call_count("syft"), 1);
    assert_eq!(
        tools.calls("syft")[0],
        vec!["scan", "--output", "spdx-json=bom.spdx.json"]
    );

    // The produced file is registered as an Sbom artifact, basename = file.
    let sboms: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.kind == ArtifactKind::Sbom)
        .collect();
    assert_eq!(sboms.len(), 1);
    assert_eq!(sboms[0].name, "bom.spdx.json");
    assert_eq!(
        sboms[0].metadata.get("sbom_id").map(String::as_str),
        Some("syftcfg")
    );
    assert!(sboms[0].path.exists());
}

/// A macOS `.app` bundle (Installer + format=appbundle, a directory) must be
/// excluded from `artifacts: installer` SBOM generation — it is never an
/// uploaded asset, so cataloging it produces a stray SBOM. The sibling
/// Installer FILE (an MSI) is still cataloged: the tool runs exactly once.
#[cfg(unix)]
#[test]
fn external_installer_excludes_appbundle_directory() {
    use anodizer_core::artifact::{FORMAT_APPBUNDLE, FORMAT_META};

    let tools = FakeToolDir::new();
    tools
        .tool("syft")
        .script(
            "for a in \"$@\"; do case \"$a\" in *=*) echo '{\"k\":1}' > \"${a#*=}\";; esac; done",
        )
        .install();

    let (mut ctx, tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("inst".into()),
            artifacts: Some("installer".into()),
            documents: Some(vec!["{{ .ArtifactName }}.cdx.json".into()]),
            args: Some(vec![
                "scan".into(),
                "--output".into(),
                "cyclonedx-json=$document".into(),
            ]),
            env: Some(vec![]),
            ..Default::default()
        },
    );
    let dist = tmp.path().to_path_buf();

    // The `.app` bundle: a DIRECTORY, registered Installer + format=appbundle.
    let app_dir = dist.join("MyApp.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Installer,
        name: "MyApp.app".to_string(),
        path: app_dir,
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myproj".to_string(),
        metadata: HashMap::from([(FORMAT_META.to_string(), FORMAT_APPBUNDLE.to_string())]),
        size: None,
    });
    // The sibling Installer FILE (MSI): must still be cataloged.
    let msi = dist.join("myproj-1.0.0.msi");
    std::fs::write(&msi, b"MSI fake").unwrap();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Installer,
        name: "myproj-1.0.0.msi".to_string(),
        path: msi,
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myproj".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    SbomStage.run(&mut ctx).expect("sbom stage");

    // Tool ran exactly once — for the MSI file, never the `.app` directory.
    assert_eq!(
        tools.call_count("syft"),
        1,
        "syft must catalog only the MSI file, not the `.app` directory"
    );
    let sboms: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.kind == ArtifactKind::Sbom)
        .collect();
    assert_eq!(sboms.len(), 1, "exactly one SBOM, for the MSI");
    assert!(
        sboms[0].name.contains("myproj-1.0.0.msi"),
        "the SBOM is for the MSI, got: {}",
        sboms[0].name
    );
}

/// Tool exits non-zero → the stage bails and the error chain carries the
/// tool's trimmed stderr plus the `sbom[<id>]` prefix.
#[cfg(unix)]
#[test]
fn external_cmd_nonzero_exit_bails_with_stderr() {
    let tools = FakeToolDir::new();
    tools
        .tool("syft")
        .stderr("catalog failed: boom\n")
        .exit(3)
        .install();

    let (mut ctx, _tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("failer".into()),
            artifacts: Some("any".into()),
            documents: Some(vec!["bom.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            ..Default::default()
        },
    );

    let err = SbomStage
        .run(&mut ctx)
        .expect_err("non-zero exit must bail");
    let chain = format!("{err:#}");
    assert!(chain.contains("sbom[failer]"), "got: {chain}");
    assert!(chain.contains("failed"), "got: {chain}");
    assert!(chain.contains("catalog failed: boom"), "got: {chain}");
    // No artifact registered on failure.
    assert!(
        ctx.artifacts
            .all()
            .iter()
            .all(|a| a.kind != ArtifactKind::Sbom)
    );
}

/// Tool succeeds but writes a zero-byte document → the stage rejects the
/// empty SBOM rather than registering a useless file.
#[cfg(unix)]
#[test]
fn external_cmd_empty_output_file_bails() {
    let tools = FakeToolDir::new();
    // Exit 0 but create the document as an empty file.
    tools.tool("syft").script("> bom.spdx.json").install();

    let (mut ctx, _tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("empty".into()),
            artifacts: Some("any".into()),
            documents: Some(vec!["bom.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            ..Default::default()
        },
    );

    let err = SbomStage.run(&mut ctx).expect_err("empty output must bail");
    let chain = format!("{err:#}");
    assert!(chain.contains("sbom[empty]"), "got: {chain}");
    assert!(chain.contains("empty output file"), "got: {chain}");
}

/// Tool succeeds (exit 0) but produces NO output files → the stage bails
/// listing the expected document paths.
#[cfg(unix)]
#[test]
fn external_cmd_no_output_files_bails() {
    let tools = FakeToolDir::new();
    // Exit 0 and create nothing.
    tools.tool("syft").install();

    let (mut ctx, _tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("noout".into()),
            artifacts: Some("any".into()),
            documents: Some(vec!["bom.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            ..Default::default()
        },
    );

    let err = SbomStage
        .run(&mut ctx)
        .expect_err("missing output must bail");
    let chain = format!("{err:#}");
    assert!(chain.contains("sbom[noout]"), "got: {chain}");
    assert!(chain.contains("no output files"), "got: {chain}");
    assert!(chain.contains("bom.spdx.json"), "got: {chain}");
    // The tool was actually run (this is the post-success check, not a
    // pre-flight skip).
    assert_eq!(tools.call_count("syft"), 1);
}

/// A rendered document path that resolves to an absolute path is refused —
/// SBOM outputs must stay relative to `dist`. The tool must NOT be invoked
/// (the bail happens during doc rendering, before the spawn).
#[cfg(unix)]
#[test]
fn external_cmd_absolute_document_path_bails() {
    let tools = FakeToolDir::new();
    tools.tool("syft").creates("ignored", "x").install();

    let (mut ctx, _tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("abs".into()),
            artifacts: Some("any".into()),
            documents: Some(vec!["/etc/escape.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            ..Default::default()
        },
    );

    let err = SbomStage
        .run(&mut ctx)
        .expect_err("absolute document path must bail");
    let chain = format!("{err:#}");
    assert!(chain.contains("sbom[abs]"), "got: {chain}");
    assert!(chain.contains("is absolute"), "got: {chain}");
    assert!(
        !tools.was_called("syft"),
        "tool must not run when the document path is rejected"
    );
}

/// `artifacts: binary` with more than one default document is a config the
/// stage rejects up front (per-artifact document names would collide). The
/// tool is never invoked.
#[cfg(unix)]
#[test]
fn external_cmd_multiple_documents_with_typed_artifacts_bails() {
    let tools = FakeToolDir::new();
    tools.tool("syft").install();

    let (mut ctx, _tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("multi".into()),
            artifacts: Some("binary".into()),
            documents: Some(vec!["a.spdx.json".into(), "b.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            ..Default::default()
        },
    );

    let err = SbomStage
        .run(&mut ctx)
        .expect_err("multi-document + typed artifacts must bail");
    let chain = format!("{err:#}");
    assert!(chain.contains("sbom[multi]"), "got: {chain}");
    assert!(chain.contains("multiple SBOM outputs"), "got: {chain}");
    assert!(chain.contains("binary"), "got: {chain}");
    assert!(!tools.was_called("syft"));
}

/// Explicit `env:` entries are template-rendered and passed to the
/// subprocess. The stub dumps a chosen env var into a file so the test can
/// read back the value the stage actually exported (incl. `{{ .Version }}`
/// rendering).
#[cfg(unix)]
#[test]
fn external_cmd_renders_and_passes_env() {
    let tools = FakeToolDir::new();
    // Record the env var into a file, then write the document so the stage
    // doesn't bail on a missing output.
    tools
        .tool("syft")
        .script(
            "printf '%s' \"$SBOM_PROBE\" > env_probe.txt\n\
                 echo '{}' > bom.spdx.json",
        )
        .install();

    let (mut ctx, tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("envcfg".into()),
            artifacts: Some("any".into()),
            documents: Some(vec!["bom.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            // Value is a template — must be rendered to "v=1.0.0".
            env: Some(vec!["SBOM_PROBE=v={{ .Version }}".into()]),
            ..Default::default()
        },
    );

    SbomStage.run(&mut ctx).expect("sbom stage");

    let probe = std::fs::read_to_string(tmp.path().join("env_probe.txt")).unwrap();
    assert_eq!(
        probe, "v=1.0.0",
        "env value must be template-rendered and exported to the subprocess"
    );
}

/// `default_syft_env_for` true branch: a literal `syft` cmd with
/// `artifacts: archive` (or `source`) injects the file-metadata cataloger
/// env; every other combination is empty. Driven directly because the stage
/// always resolves `cmd` to an absolute stub path, which is never the
/// literal string `"syft"`.
#[test]
fn default_syft_env_true_branch_and_negatives() {
    assert_eq!(
        anodizer_core::config::SbomConfig::default_syft_env_for("syft", "archive"),
        vec![(
            "SYFT_FILE_METADATA_CATALOGER_ENABLED".to_string(),
            "true".to_string()
        )]
    );
    assert_eq!(
        anodizer_core::config::SbomConfig::default_syft_env_for("syft", "source"),
        vec![(
            "SYFT_FILE_METADATA_CATALOGER_ENABLED".to_string(),
            "true".to_string()
        )]
    );
    // binary/any => no special env even for syft.
    assert!(anodizer_core::config::SbomConfig::default_syft_env_for("syft", "binary").is_empty());
    assert!(anodizer_core::config::SbomConfig::default_syft_env_for("syft", "any").is_empty());
    // non-syft cmd => never injected.
    assert!(anodizer_core::config::SbomConfig::default_syft_env_for("trivy", "archive").is_empty());
}

/// Via the stage: because the resolved cmd is an absolute path (never the
/// literal `"syft"`), the default syft env is NOT injected when `env:` is
/// unset — the subprocess sees an empty `SYFT_FILE_METADATA_CATALOGER_ENABLED`.
/// Pins the None-env → `default_syft_env_for` resolution path end-to-end.
#[cfg(unix)]
#[test]
fn external_cmd_absolute_cmd_does_not_inject_default_syft_env() {
    let tools = FakeToolDir::new();
    // The configured cmd must be NAMED `syft` for default_syft_env_for to
    // fire, so install the stub under that name and point cmd at it.
    tools
        .tool("syft")
        .script(
            "printf '%s' \"$SYFT_FILE_METADATA_CATALOGER_ENABLED\" > env_probe.txt\n\
                 echo '{}' > arch.spdx.json",
        )
        .install();

    let tmpdir = tempfile::tempdir().expect("tempdir");
    let dist = tmpdir.path().to_path_buf();
    let mut ctx = TestContextBuilder::new()
        .project_name("myproj")
        .tag("v1.0.0")
        .dist(dist.clone())
        .add_sbom(SbomConfig {
            id: Some("archcfg".into()),
            // cmd basename is "syft" → default_syft_env_for triggers.
            cmd: Some(tools.tool_path("syft").to_string_lossy().into_owned()),
            artifacts: Some("archive".into()),
            documents: Some(vec!["arch.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            // env unset → falls to default_syft_env_for(cmd, "archive").
            ..Default::default()
        })
        .build();

    // Provide one Archive artifact so the `archive` filter matches.
    let arch_path = dist.join("pkg.tar.gz");
    std::fs::write(&arch_path, b"archive").unwrap();
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: "pkg.tar.gz".into(),
        path: arch_path,
        target: Some("x86_64-unknown-linux-gnu".into()),
        crate_name: "myproj".into(),
        metadata: HashMap::new(),
        size: None,
    });

    SbomStage.run(&mut ctx).expect("sbom stage");

    // default_syft_env_for keys on the resolved cmd string. The config cmd
    // is an absolute path whose basename is `syft`, but resolved_cmd()
    // returns the full path — so the default env only fires when the cmd
    // string equals "syft". Assert the actual exported value to pin which
    // branch ran.
    let probe = std::fs::read_to_string(tmpdir.path().join("env_probe.txt")).unwrap();
    assert_eq!(
        probe, "",
        "an absolute cmd path is not the literal \"syft\", so the default \
             syft env must NOT be injected (resolved_cmd compares the full string)"
    );
}

/// `artifacts: binary` catalogs each matched binary: the rendered
/// `$artifact` placeholder is the binary's path relative to dist, and the
/// per-artifact `$document` is written + registered. Pins the typed-artifact
/// iteration + `$artifact`/`$document` substitution in the external path.
#[cfg(unix)]
#[test]
fn external_cmd_binary_artifacts_substitutes_artifact_and_document() {
    let tools = FakeToolDir::new();
    tools
        .tool("syft")
        .script("for a in \"$@\"; do case \"$a\" in *=*) echo '{}' > \"${a#*=}\";; esac; done")
        .install();

    let (mut ctx, tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("bin".into()),
            artifacts: Some("binary".into()),
            documents: Some(vec!["{{ .ArtifactName }}.spdx.json".into()]),
            args: Some(vec![
                "scan".into(),
                "$artifact".into(),
                "--output".into(),
                "spdx-json=$document".into(),
            ]),
            env: Some(vec![]),
            ..Default::default()
        },
    );
    let dist = tmp.path().to_path_buf();
    add_binary(&mut ctx, &dist, "myproj-linux", "x86_64-unknown-linux-gnu");

    SbomStage.run(&mut ctx).expect("sbom stage");

    let call = &tools.calls("syft")[0];
    // $artifact -> the binary path relative to dist; $document -> rendered
    // per-artifact name.
    assert_eq!(
        call,
        &vec![
            "scan",
            "myproj-linux",
            "--output",
            "spdx-json=myproj-linux.spdx.json",
        ]
    );
    let sbom_names: Vec<String> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.kind == ArtifactKind::Sbom)
        .map(|a| a.name.clone())
        .collect();
    assert_eq!(sbom_names, vec!["myproj-linux.spdx.json"]);
}

/// `artifacts: archive` matching zero artifacts in non-strict mode is a
/// silent skip: no error, no tool run, no SBOM registered.
#[cfg(unix)]
#[test]
fn external_cmd_no_matching_artifacts_non_strict_skips() {
    let tools = FakeToolDir::new();
    tools.tool("syft").install();

    let (mut ctx, _tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("nomatch".into()),
            artifacts: Some("archive".into()),
            documents: Some(vec!["{{ .ArtifactName }}.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            ..Default::default()
        },
    );
    // No Archive artifacts registered.

    SbomStage
        .run(&mut ctx)
        .expect("zero matches under non-strict must skip, not error");
    assert!(
        !tools.was_called("syft"),
        "tool must not run with no inputs"
    );
    assert!(
        ctx.artifacts
            .all()
            .iter()
            .all(|a| a.kind != ArtifactKind::Sbom)
    );
}

/// Built-in (Cargo.lock) generator with `artifacts: archive` and MULTIPLE
/// archives emits exactly ONE workspace SBOM, not one byte-identical copy
/// per archive. The built-in output is archive-independent (it catalogs the
/// dependency graph), so N copies would only multiply the downstream
/// checksum + signature object count for no information gain.
#[cfg(unix)]
#[test]
fn builtin_archive_emits_single_workspace_sbom() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let dist = tmpdir.path().to_path_buf();
    let mut ctx = TestContextBuilder::new()
        .project_name("myproj")
        .tag("v1.0.0")
        .dist(dist.clone())
        // No `cmd`/`args` → use_builtin path. `documents:` provides only the
        // format/extension hint (.cdx.json → cyclonedx); the built-in path
        // does not render it per-archive.
        .add_sbom(SbomConfig {
            id: Some("wsbom".into()),
            artifacts: Some("archive".into()),
            documents: Some(vec!["{{ .ArtifactName }}.cdx.json".into()]),
            ..Default::default()
        })
        .build();

    // Three Archive artifacts across distinct targets — the pre-fix loop
    // would write three identical `.cdx.json` files.
    for (name, target) in [
        ("pkg-linux.tar.gz", "x86_64-unknown-linux-gnu"),
        ("pkg-mac.tar.gz", "aarch64-apple-darwin"),
        ("pkg-win.zip", "x86_64-pc-windows-msvc"),
    ] {
        let p = dist.join(name);
        std::fs::write(&p, b"archive").unwrap();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: name.into(),
            path: p,
            target: Some(target.into()),
            crate_name: "myproj".into(),
            metadata: HashMap::new(),
            size: None,
        });
    }

    SbomStage.run(&mut ctx).expect("built-in sbom stage");

    let sboms: Vec<&Artifact> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.kind == ArtifactKind::Sbom)
        .collect();
    assert_eq!(
        sboms.len(),
        1,
        "built-in generator + artifacts:archive must emit exactly ONE \
             workspace SBOM, not one per archive; got {:?}",
        sboms.iter().map(|a| &a.name).collect::<Vec<_>>()
    );
    // The single SBOM is the workspace-level `<project>-<version>.<ext>`
    // document, not a per-archive filename.
    assert_eq!(sboms[0].name, "myproj-1.0.0.cdx.json");
    assert!(sboms[0].path.exists(), "the workspace SBOM must be on disk");
    // Target-independent: it must NOT inherit the first matched archive's
    // target. A per-archive target makes each shard of a sharded release
    // stamp this byte-identical document with a different target, defeating
    // the per-shard merge's `dedupe_targetless_duplicates` and tripping its
    // duplicate-path guard.
    assert_eq!(
        sboms[0].target, None,
        "workspace SBOM must be target-independent so cross-shard merge \
             collapses the identical per-shard copies; got {:?}",
        sboms[0].target
    );
}

/// The external (syft) archive path is UNTOUCHED by the built-in dedupe:
/// per-archive scanning produces genuinely-distinct SBOMs, so two archives
/// yield two SBOMs (one rendered document each).
#[cfg(unix)]
#[test]
fn external_cmd_archive_emits_one_sbom_per_archive() {
    let tools = FakeToolDir::new();
    tools
        .tool("syft")
        .script("for a in \"$@\"; do case \"$a\" in *=*) echo '{}' > \"${a#*=}\";; esac; done")
        .install();

    let tmpdir = tempfile::tempdir().expect("tempdir");
    let dist = tmpdir.path().to_path_buf();
    let mut ctx = TestContextBuilder::new()
        .project_name("myproj")
        .tag("v1.0.0")
        .dist(dist.clone())
        .add_sbom(SbomConfig {
            id: Some("perarch".into()),
            cmd: Some(tools.tool_path("syft").to_string_lossy().into_owned()),
            artifacts: Some("archive".into()),
            documents: Some(vec!["{{ .ArtifactName }}.spdx.json".into()]),
            args: Some(vec![
                "scan".into(),
                "$artifact".into(),
                "--output".into(),
                "spdx-json=$document".into(),
            ]),
            env: Some(vec![]),
            ..Default::default()
        })
        .build();

    for (name, target) in [
        ("pkg-linux.tar.gz", "x86_64-unknown-linux-gnu"),
        ("pkg-mac.tar.gz", "aarch64-apple-darwin"),
    ] {
        let p = dist.join(name);
        std::fs::write(&p, b"archive").unwrap();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: name.into(),
            path: p,
            target: Some(target.into()),
            crate_name: "myproj".into(),
            metadata: HashMap::new(),
            size: None,
        });
    }

    SbomStage.run(&mut ctx).expect("external sbom stage");

    let mut names: Vec<String> = ctx
        .artifacts
        .all()
        .iter()
        .filter(|a| a.kind == ArtifactKind::Sbom)
        .map(|a| a.name.clone())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "pkg-linux.tar.gz.spdx.json".to_string(),
            "pkg-mac.tar.gz.spdx.json".to_string(),
        ],
        "external (syft) archive path must stay per-archive — one distinct \
             SBOM per scanned archive"
    );
}

/// An unknown `artifacts:` value is rejected with the valid-values hint.
/// The tool is never invoked.
#[cfg(unix)]
#[test]
fn external_cmd_unknown_artifacts_type_bails() {
    let tools = FakeToolDir::new();
    tools.tool("syft").install();

    let (mut ctx, _tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("bogus".into()),
            artifacts: Some("nonsense".into()),
            documents: Some(vec!["x.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            ..Default::default()
        },
    );

    let err = SbomStage
        .run(&mut ctx)
        .expect_err("unknown artifacts type must bail");
    let chain = format!("{err:#}");
    assert!(chain.contains("sbom[bogus]"), "got: {chain}");
    assert!(chain.contains("unknown artifacts type"), "got: {chain}");
    assert!(chain.contains("nonsense"), "got: {chain}");
    assert!(!tools.was_called("syft"));
}

/// Dry-run never spawns the tool but still returns Ok.
#[cfg(unix)]
#[test]
fn external_cmd_dry_run_does_not_spawn() {
    let tools = FakeToolDir::new();
    tools.tool("syft").install();

    let tmpdir = tempfile::tempdir().expect("tempdir");
    let mut ctx = TestContextBuilder::new()
        .project_name("myproj")
        .tag("v1.0.0")
        .dist(tmpdir.path().to_path_buf())
        .dry_run(true)
        .add_sbom(SbomConfig {
            id: Some("dry".into()),
            cmd: Some(tools.tool_path("syft").to_string_lossy().into_owned()),
            artifacts: Some("any".into()),
            documents: Some(vec!["bom.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            ..Default::default()
        })
        .build();

    SbomStage.run(&mut ctx).expect("dry-run sbom stage");
    assert!(
        !tools.was_called("syft"),
        "dry-run must not invoke the tool"
    );
}

/// `skip: true` short-circuits before any tool spawn.
#[cfg(unix)]
#[test]
fn external_cmd_skip_true_does_not_spawn() {
    use anodizer_core::config::StringOrBool;
    let tools = FakeToolDir::new();
    tools.tool("syft").install();

    let (mut ctx, _tmp) = external_ctx(
        tools.tool_path("syft"),
        SbomConfig {
            id: Some("skipper".into()),
            artifacts: Some("any".into()),
            documents: Some(vec!["bom.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            skip: Some(StringOrBool::Bool(true)),
            ..Default::default()
        },
    );

    SbomStage.run(&mut ctx).expect("skipped sbom stage");
    assert!(!tools.was_called("syft"), "skip:true must not run the tool");
}

/// Two SBOM configs sharing the same resolved id is a config error caught
/// before any subprocess runs.
#[cfg(unix)]
#[test]
fn duplicate_sbom_ids_bail() {
    let tools = FakeToolDir::new();
    tools.tool("syft").install();

    let tmpdir = tempfile::tempdir().expect("tempdir");
    let cmd = tools.tool_path("syft").to_string_lossy().into_owned();
    let mut ctx = TestContextBuilder::new()
        .project_name("myproj")
        .tag("v1.0.0")
        .dist(tmpdir.path().to_path_buf())
        .add_sbom(SbomConfig {
            id: Some("dup".into()),
            cmd: Some(cmd.clone()),
            artifacts: Some("any".into()),
            documents: Some(vec!["a.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            ..Default::default()
        })
        .add_sbom(SbomConfig {
            id: Some("dup".into()),
            cmd: Some(cmd),
            artifacts: Some("any".into()),
            documents: Some(vec!["b.spdx.json".into()]),
            args: Some(vec!["scan".into()]),
            env: Some(vec![]),
            ..Default::default()
        })
        .build();

    let err = SbomStage
        .run(&mut ctx)
        .expect_err("duplicate ids must bail");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("multiple sboms with the ID 'dup'"),
        "got: {chain}"
    );
    assert!(!tools.was_called("syft"));
}

// -----------------------------------------------------------------------
// Pure-helper coverage: parse_cargo_lock, find_cargo_lock, SPDX shape,
// deterministic_uuid_from.
// -----------------------------------------------------------------------

/// `parse_cargo_lock` extracts name/version/source for each `[[package]]`
/// and tolerates a missing `source` (path/workspace members).
#[test]
fn parse_cargo_lock_extracts_packages() {
    let lock = r#"
version = 3

[[package]]
name = "anyhow"
version = "1.0.86"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "localcrate"
version = "0.1.0"
"#;
    let pkgs = parse_cargo_lock(lock).expect("parse");
    assert_eq!(pkgs.len(), 2);
    assert_eq!(pkgs[0].name, "anyhow");
    assert_eq!(pkgs[0].version, "1.0.86");
    assert_eq!(
        pkgs[0].source.as_deref(),
        Some("registry+https://github.com/rust-lang/crates.io-index")
    );
    assert_eq!(pkgs[1].name, "localcrate");
    assert!(pkgs[1].source.is_none(), "path members have no source");
}

/// `parse_cargo_lock` returns an error on non-TOML input rather than
/// silently yielding an empty package list.
#[test]
fn parse_cargo_lock_rejects_invalid_toml() {
    let err = parse_cargo_lock("this is = = not toml ][").expect_err("must reject");
    assert!(format!("{err:#}").contains("Cargo.lock"));
}

/// `find_cargo_lock` walks up from a nested dir to the ancestor holding
/// `Cargo.lock`.
#[test]
fn find_cargo_lock_walks_up_to_ancestor() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Cargo.lock"), "version = 3\n").unwrap();
    let nested = tmp.path().join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();

    let found = find_cargo_lock(&nested).expect("walk up");
    assert_eq!(found, tmp.path().join("Cargo.lock"));
}

/// `find_cargo_lock` bails (naming the start dir) when no ancestor has a
/// lockfile.
#[test]
fn find_cargo_lock_missing_bails() {
    let tmp = tempfile::tempdir().unwrap();
    let nested = tmp.path().join("no/lock/here");
    std::fs::create_dir_all(&nested).unwrap();
    let err = find_cargo_lock(&nested).expect_err("no lockfile");
    assert!(format!("{err:#}").contains("Cargo.lock not found"));
}

/// `generate_spdx` emits a DESCRIBES relationship for the root package and
/// a DEPENDS_ON + purl externalRef per dependency, and threads the supplied
/// namespace uuid into `documentNamespace`.
#[test]
fn spdx_shape_and_namespace() {
    let pkgs = vec![CargoPackage {
        name: "serde".into(),
        version: "1.0.200".into(),
        source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
    }];
    let doc = generate_spdx(
        "myproj",
        "2.0.0",
        "2024-05-06T12:53:20+00:00",
        "NS-UUID",
        &pkgs,
    )
    .unwrap();

    assert_eq!(doc["spdxVersion"], "SPDX-2.3");
    assert_eq!(doc["name"], "myproj-2.0.0");
    assert_eq!(
        doc["documentNamespace"],
        "https://spdx.org/spdxdocs/myproj-2.0.0-NS-UUID"
    );

    let packages = doc["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 2, "root + 1 dependency");
    assert_eq!(packages[1]["name"], "serde");
    assert_eq!(
        packages[1]["externalRefs"][0]["referenceLocator"],
        "pkg:cargo/serde@1.0.200"
    );
    // registry source -> crates.io download location.
    assert_eq!(
        packages[1]["downloadLocation"],
        "https://crates.io/crates/serde/1.0.200"
    );

    let rels = doc["relationships"].as_array().unwrap();
    assert_eq!(rels[0]["relationshipType"], "DESCRIBES");
    assert_eq!(rels[1]["relationshipType"], "DEPENDS_ON");
    assert_eq!(rels[1]["relatedSpdxElement"], "SPDXRef-Package-0");
}

/// A non-registry source (git/path) is passed through verbatim as the
/// SPDX downloadLocation rather than rewritten to a crates.io URL.
#[test]
fn spdx_non_registry_source_passthrough() {
    let pkgs = vec![CargoPackage {
        name: "forked".into(),
        version: "0.1.0".into(),
        source: Some("git+https://example.com/forked.git#abc123".into()),
    }];
    let doc = generate_spdx("p", "0", "t", "ns", &pkgs).unwrap();
    assert_eq!(
        doc["packages"][1]["downloadLocation"],
        "git+https://example.com/forked.git#abc123"
    );
}

/// `deterministic_uuid_from` is stable for the same seed, differs across
/// seeds, and has a UUID-v4-shaped layout (version nibble `4`, RFC4122
/// variant bits in the 8/9/a/b range).
#[test]
fn deterministic_uuid_stable_and_shaped() {
    let a = deterministic_uuid_from("myproj-1.0.0");
    let b = deterministic_uuid_from("myproj-1.0.0");
    let c = deterministic_uuid_from("myproj-1.0.1");
    assert_eq!(a, b, "same seed -> same uuid");
    assert_ne!(a, c, "different seed -> different uuid");

    let groups: Vec<&str> = a.split('-').collect();
    assert_eq!(groups.len(), 5);
    assert_eq!(groups[0].len(), 8);
    assert_eq!(groups[2].len(), 4);
    assert!(groups[2].starts_with('4'), "version nibble must be 4: {a}");
    let variant = groups[3].chars().next().unwrap();
    assert!(
        matches!(variant, '8' | '9' | 'a' | 'b'),
        "RFC4122 variant nibble, got {variant} in {a}"
    );
}
