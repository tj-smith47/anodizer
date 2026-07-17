//! Dogfood guard: anodizer's own `.anodizer.yaml` omits every per-crate
//! `depends_on` and relies on derivation from each crate's `Cargo.toml` to
//! order the cargo publish. This test proves that derivation yields a valid,
//! acyclic, dependency-first publish order for the real workspace — the exact
//! property whose absence (a hand-listed `depends_on` missing the
//! `anodizer -> anodizer-stage-install-script` edge) can time out the
//! workspace-dep wait gate and fail a release.

use std::path::PathBuf;

use anodizer_core::config::Config;
use anodizer_core::util::topological_sort;

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/cli; the workspace root is two levels up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve workspace root")
}

fn load_derived_config() -> (Config, PathBuf) {
    let root = repo_root();
    let yaml = std::fs::read_to_string(root.join(".anodizer.yaml")).expect("read .anodizer.yaml");
    let mut config: Config = serde_yaml_ng::from_str(&yaml).expect("parse .anodizer.yaml");
    // Derivation reads each crate's Cargo.toml relative to the base dir, so it
    // must be the workspace root (not the test's crates/cli working directory).
    config.populate_derived_depends_on(&root);
    (config, root)
}

#[test]
fn own_config_lists_no_explicit_depends_on() {
    let root = repo_root();
    let yaml = std::fs::read_to_string(root.join(".anodizer.yaml")).unwrap();
    let raw: Config = serde_yaml_ng::from_str(&yaml).unwrap();
    let hand_listed: Vec<&str> = raw
        .crates
        .iter()
        .filter(|c| c.depends_on.is_some())
        .map(|c| c.name.as_str())
        .collect();
    assert!(
        hand_listed.is_empty(),
        "`.anodizer.yaml` must not hand-list `depends_on` (derivation owns the \
         publish order; a hand list drifts and silently breaks it) — found on: {hand_listed:?}"
    );
}

#[test]
fn derived_depends_on_yields_valid_publish_order() {
    let (config, _root) = load_derived_config();

    let graph: Vec<(String, Vec<String>)> = config
        .crates
        .iter()
        .map(|c| (c.name.clone(), c.depends_on.clone().unwrap_or_default()))
        .collect();
    assert!(!graph.is_empty(), "config has no crates");

    let names: std::collections::HashSet<&str> = graph.iter().map(|(n, _)| n.as_str()).collect();

    let order = topological_sort(&graph);
    assert_eq!(
        order.len(),
        graph.len(),
        "topological_sort dropped or duplicated crates (cycle?): {order:?}"
    );

    let position: std::collections::HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();

    // Every intra-workspace dependency must publish before the crate that
    // needs it — the invariant the cargo publisher's topological_sort exists
    // to guarantee, checked here against anodizer's real derived graph.
    for (name, deps) in &graph {
        let here = position[name.as_str()];
        for dep in deps {
            if !names.contains(dep.as_str()) {
                continue; // external dep, not part of the workspace publish set
            }
            assert!(
                position[dep.as_str()] < here,
                "publish order violates dependency: '{dep}' must come before '{name}' \
                 (dep at {}, crate at {here})",
                position[dep.as_str()]
            );
        }
    }
}

#[test]
fn derivation_recovers_the_install_script_edge_the_hand_list_missed() {
    let (config, _root) = load_derived_config();

    let anodizer = config
        .crates
        .iter()
        .find(|c| c.name == "anodizer")
        .expect("`anodizer` CLI crate present");
    let deps = anodizer
        .depends_on
        .as_ref()
        .expect("derivation populated `anodizer` depends_on");
    assert!(
        deps.iter().any(|d| d == "anodizer-stage-install-script"),
        "derivation must find the `anodizer -> anodizer-stage-install-script` \
         edge (the v0.19.0 hand-list omission), got: {deps:?}"
    );
}
