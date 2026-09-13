use super::*;
use anodizer_core::config::{Config, CrateConfig, WorkspaceConfig};
use std::fs;
use tempfile::tempdir;

fn make_crate(name: &str, tag_template: &str, depends_on: Option<Vec<&str>>) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some(tag_template.to_string()),
        depends_on: depends_on.map(|d| d.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

fn make_config(crates: Vec<CrateConfig>) -> Config {
    Config {
        project_name: "test".to_string(),
        crates,
        ..Default::default()
    }
}

fn test_logger() -> StageLogger {
    StageLogger::new("check", Verbosity::Quiet)
}

/// A guaranteed-nonexistent directory: `discover_cargo_workspace_member_names`
/// finds no `Cargo.toml` there, so [`check_workspace_membership`] no-ops —
/// keeping every other test in this module independent of the
/// workspace-membership guard, which has its own dedicated tests below.
const NO_WORKSPACE_BASE: &str = "/nonexistent/anodizer-check-config-test-base";

/// Write a hermetic on-disk Cargo workspace at `root`: a root
/// `Cargo.toml` declaring `members`, and each `(member_path, package_name,
/// intra_workspace_deps)` tuple's own `Cargo.toml`, with each dep written
/// as `dep.workspace = true` (this repo's own dependency shape).
fn write_disk_workspace(root: &std::path::Path, members: &[(&str, &str, &[&str])]) {
    fs::create_dir_all(root).unwrap();
    let member_list = members
        .iter()
        .map(|(path, _, _)| format!("\"{path}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        root.join("Cargo.toml"),
        format!("[workspace]\nmembers = [{member_list}]\n"),
    )
    .unwrap();
    for (path, name, deps) in members {
        let dir = root.join(path);
        fs::create_dir_all(&dir).unwrap();
        let mut body = format!("[package]\nname = \"{name}\"\n");
        if !deps.is_empty() {
            body.push_str("[dependencies]\n");
            for dep in *deps {
                body.push_str(&format!("{dep}.workspace = true\n"));
            }
        }
        fs::write(dir.join("Cargo.toml"), body).unwrap();
    }
}

/// `check_crate_paths` resolves `CrateConfig.path` against the PROCESS
/// cwd, not `base_dir` — so fixture crate paths must be absolute
/// (`base_dir.join(rel)`) to exist regardless of where `cargo test` runs
/// from. `Path::join` with an absolute `path` (as `check_workspace_membership`
/// does via `base_dir.join(&c.path)`) discards `base_dir` and returns the
/// absolute path unchanged, so this also resolves correctly there.
fn p(root: &std::path::Path, rel: &str) -> String {
    root.join(rel).to_string_lossy().to_string()
}

/// Opt a fixture crate into an active cargo publisher — the gate
/// `check_workspace_membership` requires before it will raise a
/// missing-dependency error for that crate.
fn with_active_cargo_publisher(mut c: CrateConfig) -> CrateConfig {
    c.publish = Some(anodizer_core::config::PublishConfig {
        cargo: Some(anodizer_core::config::CargoPublishConfig::default()),
        ..Default::default()
    });
    c
}

#[test]
fn check_workspace_membership_direct_missing_dep_names_both_crates() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_disk_workspace(
        root,
        &[
            ("crates/main", "main", &["helper"]),
            ("crates/helper", "helper", &[]),
        ],
    );
    let config = make_config(vec![with_active_cargo_publisher(CrateConfig {
        name: "main".to_string(),
        path: p(root, "crates/main"),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    })]);
    let all_names = flatten_crate_names(&config);
    let mut errors = vec![];
    check_workspace_membership(&config, root, &all_names, &mut errors);
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one missing-membership error: {errors:?}"
    );
    assert!(
        errors[0].contains("helper"),
        "error should name the missing crate: {}",
        errors[0]
    );
    assert!(
        errors[0].contains("main"),
        "error should name the dependent crate: {}",
        errors[0]
    );
}

// ---- single-crate mode: exactly one top-level `crates:` entry ----

#[test]
fn check_workspace_membership_single_crate_mode_missing_dep_fails() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_disk_workspace(
        root,
        &[
            ("crates/main", "main", &["helper"]),
            ("crates/helper", "helper", &[]),
        ],
    );
    let config = make_config(vec![with_active_cargo_publisher(CrateConfig {
        name: "main".to_string(),
        path: p(root, "crates/main"),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    })]);
    let result = run_checks(&config, false, &test_logger(), root);
    assert!(
        result.is_err(),
        "single-crate config missing an on-disk workspace dep should fail"
    );
}

#[test]
fn check_workspace_membership_single_crate_mode_complete_passes() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_disk_workspace(
        root,
        &[
            ("crates/main", "main", &["helper"]),
            ("crates/helper", "helper", &[]),
        ],
    );
    let config = make_config(vec![
        with_active_cargo_publisher(CrateConfig {
            name: "main".to_string(),
            path: p(root, "crates/main"),
            tag_template: Some("v{{ .Version }}".to_string()),
            depends_on: Some(vec!["helper".to_string()]),
            ..Default::default()
        }),
        with_active_cargo_publisher(CrateConfig {
            name: "helper".to_string(),
            path: p(root, "crates/helper"),
            tag_template: Some("helper-v{{ .Version }}".to_string()),
            ..Default::default()
        }),
    ]);
    let result = run_checks(&config, false, &test_logger(), root);
    assert!(
        result.is_ok(),
        "complete single-crate-mode membership should pass: {:?}",
        result.err()
    );
}

// ---- lockstep mode: multiple top-level `crates:` entries, one version ----

#[test]
fn check_workspace_membership_lockstep_multi_crate_missing_dep_fails() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_disk_workspace(
        root,
        &[
            ("crates/api", "api", &["shared"]),
            ("crates/cli", "cli", &["shared"]),
            ("crates/shared", "shared", &[]),
        ],
    );
    let config = make_config(vec![
        with_active_cargo_publisher(CrateConfig {
            name: "api".to_string(),
            path: p(root, "crates/api"),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }),
        with_active_cargo_publisher(CrateConfig {
            name: "cli".to_string(),
            path: p(root, "crates/cli"),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }),
    ]);
    let result = run_checks(&config, false, &test_logger(), root);
    assert!(
        result.is_err(),
        "lockstep config missing an on-disk workspace dep should fail"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("2 error(s)"),
        "expected one error per dependent crate referencing 'shared': {}",
        msg
    );
}

#[test]
fn check_workspace_membership_lockstep_multi_crate_complete_passes() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_disk_workspace(
        root,
        &[
            ("crates/api", "api", &["shared"]),
            ("crates/cli", "cli", &["shared"]),
            ("crates/shared", "shared", &[]),
        ],
    );
    let config = make_config(vec![
        with_active_cargo_publisher(CrateConfig {
            name: "api".to_string(),
            path: p(root, "crates/api"),
            tag_template: Some("v{{ .Version }}".to_string()),
            depends_on: Some(vec!["shared".to_string()]),
            ..Default::default()
        }),
        with_active_cargo_publisher(CrateConfig {
            name: "cli".to_string(),
            path: p(root, "crates/cli"),
            tag_template: Some("v{{ .Version }}".to_string()),
            depends_on: Some(vec!["shared".to_string()]),
            ..Default::default()
        }),
        with_active_cargo_publisher(CrateConfig {
            name: "shared".to_string(),
            path: p(root, "crates/shared"),
            tag_template: Some("shared-v{{ .Version }}".to_string()),
            ..Default::default()
        }),
    ]);
    let result = run_checks(&config, false, &test_logger(), root);
    assert!(
        result.is_ok(),
        "complete lockstep membership should pass: {:?}",
        result.err()
    );
}

// ---- per-crate mode: nested `workspaces:` groups, independent cadence ----

#[test]
fn check_workspace_membership_per_crate_workspace_missing_dep_fails() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_disk_workspace(
        root,
        &[
            ("crates/frontend", "frontend", &["util"]),
            ("crates/util", "util", &[]),
        ],
    );
    let mut config = make_config(vec![]);
    config.workspaces = Some(vec![WorkspaceConfig {
        name: "web".to_string(),
        crates: vec![with_active_cargo_publisher(CrateConfig {
            name: "frontend".to_string(),
            path: p(root, "crates/frontend"),
            tag_template: Some("frontend-v{{ .Version }}".to_string()),
            ..Default::default()
        })],
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), root);
    assert!(
        result.is_err(),
        "per-crate workspace config missing an on-disk dep should fail"
    );
}

#[test]
fn check_workspace_membership_per_crate_workspace_complete_passes() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_disk_workspace(
        root,
        &[
            ("crates/frontend", "frontend", &["util"]),
            ("crates/util", "util", &[]),
        ],
    );
    let mut config = make_config(vec![]);
    config.workspaces = Some(vec![WorkspaceConfig {
        name: "web".to_string(),
        crates: vec![
            with_active_cargo_publisher(CrateConfig {
                name: "frontend".to_string(),
                path: p(root, "crates/frontend"),
                tag_template: Some("frontend-v{{ .Version }}".to_string()),
                depends_on: Some(vec!["util".to_string()]),
                ..Default::default()
            }),
            with_active_cargo_publisher(CrateConfig {
                name: "util".to_string(),
                path: p(root, "crates/util"),
                tag_template: Some("util-v{{ .Version }}".to_string()),
                ..Default::default()
            }),
        ],
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), root);
    assert!(
        result.is_ok(),
        "complete per-crate workspace membership should pass: {:?}",
        result.err()
    );
}

// ---- publisher-gating: only crates with an active cargo publisher are checked ----

#[test]
fn check_workspace_membership_no_active_publisher_skips_check() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_disk_workspace(
        root,
        &[
            ("crates/main", "main", &["helper"]),
            ("crates/helper", "helper", &[]),
        ],
    );
    // `main` has a genuine missing on-disk dep ("helper"), but no active
    // cargo publisher — the check must not flag it (nothing will ever be
    // `cargo publish`ed, so a missing crates: entry for its dep is moot).
    let config = make_config(vec![CrateConfig {
        name: "main".to_string(),
        path: p(root, "crates/main"),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }]);
    let all_names = flatten_crate_names(&config);
    let mut errors = vec![];
    check_workspace_membership(&config, root, &all_names, &mut errors);
    assert!(
        errors.is_empty(),
        "crate with no active cargo publisher must not be checked for workspace membership: {errors:?}"
    );
}

#[test]
fn check_workspace_membership_dep_with_cargo_skip_still_errors() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write_disk_workspace(
        root,
        &[
            ("crates/main", "main", &["helper"]),
            ("crates/helper", "helper", &[]),
        ],
    );
    // "helper" IS present in `crates:`, but its cargo publisher is
    // explicitly skipped — `main` publishing to crates.io would still
    // fail because "helper" is never uploaded to the registry.
    let mut helper = CrateConfig {
        name: "helper".to_string(),
        path: p(root, "crates/helper"),
        tag_template: Some("helper-v{{ .Version }}".to_string()),
        ..Default::default()
    };
    helper.publish = Some(anodizer_core::config::PublishConfig {
        cargo: Some(anodizer_core::config::CargoPublishConfig {
            skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
            ..Default::default()
        }),
        ..Default::default()
    });
    let config = make_config(vec![
        with_active_cargo_publisher(CrateConfig {
            name: "main".to_string(),
            path: p(root, "crates/main"),
            tag_template: Some("v{{ .Version }}".to_string()),
            depends_on: Some(vec!["helper".to_string()]),
            ..Default::default()
        }),
        helper,
    ]);
    let all_names = flatten_crate_names(&config);
    let mut errors = vec![];
    check_workspace_membership(&config, root, &all_names, &mut errors);
    assert_eq!(
        errors.len(),
        1,
        "dependency with publish.cargo.skip=true should still fail the membership check: {errors:?}"
    );
    assert!(
        errors[0].contains("no active cargo publisher"),
        "error should explain the skipped publisher, got: {}",
        errors[0]
    );
}

// ---- multi-root: `workspaces:` spanning distinct physical Cargo workspaces ----

#[test]
fn check_workspace_membership_discriminates_distinct_cargo_workspace_roots() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    // Two SEPARATE physical Cargo workspaces, each rooted below `root`
    // (no Cargo.toml at `root` itself) — proves `find_cargo_workspace_root`
    // climbs per-crate rather than coincidentally reaching `base_dir`,
    // and that `member_cache` keys on the resolved root without
    // cross-contaminating the two workspaces' member sets.
    write_disk_workspace(
        &root.join("ws-a"),
        &[
            ("crates/frontend", "frontend", &["util"]),
            ("crates/util", "util", &[]),
        ],
    );
    write_disk_workspace(
        &root.join("ws-b"),
        &[
            ("crates/backend", "backend", &["dbutil"]),
            ("crates/dbutil", "dbutil", &[]),
        ],
    );
    let config = make_config(vec![
        // ws-a: "frontend" omits depends_on for its genuine dep "util" — expect an error.
        with_active_cargo_publisher(CrateConfig {
            name: "frontend".to_string(),
            path: p(root, "ws-a/crates/frontend"),
            tag_template: Some("frontend-v{{ .Version }}".to_string()),
            ..Default::default()
        }),
        // ws-b: "backend" correctly declares depends_on for its genuine dep "dbutil" — expect none.
        with_active_cargo_publisher(CrateConfig {
            name: "backend".to_string(),
            path: p(root, "ws-b/crates/backend"),
            tag_template: Some("backend-v{{ .Version }}".to_string()),
            depends_on: Some(vec!["dbutil".to_string()]),
            ..Default::default()
        }),
        with_active_cargo_publisher(CrateConfig {
            name: "dbutil".to_string(),
            path: p(root, "ws-b/crates/dbutil"),
            tag_template: Some("dbutil-v{{ .Version }}".to_string()),
            ..Default::default()
        }),
    ]);
    let all_names = flatten_crate_names(&config);
    let mut errors = vec![];
    check_workspace_membership(&config, root, &all_names, &mut errors);
    assert_eq!(
        errors.len(),
        1,
        "only ws-a's frontend/util gap should error; ws-b's backend/dbutil is complete: {errors:?}"
    );
    assert!(
        errors[0].contains("util") && errors[0].contains("frontend"),
        "error should name ws-a's missing dep, got: {}",
        errors[0]
    );
}

/// `check config --workspace X` validates X's resolved config only: a
/// SIBLING workspace's error (here a `depends_on` cycle confined to ws-b)
/// must not fail ws-a's scoped validation. The overlay clears
/// `workspaces`, so the resolved universe is exactly ws-a's crates.
#[test]
fn workspace_scoped_checks_ignore_sibling_errors() {
    let config = Config {
        project_name: "test".to_string(),
        workspaces: Some(vec![
            WorkspaceConfig {
                name: "ws-a".to_string(),
                crates: vec![make_crate("a-one", "a-one-v{{ .Version }}", None)],
                ..Default::default()
            },
            WorkspaceConfig {
                name: "ws-b".to_string(),
                crates: vec![
                    make_crate("b-one", "b-one-v{{ .Version }}", Some(vec!["b-two"])),
                    make_crate("b-two", "b-two-v{{ .Version }}", Some(vec!["b-one"])),
                ],
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    // The raw (un-overlaid) config fails on ws-b's cycle.
    assert!(
        run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_err(),
        "raw config must fail on the sibling's cycle"
    );
    // The ws-a-resolved config must pass — the sibling is out of scope.
    let ws = config.workspaces.as_ref().unwrap()[0].clone();
    let mut resolved = config.clone();
    helpers::apply_workspace_overlay(&mut resolved, &ws);
    run_checks(
        &resolved,
        false,
        &test_logger(),
        Path::new(NO_WORKSPACE_BASE),
    )
    .expect("workspace-scoped validation must ignore sibling workspace errors");
}

/// The COMMAND path of the sibling-isolation rule: `check config
/// --workspace ws-a` must exit clean when the only error (a `depends_on`
/// cycle) is confined to sibling ws-b, while the no-flag form still fails
/// on it. The hand-overlaid `run_checks` pin above cannot catch a command
/// that validates the raw config before scoping — this one drives `run`.
#[test]
fn command_workspace_scoped_run_ignores_sibling_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let config_path = root.join(".anodizer.yaml");
    // `path: .` throughout — the crate-path existence check resolves
    // against the PROCESS cwd (which a unit test must not change), and
    // the sibling-isolation subject here is the cycle, not the paths.
    std::fs::write(
        &config_path,
        r#"project_name: fixture
workspaces:
  - name: ws-a
    crates:
      - name: a-one
        path: .
        tag_template: "a-one-v{{ .Version }}"
  - name: ws-b
    crates:
      - name: b-one
        path: .
        tag_template: "b-one-v{{ .Version }}"
        depends_on: [b-two]
      - name: b-two
        path: .
        tag_template: "b-two-v{{ .Version }}"
        depends_on: [b-one]
"#,
    )
    .unwrap();

    run(Some(&config_path), Some("ws-a"), &[], false, false, true)
        .expect("scoped run must not fail on the sibling workspace's cycle");
    let err = run(Some(&config_path), None, &[], false, false, true)
        .expect_err("the no-flag run still validates the whole file");
    assert!(err.to_string().contains("validation failed"), "got: {err}");
}

// ---- Cycle detection tests ----

#[test]
fn test_no_cycle_linear() {
    let crates = vec![
        make_crate("a", "a-v{{ .Version }}", None),
        make_crate("b", "b-v{{ .Version }}", Some(vec!["a"])),
        make_crate("c", "c-v{{ .Version }}", Some(vec!["b"])),
    ];
    assert!(find_cycle(&crates).is_none());
}

#[test]
fn test_cycle_two_nodes() {
    let crates = vec![
        make_crate("a", "a-v{{ .Version }}", Some(vec!["b"])),
        make_crate("b", "b-v{{ .Version }}", Some(vec!["a"])),
    ];
    let cycle = find_cycle(&crates);
    assert!(cycle.is_some(), "expected a cycle to be detected");
}

#[test]
fn test_cycle_three_nodes() {
    let crates = vec![
        make_crate("a", "a-v{{ .Version }}", Some(vec!["c"])),
        make_crate("b", "b-v{{ .Version }}", Some(vec!["a"])),
        make_crate("c", "c-v{{ .Version }}", Some(vec!["b"])),
    ];
    let cycle = find_cycle(&crates);
    assert!(cycle.is_some(), "expected a cycle to be detected");
}

#[test]
fn test_no_cycle_diamond() {
    let crates = vec![
        make_crate("base", "base-v{{ .Version }}", None),
        make_crate("left", "left-v{{ .Version }}", Some(vec!["base"])),
        make_crate("right", "right-v{{ .Version }}", Some(vec!["base"])),
        make_crate("top", "top-v{{ .Version }}", Some(vec!["left", "right"])),
    ];
    assert!(find_cycle(&crates).is_none());
}

// ---- tag_template validation tests ----

#[test]
fn test_tag_template_valid() {
    let config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

#[test]
fn test_tag_template_missing_version() {
    let config = make_config(vec![make_crate("a", "release-tag", None)]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("validation failed"), "got: {}", msg);
}

#[test]
fn test_tag_template_empty_skipped() {
    // Empty tag_template should not trigger the error (it's just unconfigured)
    let config = make_config(vec![make_crate("a", "", None)]);
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

// ---- depends_on reference tests ----

#[test]
fn test_depends_on_missing_crate() {
    let config = make_config(vec![make_crate(
        "a",
        "a-v{{ .Version }}",
        Some(vec!["nonexistent"]),
    )]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("validation failed"), "got: {}", msg);
}

#[test]
fn test_depends_on_cycle_fails() {
    let crates = vec![
        make_crate("a", "a-v{{ .Version }}", Some(vec!["b"])),
        make_crate("b", "b-v{{ .Version }}", Some(vec!["a"])),
    ];
    let config = make_config(crates);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err());
}

// ---- copy_from tests ----

#[test]
fn test_copy_from_valid() {
    use anodizer_core::config::BuildConfig;
    let mut c = make_crate("a", "a-v{{ .Version }}", None);
    c.builds = Some(vec![
        BuildConfig {
            binary: Some("a".to_string()),
            ..Default::default()
        },
        BuildConfig {
            binary: Some("b".to_string()),
            copy_from: Some("a".to_string()),
            ..Default::default()
        },
    ]);
    let config = make_config(vec![c]);
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

#[test]
fn test_copy_from_invalid() {
    use anodizer_core::config::BuildConfig;
    let mut c = make_crate("a", "a-v{{ .Version }}", None);
    c.builds = Some(vec![BuildConfig {
        binary: Some("b".to_string()),
        copy_from: Some("nonexistent".to_string()),
        ..Default::default()
    }]);
    let config = make_config(vec![c]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err());
}

// ---- Contradictory config warning tests ----

#[test]
fn test_check_changelog_disabled_with_other_fields_passes() {
    use anodizer_core::config::{ChangelogConfig, ChangelogGroup};
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.changelog = Some(ChangelogConfig {
        skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
        sort: Some("desc".to_string()),
        header: Some(anodizer_core::config::ContentSource::Inline(
            "header".to_string(),
        )),
        groups: Some(vec![ChangelogGroup {
            title: "Features".to_string(),
            regexp: Some("^feat".to_string()),
            order: Some(0),
            groups: None,
        }]),
        ..Default::default()
    });
    // Should pass (warnings only, not errors)
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

#[test]
fn test_check_checksum_disabled_with_other_fields_passes() {
    use anodizer_core::config::{ChecksumConfig, Defaults, StringOrBool};
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.defaults = Some(Defaults {
        checksum: Some(ChecksumConfig {
            skip: Some(StringOrBool::Bool(true)),
            algorithm: Some("sha512".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    // Should pass (warnings only, not errors)
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

// ---- Empty crate name validation tests ----

#[test]
fn test_empty_crate_name_fails() {
    let config = make_config(vec![make_crate("", "v{{ .Version }}", None)]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err(), "empty crate name should fail validation");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("validation failed"), "got: {}", msg);
}

#[test]
fn test_whitespace_only_crate_name_fails() {
    let config = make_config(vec![make_crate("  ", "v{{ .Version }}", None)]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(
        result.is_err(),
        "whitespace-only crate name should fail validation"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("1 error(s)"),
        "error should report 1 validation error, got: {msg}"
    );
}

// ---- tag_template compact spacing variant tests ----

#[test]
fn test_tag_template_compact_version_accepted() {
    // {{.Version}} without spaces should also be accepted
    let config = make_config(vec![make_crate("a", "v{{.Version}}", None)]);
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

#[test]
fn test_tag_template_tera_native_version_accepted() {
    // {{ Version }} (Tera-native, no dot) should also be accepted
    let config = make_config(vec![make_crate("a", "v{{ Version }}", None)]);
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

#[test]
fn test_tag_template_tera_native_compact_version_accepted() {
    // {{Version}} (Tera-native, no dot, no spaces) should also be accepted
    let config = make_config(vec![make_crate("a", "v{{Version}}", None)]);
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

#[test]
fn test_tag_template_missing_version_with_other_placeholder() {
    // Has a placeholder but not {{ .Version }}
    let config = make_config(vec![make_crate("a", "{{ .Tag }}-release", None)]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(
        result.is_err(),
        "tag_template without Version placeholder should fail"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("1 error(s)"),
        "error should report 1 validation error, got: {msg}"
    );
}

// ---- Multiple validation errors test ----

#[test]
fn test_multiple_validation_errors_reported() {
    let crates = vec![
        make_crate("", "v{{ .Version }}", None), // empty name
        make_crate("b", "bad-tag", Some(vec!["nonexistent"])), // missing dep + bad template
    ];
    let config = make_config(crates);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    // Should report exactly 3 errors: empty name, missing dep, bad tag_template
    assert!(
        msg.contains("3 error(s)"),
        "should report 3 error(s), got: {}",
        msg
    );
}

#[test]
fn test_check_per_crate_checksum_disabled_with_other_fields_passes() {
    use anodizer_core::config::{ChecksumConfig, StringOrBool};
    let mut c = make_crate("a", "a-v{{ .Version }}", None);
    c.checksum = Some(ChecksumConfig {
        skip: Some(StringOrBool::Bool(true)),
        algorithm: Some("sha512".to_string()),
        name_template: Some("checksums.txt".to_string()),
        ..Default::default()
    });
    let config = make_config(vec![c]);
    // Should pass (warnings only, not errors)
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

// ---- Workspace validation tests ----

#[test]
fn workspace_only_crates_flag_tool_needs() {
    // A crate declared only under `workspaces[].crates` must arm the
    // same tool-requirement checks a top-level crate does; a
    // top-level-only walk would let its docker/release/nfpm needs pass
    // `check config` silently.
    use anodizer_core::config::{DockerV2Config, NfpmConfig, ReleaseConfig};
    let mut member = make_crate("svc", "svc-v{{ .Version }}", None);
    member.dockers_v2 = Some(vec![DockerV2Config::default()]);
    member.release = Some(ReleaseConfig::default());
    member.nfpms = Some(vec![NfpmConfig::default()]);
    let mut config = make_config(vec![]);
    config.workspaces = Some(vec![WorkspaceConfig {
        name: "grp".to_string(),
        crates: vec![member],
        ..Default::default()
    }]);

    assert!(config_needs_docker(&config));
    assert!(config_needs_release(&config));
    assert!(config_needs_nfpm(&config));
}

#[test]
fn test_workspace_names_unique_passes() {
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.workspaces = Some(vec![
        WorkspaceConfig {
            name: "frontend".to_string(),
            crates: vec![make_crate("fe", "fe-v{{ .Version }}", None)],
            ..Default::default()
        },
        WorkspaceConfig {
            name: "backend".to_string(),
            crates: vec![make_crate("be", "be-v{{ .Version }}", None)],
            ..Default::default()
        },
    ]);
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

#[test]
fn test_workspace_duplicate_name_fails() {
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.workspaces = Some(vec![
        WorkspaceConfig {
            name: "dup".to_string(),
            crates: vec![make_crate("x", "x-v{{ .Version }}", None)],
            ..Default::default()
        },
        WorkspaceConfig {
            name: "dup".to_string(),
            crates: vec![make_crate("y", "y-v{{ .Version }}", None)],
            ..Default::default()
        },
    ]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err(), "duplicate workspace names should fail");
}

#[test]
fn test_workspace_empty_name_fails() {
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.workspaces = Some(vec![WorkspaceConfig {
        name: "".to_string(),
        crates: vec![make_crate("x", "x-v{{ .Version }}", None)],
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err(), "empty workspace name should fail");
}

#[test]
fn test_workspace_crate_empty_name_fails() {
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.workspaces = Some(vec![WorkspaceConfig {
        name: "ws1".to_string(),
        crates: vec![make_crate("", "v{{ .Version }}", None)],
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err(), "empty crate name in workspace should fail");
}

#[test]
fn test_workspace_crate_bad_tag_template_fails() {
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.workspaces = Some(vec![WorkspaceConfig {
        name: "ws1".to_string(),
        crates: vec![make_crate("x", "no-version-here", None)],
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(
        result.is_err(),
        "bad tag_template in workspace crate should fail"
    );
}

#[test]
fn test_no_workspaces_passes() {
    let config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    assert!(run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok());
}

#[test]
fn test_workspace_duplicate_crate_name_fails() {
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.workspaces = Some(vec![WorkspaceConfig {
        name: "ws1".to_string(),
        crates: vec![
            make_crate("dup", "dup-v{{ .Version }}", None),
            make_crate("dup", "dup-v{{ .Version }}", None),
        ],
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(
        result.is_err(),
        "duplicate crate names within a workspace should fail"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("1 error(s)"),
        "should report 1 validation error for duplicate crate name: {}",
        msg
    );
}

#[test]
fn test_workspace_depends_on_missing_fails() {
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.workspaces = Some(vec![WorkspaceConfig {
        name: "ws1".to_string(),
        crates: vec![make_crate(
            "x",
            "x-v{{ .Version }}",
            Some(vec!["nonexistent"]),
        )],
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(
        result.is_err(),
        "workspace crate with missing depends_on should fail"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("1 error(s)"),
        "should report 1 validation error for missing depends_on: {}",
        msg
    );
}

#[test]
fn test_workspace_depends_on_cross_workspace_passes() {
    // A crate in one workspace can depend on a crate in another workspace.
    // The release engine topo-sorts across all workspaces, so the check
    // validator must not flag cross-workspace references as missing.
    let config = Config {
        project_name: "test".to_string(),
        workspaces: Some(vec![
            WorkspaceConfig {
                name: "core-ws".to_string(),
                crates: vec![make_crate("core", "core-v{{ .Version }}", None)],
                ..Default::default()
            },
            WorkspaceConfig {
                name: "app-ws".to_string(),
                crates: vec![make_crate("app", "app-v{{ .Version }}", Some(vec!["core"]))],
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    assert!(
        run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok(),
        "cross-workspace depends_on should be accepted"
    );
}

#[test]
fn test_workspace_depends_on_valid_passes() {
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.workspaces = Some(vec![WorkspaceConfig {
        name: "ws1".to_string(),
        crates: vec![
            make_crate("lib", "lib-v{{ .Version }}", None),
            make_crate("app", "app-v{{ .Version }}", Some(vec!["lib"])),
        ],
        ..Default::default()
    }]);
    assert!(
        run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok(),
        "valid depends_on within workspace should pass"
    );
}

// ---- Source/SBOM format validation tests ----

#[test]
fn test_invalid_source_format_fails() {
    use anodizer_core::config::SourceConfig;
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.source = Some(SourceConfig {
        enabled: Some(true),
        format: Some("tar.bz2".to_string()),
        name_template: None,
        prefix_template: None,
        files: vec![],
    });
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err(), "invalid source format should fail");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("validation failed"), "got: {}", msg);
}

#[test]
fn test_valid_source_formats_pass() {
    use anodizer_core::config::SourceConfig;
    for fmt in &["tar.gz", "tgz", "tar", "zip"] {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.source = Some(SourceConfig {
            enabled: Some(true),
            format: Some(fmt.to_string()),
            name_template: None,
            prefix_template: None,
            files: vec![],
        });
        assert!(
            run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok(),
            "source format '{}' should pass",
            fmt
        );
    }
}

#[test]
fn test_invalid_sbom_artifacts_fails() {
    use anodizer_core::config::SbomConfig;
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.sboms = vec![SbomConfig {
        artifacts: Some("invalid".to_string()),
        ..Default::default()
    }];
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err(), "invalid sbom artifacts should fail");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("validation failed"), "got: {}", msg);
}

#[test]
fn test_valid_sbom_artifacts_pass() {
    use anodizer_core::config::SbomConfig;
    for art in &[
        "source",
        "archive",
        "binary",
        "package",
        "diskimage",
        "installer",
        "any",
    ] {
        let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
        config.sboms = vec![SbomConfig {
            artifacts: Some(art.to_string()),
            ..Default::default()
        }];
        assert!(
            run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok(),
            "sbom artifacts '{}' should pass",
            art
        );
    }
}

// -----------------------------------------------------------------------
// Blob config validation tests
// -----------------------------------------------------------------------

#[test]
fn test_blob_config_valid_provider() {
    use anodizer_core::config::BlobConfig;
    for provider in &["s3", "gcs", "gs", "azblob", "azure"] {
        let mut config = make_config(vec![make_crate("a", "v{{ .Version }}", None)]);
        config.crates[0].blobs = Some(vec![BlobConfig {
            provider: provider.to_string(),
            bucket: "my-bucket".to_string(),
            ..Default::default()
        }]);
        assert!(
            run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE)).is_ok(),
            "blob provider '{}' should pass",
            provider
        );
    }
}

#[test]
fn test_blob_config_invalid_provider() {
    use anodizer_core::config::BlobConfig;
    let mut config = make_config(vec![make_crate("a", "v{{ .Version }}", None)]);
    config.crates[0].blobs = Some(vec![BlobConfig {
        provider: "dropbox".to_string(),
        bucket: "b".to_string(),
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err(), "invalid blob provider should fail");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("validation failed"), "got: {}", msg);
}

#[test]
fn test_blob_config_empty_provider() {
    use anodizer_core::config::BlobConfig;
    let mut config = make_config(vec![make_crate("a", "v{{ .Version }}", None)]);
    config.crates[0].blobs = Some(vec![BlobConfig {
        provider: String::new(),
        bucket: "b".to_string(),
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err(), "empty blob provider should fail");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("validation failed"), "got: {}", msg);
}

#[test]
fn test_blob_config_empty_bucket() {
    use anodizer_core::config::BlobConfig;
    let mut config = make_config(vec![make_crate("a", "v{{ .Version }}", None)]);
    config.crates[0].blobs = Some(vec![BlobConfig {
        provider: "s3".to_string(),
        bucket: String::new(),
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err(), "empty blob bucket should fail");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("validation failed"), "got: {}", msg);
}

#[test]
fn test_blob_config_id_in_error_label() {
    use anodizer_core::config::BlobConfig;
    let mut config = make_config(vec![make_crate("a", "v{{ .Version }}", None)]);
    config.crates[0].blobs = Some(vec![BlobConfig {
        id: Some("my-upload".to_string()),
        provider: "invalid".to_string(),
        bucket: "b".to_string(),
        ..Default::default()
    }]);
    let result = run_checks(&config, false, &test_logger(), Path::new(NO_WORKSPACE_BASE));
    assert!(result.is_err(), "invalid provider with id should fail");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("validation failed"), "got: {}", msg);
}

// -----------------------------------------------------------------------
// Announce secret-exposure lint tests
// -----------------------------------------------------------------------

use anodizer_core::config::{
    AnnounceConfig, BlueskyAnnounce, DiscourseAnnounce, EmailAnnounce, SlackAnnounce,
    SlackAttachment, SlackBlock, SlackTextObject, TwitterAnnounce,
};

fn collect_announce_warnings(announce: AnnounceConfig) -> Vec<String> {
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.announce = Some(announce);
    let mut warnings = Vec::new();
    check_announce_secret_exposure(&config, &mut warnings);
    warnings
}

#[test]
fn test_announce_secret_warns_on_token_in_message() {
    let warnings = collect_announce_warnings(AnnounceConfig {
        twitter: Some(TwitterAnnounce {
            message_template: Some("deploy {{ Env.GITHUB_TOKEN }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert_eq!(
        warnings.len(),
        1,
        "expected one warning, got: {:?}",
        warnings
    );
    assert!(warnings[0].contains("announce.twitter.message_template"));
    assert!(warnings[0].contains("Env.GITHUB_TOKEN"));
    assert!(
        warnings[0].contains("$GITHUB_TOKEN"),
        "warning should state the masked form: {}",
        warnings[0]
    );
}

#[test]
fn test_announce_secret_warns_on_title_and_email_subject() {
    let title_warnings = collect_announce_warnings(AnnounceConfig {
        discourse: Some(DiscourseAnnounce {
            title_template: Some("release {{ Env.SIGNING_KEY }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert_eq!(title_warnings.len(), 1, "got: {:?}", title_warnings);
    assert!(title_warnings[0].contains("announce.discourse.title_template"));
    assert!(title_warnings[0].contains("Env.SIGNING_KEY"));

    let email_warnings = collect_announce_warnings(AnnounceConfig {
        email: Some(EmailAnnounce {
            subject_template: Some("v{{ Env.NPM_PASSWORD }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert_eq!(email_warnings.len(), 1, "got: {:?}", email_warnings);
    assert!(email_warnings[0].contains("announce.email.subject_template"));
    assert!(email_warnings[0].contains("Env.NPM_PASSWORD"));
}

#[test]
fn test_announce_secret_warns_on_go_style_dotted_env() {
    let warnings = collect_announce_warnings(AnnounceConfig {
        twitter: Some(TwitterAnnounce {
            message_template: Some("{{ .Env.CARGO_REGISTRY_TOKEN }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert_eq!(warnings.len(), 1, "got: {:?}", warnings);
    assert!(warnings[0].contains("Env.CARGO_REGISTRY_TOKEN"));
}

#[test]
fn test_announce_secret_warns_in_slack_blocks_and_attachments() {
    let warnings = collect_announce_warnings(AnnounceConfig {
        slack: Some(SlackAnnounce {
            blocks: Some(vec![SlackBlock {
                block_type: "section".to_string(),
                text: Some(SlackTextObject {
                    text_type: "mrkdwn".to_string(),
                    text: "see {{ Env.SLACK_API_TOKEN }}".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            attachments: Some(vec![SlackAttachment {
                footer: Some("built by {{ Env.BUILDER_SECRET }}".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert_eq!(warnings.len(), 2, "got: {:?}", warnings);
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("announce.slack.blocks[0].text")
                && w.contains("Env.SLACK_API_TOKEN")),
        "block-nested secret not warned: {:?}",
        warnings
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("announce.slack.attachments[0].footer")
                && w.contains("Env.BUILDER_SECRET")),
        "attachment-nested secret not warned: {:?}",
        warnings
    );
}

#[test]
fn test_announce_secret_silent_on_non_secret_refs() {
    // Non-secret placeholders, a non-secret env var, a provider with no
    // template, and an absent announce block all stay silent.
    let warnings = collect_announce_warnings(AnnounceConfig {
        bluesky: Some(BlueskyAnnounce {
            message_template: Some("{{ ProjectName }} {{ Tag }} home={{ Env.HOME }}".to_string()),
            ..Default::default()
        }),
        twitter: Some(TwitterAnnounce {
            message_template: None,
            ..Default::default()
        }),
        ..Default::default()
    });
    assert!(
        warnings.is_empty(),
        "non-secret refs should not warn: {:?}",
        warnings
    );

    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.announce = None;
    let mut no_announce = Vec::new();
    check_announce_secret_exposure(&config, &mut no_announce);
    assert!(
        no_announce.is_empty(),
        "absent announce block should not warn: {:?}",
        no_announce
    );
}

#[test]
fn test_announce_secret_silent_on_bare_prose_no_braces() {
    // A secret-named ref in plain prose (outside any {{ }} / {% %} block)
    // never renders under Tera, so it cannot leak and must stay silent.
    let warnings = collect_announce_warnings(AnnounceConfig {
        twitter: Some(TwitterAnnounce {
            message_template: Some("contact Env.GITHUB_TOKEN admin".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert!(
        warnings.is_empty(),
        "bare-prose Env ref outside a render block should not warn: {:?}",
        warnings
    );
}

#[test]
fn test_announce_secret_warns_on_both_refs_in_one_block() {
    // Two Env refs inside ONE render block must both be flagged; only the
    // secret-named one(s) warn (PROJECT is not secret, B_TOKEN is).
    let warnings = collect_announce_warnings(AnnounceConfig {
        twitter: Some(TwitterAnnounce {
            message_template: Some("{{ Env.A_TOKEN | default(Env.B_TOKEN) }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert_eq!(
        warnings.len(),
        2,
        "both secret refs should warn: {:?}",
        warnings
    );
    assert!(
        warnings.iter().any(|w| w.contains("Env.A_TOKEN")),
        "first ref missed: {:?}",
        warnings
    );
    assert!(
        warnings.iter().any(|w| w.contains("Env.B_TOKEN")),
        "second ref in same block missed: {:?}",
        warnings
    );
}

// ---- Sign artifact-filter validation tests ----

fn config_with_sign_artifacts(filter: &str) -> Config {
    Config {
        project_name: "test".to_string(),
        signs: vec![anodizer_core::config::SignConfig {
            artifacts: Some(filter.to_string()),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn sign_filter_accepts_runtime_recognized_values_without_warning() {
    // Every value the runtime `should_sign_artifact` resolver accepts must
    // be accepted by the check validator too — otherwise a config that
    // signs correctly at release time emits a spurious "unrecognized
    // artifact filter" warning at check time. The previously-missing
    // values (`any`, `installer`, `diskimage`, `sbom`, `snap`,
    // `macos_package`) are the regression this guards.
    for filter in anodizer_stage_sign::VALID_SIGN_ARTIFACT_FILTERS {
        let config = config_with_sign_artifacts(filter);
        let mut warnings: Vec<String> = vec![];
        check_sign_artifact_filters(&config, &mut warnings);
        assert!(
            warnings.is_empty(),
            "filter '{filter}' must NOT warn (it is runtime-valid), got: {warnings:?}"
        );
    }
}

#[test]
fn sign_filter_warns_on_unrecognized_value() {
    let config = config_with_sign_artifacts("bogus");
    let mut warnings: Vec<String> = vec![];
    check_sign_artifact_filters(&config, &mut warnings);
    assert_eq!(warnings.len(), 1, "an unknown filter must still warn");
    assert!(
        warnings[0].contains("bogus"),
        "warning should name the offending filter: {:?}",
        warnings
    );
}

#[test]
fn sign_authenticode_filter_warns_on_unrecognized_value() {
    // The authenticode sub-block carries its own `artifacts` selector,
    // resolved through the same vocabulary; an unknown value must warn too.
    let config = Config {
        project_name: "test".to_string(),
        signs: vec![anodizer_core::config::SignConfig {
            authenticode: Some(anodizer_core::config::AuthenticodeConfig {
                artifacts: Some("nonsense".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut warnings: Vec<String> = vec![];
    check_sign_artifact_filters(&config, &mut warnings);
    assert_eq!(warnings.len(), 1, "unknown authenticode filter must warn");
    assert!(
        warnings[0].contains("authenticode") && warnings[0].contains("nonsense"),
        "authenticode warning should name the block and value: {:?}",
        warnings
    );
}

/// `asset_name_template:` parses on every sign config but is read only on the
/// `binary_signs:` slice — a `signs:` entry that sets it gets the derived name
/// with no error, so check names the omission.
#[test]
fn asset_name_template_on_signs_warns_that_it_is_ignored() {
    let sign_with_template = || anodizer_core::config::SignConfig {
        asset_name_template: Some("{{ Binary }}-{{ Version }}".to_string()),
        ..Default::default()
    };
    let config = Config {
        project_name: "test".to_string(),
        signs: vec![sign_with_template()],
        binary_signs: vec![sign_with_template()],
        workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
            name: "ws".to_string(),
            signs: vec![sign_with_template()],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut warnings: Vec<String> = vec![];
    check_sign_asset_name_templates(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "signs[0].asset_name_template is set but only binary_signs honors it (it will be ignored)".to_string(),
            "workspaces.ws.signs[0].asset_name_template is set but only binary_signs honors it (it will be ignored)".to_string(),
        ],
        "binary_signs honors the field and must not warn"
    );
}

/// `defaults.sign:` fills an empty top-level `signs:`, so the warning must
/// name the block the user actually wrote — `signs[0]` points at nothing in
/// their file.
#[test]
fn asset_name_template_under_defaults_sign_names_the_defaults_block() {
    let mut config = Config {
        project_name: "test".to_string(),
        defaults: Some(anodizer_core::config::Defaults {
            sign: Some(anodizer_core::config::SignConfig {
                asset_name_template: Some("{{ Binary }}-{{ Version }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    anodizer_core::defaults_merge::apply_defaults(&mut config);
    let mut warnings: Vec<String> = vec![];
    check_sign_asset_name_templates(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "defaults.sign.asset_name_template is set but only binary_signs honors it (it will be ignored)".to_string(),
        ]
    );
}

/// A `signs:` entry the operator wrote is named as `signs[0]` even when it
/// repeats the `defaults.sign:` value verbatim — the fold filled nothing, so
/// naming `defaults.sign` would point at a block that changed no behaviour.
#[test]
fn an_entry_repeating_the_defaults_value_is_still_named_as_the_entry() {
    let mut config = Config {
        project_name: "test".to_string(),
        defaults: Some(anodizer_core::config::Defaults {
            sign: Some(anodizer_core::config::SignConfig {
                asset_name_template: Some("{{ Binary }}-{{ Version }}".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        signs: vec![anodizer_core::config::SignConfig {
            asset_name_template: Some("{{ Binary }}-{{ Version }}".to_string()),
            cmd: Some("cosign".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    anodizer_core::defaults_merge::apply_defaults(&mut config);
    let mut warnings: Vec<String> = vec![];
    check_sign_asset_name_templates(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "signs[0].asset_name_template is set but only binary_signs honors it (it will be ignored)".to_string(),
        ]
    );
}

/// `check config --workspace <name>` runs the checks on the OVERLAID config.
/// A workspace that declares its own `signs:` replaces the slice the
/// `defaults:` fold filled, so the warning must name the workspace entry — the
/// block the operator actually wrote — not `defaults.sign`.
#[test]
fn a_workspace_signs_entry_is_named_as_the_entry_after_the_overlay() {
    let workspace = WorkspaceConfig {
        name: "tools".to_string(),
        signs: vec![anodizer_core::config::SignConfig {
            cmd: Some("cosign".to_string()),
            asset_name_template: Some("{{ Binary }}-{{ Version }}".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut config = Config {
        project_name: "test".to_string(),
        defaults: Some(anodizer_core::config::Defaults {
            sign: Some(anodizer_core::config::SignConfig {
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }),
        workspaces: Some(vec![workspace.clone()]),
        ..Default::default()
    };
    anodizer_core::defaults_merge::apply_defaults(&mut config);
    assert!(
        config.filled_from_defaults.contains("signs"),
        "the fold must have filled the top-level slice for this to be a test"
    );

    let mut resolved = config.clone();
    helpers::apply_workspace_overlay(&mut resolved, &workspace);
    let mut warnings: Vec<String> = vec![];
    check_sign_asset_name_templates(&resolved, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "signs[0].asset_name_template is set but only binary_signs honors it (it will be ignored)".to_string(),
        ]
    );
}

/// A config that sets the field nowhere warns nowhere.
#[test]
fn no_asset_name_template_warns_nothing() {
    let config = Config {
        project_name: "test".to_string(),
        signs: vec![anodizer_core::config::SignConfig::default()],
        ..Default::default()
    };
    let mut warnings: Vec<String> = vec![];
    check_sign_asset_name_templates(&config, &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
}

/// The duplicate-output warning as `check_sign_duplicate_outputs` prints it,
/// spelled out here so a broken continuation in the production format string
/// fails a test instead of an operator's terminal.
fn one_file_warning(first: &str, second: &str, field: &str) -> String {
    format!(
        "{first} and {second} resolve one {field} file for the artifacts both \
         select — the second {field} overwrites the first, so one file ships \
         where two were configured"
    )
}

/// Two `binary_signs:` entries with one `signature:` template sign one file,
/// and the second `cmd:` overwrites the first's bytes. The asset-name claim
/// accepts the pair (one name over one file is one release asset), so check
/// is the only place that says so.
#[test]
fn two_binary_signs_entries_over_one_file_warn() {
    use anodizer_core::config::SignConfig;
    let config = Config {
        binary_signs: vec![
            SignConfig {
                cmd: Some("cosign".to_string()),
                ..Default::default()
            },
            SignConfig {
                cmd: Some("gpg".to_string()),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "signature"
        )]
    );
}

/// The `workspaces.<name>.binary_signs:` slice is the other half of the
/// population, and its label names the workspace.
#[test]
fn two_workspace_binary_signs_entries_over_one_file_warn() {
    use anodizer_core::config::SignConfig;
    let entry = |cmd: &str| SignConfig {
        cmd: Some(cmd.to_string()),
        ..Default::default()
    };
    let config = Config {
        workspaces: Some(vec![anodizer_core::config::WorkspaceConfig {
            name: "ws".to_string(),
            binary_signs: vec![entry("cosign"), entry("gpg")],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "workspaces.ws.binary_signs[0]",
            "workspaces.ws.binary_signs[1]",
            "signature"
        )]
    );
}

/// `signature:` has a default, so leaving it unset and spelling it out name
/// one file; the certificate has none, so two absent ones name nothing.
#[test]
fn an_unset_signature_and_its_default_spelling_are_one_file() {
    use anodizer_core::config::SignConfig;
    let config = Config {
        binary_signs: vec![
            SignConfig {
                cmd: Some("cosign".to_string()),
                ..Default::default()
            },
            SignConfig {
                cmd: Some("gpg".to_string()),
                signature: Some(SignConfig::DEFAULT_BINARY_SIGNATURE_TEMPLATE.to_string()),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "signature"
        )]
    );
}

/// Two entries sharing a `certificate:` template overwrite that file too, and
/// the message names the certificate on both sides.
#[test]
fn two_binary_signs_entries_over_one_certificate_warn() {
    use anodizer_core::config::SignConfig;
    let entry = |cmd: &str| SignConfig {
        cmd: Some(cmd.to_string()),
        signature: Some(format!("{{{{ .Artifact }}}}.{cmd}.sig")),
        certificate: Some("{{ .Artifact }}.pem".to_string()),
        ..Default::default()
    };
    let config = Config {
        binary_signs: vec![entry("cosign"), entry("gpg")],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "certificate"
        )]
    );
}

/// An Authenticode entry signs the PE in place and `artifacts: none` signs
/// nothing, so neither can overwrite a sibling's detached output.
#[test]
fn entries_that_write_no_detached_output_warn_nothing() {
    use anodizer_core::config::SignConfig;
    let cosign = || SignConfig {
        cmd: Some("cosign".to_string()),
        ..Default::default()
    };
    for quiet in [
        SignConfig {
            authenticode: Some(anodizer_core::config::AuthenticodeConfig::default()),
            ..Default::default()
        },
        SignConfig {
            artifacts: Some("none".to_string()),
            ..Default::default()
        },
    ] {
        let config = Config {
            binary_signs: vec![cosign(), quiet],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_sign_duplicate_outputs(&config, &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}

/// An entry the filter drops keeps the entries after it on the index the
/// operator wrote.
#[test]
fn a_filtered_entry_does_not_renumber_the_labels_after_it() {
    use anodizer_core::config::SignConfig;
    let cosign = |cmd: &str| SignConfig {
        cmd: Some(cmd.to_string()),
        ..Default::default()
    };
    let config = Config {
        binary_signs: vec![
            SignConfig {
                artifacts: Some("none".to_string()),
                ..Default::default()
            },
            cosign("cosign"),
            cosign("gpg"),
        ],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[1]",
            "binary_signs[2]",
            "signature"
        )]
    );
}

/// Two entries under different gates cannot be proven to both fire; two under
/// the same gate still overwrite each other, and so does an ungated entry
/// paired with a gated one — the ungated one runs every time.
#[test]
fn entries_under_different_gates_warn_nothing() {
    use anodizer_core::config::SignConfig;
    let entry = |gate: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        if_condition: Some(gate.to_string()),
        ..Default::default()
    };
    let ungated = SignConfig {
        cmd: Some("cosign".to_string()),
        ..Default::default()
    };
    let warnings_for = |pair: Vec<SignConfig>| {
        let config = Config {
            binary_signs: pair,
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_sign_duplicate_outputs(&config, &mut warnings);
        warnings
    };
    assert!(
        warnings_for(vec![
            entry("{{ IsSnapshot }}"),
            entry("{{ not IsSnapshot }}")
        ])
        .is_empty()
    );
    assert_eq!(
        warnings_for(vec![entry("{{ IsSnapshot }}"), entry("{{ IsSnapshot }}")]).len(),
        1
    );
    assert_eq!(
        warnings_for(vec![ungated.clone(), entry("{{ IsSnapshot }}")]).len(),
        1
    );
    assert_eq!(
        warnings_for(vec![entry("{{ IsSnapshot }}"), ungated]).len(),
        1
    );
    // An empty `if:` always runs, exactly like an absent one, so it pairs
    // with a real gate rather than reading as a second gate.
    assert_eq!(
        warnings_for(vec![entry(""), entry("{{ IsSnapshot }}")]).len(),
        1
    );
    assert_eq!(
        warnings_for(vec![entry("{{ IsSnapshot }}"), entry("")]).len(),
        1
    );
}

/// Two spellings of one literal path are one file, the same answer the sign
/// stage reaches by folding `.` and `..` before it compares two outputs.
#[test]
fn two_spellings_of_one_signature_path_warn() {
    use anodizer_core::config::SignConfig;
    let entry = |signature: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        signature: Some(signature.to_string()),
        ..Default::default()
    };
    let config = Config {
        binary_signs: vec![entry("dist/sigs/app.sig"), entry("./dist/sigs/app.sig")],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "signature"
        )]
    );

    // `{{ .Artifact }}` expands to the artifact's whole path, which already
    // carries `dist`, so the two spellings really do write two files
    // (`dist/app.sig` and `dist/dist/app.sig`) and neither is joined onto
    // the other.
    let templated = Config {
        binary_signs: vec![
            entry("{{ .Artifact }}.sig"),
            entry("dist/{{ .Artifact }}.sig"),
        ],
        ..Default::default()
    };
    let mut none = Vec::new();
    check_sign_duplicate_outputs(&templated, &mut none);
    assert!(none.is_empty(), "{none:?}");
}

/// A rendering that is not under `dist` is placed under it by the sign
/// stage, so `app.sig` and `dist/app.sig` are one file and the second `cmd:`
/// overwrites the first.
#[test]
fn a_signature_outside_dist_names_the_same_file_as_its_dist_spelling() {
    use anodizer_core::config::SignConfig;
    let entry = |signature: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        signature: Some(signature.to_string()),
        ..Default::default()
    };
    let config = Config {
        binary_signs: vec![entry("app.sig"), entry("dist/app.sig")],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "signature"
        )]
    );

    // A `dist:` the config moved is the one the join uses, so a spelling
    // under the OLD default is a different file.
    let moved = Config {
        dist: std::path::PathBuf::from("out"),
        binary_signs: vec![entry("app.sig"), entry("dist/app.sig")],
        ..Default::default()
    };
    let mut none = Vec::new();
    check_sign_duplicate_outputs(&moved, &mut none);
    assert!(none.is_empty(), "{none:?}");
}

/// Two templates that differ only in a `./` around the SAME placeholder name
/// one file: the literal segments fold as path components while the
/// placeholder stays opaque.
#[test]
fn two_spellings_of_one_templated_signature_path_warn() {
    use anodizer_core::config::SignConfig;
    let entry = |signature: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        signature: Some(signature.to_string()),
        ..Default::default()
    };
    let config = Config {
        binary_signs: vec![
            entry("dist/sigs/{{ .Artifact }}.sig"),
            entry("dist/./sigs/../sigs/{{ .Artifact }}.sig"),
        ],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "signature"
        )]
    );

    // Two DIFFERENT placeholders render two names, so the fold must keep
    // them apart.
    let distinct = Config {
        binary_signs: vec![
            entry("dist/{{ .Artifact }}.sig"),
            entry("./dist/{{ .Binary }}.sig"),
        ],
        ..Default::default()
    };
    let mut none = Vec::new();
    check_sign_duplicate_outputs(&distinct, &mut none);
    assert!(none.is_empty(), "{none:?}");
}

/// A template that renders outside `dist` is placed under it by the sign
/// stage, exactly as a literal path is, so `{{ ProjectName }}.sig` and
/// `dist/{{ ProjectName }}.sig` name one file.
#[test]
fn a_templated_signature_outside_dist_names_its_dist_spelling() {
    use anodizer_core::config::SignConfig;
    let entry = |signature: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        signature: Some(signature.to_string()),
        ..Default::default()
    };
    let config = Config {
        binary_signs: vec![
            entry("{{ ProjectName }}-{{ Version }}.sig"),
            entry("dist/{{ ProjectName }}-{{ Version }}.sig"),
        ],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "signature"
        )]
    );

    // Two different placeholders render two names, so the join must not
    // fold them together.
    let distinct = Config {
        binary_signs: vec![
            entry("{{ ProjectName }}.sig"),
            entry("dist/{{ Binary }}.sig"),
        ],
        ..Default::default()
    };
    let mut none = Vec::new();
    check_sign_duplicate_outputs(&distinct, &mut none);
    assert!(none.is_empty(), "{none:?}");
}

/// The padding inside `{{ … }}` is not part of what a placeholder renders,
/// so `{{ Target }}` and `{{Target}}` are one placeholder.
#[test]
fn placeholder_spacing_does_not_split_one_template_in_two() {
    use anodizer_core::config::SignConfig;
    let entry = |signature: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        signature: Some(signature.to_string()),
        ..Default::default()
    };
    for pair in [
        ["dist/{{ Target }}.sig", "./dist/{{Target}}.sig"],
        ["{{ Version }}.sig", "dist/{{Version}}.sig"],
    ] {
        let config = Config {
            binary_signs: vec![entry(pair[0]), entry(pair[1])],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_sign_duplicate_outputs(&config, &mut warnings);
        assert_eq!(
            warnings,
            vec![one_file_warning(
                "binary_signs[0]",
                "binary_signs[1]",
                "signature"
            )],
            "{pair:?}"
        );
    }
}

/// An unterminated `{{` is answered, not parsed: the run is opaque to the
/// end of the string, so the pair still compares and a `..` inside it cannot
/// climb out of the placeholder.
#[test]
fn an_unterminated_placeholder_is_opaque_to_the_end() {
    use anodizer_core::config::SignConfig;
    let entry = |signature: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        signature: Some(signature.to_string()),
        ..Default::default()
    };
    let config = Config {
        binary_signs: vec![entry("dist/{{ Version.sig"), entry("./dist/{{ Version.sig")],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "signature"
        )]
    );

    let climbing = Config {
        binary_signs: vec![entry("dist/{{ Version/../app.sig"), entry("dist/app.sig")],
        ..Default::default()
    };
    let mut none = Vec::new();
    check_sign_duplicate_outputs(&climbing, &mut none);
    assert!(none.is_empty(), "{none:?}");
}

/// A spelling holding a placeholder that renders a whole PATH is compared
/// without the `dist` join, whatever the placeholder is called: the run
/// resolves it to a path that already carries `dist`, so joining `dist` a
/// second time would call two different files one.
#[test]
fn a_spelling_that_renders_a_path_is_compared_without_the_dist_join() {
    use anodizer_core::config::SignConfig;
    let entry = |signature: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        signature: Some(signature.to_string()),
        ..Default::default()
    };
    for pair in [
        // The shell-style siblings of `{{ .Artifact }}`, expanded after the
        // render — `${artifact}.sig` is also the imported GoReleaser default.
        ["${artifact}.sig", "dist/${artifact}.sig"],
        ["$artifact.sig", "dist/$artifact.sig"],
        ["${signature}.asc", "dist/${signature}.asc"],
        ["$signature.asc", "dist/$signature.asc"],
        ["${certificate}.pem", "dist/${certificate}.pem"],
        ["$certificate.pem", "dist/$certificate.pem"],
        // A variable the operator points wherever they like.
        [
            "{{ .Env.SIG_DIR }}/app.sig",
            "dist/{{ .Env.SIG_DIR }}/app.sig",
        ],
        [
            "{{ Var.sig_dir }}/app.sig",
            "dist/{{ Var.sig_dir }}/app.sig",
        ],
        // One unbounded placeholder is enough, however many bounded ones
        // stand beside it.
        [
            "{{ .Artifact }}-{{ Version }}.sig",
            "dist/{{ .Artifact }}-{{ Version }}.sig",
        ],
    ] {
        let config = Config {
            binary_signs: vec![entry(pair[0]), entry(pair[1])],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_sign_duplicate_outputs(&config, &mut warnings);
        assert!(warnings.is_empty(), "{pair:?}: {warnings:?}");
    }

    // `$artifactName` and `$artifactID` are variables of those names, each
    // expanding to a NAME, so a prefix match on `$artifact` must not read
    // them as paths and skip the join.
    for pair in [
        ["$artifactName.sig", "dist/$artifactName.sig"],
        ["$artifactID.sig", "dist/$artifactID.sig"],
    ] {
        let config = Config {
            binary_signs: vec![entry(pair[0]), entry(pair[1])],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_sign_duplicate_outputs(&config, &mut warnings);
        assert_eq!(
            warnings,
            vec![one_file_warning(
                "binary_signs[0]",
                "binary_signs[1]",
                "signature"
            )],
            "{pair:?}"
        );
    }

    // Only the bounded name-only variables keep the join, so the pair the
    // check really is about still warns.
    let bounded = Config {
        binary_signs: vec![
            entry("{{ ProjectName }}-{{ Version }}.sig"),
            entry("dist/{{ ProjectName }}-{{ Version }}.sig"),
        ],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&bounded, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "signature"
        )]
    );
}

/// A `}}` inside a quoted literal closes the run early, so the tail it
/// leaves behind is read as real path components rather than as part of the
/// placeholder.
#[test]
fn a_placeholder_closed_by_a_quoted_brace_leaves_its_tail_unmasked() {
    use anodizer_core::config::SignConfig;
    let entry = |signature: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        signature: Some(signature.to_string()),
        ..Default::default()
    };
    let config = Config {
        binary_signs: vec![
            entry(r#"dist/{{ printf "}}" }}/sigs/app.sig"#),
            entry(r#"./dist/{{ printf "}}" }}/sigs/../sigs/app.sig"#),
        ],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "signature"
        )]
    );
}

/// `signs:` entries overwrite each other exactly as `binary_signs:` entries
/// do — the same struct, the same resolver, the same `dist`.
#[test]
fn two_signs_entries_resolving_one_file_warn() {
    use anodizer_core::config::SignConfig;
    let entry = |signature: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        artifacts: Some("all".to_string()),
        signature: Some(signature.to_string()),
        ..Default::default()
    };
    let config = Config {
        signs: vec![entry("app.sig"), entry("dist/app.sig")],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning("signs[0]", "signs[1]", "signature")]
    );
}

/// The repo's own documented multi-signer config signs two disjoint kinds,
/// so neither entry can overwrite the other's output however they name it.
/// `signs:` resolves an absent `artifacts:` to `none`, which selects nothing
/// at all, so a pair that sets no filter is quiet for the same reason.
#[test]
fn sign_entries_selecting_disjoint_artifact_kinds_warn_nothing() {
    use anodizer_core::config::SignConfig;
    let entry = |artifacts: Option<&str>, cmd: &str| SignConfig {
        artifacts: artifacts.map(str::to_string),
        cmd: Some(cmd.to_string()),
        args: Some(vec!["${signature}".to_string(), "${artifact}".to_string()]),
        ..Default::default()
    };
    for pair in [
        // docs/site/content/docs/sign/binaries-archives.md, "Multiple
        // signing configs".
        vec![
            entry(Some("archive"), "gpg"),
            entry(Some("checksum"), "cosign"),
        ],
        vec![entry(None, "gpg"), entry(None, "cosign")],
    ] {
        let config = Config {
            signs: pair,
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_sign_duplicate_outputs(&config, &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    // The same two entries under a filter that takes every kind really do
    // overwrite each other.
    let both = Config {
        signs: vec![entry(Some("all"), "gpg"), entry(Some("all"), "cosign")],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&both, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning("signs[0]", "signs[1]", "signature")]
    );
}

/// `binary_signs:` resolves an absent `artifacts:` to `binary`, so two
/// entries that set no filter still meet — the default of that slice is not
/// the default of `signs:`.
#[test]
fn the_artifacts_term_reads_each_slices_own_default() {
    use anodizer_core::config::SignConfig;
    let entry = |cmd: &str| SignConfig {
        cmd: Some(cmd.to_string()),
        ..Default::default()
    };
    let config = Config {
        binary_signs: vec![entry("cosign"), entry("gpg")],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning(
            "binary_signs[0]",
            "binary_signs[1]",
            "signature"
        )]
    );

    // `windows` and `binary` both take a `Binary`, so the two meet. Written
    // on `signs:`, the slice whose loader accepts both values — a
    // `binary_signs:` entry can only ever say `binary` or `none`.
    let windows = Config {
        signs: vec![
            SignConfig {
                artifacts: Some("windows".to_string()),
                ..entry("osslsigncode")
            },
            SignConfig {
                artifacts: Some("binary".to_string()),
                ..entry("gpg")
            },
        ],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_sign_duplicate_outputs(&windows, &mut warnings);
    assert_eq!(
        warnings,
        vec![one_file_warning("signs[0]", "signs[1]", "signature")]
    );
}

/// An unpadded `{{.Artifact}}` is substituted by nothing and reaches the
/// template engine as an undefined variable, so it can only fail the run.
/// Every other padding of the same name fails identically, because the
/// substitution is by exact literal.
#[test]
fn a_mis_padded_literal_placeholder_warns() {
    use anodizer_core::config::SignConfig;
    for spelling in [
        "{{.Artifact}}",
        "{{Artifact}}",
        "{{ .Artifact}}",
        "{{.Artifact }}",
        "{{  .Artifact  }}",
        "{{ Artifact}}",
    ] {
        let config = Config {
            binary_signs: vec![SignConfig {
                signature: Some(format!("{spelling}.sig")),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_unpadded_sign_placeholders(&config, &mut warnings);
        assert_eq!(
            warnings,
            vec![format!(
                "binary_signs[0].signature names `{spelling}`, which anodizer \
                 substitutes only as the literal `{{{{ .Artifact }}}}` or \
                 `{{{{ Artifact }}}}` — every other spelling reaches the \
                 template engine as an undefined variable and fails the sign \
                 stage"
            )]
        );
    }

    // The two spellings the stage really does substitute warn nothing.
    for spelling in ["{{ .Artifact }}", "{{ Artifact }}"] {
        let config = Config {
            binary_signs: vec![SignConfig {
                signature: Some(format!("{spelling}.sig")),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_unpadded_sign_placeholders(&config, &mut warnings);
        assert!(warnings.is_empty(), "{spelling}: {warnings:?}");
    }
}

/// `signature:` and `certificate:` are what the signature and certificate
/// paths are derived FROM, so those two names have no value there in any
/// padding. The remedy has to be something that works: the sibling output's
/// shell-style name resolves after the render, but the field's OWN name
/// resolves to this template's unexpanded text, so there the only answer is
/// to drop the reference.
#[test]
fn a_placeholder_the_field_never_substitutes_warns_in_every_padding() {
    use anodizer_core::config::SignConfig;
    let derived = |field: &str, shell: &str| {
        format!(
            "; the {field} path is what this template renders, so \
             `${{{shell}}}` has no value here either — remove the reference"
        )
    };
    let sibling = |shell: &str| {
        format!("; write `${{{shell}}}`, which the sign stage expands after the render")
    };
    for (field, spelling, remedy) in [
        (
            "signature",
            "{{ .Signature }}",
            derived("signature", "signature"),
        ),
        (
            "signature",
            "{{.Signature}}",
            derived("signature", "signature"),
        ),
        (
            "certificate",
            "{{ Certificate }}",
            derived("certificate", "certificate"),
        ),
        ("signature", "{{ Certificate }}", sibling("certificate")),
        ("certificate", "{{ .Signature }}", sibling("signature")),
    ] {
        let mut cfg = SignConfig::default();
        match field {
            "signature" => cfg.signature = Some(format!("{spelling}.x")),
            _ => cfg.certificate = Some(format!("{spelling}.x")),
        }
        let config = Config {
            binary_signs: vec![cfg],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_unpadded_sign_placeholders(&config, &mut warnings);
        assert_eq!(
            warnings,
            vec![format!(
                "binary_signs[0].{field} names `{spelling}`, which anodizer \
                 does not substitute in {field}: — it reaches the template \
                 engine as an undefined variable and fails the sign \
                 stage{remedy}"
            )],
            "{spelling} in {field}"
        );
    }
}

/// A placeholder written inside an expression is missed by the literal
/// replacement exactly as a mis-padded one is, and the name is seeded in no
/// template context, so the render fails either way.
#[test]
fn a_placeholder_inside_an_expression_warns() {
    use anodizer_core::config::SignConfig;
    for spelling in [
        "{{ Artifact | upper }}",
        "{{ Artifact.path }}",
        "{{ .Artifact | default(value=\"x\") }}",
    ] {
        let config = Config {
            binary_signs: vec![SignConfig {
                signature: Some(format!("{spelling}.sig")),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_unpadded_sign_placeholders(&config, &mut warnings);
        assert_eq!(
            warnings,
            vec![format!(
                "binary_signs[0].signature names `{spelling}`, which anodizer \
                 substitutes only as the literal `{{{{ .Artifact }}}}` or \
                 `{{{{ Artifact }}}}` — every other spelling reaches the \
                 template engine as an undefined variable and fails the sign \
                 stage"
            )]
        );
    }

    // A name that merely starts or ends the same is a different variable,
    // and a name inside a quoted literal is text Tera never looks up.
    for spelling in [
        "{{ ArtifactName }}",
        "{{ my_Artifact }}",
        "{{ my_artifact }}",
        "{{ \"Artifact\" }}",
        "{{ Version | replace(from=\"Artifact\", to=\"x\") }}",
        "{{ ['Artifact'] | join(sep=\"-\") }}",
    ] {
        let config = Config {
            binary_signs: vec![SignConfig {
                signature: Some(format!("{spelling}.sig")),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_unpadded_sign_placeholders(&config, &mut warnings);
        assert!(warnings.is_empty(), "{spelling}: {warnings:?}");
    }
}

/// A statement block reads its names out of the same template context an
/// expression does, so `{% set x = Artifact %}` fails the render exactly as
/// `{{ Artifact | upper }}` does and is warned about the same way.
#[test]
fn a_placeholder_inside_a_statement_block_warns() {
    use anodizer_core::config::SignConfig;
    let spelling = "{% set x = Artifact %}";
    let config = Config {
        binary_signs: vec![SignConfig {
            signature: Some(format!("{spelling}out.sig")),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_unpadded_sign_placeholders(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![format!(
            "binary_signs[0].signature names `{spelling}`, which anodizer \
             substitutes only as the literal `{{{{ .Artifact }}}}` or \
             `{{{{ Artifact }}}}` — every other spelling reaches the template \
             engine as an undefined variable and fails the sign stage"
        )]
    );
}

/// The closing delimiter is searched for in the whole remainder, so a
/// template whose `{{` never closes holds no complete run after it either —
/// ending the scan there loses no spelling. A run that closes AFTER another
/// opener is still one run, and still warned about.
#[test]
fn an_unclosed_run_leaves_no_later_run_to_find() {
    use anodizer_core::config::SignConfig;
    let warnings_for = |template: &str| {
        let config = Config {
            binary_signs: vec![SignConfig {
                signature: Some(template.to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_unpadded_sign_placeholders(&config, &mut warnings);
        warnings
    };
    assert!(warnings_for("{{ Artifact | upper").is_empty());
    assert!(warnings_for("out.sig {{ .Artifact").is_empty());
    assert_eq!(warnings_for("{{ oops {{ Artifact }}.sig").len(), 1);
    assert!(warnings_for("{{ .Artifact }}{{ Artifact }}.sig").is_empty());
}

/// Tera strips a `{# … #}` comment before evaluating the template, so a
/// placeholder written inside one cannot fail the render and is not warned
/// about — while one written after the comment still is.
#[test]
fn a_placeholder_inside_a_tera_comment_warns_nothing() {
    use anodizer_core::config::SignConfig;
    let config = Config {
        binary_signs: vec![SignConfig {
            signature: Some("{# {{.Artifact}} #}out.sig".to_string()),
            certificate: Some("{# note #}{{.Artifact}}.pem".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_unpadded_sign_placeholders(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "binary_signs[0].certificate names `{{.Artifact}}`, which anodizer \
             substitutes only as the literal `{{ .Artifact }}` or \
             `{{ Artifact }}` — every other spelling reaches the template \
             engine as an undefined variable and fails the sign stage"
                .to_string(),
        ]
    );
}

/// `args:` substitutes all three names, so only the padding is wrong there.
#[test]
fn an_args_template_substitutes_every_placeholder_name() {
    use anodizer_core::config::SignConfig;
    let config = Config {
        binary_signs: vec![SignConfig {
            args: Some(vec![
                "{{ .Signature }}".to_string(),
                "{{ Certificate }}".to_string(),
                "{{.Signature}}".to_string(),
            ]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_unpadded_sign_placeholders(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "binary_signs[0].args names `{{.Signature}}`, which anodizer \
             substitutes only as the literal `{{ .Signature }}` or \
             `{{ Signature }}` — every other spelling reaches the template \
             engine as an undefined variable and fails the sign stage"
                .to_string(),
        ]
    );
}

/// `stdin:` is handed to the template engine raw, so every one of the three
/// names fails there in either padding. A `signs:` entry can still reach the
/// value through the shell-style spelling; a `docker_signs:` entry's stdin
/// is not shell-expanded, so the warning offers no remedy it cannot keep.
#[test]
fn a_stdin_template_substitutes_no_placeholder() {
    use anodizer_core::config::{DockerSignConfig, SignConfig};
    let config = Config {
        binary_signs: vec![SignConfig {
            stdin: Some("{{ .Artifact }}".to_string()),
            ..Default::default()
        }],
        docker_signs: Some(vec![DockerSignConfig {
            stdin: Some("{{ Signature }}".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_unpadded_sign_placeholders(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "binary_signs[0].stdin names `{{ .Artifact }}`, which anodizer does \
             not substitute in stdin: — it reaches the template engine as an \
             undefined variable and fails the sign stage; write `${artifact}`, \
             which the sign stage expands after the render"
                .to_string(),
            "docker_signs[0].stdin names `{{ Signature }}`, which anodizer does \
             not substitute in stdin: — it reaches the template engine as an \
             undefined variable and fails the sign stage"
                .to_string(),
        ]
    );
}

/// The `docker_signs:` argv is substituted the same three ways as a sign
/// entry's, so a mis-padded name warns there too.
#[test]
fn a_docker_args_placeholder_warns_on_its_padding() {
    use anodizer_core::config::DockerSignConfig;
    let config = Config {
        docker_signs: Some(vec![DockerSignConfig {
            args: Some(vec!["{{.Certificate}}".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_unpadded_sign_placeholders(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "docker_signs[0].args names `{{.Certificate}}`, which anodizer \
             substitutes only as the literal `{{ .Certificate }}` or \
             `{{ Certificate }}` — every other spelling reaches the template \
             engine as an undefined variable and fails the sign stage"
                .to_string(),
        ]
    );
}

/// A slice the `defaults:` fold filled holds an entry the operator never
/// wrote, so every check that names a block names the `defaults.` one.
#[test]
fn a_defaults_filled_sign_slice_is_named_as_the_defaults_block() {
    use anodizer_core::config::{Defaults, DockerSignConfig, SignConfig};
    let mut config = Config {
        project_name: "test".to_string(),
        defaults: Some(Defaults {
            sign: Some(SignConfig {
                args: Some(vec!["{{.Artifact}}".to_string()]),
                artifacts: Some("bogus".to_string()),
                ..Default::default()
            }),
            binary_signs: Some(SignConfig {
                stdin: Some("{{ Signature }}".to_string()),
                ..Default::default()
            }),
            docker_signs: Some(DockerSignConfig {
                args: Some(vec!["{{Artifact}}".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    anodizer_core::defaults_merge::apply_defaults(&mut config);

    let mut warnings = Vec::new();
    check_unpadded_sign_placeholders(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "defaults.sign.args names `{{.Artifact}}`, which anodizer \
             substitutes only as the literal `{{ .Artifact }}` or \
             `{{ Artifact }}` — every other spelling reaches the template \
             engine as an undefined variable and fails the sign stage"
                .to_string(),
            "defaults.binary_signs.stdin names `{{ Signature }}`, which \
             anodizer does not substitute in stdin: — it reaches the template \
             engine as an undefined variable and fails the sign stage; write \
             `${signature}`, which the sign stage expands after the render"
                .to_string(),
            "defaults.docker_signs.args names `{{Artifact}}`, which anodizer \
             substitutes only as the literal `{{ .Artifact }}` or \
             `{{ Artifact }}` — every other spelling reaches the template \
             engine as an undefined variable and fails the sign stage"
                .to_string(),
        ]
    );

    let mut warnings = Vec::new();
    check_sign_artifact_filters(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![format!(
            "unrecognized defaults.sign artifacts filter 'bogus' (valid: {})",
            anodizer_stage_sign::VALID_SIGN_ARTIFACT_FILTERS.join(", ")
        )]
    );
}

/// Every double-quoted string literal in `src`, with `\`-newline
/// continuations resolved the way rustc resolves them: the escape eats the
/// newline and the indentation after it, so what is left is the text a
/// message really carries.
fn message_literals(src: &str) -> Vec<String> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // A `//` comment can hold an unbalanced quote.
        if chars[i] == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // A raw string processes no escape, so it is read to its own
        // terminator rather than through the escape rules below.
        if chars[i] == 'r' {
            let mut j = i + 1;
            while chars.get(j) == Some(&'#') {
                j += 1;
            }
            if chars.get(j) == Some(&'"') {
                let hashes = j - i - 1;
                let mut k = j + 1;
                loop {
                    match chars.get(k) {
                        None => break,
                        Some('"') if chars[k + 1..k + 1 + hashes].iter().all(|c| *c == '#') => {
                            break;
                        }
                        Some(_) => k += 1,
                    }
                }
                out.push(chars[j + 1..k.min(chars.len())].iter().collect());
                i = (k + 1 + hashes).min(chars.len());
                continue;
            }
        }
        if chars[i] != '"' {
            i += 1;
            continue;
        }
        i += 1;
        let mut literal = String::new();
        while i < chars.len() && chars[i] != '"' {
            if chars[i] == '\\' {
                i += 1;
                match chars.get(i) {
                    Some('\n') => {
                        i += 1;
                        while matches!(chars.get(i), Some(' ') | Some('\t')) {
                            i += 1;
                        }
                    }
                    Some(_) => i += 1,
                    None => break,
                }
                continue;
            }
            literal.push(chars[i]);
            i += 1;
        }
        i += 1;
        out.push(literal);
    }
    out
}

/// A format string broken across source lines needs a `\` continuation: the
/// escape eats the newline AND the indentation after it. Written without
/// one, that indentation goes into the message, and `check config` prints a
/// single 300-column line carrying 26-space runs — which every assertion
/// shaped like `starts_with(…)` reads straight past.
///
/// So no message this module builds may carry a run of three spaces —
/// asked of every production source under `check/config/`, not just the one
/// that happened to hold the defect.
/// How many function bodies under `check/config/` build a message. Pinned
/// so a rename or a move that empties the walk fails instead of passing
/// with nothing to ask.
const MESSAGE_BUILDING_BODIES: usize = 31;

#[test]
fn no_check_config_message_carries_a_run_of_spaces() {
    use anodizer_core::test_helpers::test_sources::{
        function_bodies, production_half, rust_sources,
    };

    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src/commands/check/config");
    let sources = rust_sources(std::path::Path::new(dir));
    assert_eq!(
        sources.len(),
        5,
        "the production sources under {dir} the walk found: {sources:?}"
    );
    let building: Vec<String> = sources
        .iter()
        .flat_map(|path| {
            let src =
                std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            function_bodies(production_half(&src))
        })
        .filter(|body| body.contains("format!("))
        .collect();
    assert_eq!(
        building.len(),
        MESSAGE_BUILDING_BODIES,
        "the message-building bodies the walk found under {dir}"
    );
    let offenders: Vec<String> = building
        .iter()
        .flat_map(|body| message_literals(body))
        .filter(|literal| literal.contains("   "))
        .collect();
    assert!(
        offenders.is_empty(),
        "a message literal carries a run of three or more spaces, so its line \
         continuation is missing and the warning prints this file's own \
         indentation: {offenders:?}"
    );
}

/// The filter check walks every sign slice, so an unrecognized value on a
/// per-crate slice is caught exactly as one on the top-level `signs:` is.
///
/// Both arms are `signs:`, which is the only slice a YAML file can write an
/// unrecognized filter on: `binary_signs:` is loaded through a deserializer
/// that refuses everything but `binary` and `none`, so its own vocabulary is
/// asked on the one route past that deserializer —
/// `a_wide_filter_under_defaults_binary_signs_warns`.
#[test]
fn an_unrecognized_filter_warns_on_every_sign_slice() {
    let yaml = r#"
project_name: test
signs:
  - artifacts: bogus
workspaces:
  - name: ws
    crates:
      - name: app
    signs:
      - artifacts: bogus
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).expect("the loader accepts it");
    anodizer_core::defaults_merge::apply_defaults(&mut config);
    let mut warnings = Vec::new();
    check_sign_artifact_filters(&config, &mut warnings);
    let valid = anodizer_stage_sign::VALID_SIGN_ARTIFACT_FILTERS.join(", ");
    assert_eq!(
        warnings,
        vec![
            format!("unrecognized signs[0] artifacts filter 'bogus' (valid: {valid})"),
            format!(
                "unrecognized workspaces.ws.signs[0] artifacts filter 'bogus' (valid: {valid})"
            ),
        ]
    );
}

/// `defaults.binary_signs:` is a plain `SignConfig` the defaults fold copies
/// into the slice, so it is the one route by which a filter the
/// `binary_signs:` loader refuses reaches the run — where it is ignored and
/// every binary is signed anyway. Driven through YAML, which is the only way
/// the config could be written.
#[test]
fn a_wide_filter_under_defaults_binary_signs_warns() {
    let yaml = r#"
project_name: test
defaults:
  binary_signs:
    artifacts: archive
    cmd: cosign
"#;
    let mut config: Config = serde_yaml_ng::from_str(yaml).expect("the loader accepts it");
    anodizer_core::defaults_merge::apply_defaults(&mut config);
    let mut warnings = Vec::new();
    check_sign_artifact_filters(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "defaults.binary_signs artifacts filter 'archive' is not allowed \
             on binary_signs (valid: binary, none) — the sign stage signs \
             binaries whatever it says"
                .to_string(),
        ]
    );

    // The two values the field really does take warn nothing.
    for filter in ["binary", "none"] {
        let mut config: Config = serde_yaml_ng::from_str(&format!(
            "project_name: test\ndefaults:\n  binary_signs:\n    artifacts: {filter}\n"
        ))
        .expect("the loader accepts it");
        anodizer_core::defaults_merge::apply_defaults(&mut config);
        let mut warnings = Vec::new();
        check_sign_artifact_filters(&config, &mut warnings);
        assert!(warnings.is_empty(), "{filter}: {warnings:?}");
    }
}

/// The docker sign path renders its templates and substitutes
/// `{{ .Artifact }}` / `{{ .Signature }}` by literal; it never expands the
/// `${…}` variables the detached sign path does, so a shell-style reference
/// reaches cosign as text. `stdin:` substitutes nothing at all, so it is
/// offered no remedy.
#[test]
fn a_docker_shell_variable_warns_that_it_is_never_expanded() {
    use anodizer_core::config::DockerSignConfig;
    let config = Config {
        docker_signs: Some(vec![DockerSignConfig {
            args: Some(vec![
                "sign".to_string(),
                "${artifact}".to_string(),
                "--certificate=$certificate".to_string(),
            ]),
            stdin: Some("${signature}".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_unpadded_sign_placeholders(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "docker_signs[0].args names `${artifact}`, which the docker sign \
             path never expands — it reaches the signing command as that \
             literal text; write `{{ .Artifact }}`, which anodizer substitutes \
             before the render"
                .to_string(),
            "docker_signs[0].args names `${certificate}`, which the docker \
             sign path never expands — it reaches the signing command as that \
             literal text; a docker certificate path is read nowhere, so \
             remove the reference"
                .to_string(),
            "docker_signs[0].stdin names `${signature}`, which the docker sign \
             path never expands — it reaches the signing command as that \
             literal text"
                .to_string(),
        ]
    );

    // The spellings the docker path really does substitute warn nothing.
    let config = Config {
        docker_signs: Some(vec![DockerSignConfig {
            args: Some(vec![
                "{{ .Artifact }}@{{ .Digest }}".to_string(),
                "{{ Signature }}".to_string(),
            ]),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_unpadded_sign_placeholders(&config, &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
}

/// A docker certificate path is read nowhere — only its presence is, to pick
/// cosign's bundle verify mode — so the placeholder the argv substitutes
/// resolves to the empty string and the argument ships with no value.
#[test]
fn a_docker_certificate_placeholder_warns_that_it_renders_empty() {
    use anodizer_core::config::DockerSignConfig;
    let config = Config {
        docker_signs: Some(vec![DockerSignConfig {
            certificate: Some("cert.pem".to_string()),
            args: Some(vec!["--certificate={{ .Certificate }}".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_unpadded_sign_placeholders(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "docker_signs[0].args names `{{ .Certificate }}`, which anodizer \
             substitutes with the empty string on the docker path — a docker \
             certificate path is read nowhere, so the argument reaches the \
             signing command with no value"
                .to_string(),
        ]
    );
}

/// A `docker_signs:` entry's `signature:` names no file — the signature is
/// stored in the registry — so setting it does nothing and check says so.
#[test]
fn a_docker_sign_signature_template_warns_that_it_is_ignored() {
    use anodizer_core::config::DockerSignConfig;
    let config = Config {
        docker_signs: Some(vec![
            DockerSignConfig {
                signature: Some("{{ .Artifact }}.sig".to_string()),
                ..Default::default()
            },
            DockerSignConfig::default(),
        ]),
        ..Default::default()
    };
    let mut warnings = Vec::new();
    check_docker_sign_signature_templates(&config, &mut warnings);
    assert_eq!(
        warnings,
        vec![
            "docker_signs[0].signature is set but a docker signature is stored \
             in the registry rather than written to a file (it will be ignored)"
                .to_string(),
        ]
    );
}

/// Two entries whose `signature:` templates differ write two files, and two
/// entries whose `ids:` cannot both take one binary never meet — neither is
/// the overwrite this warns about.
#[test]
fn binary_signs_entries_that_write_two_files_warn_nothing() {
    use anodizer_core::config::SignConfig;
    let entry = |ids: Option<Vec<String>>, signature: &str| SignConfig {
        cmd: Some("cosign".to_string()),
        ids,
        signature: Some(signature.to_string()),
        ..Default::default()
    };
    for pair in [
        vec![
            entry(None, "{{ .Artifact }}.sig"),
            entry(None, "{{ .Artifact }}.bundle.sig"),
        ],
        vec![
            entry(Some(vec!["app".to_string()]), "{{ .Artifact }}.sig"),
            entry(Some(vec!["helper".to_string()]), "{{ .Artifact }}.sig"),
        ],
    ] {
        let config = Config {
            binary_signs: pair,
            ..Default::default()
        };
        let mut warnings = Vec::new();
        check_sign_duplicate_outputs(&config, &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}

// ---- Target-triple validation tests ----

#[test]
fn target_triple_warns_on_unrecognized_in_defaults() {
    use anodizer_core::config::Defaults;
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.defaults = Some(Defaults {
        targets: Some(vec![
            "x86_64-unknown-linux-gnu".to_string(), // valid
            "sparc-sun-solaris".to_string(),        // unknown arch AND os
        ]),
        ..Default::default()
    });
    let mut warnings: Vec<String> = vec![];
    check_target_triples(&config, &mut warnings);
    assert_eq!(warnings.len(), 1, "only the bad triple warns: {warnings:?}");
    assert!(
        warnings[0].contains("sparc-sun-solaris") && warnings[0].contains("defaults.targets"),
        "warning should name the triple and its context: {:?}",
        warnings
    );
}

#[test]
fn target_triple_warns_on_unrecognized_in_crate_build() {
    use anodizer_core::config::BuildConfig;
    let mut crate_cfg = make_crate("mycrate", "v{{ .Version }}", None);
    crate_cfg.builds = Some(vec![BuildConfig {
        binary: Some("mybin".to_string()),
        targets: Some(vec!["not-a-real-triple".to_string()]),
        ..Default::default()
    }]);
    let config = make_config(vec![crate_cfg]);
    let mut warnings: Vec<String> = vec![];
    check_target_triples(&config, &mut warnings);
    assert_eq!(warnings.len(), 1, "got: {warnings:?}");
    assert!(
        warnings[0].contains("not-a-real-triple")
            && warnings[0].contains("crate 'mycrate'")
            && warnings[0].contains("build 'mybin'"),
        "warning should name the triple, crate, and build binary: {:?}",
        warnings
    );
}

#[test]
fn target_triple_silent_on_known_triples() {
    use anodizer_core::config::Defaults;
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.defaults = Some(Defaults {
        targets: Some(vec![
            "aarch64-apple-darwin".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
        ]),
        ..Default::default()
    });
    let mut warnings: Vec<String> = vec![];
    check_target_triples(&config, &mut warnings);
    assert!(
        warnings.is_empty(),
        "known triples must not warn: {warnings:?}"
    );
}

// ---- Changelog `use` validation tests ----

#[test]
fn changelog_use_warns_on_unrecognized_value() {
    use anodizer_core::config::ChangelogConfig;
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.changelog = Some(ChangelogConfig {
        use_source: Some("mercurial".to_string()),
        ..Default::default()
    });
    let mut warnings: Vec<String> = vec![];
    check_changelog(&config, &mut warnings);
    assert_eq!(warnings.len(), 1, "got: {warnings:?}");
    assert!(
        warnings[0].contains("mercurial") && warnings[0].contains("git, github-native"),
        "warning should name the bad value and valid set: {:?}",
        warnings
    );
}

#[test]
fn changelog_use_silent_on_github_native() {
    use anodizer_core::config::ChangelogConfig;
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.changelog = Some(ChangelogConfig {
        use_source: Some("github-native".to_string()),
        ..Default::default()
    });
    let mut warnings: Vec<String> = vec![];
    check_changelog(&config, &mut warnings);
    assert!(warnings.is_empty(), "github-native is valid: {warnings:?}");
}

// ---- Checksum-algorithm validation tests ----

#[test]
fn checksum_algorithm_warns_on_unrecognized_in_defaults() {
    use anodizer_core::config::{ChecksumConfig, Defaults};
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.defaults = Some(Defaults {
        checksum: Some(ChecksumConfig {
            algorithm: Some("crc32".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let mut warnings: Vec<String> = vec![];
    check_checksum_algorithms(&config, &mut warnings);
    assert_eq!(warnings.len(), 1, "got: {warnings:?}");
    assert!(
        warnings[0].contains("crc32") && warnings[0].contains("defaults.checksum"),
        "warning should name the algorithm and context: {:?}",
        warnings
    );
}

#[test]
fn checksum_algorithm_warns_on_unrecognized_per_crate() {
    use anodizer_core::config::ChecksumConfig;
    let mut crate_cfg = make_crate("mycrate", "v{{ .Version }}", None);
    crate_cfg.checksum = Some(ChecksumConfig {
        algorithm: Some("md5".to_string()),
        ..Default::default()
    });
    let config = make_config(vec![crate_cfg]);
    let mut warnings: Vec<String> = vec![];
    check_checksum_algorithms(&config, &mut warnings);
    assert_eq!(warnings.len(), 1, "got: {warnings:?}");
    assert!(
        warnings[0].contains("md5") && warnings[0].contains("mycrate"),
        "warning should name the algorithm and crate: {:?}",
        warnings
    );
}

#[test]
fn checksum_algorithm_silent_on_known_algorithm() {
    use anodizer_core::config::{ChecksumConfig, Defaults};
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.defaults = Some(Defaults {
        checksum: Some(ChecksumConfig {
            algorithm: Some("blake2b".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let mut warnings: Vec<String> = vec![];
    check_checksum_algorithms(&config, &mut warnings);
    assert!(warnings.is_empty(), "blake2b is valid: {warnings:?}");
}

// ---- SBOM artifacts validation tests ----

#[test]
fn sbom_artifacts_errors_on_unrecognized_value() {
    use anodizer_core::config::SbomConfig;
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.sboms = vec![SbomConfig {
        id: Some("main".to_string()),
        artifacts: Some("everything".to_string()),
        ..Default::default()
    }];
    let mut errors: Vec<String> = vec![];
    check_sbom_configs(&config, &mut errors);
    assert_eq!(errors.len(), 1, "got: {errors:?}");
    assert!(
        errors[0].contains("everything") && errors[0].contains("main"),
        "error should name the value and the sbom label: {:?}",
        errors
    );
}

#[test]
fn sbom_artifacts_silent_on_known_value() {
    use anodizer_core::config::SbomConfig;
    let mut config = make_config(vec![make_crate("a", "a-v{{ .Version }}", None)]);
    config.sboms = vec![SbomConfig {
        artifacts: Some("binary".to_string()),
        ..Default::default()
    }];
    let mut errors: Vec<String> = vec![];
    check_sbom_configs(&config, &mut errors);
    assert!(
        errors.is_empty(),
        "'binary' is a valid artifacts type: {errors:?}"
    );
}

// ---- Announce secret-exposure: remaining channels ----

#[test]
fn announce_secret_warns_across_all_remaining_channels() {
    use anodizer_core::config::{
        DiscordAnnounce, LinkedInAnnounce, MastodonAnnounce, MattermostAnnounce,
        OpenCollectiveAnnounce, RedditAnnounce, TeamsAnnounce, TelegramAnnounce, WebhookConfig,
    };
    // Each channel content field carries a distinct secret-named ref so the
    // per-field warning routing (field label in the message) is exercised
    // once per channel branch.
    let warnings = collect_announce_warnings(AnnounceConfig {
        linkedin: Some(LinkedInAnnounce {
            message_template: Some("{{ Env.LINKEDIN_TOKEN }}".to_string()),
            ..Default::default()
        }),
        opencollective: Some(OpenCollectiveAnnounce {
            title_template: Some("{{ Env.OC_API_KEY }}".to_string()),
            message_template: Some("{{ Env.OC_SECRET }}".to_string()),
            ..Default::default()
        }),
        mastodon: Some(MastodonAnnounce {
            message_template: Some("{{ Env.MASTODON_TOKEN }}".to_string()),
            ..Default::default()
        }),
        discord: Some(DiscordAnnounce {
            message_template: Some("{{ Env.DISCORD_TOKEN }}".to_string()),
            author: Some("{{ Env.DISCORD_SECRET }}".to_string()),
            ..Default::default()
        }),
        webhook: Some(WebhookConfig {
            message_template: Some("{{ Env.WEBHOOK_TOKEN }}".to_string()),
            ..Default::default()
        }),
        telegram: Some(TelegramAnnounce {
            message_template: Some("{{ Env.TELEGRAM_TOKEN }}".to_string()),
            ..Default::default()
        }),
        teams: Some(TeamsAnnounce {
            message_template: Some("{{ Env.TEAMS_TOKEN }}".to_string()),
            title_template: Some("{{ Env.TEAMS_SECRET }}".to_string()),
            ..Default::default()
        }),
        mattermost: Some(MattermostAnnounce {
            message_template: Some("{{ Env.MM_TOKEN }}".to_string()),
            title_template: Some("{{ Env.MM_SECRET }}".to_string()),
            ..Default::default()
        }),
        reddit: Some(RedditAnnounce {
            title_template: Some("{{ Env.REDDIT_TOKEN }}".to_string()),
            url_template: Some("https://x/{{ Env.REDDIT_SECRET }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    // Two each for opencollective/discord/teams/mattermost/reddit + one each
    // for linkedin/mastodon/webhook/telegram = 5*2 + 4 = 14.
    assert_eq!(
        warnings.len(),
        14,
        "one warning per secret-named field: {warnings:?}"
    );
    for needle in [
        "announce.linkedin.message_template",
        "announce.opencollective.title_template",
        "announce.opencollective.message_template",
        "announce.mastodon.message_template",
        "announce.discord.message_template",
        "announce.discord.author",
        "announce.webhook.message_template",
        "announce.telegram.message_template",
        "announce.teams.message_template",
        "announce.teams.title_template",
        "announce.mattermost.message_template",
        "announce.mattermost.title_template",
        "announce.reddit.title_template",
        "announce.reddit.url_template",
    ] {
        assert!(
            warnings.iter().any(|w| w.contains(needle)),
            "missing warning for {needle}: {warnings:?}"
        );
    }
}

#[test]
fn announce_secret_warns_in_slack_attachment_text_fields() {
    use anodizer_core::config::{SlackAnnounce, SlackAttachment};
    // The attachment scan covers text/title/fallback/pretext/footer; drive
    // the first four (footer already has a dedicated test above).
    let warnings = collect_announce_warnings(AnnounceConfig {
        slack: Some(SlackAnnounce {
            attachments: Some(vec![SlackAttachment {
                text: Some("{{ Env.SLACK_A_TOKEN }}".to_string()),
                title: Some("{{ Env.SLACK_B_TOKEN }}".to_string()),
                fallback: Some("{{ Env.SLACK_C_TOKEN }}".to_string()),
                pretext: Some("{{ Env.SLACK_D_TOKEN }}".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    });
    assert_eq!(
        warnings.len(),
        4,
        "one per attachment content field: {warnings:?}"
    );
    for suffix in [".text", ".title", ".fallback", ".pretext"] {
        assert!(
            warnings
                .iter()
                .any(|w| w.contains(&format!("announce.slack.attachments[0]{suffix}"))),
            "missing attachment{suffix} warning: {warnings:?}"
        );
    }
}

// ---- Signing-tool availability warnings ----

#[test]
fn signing_tools_warns_on_missing_sign_cmd() {
    // A `signs.cmd` naming a binary not on PATH must warn — the release
    // would otherwise fail at sign time with a less-actionable spawn error.
    let config = Config {
        project_name: "test".to_string(),
        signs: vec![anodizer_core::config::SignConfig {
            cmd: Some("anodizer-nonexistent-signer".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut warnings: Vec<String> = vec![];
    check_signing_tools(&config, &mut warnings);
    assert_eq!(warnings.len(), 1, "got: {warnings:?}");
    assert!(
        warnings[0].contains("anodizer-nonexistent-signer")
            && warnings[0].contains("signs section"),
        "warning should name the missing tool and section: {:?}",
        warnings
    );
}

#[test]
fn signing_tools_warns_on_missing_docker_sign_cmd() {
    let config = Config {
        project_name: "test".to_string(),
        docker_signs: Some(vec![anodizer_core::config::DockerSignConfig {
            cmd: Some("anodizer-nonexistent-cosign".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let mut warnings: Vec<String> = vec![];
    check_signing_tools(&config, &mut warnings);
    assert_eq!(warnings.len(), 1, "got: {warnings:?}");
    assert!(
        warnings[0].contains("anodizer-nonexistent-cosign")
            && warnings[0].contains("docker_signs section"),
        "warning should name the missing tool and section: {:?}",
        warnings
    );
}

#[test]
fn signing_tools_silent_when_no_signing_configured() {
    let config = Config {
        project_name: "test".to_string(),
        ..Default::default()
    };
    let mut warnings: Vec<String> = vec![];
    check_signing_tools(&config, &mut warnings);
    assert!(
        warnings.is_empty(),
        "no signing config → no warnings: {warnings:?}"
    );
}

/// The sign docs page quotes `check config` output as the operator sees it,
/// and a reworded message leaves those blocks quoting a line the binary no
/// longer prints. So the page's own ```yaml blocks ARE the fixtures: each
/// one is parsed, the five sign checks are run over it, and what they
/// produce must be exactly what the ```text block after it quotes — every
/// quoted line produced, and every produced line quoted, in that order.
///
/// What this covers is `check config`'s own sign warnings. The page's
/// `Error sign:` blocks come from the sign stage rather than from a check
/// function and are pinned where that stage lives
/// (`crates/stage-sign`, `the_collision_errors_quoted_in_the_sign_docs_are_the_messages_the_stage_produces`).
#[test]
fn every_warning_quoted_in_the_sign_docs_is_a_message_the_checks_produce() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/site/content/docs/sign/binaries-archives.md"
    );
    let page = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let blocks = fenced_blocks(&page);

    let mut fixtures = 0usize;
    let mut quoted_lines = 0usize;
    for (at, (language, body)) in blocks.iter().enumerate() {
        if *language != "yaml" {
            continue;
        }
        // A fragment the loader refuses is not a config the page claims to
        // show output for.
        let Ok(mut config) = serde_yaml_ng::from_str::<Config>(body) else {
            continue;
        };
        anodizer_core::defaults_merge::apply_defaults(&mut config);
        fixtures += 1;
        let mut produced: Vec<String> = vec![];
        check_sign_artifact_filters(&config, &mut produced);
        check_sign_asset_name_templates(&config, &mut produced);
        check_sign_duplicate_outputs(&config, &mut produced);
        check_unpadded_sign_placeholders(&config, &mut produced);
        check_docker_sign_signature_templates(&config, &mut produced);

        let quoted: Vec<String> = blocks[at + 1..]
            .iter()
            .take_while(|(language, _)| *language != "yaml")
            .flat_map(|(_, body)| body.lines())
            .filter_map(|line| line.trim_start().strip_prefix("Warning "))
            .map(str::to_string)
            .collect();
        quoted_lines += quoted.len();
        assert_eq!(
            produced, quoted,
            "the config in block {at} and the output quoted under it disagree"
        );
    }
    assert_eq!(fixtures, 10, "the page's parseable config blocks");
    assert_eq!(quoted_lines, 9, "the warnings the page quotes");
}

/// Every fenced block on a docs page as `(language, body)`, in page order.
fn fenced_blocks(page: &str) -> Vec<(&str, &str)> {
    let mut blocks = Vec::new();
    let mut open: Option<(&str, usize)> = None;
    for line in page.lines() {
        let at = line.as_ptr() as usize - page.as_ptr() as usize;
        match (line.strip_prefix("```"), open) {
            (Some(_), Some((language, from))) => {
                blocks.push((language, &page[from..at]));
                open = None;
            }
            (Some(language), None) => open = Some((language.trim(), at + line.len() + 1)),
            (None, _) => {}
        }
    }
    blocks
}

/// The release-resilience page quotes the announce secret-exposure warning
/// the same way the sign page quotes its own, and the same rewording breaks
/// it. So that page's lint section is driven as a fixture too: its `yaml`
/// block is the config, and the `text` block under it is what the check has
/// to produce for it.
#[test]
fn the_warning_quoted_in_the_resilience_docs_is_a_message_the_check_produces() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/site/content/docs/advanced/release-resilience.md"
    );
    let page = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let section = page
        .split_once("### Static lint — `anodizer check config`")
        .expect("the lint section is on the page")
        .1;
    let blocks = fenced_blocks(section);
    let yaml = blocks
        .iter()
        .find(|(language, _)| *language == "yaml")
        .expect("the section shows a config");
    let quoted: Vec<String> = blocks
        .iter()
        .find(|(language, _)| *language == "text")
        .expect("the section shows the output")
        .1
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("Warning "))
        .map(str::to_string)
        .collect();

    let mut config: Config = serde_yaml_ng::from_str(yaml.1).expect("the loader accepts it");
    anodizer_core::defaults_merge::apply_defaults(&mut config);
    let mut produced: Vec<String> = vec![];
    check_announce_secret_exposure(&config, &mut produced);
    assert_eq!(produced, quoted);
    assert_eq!(quoted.len(), 1, "the section quotes one warning");
}

/// Read every `Error ` line a docs page quotes, in page order, with the label
/// and the renderer's indentation stripped. A line carrying the elision
/// character is an abbreviation of real output rather than a claim about it,
/// so it is left out.
fn quoted_error_lines(relative: &str) -> Vec<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/site/content/docs")
        .join(relative);
    let page =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    page.lines()
        .filter_map(|line| line.trim_start().strip_prefix("Error "))
        .filter(|line| !line.contains('\u{2026}'))
        .map(str::to_string)
        .collect()
}

/// The monorepo page and the release-resilience page both quote the workspace
/// membership guard's errors as the operator sees them, so a reword leaves
/// them showing a line the binary no longer prints. Each documented situation
/// is reproduced on its own temporary workspace and the pages' lines have to
/// be exactly what the guard produced.
///
/// The release-resilience page's other errors come from four more producers
/// and are pinned where each of them lives; this test asserts that page's
/// total error count too, so a newly quoted line fails here until it is
/// pinned somewhere.
#[test]
fn the_membership_errors_quoted_in_the_docs_are_what_the_guard_produces() {
    // A dependency on disk that the config never lists.
    let absent = tempdir().unwrap();
    write_disk_workspace(
        absent.path(),
        &[
            ("crates/cli", "anodizer", &["anodizer-stage-install-script"]),
            (
                "crates/stage-install-script",
                "anodizer-stage-install-script",
                &[],
            ),
        ],
    );
    let config = make_config(vec![with_active_cargo_publisher(CrateConfig {
        name: "anodizer".to_string(),
        path: p(absent.path(), "crates/cli"),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    })]);
    let mut produced = vec![];
    check_workspace_membership(
        &config,
        absent.path(),
        &flatten_crate_names(&config),
        &mut produced,
    );

    // A dependency the config lists but never uploads.
    let unpublished = tempdir().unwrap();
    write_disk_workspace(
        unpublished.path(),
        &[
            ("crates/cli", "anodizer", &["anodizer-core"]),
            ("crates/core", "anodizer-core", &[]),
        ],
    );
    let config = make_config(vec![
        with_active_cargo_publisher(CrateConfig {
            name: "anodizer".to_string(),
            path: p(unpublished.path(), "crates/cli"),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }),
        CrateConfig {
            name: "anodizer-core".to_string(),
            path: p(unpublished.path(), "crates/core"),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        },
    ]);
    check_workspace_membership(
        &config,
        unpublished.path(),
        &flatten_crate_names(&config),
        &mut produced,
    );

    let monorepo = quoted_error_lines("advanced/monorepo.md");
    assert_eq!(monorepo, produced[..1], "the monorepo page's Error lines");
    assert_eq!(monorepo.len(), 1, "the errors the monorepo page quotes");

    let resilience = quoted_error_lines("advanced/release-resilience.md");
    assert_eq!(
        resilience[3..],
        produced[..],
        "the release-resilience page's last two Error lines"
    );
    assert_eq!(
        resilience.len(),
        5,
        "the errors the release-resilience page quotes"
    );
}
