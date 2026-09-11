//! The workspace's declared `rust-version` must be at least the `rust-version`
//! of every crate in the locked dependency graph. CI builds on `stable`, so a
//! dependency whose own floor moved above the declared one is only visible to
//! a consumer building on the declared toolchain, where `cargo check --locked`
//! refuses the graph. This test asks `cargo metadata` the same question on
//! every run.

use std::process::Command;

use semver::Version;

/// Parse the `MAJOR.MINOR[.PATCH]` a `rust-version` field carries.
fn rust_version(raw: &str) -> Version {
    let padded = match raw.matches('.').count() {
        0 => format!("{raw}.0.0"),
        1 => format!("{raw}.0"),
        _ => raw.to_string(),
    };
    Version::parse(&padded).unwrap_or_else(|e| panic!("rust-version {raw:?}: {e}"))
}

#[test]
fn declared_msrv_covers_every_locked_dependency() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(root)
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata --locked failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse cargo metadata");
    let packages = metadata["packages"].as_array().expect("packages array");
    let workspace_members = metadata["workspace_members"]
        .as_array()
        .expect("workspace_members array");

    let declared_raw = packages
        .iter()
        .find(|p| workspace_members.contains(&p["id"]))
        .and_then(|p| p["rust_version"].as_str())
        .expect("a workspace member inherits [workspace.package].rust-version");
    let declared = rust_version(declared_raw);

    let above: Vec<String> = packages
        .iter()
        .filter(|p| !workspace_members.contains(&p["id"]))
        .filter_map(|p| {
            p["rust_version"]
                .as_str()
                .map(|v| (p["name"].as_str().unwrap_or("?"), v))
        })
        .filter(|(_, v)| rust_version(v) > declared)
        .map(|(name, v)| format!("{name} needs {v}"))
        .collect();

    assert!(
        above.is_empty(),
        "[workspace.package].rust-version = \"{declared_raw}\" is below the floor the locked \
         dependency graph imposes; raise it to the highest of: {}",
        above.join(", ")
    );
}
