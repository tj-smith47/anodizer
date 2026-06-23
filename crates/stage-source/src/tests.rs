//! Tests for the `stage-source` crate.

#![cfg(test)]

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::stage::Stage;
use anodizer_core::test_helpers::TestContextBuilder;
use tempfile::TempDir;

use crate::archive::{SourceArchiveInputs, create_source_archive};
use crate::run::SourceStage;
use crate::sbom::{
    CargoPackage, deterministic_uuid_from, generate_cyclonedx, generate_spdx, parse_cargo_lock,
};

// -----------------------------------------------------------------------

#[test]
fn test_parse_cargo_lock_basic() {
    let content = r#"
version = 4

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "anyhow"
version = "1.0.82"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "my-project"
version = "0.1.0"
"#;
    let packages = parse_cargo_lock(content).unwrap();
    assert_eq!(packages.len(), 3);

    assert_eq!(packages[0].name, "serde");
    assert_eq!(packages[0].version, "1.0.200");
    assert!(packages[0].source.is_some());
    assert!(
        packages[0]
            .source
            .as_ref()
            .unwrap()
            .starts_with("registry+")
    );

    assert_eq!(packages[1].name, "anyhow");
    assert_eq!(packages[1].version, "1.0.82");

    assert_eq!(packages[2].name, "my-project");
    assert_eq!(packages[2].version, "0.1.0");
    assert!(packages[2].source.is_none());
}

#[test]
fn test_parse_cargo_lock_empty() {
    let content = "version = 4\n";
    let packages = parse_cargo_lock(content).unwrap();
    assert!(packages.is_empty());
}

#[test]
fn test_parse_cargo_lock_with_dependencies() {
    let content = r#"
version = 4

[[package]]
name = "aho-corasick"
version = "1.1.4"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "ddd31a130427c27518df266943a5308ed92d4b226cc639f5a8f1002816174301"
dependencies = [
 "memchr",
]

[[package]]
name = "memchr"
version = "2.7.4"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
    let packages = parse_cargo_lock(content).unwrap();
    assert_eq!(packages.len(), 2);
    assert_eq!(packages[0].name, "aho-corasick");
    assert_eq!(packages[1].name, "memchr");
}

#[test]
fn test_parse_cargo_lock_invalid_toml() {
    let content = "this is not valid toml {{{{";
    let result = parse_cargo_lock(content);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("parse"));
}

// -----------------------------------------------------------------------
// CycloneDX generation
// -----------------------------------------------------------------------

#[test]
fn test_generate_cyclonedx_basic() {
    let packages = vec![
        CargoPackage {
            name: "serde".to_string(),
            version: "1.0.200".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
        },
        CargoPackage {
            name: "my-lib".to_string(),
            version: "0.1.0".to_string(),
            source: None,
        },
    ];

    let sbom =
        generate_cyclonedx("my-project", "1.0.0", "2024-01-01T00:00:00Z", &packages).unwrap();

    // Check top-level structure
    assert_eq!(sbom["bomFormat"], "CycloneDX");
    assert_eq!(sbom["specVersion"], "1.5");
    assert_eq!(sbom["version"], 1);

    // Check metadata
    assert_eq!(sbom["metadata"]["component"]["name"], "my-project");
    assert_eq!(sbom["metadata"]["component"]["version"], "1.0.0");
    assert_eq!(sbom["metadata"]["component"]["type"], "application");
    assert!(sbom["metadata"]["timestamp"].is_string());

    // Check components
    let components = sbom["components"].as_array().unwrap();
    assert_eq!(components.len(), 2);

    assert_eq!(components[0]["name"], "serde");
    assert_eq!(components[0]["version"], "1.0.200");
    assert_eq!(components[0]["type"], "library");
    assert_eq!(components[0]["purl"], "pkg:cargo/serde@1.0.200");
    // Registry package should have externalReferences
    assert!(components[0]["externalReferences"].is_array());

    assert_eq!(components[1]["name"], "my-lib");
    assert_eq!(components[1]["version"], "0.1.0");
    // Non-registry package should not have externalReferences
    assert!(components[1]["externalReferences"].is_null());
}

#[test]
fn test_generate_cyclonedx_empty_packages() {
    let sbom = generate_cyclonedx("empty-project", "0.0.1", "2024-01-01T00:00:00Z", &[]).unwrap();
    assert_eq!(sbom["bomFormat"], "CycloneDX");
    let components = sbom["components"].as_array().unwrap();
    assert!(components.is_empty());
}

#[test]
fn test_generate_cyclonedx_purl_format() {
    let packages = vec![CargoPackage {
        name: "tokio".to_string(),
        version: "1.37.0".to_string(),
        source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
    }];

    let sbom = generate_cyclonedx("test", "1.0.0", "2024-01-01T00:00:00Z", &packages).unwrap();
    let components = sbom["components"].as_array().unwrap();
    assert_eq!(components[0]["purl"], "pkg:cargo/tokio@1.37.0");
}

// -----------------------------------------------------------------------
// SPDX generation
// -----------------------------------------------------------------------

#[test]
fn test_generate_spdx_basic() {
    let packages = vec![
        CargoPackage {
            name: "serde".to_string(),
            version: "1.0.200".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
        },
        CargoPackage {
            name: "local-dep".to_string(),
            version: "0.1.0".to_string(),
            source: None,
        },
    ];

    let sbom = generate_spdx(
        "my-app",
        "2.0.0",
        "2024-01-01T00:00:00Z",
        "deadbeef-0000-4000-8000-000000000001",
        &packages,
    )
    .unwrap();

    // Check top-level structure
    assert_eq!(sbom["spdxVersion"], "SPDX-2.3");
    assert_eq!(sbom["dataLicense"], "CC0-1.0");
    assert_eq!(sbom["SPDXID"], "SPDXRef-DOCUMENT");
    assert_eq!(sbom["name"], "my-app-2.0.0");
    assert!(
        sbom["documentNamespace"]
            .as_str()
            .unwrap()
            .starts_with("https://spdx.org/spdxdocs/my-app-2.0.0-")
    );

    // Check packages (root + 2 deps)
    let spdx_packages = sbom["packages"].as_array().unwrap();
    assert_eq!(spdx_packages.len(), 3);

    // Root package
    assert_eq!(spdx_packages[0]["SPDXID"], "SPDXRef-Package");
    assert_eq!(spdx_packages[0]["name"], "my-app");
    assert_eq!(spdx_packages[0]["versionInfo"], "2.0.0");

    // First dependency
    assert_eq!(spdx_packages[1]["SPDXID"], "SPDXRef-Package-0");
    assert_eq!(spdx_packages[1]["name"], "serde");
    assert_eq!(spdx_packages[1]["versionInfo"], "1.0.200");
    assert!(
        spdx_packages[1]["downloadLocation"]
            .as_str()
            .unwrap()
            .contains("crates.io")
    );

    // Local dependency
    assert_eq!(spdx_packages[2]["SPDXID"], "SPDXRef-Package-1");
    assert_eq!(spdx_packages[2]["name"], "local-dep");
    assert_eq!(spdx_packages[2]["downloadLocation"], "NOASSERTION");

    // Check relationships
    let relationships = sbom["relationships"].as_array().unwrap();
    // DESCRIBES + 2 DEPENDS_ON
    assert_eq!(relationships.len(), 3);
    assert_eq!(relationships[0]["relationshipType"], "DESCRIBES");
    assert_eq!(relationships[1]["relationshipType"], "DEPENDS_ON");
    assert_eq!(relationships[2]["relationshipType"], "DEPENDS_ON");
}

#[test]
fn test_generate_spdx_empty_packages() {
    let sbom = generate_spdx(
        "empty",
        "0.0.1",
        "2024-01-01T00:00:00Z",
        "deadbeef-0000-4000-8000-000000000001",
        &[],
    )
    .unwrap();
    assert_eq!(sbom["spdxVersion"], "SPDX-2.3");
    let spdx_packages = sbom["packages"].as_array().unwrap();
    // Only root package
    assert_eq!(spdx_packages.len(), 1);
    let relationships = sbom["relationships"].as_array().unwrap();
    // Only DESCRIBES
    assert_eq!(relationships.len(), 1);
}

#[test]
fn test_generate_spdx_purl_in_external_refs() {
    let packages = vec![CargoPackage {
        name: "clap".to_string(),
        version: "4.5.0".to_string(),
        source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
    }];

    let sbom = generate_spdx(
        "test",
        "1.0.0",
        "2024-01-01T00:00:00Z",
        "deadbeef-0000-4000-8000-000000000001",
        &packages,
    )
    .unwrap();
    let spdx_packages = sbom["packages"].as_array().unwrap();
    let dep = &spdx_packages[1];
    let ext_refs = dep["externalRefs"].as_array().unwrap();
    assert_eq!(ext_refs[0]["referenceCategory"], "PACKAGE-MANAGER");
    assert_eq!(ext_refs[0]["referenceType"], "purl");
    assert_eq!(ext_refs[0]["referenceLocator"], "pkg:cargo/clap@4.5.0");
}

// -----------------------------------------------------------------------
// Config parsing
// -----------------------------------------------------------------------

#[test]
fn test_source_config_defaults() {
    use anodizer_core::config::SourceConfig;
    let cfg = SourceConfig::default();
    assert!(!cfg.is_enabled());
    assert_eq!(cfg.archive_format(), "tar.gz");
}

#[test]
fn test_source_config_enabled() {
    use anodizer_core::config::{SourceConfig, SourceFileEntry};
    let cfg = SourceConfig {
        enabled: Some(true),
        format: Some("zip".to_string()),
        name_template: Some("{{ .ProjectName }}-src-{{ .Version }}".to_string()),
        prefix_template: None,
        files: vec![SourceFileEntry {
            src: "LICENSE".to_string(),
            ..Default::default()
        }],
    };
    assert!(cfg.is_enabled());
    assert_eq!(cfg.archive_format(), "zip");
}

#[test]
fn test_sbom_config_defaults() {
    use anodizer_core::config::SbomConfig;
    let cfg = SbomConfig::default();
    // All fields are None by default
    assert!(cfg.cmd.is_none());
    assert!(cfg.artifacts.is_none());
    assert!(cfg.skip.is_none());
}

#[test]
fn test_config_with_source_and_sbom_yaml() {
    let yaml = r#"
project_name: my-app
crates: []
source:
  enabled: true
  format: tar.gz
  name_template: "{{ .ProjectName }}-source-{{ .Version }}"
sboms:
  cmd: syft
  artifacts: archive
"#;
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.source.is_some());
    let source = config.source.as_ref().unwrap();
    assert!(source.is_enabled());
    assert_eq!(source.archive_format(), "tar.gz");
    assert!(source.name_template.is_some());

    assert_eq!(config.sboms.len(), 1);
    let sbom = &config.sboms[0];
    assert_eq!(sbom.cmd.as_deref(), Some("syft"));
    assert_eq!(sbom.artifacts.as_deref(), Some("archive"));
}

#[test]
fn test_config_without_source_and_sbom() {
    let yaml = r#"
project_name: minimal
crates: []
"#;
    let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(config.source.is_none());
    assert!(config.sboms.is_empty());
}

// -----------------------------------------------------------------------
// Source archive stage (integration-style)
// -----------------------------------------------------------------------

#[test]
fn test_source_archive_with_git_repo() {
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    // Create a test project and git repo
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    // First create dist dir
    std::fs::create_dir_all(&dist).unwrap();

    let output = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = std::process::Command::new("git");
            cmd.args([
                "archive",
                "--format",
                "tar.gz",
                "--prefix",
                "test-project-1.2.3/",
                "--output",
            ])
            .arg(dist.join("test-project-1.2.3.tar.gz").to_str().unwrap())
            .arg("HEAD")
            .current_dir(tmp.path());
            cmd
        },
        "git",
    );

    assert!(
        output.status.success(),
        "git archive failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let archive_path = dist.join("test-project-1.2.3.tar.gz");
    assert!(archive_path.exists());
    assert!(std::fs::metadata(&archive_path).unwrap().len() > 0);
}

#[test]
fn test_source_archive_zip_format_with_git_repo() {
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let output = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = std::process::Command::new("git");
            cmd.args([
                "archive",
                "--format",
                "zip",
                "--prefix",
                "test-project-1.2.3/",
                "--output",
            ])
            .arg(dist.join("test-project-1.2.3.zip").to_str().unwrap())
            .arg("HEAD")
            .current_dir(tmp.path());
            cmd
        },
        "git",
    );

    assert!(
        output.status.success(),
        "git archive failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let archive_path = dist.join("test-project-1.2.3.zip");
    assert!(archive_path.exists());
    assert!(std::fs::metadata(&archive_path).unwrap().len() > 0);
}

// -----------------------------------------------------------------------
// SBOM stage (integration-style using actual Cargo.lock)
// -----------------------------------------------------------------------

#[test]
fn test_sbom_from_real_cargo_lock() {
    let content = r#"
version = 4

[[package]]
name = "anyhow"
version = "1.0.82"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc123"

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "def456"

[[package]]
name = "my-app"
version = "0.1.0"
dependencies = [
 "anyhow",
 "serde",
]
"#;

    let packages = parse_cargo_lock(content).unwrap();
    assert_eq!(packages.len(), 3);

    // Test CycloneDX generation from these packages
    let cdx = generate_cyclonedx("my-app", "0.1.0", "2024-01-01T00:00:00Z", &packages).unwrap();
    let cdx_str = serde_json::to_string_pretty(&cdx).unwrap();
    assert!(cdx_str.contains("CycloneDX"));
    assert!(cdx_str.contains("anyhow"));
    assert!(cdx_str.contains("serde"));

    // Test SPDX generation from these packages
    let spdx = generate_spdx(
        "my-app",
        "0.1.0",
        "2024-01-01T00:00:00Z",
        "deadbeef-0000-4000-8000-000000000001",
        &packages,
    )
    .unwrap();
    let spdx_str = serde_json::to_string_pretty(&spdx).unwrap();
    assert!(spdx_str.contains("SPDX-2.3"));
    assert!(spdx_str.contains("anyhow"));
    assert!(spdx_str.contains("serde"));
}

#[test]
fn test_sbom_written_to_file() {
    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    let packages = vec![CargoPackage {
        name: "tokio".to_string(),
        version: "1.37.0".to_string(),
        source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
    }];

    // CycloneDX
    let cdx = generate_cyclonedx("my-app", "1.0.0", "2024-01-01T00:00:00Z", &packages).unwrap();
    let cdx_path = dist.join("my-app-1.0.0.cdx.json");
    let json_str = serde_json::to_string_pretty(&cdx).unwrap();
    std::fs::write(&cdx_path, &json_str).unwrap();
    assert!(cdx_path.exists());

    // Read it back and verify
    let read_back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cdx_path).unwrap()).unwrap();
    assert_eq!(read_back["bomFormat"], "CycloneDX");

    // SPDX
    let spdx = generate_spdx(
        "my-app",
        "1.0.0",
        "2024-01-01T00:00:00Z",
        "deadbeef-0000-4000-8000-000000000001",
        &packages,
    )
    .unwrap();
    let spdx_path = dist.join("my-app-1.0.0.spdx.json");
    let json_str = serde_json::to_string_pretty(&spdx).unwrap();
    std::fs::write(&spdx_path, &json_str).unwrap();
    assert!(spdx_path.exists());

    let read_back: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&spdx_path).unwrap()).unwrap();
    assert_eq!(read_back["spdxVersion"], "SPDX-2.3");
}

// -----------------------------------------------------------------------
// Dry-run behavior
// -----------------------------------------------------------------------

#[test]
fn test_stage_dry_run_does_not_create_files() {
    use anodizer_core::config::{SbomConfig, SourceConfig};
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    // Construct a real workspace so SourceStage's `get_repo_root` call has
    // an explicit git tree to anchor to. Without this and an explicit
    // `project_root` below, the stage falls back to `std::env::current_dir`,
    // which can be a dangling tempdir under cargo's default parallel test
    // schedule (a peer test in this file may have `set_current_dir`-ed into
    // a tempdir that has since been dropped).
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let mut ctx = TestContextBuilder::new()
        .project_name("test-app")
        .dry_run(true)
        .dist(dist.clone())
        .project_root(tmp.path().to_path_buf())
        .build();

    ctx.config.source = Some(SourceConfig {
        enabled: Some(true),
        format: Some("tar.gz".to_string()),
        name_template: None,
        prefix_template: None,
        files: vec![],
    });
    ctx.config.sboms = vec![SbomConfig {
        ..Default::default()
    }];

    let stage = SourceStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_ok(), "dry-run should succeed: {:?}", result.err());

    // Dist dir should not be created in dry-run mode
    assert!(!dist.exists(), "dist dir should not be created in dry-run");
    assert_eq!(
        ctx.artifacts.all().len(),
        0,
        "no artifacts should be registered in dry-run"
    );
}

/// Regression: `SourceStage::run` must not depend on the process cwd when
/// the caller supplies an explicit `project_root`. CI macOS runners hit a
/// race where a peer cargo test had `set_current_dir`-ed into a tempdir
/// that was subsequently dropped, leaving the process cwd dangling; the
/// stage then fell back to `std::env::current_dir()` and the spawned
/// `git rev-parse --show-toplevel` aborted with "Unable to read current
/// working directory".
///
/// Here we point the process cwd at a directory that is NOT a git repo
/// (`/`) and prove the stage still resolves the workspace via the
/// explicit `project_root`. The test is `#[serial]` because it mutates
/// the process-wide cwd; if any other cwd-sensitive test joins this crate
/// the runner will keep them mutually exclusive.
#[test]
#[serial_test::serial]
fn test_stage_run_does_not_depend_on_cwd() {
    use anodizer_core::config::{SbomConfig, SourceConfig};
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let dist = tmp.path().join("dist");

    let real_commit = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = std::process::Command::new("git");
            cmd.args(["rev-parse", "HEAD"]).current_dir(tmp.path());
            cmd
        },
        "git",
    );
    let real_commit = String::from_utf8_lossy(&real_commit.stdout)
        .trim()
        .to_string();

    let mut ctx = TestContextBuilder::new()
        .project_name("cwd-probe")
        .commit(&real_commit)
        .dist(dist.clone())
        .project_root(tmp.path().to_path_buf())
        .build();

    ctx.config.source = Some(SourceConfig {
        enabled: Some(true),
        format: Some("tar.gz".to_string()),
        name_template: None,
        prefix_template: None,
        files: vec![],
    });
    ctx.config.sboms = vec![SbomConfig {
        ..Default::default()
    }];

    // Stash the real cwd, then point cwd at a directory that is not a git
    // repo. `/` is non-traversable for `git rev-parse --show-toplevel`,
    // mirroring the macOS-CI failure shape. Restore at end-of-test so the
    // test harness's own pre/post teardown still sees a sane cwd.
    let saved = std::env::current_dir().ok();
    std::env::set_current_dir("/").expect("cwd to / must succeed in tests");

    let result = SourceStage.run(&mut ctx);

    // Restore before asserting so a failed assertion does not strand cwd.
    if let Some(prev) = saved {
        let _ = std::env::set_current_dir(prev);
    }

    result.unwrap_or_else(|e| panic!("stage must succeed regardless of cwd: {e}"));
    let artifacts = ctx.artifacts.all();
    assert_eq!(
        artifacts.len(),
        1,
        "exactly one source archive should be emitted: {artifacts:?}"
    );
    assert_eq!(artifacts[0].kind, ArtifactKind::SourceArchive);
    assert!(
        artifacts[0].path.exists(),
        "archive path must exist on disk: {:?}",
        artifacts[0].path
    );
}

#[test]
fn test_stage_skips_when_nothing_enabled() {
    let mut ctx = TestContextBuilder::new().build();
    // No source or sbom config at all
    ctx.config.source = None;
    ctx.config.sboms = vec![];

    let stage = SourceStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_ok());
    assert_eq!(ctx.artifacts.all().len(), 0);
}

#[test]
fn test_stage_skips_when_disabled() {
    use anodizer_core::config::SourceConfig;

    let mut ctx = TestContextBuilder::new().build();
    ctx.config.source = Some(SourceConfig {
        enabled: Some(false),
        ..Default::default()
    });
    // Empty sboms vec means no SBOM generation
    ctx.config.sboms = vec![];

    let stage = SourceStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_ok());
    assert_eq!(ctx.artifacts.all().len(), 0);
}

#[test]
fn source_name_empty_bails_with_actionable_error() {
    // A `source.name_template` that renders to an empty string would
    // produce `dist/.tar.gz` (a hidden file) which downstream stages
    // cannot resolve. The stage must bail with an actionable hint
    // naming the format and a remediation step.
    use anodizer_core::config::SourceConfig;

    let mut ctx = TestContextBuilder::new().build();
    ctx.config.project_name = String::new();
    ctx.config.source = Some(SourceConfig {
        enabled: Some(true),
        name_template: Some(String::new()),
        ..Default::default()
    });
    ctx.config.sboms = vec![];

    let stage = SourceStage;
    let err = stage
        .run(&mut ctx)
        .expect_err("empty source archive name must bail");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("source:"),
        "error must carry the source: prefix, got: {chain}"
    );
    assert!(
        chain.contains("empty"),
        "error must describe the empty-name condition, got: {chain}"
    );
    assert!(
        chain.contains("name_template") || chain.contains("project_name"),
        "error must name the source fields to fix, got: {chain}"
    );
}

#[test]
fn source_prefix_var_set_from_rendered_prefix_template() {
    // A configured `prefix_template` becomes the `SourcePrefix` var with any
    // trailing `/` stripped, so downstream consumers (e.g. an srpm
    // `%autosetup -n`) get the exact top-level archive dir.
    use anodizer_core::config::{SbomConfig, SourceConfig};
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let mut ctx = TestContextBuilder::new()
        .project_name("test-app")
        .dry_run(true)
        .dist(tmp.path().join("dist"))
        .project_root(tmp.path().to_path_buf())
        .build();
    // Pin Version so the rendered prefix is deterministic regardless of the
    // synthetic git tag.
    ctx.template_vars_mut().set("Version", "0.5.0-SNAPSHOT-abc");
    ctx.config.source = Some(SourceConfig {
        enabled: Some(true),
        format: Some("tar.gz".to_string()),
        name_template: None,
        prefix_template: Some("{{ ProjectName }}-{{ Version }}/".to_string()),
        files: vec![],
    });
    ctx.config.sboms = vec![SbomConfig::default()];

    SourceStage.run(&mut ctx).expect("dry-run should succeed");

    assert_eq!(
        ctx.template_vars().get("SourcePrefix").map(String::as_str),
        Some("test-app-0.5.0-SNAPSHOT-abc"),
        "SourcePrefix must be the rendered prefix with trailing `/` stripped"
    );
}

#[test]
fn source_prefix_var_empty_when_prefix_template_unset() {
    // Default: no prefix_template → flat archive, empty
    // SourcePrefix. The srpm auto-gen spec turns this into `%autosetup -c`.
    use anodizer_core::config::{SbomConfig, SourceConfig};
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let mut ctx = TestContextBuilder::new()
        .project_name("test-app")
        .dry_run(true)
        .dist(tmp.path().join("dist"))
        .project_root(tmp.path().to_path_buf())
        .build();
    ctx.config.source = Some(SourceConfig {
        enabled: Some(true),
        format: Some("tar.gz".to_string()),
        name_template: None,
        prefix_template: None,
        files: vec![],
    });
    ctx.config.sboms = vec![SbomConfig::default()];

    SourceStage.run(&mut ctx).expect("dry-run should succeed");

    assert_eq!(
        ctx.template_vars().get("SourcePrefix").map(String::as_str),
        Some(""),
        "SourcePrefix must be empty when prefix_template is unset"
    );
}

#[test]
fn source_prefix_var_reflects_trailing_slash_directory_semantics() {
    // `git archive --prefix` only creates a top-level directory when the
    // prefix ends with `/`. A slash-less prefix is glued onto every path
    // (`foomain.rs`) → FLAT archive, no dir → SourcePrefix must be empty so
    // the srpm spec routes to `%autosetup -c`. A nested trailing-slash prefix
    // keeps its inner slashes (the dir path), only the trailing one stripped.
    use anodizer_core::config::{SbomConfig, SourceConfig};
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let cases = [
        // (prefix_template, expected SourcePrefix)
        ("app-1.0/", "app-1.0"), // dir prefix → dir name
        ("app-1.0", ""),         // slash-less → flat → empty
        ("foo/bar/", "foo/bar"), // nested dir → inner slashes kept
    ];
    for (template, expected) in cases {
        let tmp = TempDir::new().unwrap();
        create_test_project(tmp.path());
        init_git_repo(tmp.path());

        let mut ctx = TestContextBuilder::new()
            .project_name("test-app")
            .dry_run(true)
            .dist(tmp.path().join("dist"))
            .project_root(tmp.path().to_path_buf())
            .build();
        ctx.config.source = Some(SourceConfig {
            enabled: Some(true),
            format: Some("tar.gz".to_string()),
            name_template: None,
            prefix_template: Some(template.to_string()),
            files: vec![],
        });
        ctx.config.sboms = vec![SbomConfig::default()];

        SourceStage.run(&mut ctx).expect("dry-run should succeed");

        assert_eq!(
            ctx.template_vars().get("SourcePrefix").map(String::as_str),
            Some(expected),
            "prefix_template {template:?} must yield SourcePrefix {expected:?}"
        );
    }
}

// -----------------------------------------------------------------------
// ArtifactKind variants
// -----------------------------------------------------------------------

#[test]
fn test_artifact_kind_source_archive() {
    assert_eq!(ArtifactKind::SourceArchive.as_str(), "source_archive");
    let json = serde_json::to_value(ArtifactKind::SourceArchive).unwrap();
    assert_eq!(json, "source_archive");
}

#[test]
fn test_artifact_kind_sbom() {
    assert_eq!(ArtifactKind::Sbom.as_str(), "sbom");
    let json = serde_json::to_value(ArtifactKind::Sbom).unwrap();
    assert_eq!(json, "sbom");
}

// -----------------------------------------------------------------------
// UUID generation
// -----------------------------------------------------------------------

#[test]
fn test_deterministic_uuid_from_format_and_stability() {
    let uuid = deterministic_uuid_from("proj-1.0.0");
    // Should be in format: 8-4-4-4-12 hex chars
    let parts: Vec<&str> = uuid.split('-').collect();
    assert_eq!(parts.len(), 5, "UUID should have 5 parts: {}", uuid);
    assert_eq!(parts[0].len(), 8);
    assert_eq!(parts[1].len(), 4);
    assert_eq!(parts[2].len(), 4);
    assert_eq!(parts[3].len(), 4);
    assert_eq!(parts[4].len(), 12);

    // Version nibble should be 4
    assert!(
        parts[2].starts_with('4'),
        "UUID version nibble should be 4: {}",
        uuid
    );

    // Same seed → identical output (load-bearing for release-asset idempotency)
    assert_eq!(uuid, deterministic_uuid_from("proj-1.0.0"));
    // Different seed → different output (avoids namespace collisions)
    assert_ne!(uuid, deterministic_uuid_from("proj-1.0.1"));
}

#[test]
fn test_sbom_byte_identical_across_runs() {
    // Load-bearing for release-asset idempotency: anodizer-action's outer
    // retry wrapper may regenerate the SBOM between `release` uploads; if
    // the bytes differ, GitHub's ReleaseAsset API rejects the re-upload
    // with `already_exists` (size mismatch).
    let packages = vec![
        CargoPackage {
            name: "serde".to_string(),
            version: "1.0.200".to_string(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
        },
        CargoPackage {
            name: "local".to_string(),
            version: "0.1.0".to_string(),
            source: None,
        },
    ];

    let ts = "2024-06-01T12:34:56+00:00";
    let ns = deterministic_uuid_from("sample-app-0.2.0");

    let a = generate_cyclonedx("sample-app", "0.2.0", ts, &packages).unwrap();
    let b = generate_cyclonedx("sample-app", "0.2.0", ts, &packages).unwrap();
    assert_eq!(
        serde_json::to_string_pretty(&a).unwrap(),
        serde_json::to_string_pretty(&b).unwrap(),
    );

    let a = generate_spdx("sample-app", "0.2.0", ts, &ns, &packages).unwrap();
    let b = generate_spdx("sample-app", "0.2.0", ts, &ns, &packages).unwrap();
    assert_eq!(
        serde_json::to_string_pretty(&a).unwrap(),
        serde_json::to_string_pretty(&b).unwrap(),
    );
}

// -----------------------------------------------------------------------
// SBOM format validation tests
// -----------------------------------------------------------------------

#[test]
fn test_cyclonedx_has_required_fields() {
    let packages = vec![CargoPackage {
        name: "test-dep".to_string(),
        version: "1.0.0".to_string(),
        source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
    }];

    let sbom = generate_cyclonedx("proj", "1.0.0", "2024-01-01T00:00:00Z", &packages).unwrap();

    // Required CycloneDX 1.5 fields
    assert!(sbom.get("bomFormat").is_some(), "missing bomFormat");
    assert!(sbom.get("specVersion").is_some(), "missing specVersion");
    assert!(sbom.get("version").is_some(), "missing version");
    assert!(sbom.get("metadata").is_some(), "missing metadata");
    assert!(sbom.get("components").is_some(), "missing components");

    // Metadata sub-fields
    let metadata = &sbom["metadata"];
    assert!(metadata.get("timestamp").is_some(), "missing timestamp");
    assert!(metadata.get("component").is_some(), "missing component");
    assert!(metadata.get("tools").is_some(), "missing tools");

    // Component sub-fields
    let comp = &sbom["components"][0];
    assert!(comp.get("type").is_some(), "missing component type");
    assert!(comp.get("name").is_some(), "missing component name");
    assert!(comp.get("version").is_some(), "missing component version");
    assert!(comp.get("purl").is_some(), "missing component purl");
}

#[test]
fn test_spdx_has_required_fields() {
    let packages = vec![CargoPackage {
        name: "test-dep".to_string(),
        version: "1.0.0".to_string(),
        source: Some("registry+https://github.com/rust-lang/crates.io-index".to_string()),
    }];

    let sbom = generate_spdx(
        "proj",
        "1.0.0",
        "2024-01-01T00:00:00Z",
        "deadbeef-0000-4000-8000-000000000001",
        &packages,
    )
    .unwrap();

    // Required SPDX 2.3 fields
    assert!(sbom.get("spdxVersion").is_some(), "missing spdxVersion");
    assert!(sbom.get("dataLicense").is_some(), "missing dataLicense");
    assert!(sbom.get("SPDXID").is_some(), "missing SPDXID");
    assert!(sbom.get("name").is_some(), "missing name");
    assert!(
        sbom.get("documentNamespace").is_some(),
        "missing documentNamespace"
    );
    assert!(sbom.get("creationInfo").is_some(), "missing creationInfo");
    assert!(sbom.get("packages").is_some(), "missing packages");
    assert!(sbom.get("relationships").is_some(), "missing relationships");

    // Package sub-fields
    let pkg = &sbom["packages"][1]; // first dependency (index 0 is root)
    assert!(pkg.get("SPDXID").is_some(), "missing package SPDXID");
    assert!(pkg.get("name").is_some(), "missing package name");
    assert!(
        pkg.get("versionInfo").is_some(),
        "missing package versionInfo"
    );
    assert!(
        pkg.get("downloadLocation").is_some(),
        "missing package downloadLocation"
    );
    assert!(
        pkg.get("externalRefs").is_some(),
        "missing package externalRefs"
    );
}

// -----------------------------------------------------------------------
// SourceStage integration test (runs through the Stage interface)
// -----------------------------------------------------------------------

#[test]
fn test_source_stage_run_creates_archive_in_git_repo() {
    use anodizer_core::config::SourceConfig;
    use anodizer_core::stage::Stage;
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");

    // Create a test project and git repo
    create_test_project(tmp.path());
    // Write a Cargo.lock so SBOM can also find it (not needed for this test
    // but keeps the fixture realistic)
    std::fs::write(tmp.path().join("Cargo.lock"), "version = 4\n").unwrap();
    init_git_repo(tmp.path());

    // Get the real commit hash from the test repo so git archive can resolve it
    let real_commit = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = std::process::Command::new("git");
            cmd.args(["rev-parse", "HEAD"]).current_dir(tmp.path());
            cmd
        },
        "git",
    );
    let real_commit = String::from_utf8_lossy(&real_commit.stdout)
        .trim()
        .to_string();

    let mut ctx = TestContextBuilder::new()
        .project_name("test-project")
        .commit(&real_commit)
        .source(SourceConfig {
            enabled: Some(true),
            format: Some("tar.gz".to_string()),
            name_template: None,
            prefix_template: None,
            files: vec![],
        })
        .dist(dist.clone())
        .project_root(tmp.path().to_path_buf())
        .build();

    let stage = SourceStage;
    let result = stage.run(&mut ctx);

    assert!(
        result.is_ok(),
        "SourceStage.run() should succeed: {:?}",
        result.err()
    );

    // Should have produced exactly one source archive artifact
    let artifacts = ctx.artifacts.all();
    assert_eq!(
        artifacts.len(),
        1,
        "expected 1 artifact, got {}",
        artifacts.len()
    );
    assert_eq!(artifacts[0].kind, ArtifactKind::SourceArchive);
    assert!(
        artifacts[0].path.exists(),
        "archive file should exist at {:?}",
        artifacts[0].path
    );
    assert!(
        std::fs::metadata(&artifacts[0].path).unwrap().len() > 0,
        "archive file should not be empty"
    );
}

// -----------------------------------------------------------------------
// strip_parent behavior
// -----------------------------------------------------------------------

#[test]
fn test_source_archive_strip_parent_flattens_nested_file() {
    use anodizer_core::config::SourceFileEntry;
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    // Create a test project and git repo FIRST
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    // Create a nested file AFTER git init so it is NOT tracked by git archive
    let nested_dir = tmp.path().join("extras").join("deep").join("nested");
    std::fs::create_dir_all(&nested_dir).unwrap();
    std::fs::write(
        nested_dir.join("config.toml"),
        "[settings]\nkey = \"value\"\n",
    )
    .unwrap();

    let log = anodizer_core::log::StageLogger::new("source", anodizer_core::log::Verbosity::Quiet);

    let extra_files = vec![SourceFileEntry {
        src: nested_dir.join("config.toml").to_string_lossy().to_string(),
        dst: None,
        strip_parent: Some(true),
        info: None,
    }];

    // create_source_archive uses repo_root (tmp.path()) directly via current_dir(),
    // so no process-wide CWD mutation is needed.

    let result = create_source_archive(&SourceArchiveInputs {
        dist: &dist,
        format: "tar.gz",
        name: "test-project-1.0.0",
        prefix: "test-project-1.0.0",
        extra_files: &extra_files,
        repo_root: tmp.path(),
        commit: "HEAD",
        log: &log,
        strict: false,
        sde_mtime: None,
    });

    let archive_path =
        result.unwrap_or_else(|e| panic!("create_source_archive should succeed: {e}"));
    assert!(archive_path.exists(), "archive should exist");

    // Open the tar.gz and check that config.toml appears directly under
    // the prefix, NOT under deep/nested/
    let file = std::fs::File::open(&archive_path).unwrap();
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);

    let entries: Vec<String> = tar
        .entries()
        .unwrap()
        .filter_map(|e| {
            let e = e.ok()?;
            Some(e.path().ok()?.to_string_lossy().to_string())
        })
        .collect();

    // Should contain "test-project-1.0.0/config.toml"
    assert!(
        entries
            .iter()
            .any(|e| e == "test-project-1.0.0/config.toml"),
        "expected 'test-project-1.0.0/config.toml' in archive, got entries: {:?}",
        entries
    );
    // Should NOT contain the nested path
    assert!(
        !entries.iter().any(|e| e.contains("deep/nested")),
        "should not contain deep/nested path, got entries: {:?}",
        entries
    );
}

#[test]
fn test_source_archive_strip_parent_with_dst() {
    use anodizer_core::config::SourceFileEntry;
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    // Create extra file AFTER git init so it is not tracked
    let nested_dir = tmp.path().join("extras").join("deep");
    std::fs::create_dir_all(&nested_dir).unwrap();
    std::fs::write(nested_dir.join("app.conf"), "port = 8080\n").unwrap();

    let log = anodizer_core::log::StageLogger::new("source", anodizer_core::log::Verbosity::Quiet);

    // strip_parent=true + dst="etc" => file should appear as prefix/etc/app.conf
    let extra_files = vec![SourceFileEntry {
        src: nested_dir.join("app.conf").to_string_lossy().to_string(),
        dst: Some("etc".to_string()),
        strip_parent: Some(true),
        info: None,
    }];

    let result = create_source_archive(&SourceArchiveInputs {
        dist: &dist,
        format: "tar.gz",
        name: "myapp-2.0.0",
        prefix: "myapp-2.0.0",
        extra_files: &extra_files,
        repo_root: tmp.path(),
        commit: "HEAD",
        log: &log,
        strict: false,
        sde_mtime: None,
    });

    let archive_path =
        result.unwrap_or_else(|e| panic!("create_source_archive should succeed: {e}"));

    let file = std::fs::File::open(&archive_path).unwrap();
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);

    let entries: Vec<String> = tar
        .entries()
        .unwrap()
        .filter_map(|e| {
            let e = e.ok()?;
            Some(e.path().ok()?.to_string_lossy().to_string())
        })
        .collect();

    // strip_parent + dst: filename goes under dst directory
    assert!(
        entries.iter().any(|e| e == "myapp-2.0.0/etc/app.conf"),
        "expected 'myapp-2.0.0/etc/app.conf' in archive, got entries: {:?}",
        entries
    );
}

#[test]
fn test_source_archive_no_strip_parent_dst_is_literal_rename() {
    use anodizer_core::config::SourceFileEntry;
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    // Create extra file AFTER git init so it is not tracked
    let extra_file = tmp.path().join("README.md");
    std::fs::write(&extra_file, "# Hello\n").unwrap();

    let log = anodizer_core::log::StageLogger::new("source", anodizer_core::log::Verbosity::Quiet);

    // strip_parent=false (default) + dst="docs/README.txt" => literal rename
    let extra_files = vec![SourceFileEntry {
        src: extra_file.to_string_lossy().to_string(),
        dst: Some("docs/README.txt".to_string()),
        strip_parent: None,
        info: None,
    }];

    let result = create_source_archive(&SourceArchiveInputs {
        dist: &dist,
        format: "tar.gz",
        name: "proj-3.0.0",
        prefix: "proj-3.0.0",
        extra_files: &extra_files,
        repo_root: tmp.path(),
        commit: "HEAD",
        log: &log,
        strict: false,
        sde_mtime: None,
    });

    let archive_path =
        result.unwrap_or_else(|e| panic!("create_source_archive should succeed: {e}"));

    let file = std::fs::File::open(&archive_path).unwrap();
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);

    let entries: Vec<String> = tar
        .entries()
        .unwrap()
        .filter_map(|e| {
            let e = e.ok()?;
            Some(e.path().ok()?.to_string_lossy().to_string())
        })
        .collect();

    // Without strip_parent, dst is used literally
    assert!(
        entries.iter().any(|e| e == "proj-3.0.0/docs/README.txt"),
        "expected 'proj-3.0.0/docs/README.txt' in archive, got entries: {:?}",
        entries
    );
}

#[test]
fn test_source_extra_files_with_info() {
    use anodizer_core::config::{SourceFileEntry, SourceFileInfo};
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    // Create extra file AFTER git init so it is not tracked
    let extra_file = tmp.path().join("config.toml");
    std::fs::write(&extra_file, b"[settings]\nfoo = true").unwrap();

    let log = anodizer_core::log::StageLogger::new("source", anodizer_core::log::Verbosity::Quiet);

    let extra_files = vec![SourceFileEntry {
        src: extra_file.to_string_lossy().to_string(),
        dst: None,
        strip_parent: None,
        info: Some(SourceFileInfo {
            owner: Some("deploy".to_string()),
            group: Some("staff".to_string()),
            mode: Some(anodizer_core::config::StringOrU32(0o644)),
            mtime: Some("2024-01-01T00:00:00Z".to_string()),
        }),
    }];

    let result = create_source_archive(&SourceArchiveInputs {
        dist: &dist,
        format: "tar.gz",
        name: "test-src",
        prefix: "test-src",
        extra_files: &extra_files,
        repo_root: tmp.path(),
        commit: "HEAD",
        log: &log,
        strict: false,
        sde_mtime: None,
    });

    assert!(result.is_ok(), "failed: {:?}", result.err());

    // Read back and verify metadata
    let archive_path = result.unwrap();
    let file = std::fs::File::open(&archive_path).unwrap();
    let dec = flate2::read::GzDecoder::new(file);
    let mut tar_archive = tar::Archive::new(dec);

    for tar_entry in tar_archive.entries().unwrap() {
        let tar_entry = tar_entry.unwrap();
        let path = tar_entry.path().unwrap().to_string_lossy().to_string();
        if path.ends_with("config.toml") {
            let header = tar_entry.header();
            assert_eq!(header.mode().unwrap(), 0o644, "mode mismatch");
            assert_eq!(
                header.username().unwrap().unwrap(),
                "deploy",
                "owner mismatch"
            );
            assert_eq!(
                header.groupname().unwrap().unwrap(),
                "staff",
                "group mismatch"
            );
            // 2024-01-01T00:00:00Z = 1704067200 unix timestamp
            assert_eq!(header.mtime().unwrap(), 1704067200, "mtime mismatch");
            return;
        }
    }
    panic!("config.toml not found in source archive");
}

// ---------------------------------------------------------------------------
// 2026-05-08 second-opinion parity audit regressions (Q-src1, Q-src2)
// ---------------------------------------------------------------------------

/// Q-src1 — `core::git::get_head_commit` resolves HEAD to the full SHA via
/// `git rev-parse HEAD`. Used by `SourceStage` when `ctx.git_info` was not
/// pre-populated, replacing the previous literal `"HEAD"` string passed to
/// `git archive`, pinned to the full commit.
///
/// Runs `git rev-parse HEAD` against the cargo workspace itself (the
/// anodizer repo is a git repo); avoids `set_current_dir` so the test is
/// safe under cargo's parallel-test default.
#[test]
fn test_get_head_commit_resolves_to_sha() {
    let sha = match anodizer_core::git::get_head_commit() {
        Ok(s) => s,
        Err(_) => {
            // Skip when the test runner is not inside a git repo (rare, but
            // tolerated — the contract is only about return shape, not
            // about the runner environment).
            return;
        }
    };
    assert_eq!(
        sha.len(),
        40,
        "must return a 40-char hex SHA, not the literal 'HEAD' (got {sha:?})"
    );
    assert_ne!(
        sha, "HEAD",
        "must resolve HEAD, never return the literal ref"
    );
    assert!(
        sha.chars().all(|c| c.is_ascii_hexdigit()),
        "must be a hex SHA, got {sha:?}"
    );
}

/// Q-src2 — Source-archive zip append must reuse the source archive's
/// compression method for extras. A copy-style round-trip
/// preservation. Previously hardcoded `zip::CompressionMethod::Deflated`,
/// which silently mismatched the source's method when (e.g.) git produced a
/// `Stored` zip.
///
/// Strategy: drive `create_source_archive` directly. `git archive --format
/// zip` produces Deflated entries by default, so the appended extra should
/// also be Deflated (existing behavior preserved). Then verify that when
/// the source zip's first entry IS Stored, the extra would also be Stored —
/// exercised by re-running the append loop over a hand-rolled Stored zip.
/// Both halves protect against the regression.
#[test]
fn test_source_archive_zip_extras_match_source_compression_default_deflated() {
    use anodizer_core::config::SourceFileEntry;
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    let extra_src = tmp.path().join("EXTRA.txt");
    std::fs::write(&extra_src, b"extra content").unwrap();
    let extras = vec![SourceFileEntry {
        src: extra_src.to_string_lossy().to_string(),
        dst: Some("EXTRA.txt".to_string()),
        ..Default::default()
    }];

    let ctx = TestContextBuilder::new().build();
    let log = ctx.logger("source");

    // git archive produces Deflated entries by default → extras must also
    // come out Deflated, matching the source.
    let archive_path = create_source_archive(&SourceArchiveInputs {
        dist: &dist,
        format: "zip",
        name: "deflate-src",
        prefix: "p/",
        extra_files: &extras,
        repo_root: tmp.path(),
        commit: "HEAD",
        log: &log,
        strict: false,
        sde_mtime: None,
    })
    .unwrap();

    let zip_bytes = std::fs::read(&archive_path).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&zip_bytes)).unwrap();
    // Discover the actual extra path and the source method. The extra is
    // appended under the configured prefix; source entries are everything
    // else.
    let mut source_method: Option<zip::CompressionMethod> = None;
    let mut extra_idx: Option<usize> = None;
    for i in 0..zip.len() {
        let entry = zip.by_index(i).unwrap();
        let name = entry.name().to_string();
        if name.ends_with("EXTRA.txt") {
            extra_idx = Some(i);
        } else if !entry.is_dir() && source_method.is_none() {
            source_method = Some(entry.compression());
        }
    }
    let source_method = source_method.expect("source archive must have at least one entry");
    let extra_idx = extra_idx.expect("expected an EXTRA.txt entry in the appended zip");
    let extra_entry = zip.by_index(extra_idx).unwrap();
    assert_eq!(
        extra_entry.compression(),
        source_method,
        "appended extra must reuse the source archive's compression method \
         (got source={source_method:?}, extra={:?})",
        extra_entry.compression()
    );
}

/// Q-src2 (Stored-source variant) — when the source zip uses Stored,
/// extras must too. Exercised against a hand-rolled Stored zip via the
/// SAME copy+append loop the production code uses (re-implemented here so
/// we can pre-stage a Stored zip; otherwise git archive's default Deflate
/// path applies).
#[test]
fn test_source_archive_zip_extras_match_stored_source_compression() {
    use std::io::{Read as _, Write as _};

    let tmp = TempDir::new().unwrap();
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();

    // Build a "source" zip with all entries Stored.
    let stored_zip = dist.join("stored-src.zip");
    {
        let f = std::fs::File::create(&stored_zip).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("README.md", opts).unwrap();
        zw.write_all(b"hello").unwrap();
        zw.finish().unwrap();
    }

    // Mirror the in-tree append loop verbatim.
    let zip_data = std::fs::read(&stored_zip).unwrap();
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&zip_data)).unwrap();
    let mut source_compression: Option<zip::CompressionMethod> = None;
    let mut out_buf: Vec<u8> = Vec::new();
    {
        let writer = std::io::Cursor::new(&mut out_buf);
        let mut zw = zip::ZipWriter::new(writer);
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).unwrap();
            let m = entry.compression();
            if source_compression.is_none() && !entry.is_dir() {
                source_compression = Some(m);
            }
            let opts = zip::write::SimpleFileOptions::default().compression_method(m);
            zw.start_file(entry.name().to_string(), opts).unwrap();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            zw.write_all(&data).unwrap();
        }
        let extras_method = source_compression.unwrap_or(zip::CompressionMethod::Deflated);
        let opts = zip::write::SimpleFileOptions::default().compression_method(extras_method);
        zw.start_file("EXTRA.txt", opts).unwrap();
        zw.write_all(b"extra").unwrap();
        zw.finish().unwrap();
    }

    let mut out = zip::ZipArchive::new(std::io::Cursor::new(&out_buf)).unwrap();
    let extra = out.by_name("EXTRA.txt").unwrap();
    assert_eq!(
        extra.compression(),
        zip::CompressionMethod::Stored,
        "extras must inherit the Stored compression of the source zip"
    );
}

/// A zip source archive with extra files must be byte-identical across two
/// runs under a fixed `SOURCE_DATE_EPOCH`: stable entry ordering (sort) plus a
/// pinned per-entry last-modified time. Before the fix the zip path appended
/// extras in caller order and let `SimpleFileOptions::default` choose the
/// timestamp, so the bytes drifted run-to-run.
#[test]
fn test_source_archive_zip_extras_deterministic_under_sde() {
    use anodizer_core::config::SourceFileEntry;
    use anodizer_core::test_helpers::{create_test_project, init_git_repo};

    let tmp = TempDir::new().unwrap();
    create_test_project(tmp.path());
    init_git_repo(tmp.path());

    // Several untracked extras whose `src` order differs from any plausible
    // filesystem-walk order, so the sort is load-bearing.
    let extras_dir = tmp.path().join("extras");
    std::fs::create_dir_all(&extras_dir).unwrap();
    for name in ["zeta.txt", "alpha.txt", "mid.txt"] {
        std::fs::write(extras_dir.join(name), format!("content of {name}\n")).unwrap();
    }
    let extra_files = vec![
        SourceFileEntry {
            src: extras_dir.join("zeta.txt").to_string_lossy().to_string(),
            dst: None,
            strip_parent: Some(true),
            info: None,
        },
        SourceFileEntry {
            src: extras_dir.join("alpha.txt").to_string_lossy().to_string(),
            dst: None,
            strip_parent: Some(true),
            info: None,
        },
        SourceFileEntry {
            src: extras_dir.join("mid.txt").to_string_lossy().to_string(),
            dst: None,
            strip_parent: Some(true),
            info: None,
        },
    ];

    let log = anodizer_core::log::StageLogger::new("source", anodizer_core::log::Verbosity::Quiet);

    let build = |dist: &std::path::Path| -> Vec<u8> {
        std::fs::create_dir_all(dist).unwrap();
        let path = create_source_archive(&SourceArchiveInputs {
            dist,
            format: "zip",
            name: "proj-1.0.0",
            prefix: "proj-1.0.0",
            extra_files: &extra_files,
            repo_root: tmp.path(),
            commit: "HEAD",
            log: &log,
            strict: false,
            sde_mtime: Some(1_577_836_800), // 2020-01-01T00:00:00Z
        })
        .unwrap_or_else(|e| panic!("create_source_archive should succeed: {e}"));
        std::fs::read(&path).unwrap()
    };

    let first = build(&tmp.path().join("dist-a"));
    let second = build(&tmp.path().join("dist-b"));

    assert_eq!(
        first, second,
        "zip source archive with extras must be byte-identical across runs under a fixed SOURCE_DATE_EPOCH"
    );

    // Entry order inside the zip must be the sorted extra order regardless of
    // the caller's input order (alpha < mid < zeta).
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&first)).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    let positions: Vec<usize> = ["alpha.txt", "mid.txt", "zeta.txt"]
        .iter()
        .map(|want| {
            names
                .iter()
                .position(|n| n == &format!("proj-1.0.0/{want}"))
                .unwrap_or_else(|| panic!("missing extra {want} in {names:?}"))
        })
        .collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "extras must be appended in sorted src order, got positions {positions:?} in {names:?}"
    );
}
