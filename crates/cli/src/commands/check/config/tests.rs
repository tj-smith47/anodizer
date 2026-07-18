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
    // climbs per-crate rather than coincidentally landing on `base_dir`,
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
