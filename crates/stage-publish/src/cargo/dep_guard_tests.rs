//! Workspace dependency-guard tests.

// ---------------------------------------------------------------------------
// dep-completeness guard tests
// ---------------------------------------------------------------------------

use super::*;
use anodizer_core::log::{StageLogger, Verbosity};

fn quiet_log() -> StageLogger {
    StageLogger::new("publish-test", Verbosity::Normal)
}

/// Write a crate dir with a `[package]` (version `ver`) plus a
/// `[dependencies]` block listing each `(dep_name, dep_version)`, and a
/// `[dev-dependencies]` block listing each `(dep_name, dep_version)` in
/// `dev_deps`. Returns the crate's path string.
fn write_crate(
    root: &std::path::Path,
    name: &str,
    ver: &str,
    deps: &[(&str, &str)],
    dev_deps: &[(&str, &str)],
) -> String {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let mut body = format!("[package]\nname = \"{name}\"\nversion = \"{ver}\"\n");
    if !deps.is_empty() {
        body.push_str("\n[dependencies]\n");
        for (d, dv) in deps {
            body.push_str(&format!("{d} = {{ version = \"{dv}\" }}\n"));
        }
    }
    if !dev_deps.is_empty() {
        body.push_str("\n[dev-dependencies]\n");
        for (d, dv) in dev_deps {
            body.push_str(&format!("{d} = {{ version = \"{dv}\" }}\n"));
        }
    }
    std::fs::write(dir.join("Cargo.toml"), body).expect("write manifest");
    dir.display().to_string()
}

fn crate_cfg(name: &str, path: &str, deps: &[&str]) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: path.to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
        publish: Some(anodizer_core::config::PublishConfig {
            cargo: Some(CargoPublishConfig::default()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// (1) A publishing crate whose workspace dep is missing from the set AND
/// absent from the index → the guard returns Err naming the dep + crate.
#[test]
fn guard_errors_when_dep_missing_from_set_and_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // `app` depends on `lib` (a workspace crate) but only `app` is in the
    // publish set; `lib` is not, and the index probe reports it Absent.
    let app_path = write_crate(tmp.path(), "app", "1.0.0", &[("lib", "1.0.0")], &[]);
    let lib_path = write_crate(tmp.path(), "lib", "1.0.0", &[], &[]);
    let all = vec![
        crate_cfg("app", &app_path, &["lib"]),
        crate_cfg("lib", &lib_path, &[]),
    ];
    let order = vec!["app".to_string()]; // lib intentionally NOT in the set
    let versions: HashMap<String, String> = [("app".to_string(), "1.0.0".to_string())]
        .into_iter()
        .collect();

    let probe = |_n: &str, _v: &str| DepIndexState::Absent;
    let err = check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
        .expect_err("missing-and-absent dep must fail the guard");
    let msg = format!("{err:#}");
    assert!(msg.contains("'app'"), "names the publishing crate: {msg}");
    assert!(msg.contains("'lib'"), "names the missing dep: {msg}");
    assert!(
        msg.contains("publish set"),
        "explains the fix (add to publish set): {msg}"
    );
}

/// (2) Every workspace dep is in the publish set → Ok regardless of index
/// state (the probe must not even be consulted for an in-set dep).
#[test]
fn guard_ok_when_all_deps_in_set() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app_path = write_crate(tmp.path(), "app", "1.0.0", &[("lib", "1.0.0")], &[]);
    let lib_path = write_crate(tmp.path(), "lib", "1.0.0", &[], &[]);
    let all = vec![
        crate_cfg("app", &app_path, &["lib"]),
        crate_cfg("lib", &lib_path, &[]),
    ];
    let order = vec!["lib".to_string(), "app".to_string()]; // both in set
    let versions: HashMap<String, String> = [
        ("app".to_string(), "1.0.0".to_string()),
        ("lib".to_string(), "1.0.0".to_string()),
    ]
    .into_iter()
    .collect();

    // Probe panics if called — an in-set dep must short-circuit before it.
    let probe = |_n: &str, _v: &str| panic!("index probe must not run for in-set deps");
    check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
        .expect("all deps in set → ok");
}

/// (3) A dep not in the set but already live on crates.io (mocked Present)
/// → Ok. The version probed must be the one the dependent requires.
#[test]
fn guard_ok_when_dep_not_in_set_but_already_on_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app_path = write_crate(tmp.path(), "app", "2.0.0", &[("lib", "1.5.0")], &[]);
    let lib_path = write_crate(tmp.path(), "lib", "1.5.0", &[], &[]);
    let all = vec![
        crate_cfg("app", &app_path, &["lib"]),
        crate_cfg("lib", &lib_path, &[]),
    ];
    let order = vec!["app".to_string()]; // lib not re-published this run
    let versions: HashMap<String, String> = [("app".to_string(), "2.0.0".to_string())]
        .into_iter()
        .collect();

    let seen: std::cell::RefCell<Vec<(String, String)>> = std::cell::RefCell::new(Vec::new());
    let probe = |n: &str, v: &str| {
        seen.borrow_mut().push((n.to_string(), v.to_string()));
        DepIndexState::Present
    };
    check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
        .expect("dep live on crates.io → ok");
    assert_eq!(
        *seen.borrow(),
        vec![("lib".to_string(), "1.5.0".to_string())],
        "guard probes the dep at the version the dependent pins"
    );
}

/// An inconclusive (Unknown) index probe never fails the guard — a
/// transient crates.io outage must not block a release.
#[test]
fn guard_ok_on_inconclusive_index_probe() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app_path = write_crate(tmp.path(), "app", "1.0.0", &[("lib", "1.0.0")], &[]);
    let lib_path = write_crate(tmp.path(), "lib", "1.0.0", &[], &[]);
    let all = vec![
        crate_cfg("app", &app_path, &["lib"]),
        crate_cfg("lib", &lib_path, &[]),
    ];
    let order = vec!["app".to_string()];
    let versions: HashMap<String, String> = [("app".to_string(), "1.0.0".to_string())]
        .into_iter()
        .collect();

    let probe = |_n: &str, _v: &str| DepIndexState::Unknown;
    check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
        .expect("inconclusive probe must not fail the guard");
}

/// A dev-dependency on an out-of-set, index-absent sibling must NOT trip
/// the guard: `cargo publish` strips dev-deps and does not require them on
/// the index. The probe must never be called (no non-dev edge exists).
#[test]
fn guard_ignores_dev_dependencies() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // `lib` is ONLY a dev-dependency of `app`.
    let app_path = write_crate(tmp.path(), "app", "1.0.0", &[], &[("lib", "1.0.0")]);
    let lib_path = write_crate(tmp.path(), "lib", "1.0.0", &[], &[]);
    let all = vec![
        crate_cfg("app", &app_path, &[]),
        crate_cfg("lib", &lib_path, &[]),
    ];
    let order = vec!["app".to_string()];
    let versions: HashMap<String, String> = [("app".to_string(), "1.0.0".to_string())]
        .into_iter()
        .collect();

    let probe = |_n: &str, _v: &str| panic!("dev-dep must not be probed");
    check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
        .expect("dev-dep on out-of-set sibling must not trip the guard");
}

/// The real 0.6.0/0.7.0 burn shape: a `<dep>.workspace = true` inherit.
/// The required version lives in the workspace root's
/// `[workspace.dependencies]`, not the leaf manifest — the guard must
/// resolve it and probe `lib@0.7.0`.
#[test]
fn guard_resolves_workspace_inherited_dep_version() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Workspace root with a `[workspace.dependencies]` pinning lib@0.7.0.
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"lib\"]\n\n\
             [workspace.dependencies]\nlib = { path = \"lib\", version = \"0.7.0\" }\n",
    )
    .expect("write workspace root");
    // app inherits lib via `lib.workspace = true` (no literal pin).
    let app_dir = tmp.path().join("app");
    std::fs::create_dir_all(&app_dir).expect("mkdir app");
    std::fs::write(
        app_dir.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.7.0\"\n\n\
             [dependencies]\nlib.workspace = true\n",
    )
    .expect("write app manifest");
    let lib_path = write_crate(tmp.path(), "lib", "0.7.0", &[], &[]);
    let all = vec![
        crate_cfg("app", &app_dir.display().to_string(), &["lib"]),
        crate_cfg("lib", &lib_path, &[]),
    ];
    let order = vec!["app".to_string()]; // lib missing from the set (the bug)
    let versions: HashMap<String, String> = [("app".to_string(), "0.7.0".to_string())]
        .into_iter()
        .collect();

    let seen: std::cell::RefCell<Vec<(String, String)>> = std::cell::RefCell::new(Vec::new());
    let probe = |n: &str, v: &str| {
        seen.borrow_mut().push((n.to_string(), v.to_string()));
        DepIndexState::Absent
    };
    let err = check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
        .expect_err("inherited dep missing from set + absent must fail");
    assert!(format!("{err:#}").contains("'lib'"), "names the dep");
    assert_eq!(
        *seen.borrow(),
        vec![("lib".to_string(), "0.7.0".to_string())],
        "inherited version resolved from the workspace root"
    );
}

/// A dep declared with `package = "real-name"` under an alias key must be
/// matched by its real package name, not the alias.
///
///   [dependencies]
///   core = { package = "anodizer-core", version = "0.8.0" }
///
/// Before the fix, the guard compared key `"core"` against
/// workspace_crate_names (which contains `"anodizer-core"`) — the match
/// failed and the dep was silently ignored, so a genuinely-absent
/// `anodizer-core` slipped through the guard.
#[test]
fn guard_resolves_package_renamed_dep() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Crate with a renamed dep: key is "core", real name is "anodizer-core".
    let app_dir = tmp.path().join("app");
    std::fs::create_dir_all(&app_dir).expect("mkdir app");
    std::fs::write(
        app_dir.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.8.0\"\n\n\
             [dependencies]\ncore = { package = \"anodizer-core\", version = \"0.8.0\" }\n",
    )
    .expect("write app manifest");

    let core_path = write_crate(tmp.path(), "anodizer-core", "0.8.0", &[], &[]);
    let all = vec![
        crate_cfg("app", &app_dir.display().to_string(), &[]),
        crate_cfg("anodizer-core", &core_path, &[]),
    ];
    let order = vec!["app".to_string()]; // anodizer-core NOT in publish set

    let versions: HashMap<String, String> = [("app".to_string(), "0.8.0".to_string())]
        .into_iter()
        .collect();

    let probe = |n: &str, _v: &str| {
        // anodizer-core is absent from the index, triggering the guard.
        if n == "anodizer-core" {
            DepIndexState::Absent
        } else {
            DepIndexState::Present
        }
    };
    let err = check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
        .expect_err("renamed dep absent from set and index must fail guard");
    assert!(
        format!("{err:#}").contains("anodizer-core"),
        "error must name the real package, not the alias: {err:#}"
    );
    assert!(
        format!("{err:#}").contains("declared as 'core' via package rename"),
        "error must surface the in-code alias: {err:#}"
    );
}

/// The alias key of a renamed dep must NOT be treated as a crate name.
/// With a workspace member literally named after the alias ("core") AND in
/// the publish set, matching the alias would satisfy the in-set check and
/// silently pass — even though the dep actually points at
/// "anodizer-core", which is absent from both the set and the index.
#[test]
fn guard_does_not_match_alias_key_as_crate_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app_dir = tmp.path().join("app");
    std::fs::create_dir_all(&app_dir).expect("mkdir app");
    std::fs::write(
        app_dir.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.8.0\"\n\n\
             [dependencies]\ncore = { package = \"anodizer-core\", version = \"0.8.0\" }\n",
    )
    .expect("write app manifest");

    // A workspace member that shares the alias's name, plus the real dep.
    let alias_twin_path = write_crate(tmp.path(), "core", "0.8.0", &[], &[]);
    let real_path = write_crate(tmp.path(), "anodizer-core", "0.8.0", &[], &[]);
    let all = vec![
        crate_cfg("app", &app_dir.display().to_string(), &[]),
        crate_cfg("core", &alias_twin_path, &[]),
        crate_cfg("anodizer-core", &real_path, &[]),
    ];
    // The alias-named member IS in the set; the real dep is NOT.
    let order = vec!["app".to_string(), "core".to_string()];
    let versions: HashMap<String, String> = [
        ("app".to_string(), "0.8.0".to_string()),
        ("core".to_string(), "0.8.0".to_string()),
    ]
    .into_iter()
    .collect();

    let probe = |n: &str, _v: &str| {
        if n == "anodizer-core" {
            DepIndexState::Absent
        } else {
            DepIndexState::Present
        }
    };
    let err = check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
        .expect_err("alias in set must not satisfy the check for the real package");
    assert!(
        format!("{err:#}").contains("anodizer-core"),
        "error must name the real package: {err:#}"
    );
}

/// A rename declared on the workspace root entry — the only place cargo
/// accepts `package =` for an inherited dep:
///
///   [workspace.dependencies]
///   core = { path = "core", version = "0.8.0", package = "anodizer-core" }
///
/// with the leaf inheriting via `core.workspace = true`. The leaf value
/// carries no `package` key, so the effective name must be resolved from
/// the root entry; matching the alias would silently skip the dep.
#[test]
fn guard_resolves_workspace_inherited_renamed_dep() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"core\"]\n\n\
             [workspace.dependencies]\n\
             core = { path = \"core\", version = \"0.8.0\", package = \"anodizer-core\" }\n",
    )
    .expect("write workspace root");
    let app_dir = tmp.path().join("app");
    std::fs::create_dir_all(&app_dir).expect("mkdir app");
    std::fs::write(
        app_dir.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.8.0\"\n\n\
             [dependencies]\ncore.workspace = true\n",
    )
    .expect("write app manifest");
    let core_dir = tmp.path().join("core");
    std::fs::create_dir_all(&core_dir).expect("mkdir core");
    std::fs::write(
        core_dir.join("Cargo.toml"),
        "[package]\nname = \"anodizer-core\"\nversion = \"0.8.0\"\n",
    )
    .expect("write core manifest");
    let all = vec![
        crate_cfg("app", &app_dir.display().to_string(), &[]),
        crate_cfg("anodizer-core", &core_dir.display().to_string(), &[]),
    ];
    let order = vec!["app".to_string()]; // anodizer-core NOT in publish set
    let versions: HashMap<String, String> = [("app".to_string(), "0.8.0".to_string())]
        .into_iter()
        .collect();

    let seen: std::cell::RefCell<Vec<(String, String)>> = std::cell::RefCell::new(Vec::new());
    let probe = |n: &str, v: &str| {
        seen.borrow_mut().push((n.to_string(), v.to_string()));
        DepIndexState::Absent
    };
    let err = check_publish_set_completeness(&order, &all, &versions, &probe, &quiet_log())
        .expect_err("inherited renamed dep absent from set and index must fail guard");
    assert!(
        format!("{err:#}").contains("anodizer-core"),
        "error must name the real package, not the alias: {err:#}"
    );
    assert!(
        format!("{err:#}").contains("declared as 'core' via package rename"),
        "error must surface the in-code alias: {err:#}"
    );
    assert_eq!(
        *seen.borrow(),
        vec![("anodizer-core".to_string(), "0.8.0".to_string())],
        "probe must target the real package at the root-pinned version"
    );
}
