use super::*;

/// Build a job whose "nfpm" is a stub script printing `msg` to stdout
/// (where nfpm reports its errors) and exiting 1.
#[cfg(unix)]
fn failing_job(dir: &tempfile::TempDir, msg: &str) -> NfpmJob {
    let stub = dir.path().join("nfpm-stub.sh");
    anodizer_core::test_helpers::fake_tool::write_executable_script(
        &stub,
        &format!("#!/bin/sh\necho '{msg}'\nexit 1\n"),
    );
    NfpmJob {
        _tmp_dir: tempfile::TempDir::new().unwrap(),
        pkg_path: dir.path().join("out.msix"),
        format: "msix".to_string(),
        cmd_args: vec![stub.to_string_lossy().into_owned()],
        mtime: None,
        mtime_repr: None,
        extra_env: Vec::new(),
        target: None,
        crate_name: "demo".to_string(),
        pkg_metadata: Default::default(),
    }
}

/// The version-floor hint fires only on the unregistered-packager
/// signature an old nfpm emits — not on other msix failures.
#[test]
#[cfg(unix)]
fn msix_version_floor_hint_scoped_to_unregistered_packager() {
    let dir = tempfile::TempDir::new().unwrap();
    let job = failing_job(&dir, "no packager registered for the format msix");
    let err = execute_nfpm_jobs(&[job], 1, anodizer_core::log::Verbosity::Quiet)
        .expect_err("stub exits 1");
    assert!(
        format!("{err:#}").contains("requires nfpm >= 2.46.0"),
        "hint must fire on the unregistered-packager signature: {err:#}"
    );

    let job = failing_job(&dir, "package msix.applications must be provided");
    let err = execute_nfpm_jobs(&[job], 1, anodizer_core::log::Verbosity::Quiet)
        .expect_err("stub exits 1");
    assert!(
        !format!("{err:#}").contains("requires nfpm >= 2.46.0"),
        "hint must NOT fire on a config-validation failure: {err:#}"
    );
}

/// `v1` is the baseline x86-64 level, so it names no Debian architecture
/// variant; only the optimized levels do, and only on amd64.
#[test]
fn deb_arch_variant_drops_v1() {
    let table = [
        ("amd64", Some("v1"), None),
        ("amd64", Some(""), None),
        ("amd64", None, None),
        ("amd64", Some("v3"), Some("v3")),
        ("arm64", Some("v1"), None),
        ("arm64", Some("v3"), None),
    ];
    for (arch, variant, want) in table {
        assert_eq!(
            deb_arch_variant(arch, variant).as_deref(),
            want,
            "deb_arch_variant({arch}, {variant:?})"
        );
    }
}

/// Two amd64 builds of one crate on one target triple, differing only in
/// their micro-arch level: the baseline package stamps no variant and the
/// optimized one stamps its own — not its sibling's — whether or not the
/// config declares a `deb:` block.
#[test]
fn deb_package_has_no_amd64v1_architecture() {
    use anodizer_core::config::{Config, CrateConfig, NfpmConfig, NfpmDebConfig};

    for declares_deb_block in [false, true] {
        let tmp = tempfile::TempDir::new().unwrap();
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            maintainer: Some("Jane Doe <jane@example.com>".to_string()),
            file_name_template: Some("myapp_{{ .Version }}{{ .Amd64 }}".to_string()),
            deb: declares_deb_block.then(|| NfpmDebConfig {
                compression: Some("gzip".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let config = Config {
            project_name: "myapp".to_string(),
            dist: tmp.path().join("dist"),
            crates: vec![CrateConfig {
                name: "myapp".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                nfpms: Some(vec![nfpm_cfg]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(
            config,
            anodizer_core::context::ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");
        for variant in ["v1", "v3"] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: format!("myapp-{variant}"),
                path: std::path::PathBuf::from(format!("dist/myapp-{variant}")),
                target: Some("x86_64-unknown-linux-gnu".to_string()),
                crate_name: "myapp".to_string(),
                metadata: HashMap::from([("amd64_variant".to_string(), variant.to_string())]),
                size: None,
            });
        }

        let rendered = nfpm_yaml_configs_for_crate(&ctx, "myapp").unwrap();
        let yaml_for = |variant: &str| -> String {
            rendered
                .iter()
                .find(|r| r.amd64_variant.as_deref() == Some(variant))
                .unwrap_or_else(|| panic!("a config for {variant}"))
                .yaml
                .clone()
        };
        assert!(
            !yaml_for("v1").contains("arch_variant"),
            "deb block={declares_deb_block}: a baseline amd64 build must stamp no \
             architecture variant:\n{}",
            yaml_for("v1")
        );
        assert!(
            yaml_for("v3").contains("arch_variant: v3"),
            "deb block={declares_deb_block}: an optimized build must stamp its own \
             variant:\n{}",
            yaml_for("v3")
        );
    }
}
