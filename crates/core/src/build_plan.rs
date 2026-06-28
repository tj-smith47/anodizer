//! Build-synthesis single source of truth: which build entries a crate
//! actually compiles, and over which target triples.
//!
//! Every target and toolchain enumeration MUST resolve through these helpers
//! rather than re-deriving the synthesis rule, so independent call sites cannot
//! drift on which crates build and what they produce. The build planner's
//! per-build compile gate is the reference behavior; [`build_produces`] mirrors
//! it and [`crate_target_list`] composes it with [`planned_builds`].

use std::path::Path;

use crate::config::{BuildConfig, BuilderKind, CrateConfig};

/// True when the crate at `crate_path` exposes a binary *target* named
/// `wanted` — i.e. `cargo build --bin <wanted>` would resolve. Mirrors
/// `crate_has_binary_target`'s filesystem-probe approach (no `cargo
/// metadata` spawn): an explicit `[[bin]] name = "<wanted>"`, the
/// package-named binary produced by `src/main.rs`, or an auto-discovered
/// `src/bin/<wanted>.rs`.
///
/// Distinct from `crate_has_binary_target`, which answers "does this crate
/// have ANY binary target". A library crate can carry helper binaries whose
/// names do not match the crate (e.g. `src/bin/gen.rs` renamed via `[[bin]]`
/// to `mylib-gen`); such a crate "has a binary target" yet has none named
/// after itself, so a synthesized default `--bin <crate>` build must be
/// suppressed rather than handed to cargo, which would hard-error with
/// `no bin target named '<crate>'` and fail the build/determinism legs.
///
/// Shares `crate_has_binary_target`'s documented `autobins = false`
/// limitation for the `src/bin/` probe. One further filesystem-probe blind
/// spot: a *nameless* `[[bin]]` with a custom `path` outside `src/bin/` (cargo
/// derives that target's name from the path stem) is not detected — covering
/// it would require a `cargo metadata` spawn. Such layouts are rare; declare a
/// `name` to be seen here.
pub fn crate_declares_bin(crate_path: &str, wanted: &str) -> bool {
    let path = Path::new(crate_path);
    let doc = std::fs::read_to_string(path.join("Cargo.toml"))
        .ok()
        .and_then(|c| c.parse::<toml_edit::DocumentMut>().ok());
    let bin_tables = doc
        .as_ref()
        .and_then(|d| d.get("bin"))
        .and_then(|b| b.as_array_of_tables());

    // 1. Explicit `[[bin]] name = "<wanted>"`.
    if let Some(arr) = bin_tables
        && arr
            .iter()
            .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(wanted))
    {
        return true;
    }

    // 2. `src/main.rs` yields a binary named after the package; it matches
    //    when the package name is `wanted` (the default binary name a
    //    synthesized build resolves to is the crate's own name).
    if path.join("src/main.rs").exists()
        && doc
            .as_ref()
            .and_then(|d| d.get("package"))
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str())
            == Some(wanted)
    {
        return true;
    }

    // 3. Auto-discovered `src/bin/<wanted>.rs` (cargo names the target after
    //    the file stem) — unless an explicit `[[bin]]` re-paths that file to a
    //    *different* name, which removes the stem-named target cargo would have
    //    auto-discovered. Without this guard a crate named after one of its own
    //    renamed helper files would falsely claim the target and re-trigger the
    //    doomed `--bin <wanted>`.
    let stem_file = format!("{wanted}.rs");
    if path.join("src/bin").join(&stem_file).exists() {
        let reclaimed_under_other_name = bin_tables.is_some_and(|arr| {
            arr.iter().any(|t| {
                t.get("name").and_then(|v| v.as_str()) != Some(wanted)
                    && t.get("path")
                        .and_then(|v| v.as_str())
                        .and_then(|p| Path::new(p).file_name()?.to_str().map(str::to_owned))
                        .as_deref()
                        == Some(stem_file.as_str())
            })
        });
        return !reclaimed_under_other_name;
    }
    false
}

/// The build entries the build planner will actually compile for a crate, or
/// `None` when the crate compiles nothing.
///
/// The single source of truth for the "what does this crate produce"
/// synthesis rule:
///
/// - a non-empty `builds:` list is used as-is;
/// - a crate with no `builds:` that declares a `--bin <crate>` target named
///   after itself gets a single synthesized default build (`binary = <crate>`,
///   targets inherited from `defaults.targets`);
/// - a crate with neither — a library, or one carrying only differently-named
///   helper bins — compiles nothing and yields `None`.
///
/// Target resolution (per-build `targets` overriding `defaults.targets`) is the
/// caller's concern; this answers only which build entries exist.
pub fn planned_builds(krate: &CrateConfig) -> Option<Vec<BuildConfig>> {
    match krate.builds.as_deref() {
        Some(b) if !b.is_empty() => Some(b.to_vec()),
        _ => crate_declares_bin(&krate.path, &krate.name).then(|| {
            vec![BuildConfig {
                binary: Some(krate.name.clone()),
                ..Default::default()
            }]
        }),
    }
}

/// Whether a build entry yields a shippable artifact (compiled binary or a
/// staged prebuilt). A `defaults.builds:` template materialized onto a library
/// crate carries `binary: None` and resolves no default `--bin <crate>`, so it
/// compiles nothing — the build planner skips it, and every target/toolchain
/// enumeration must skip it identically or it over-reports.
pub fn build_produces(krate: &CrateConfig, build: &BuildConfig) -> bool {
    matches!(build.builder, Some(BuilderKind::Prebuilt))
        || build.binary.is_some()
        || crate_declares_bin(&krate.path, &krate.name)
}

/// The de-duplicated, order-preserving list of target triples a crate's builds
/// will actually produce: planner synthesis ([`planned_builds`]) + the compile/
/// artifact gate ([`build_produces`]) + per-build `targets:` override of
/// `default_targets`. THE single source of truth for crate target enumeration.
pub fn crate_target_list(krate: &CrateConfig, default_targets: &[String]) -> Vec<String> {
    let Some(builds) = planned_builds(krate) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for build in &builds {
        if !build_produces(krate, build) {
            continue;
        }
        let chosen: &[String] = match build.targets.as_deref() {
            Some(ts) => ts,
            None => default_targets,
        };
        for t in chosen {
            if !out.contains(t) {
                out.push(t.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a minimal crate skeleton with the given Cargo.toml + optional
    /// `src/main.rs` so the filesystem probes have something to read.
    fn crate_dir(cargo_toml: &str, with_main: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), cargo_toml).unwrap();
        if with_main {
            std::fs::create_dir_all(dir.path().join("src")).unwrap();
            std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        }
        dir
    }

    fn krate_at(name: &str, path: &str, builds: Option<Vec<BuildConfig>>) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: path.to_string(),
            builds,
            ..Default::default()
        }
    }

    #[test]
    fn build_produces_false_for_binary_none_library_crate() {
        // Library crate (no src/main.rs, no [[bin]]) carrying a materialized
        // `binary: None` build — the planner skips it, so build_produces is false.
        let dir = crate_dir("[package]\nname = \"lib\"\nversion = \"0.0.0\"\n", false);
        let krate = krate_at("lib", dir.path().to_str().unwrap(), None);
        let build = BuildConfig::default();
        assert!(!build_produces(&krate, &build));
    }

    #[test]
    fn build_produces_true_for_prebuilt() {
        let dir = crate_dir("[package]\nname = \"lib\"\nversion = \"0.0.0\"\n", false);
        let krate = krate_at("lib", dir.path().to_str().unwrap(), None);
        let build = BuildConfig {
            builder: Some(BuilderKind::Prebuilt),
            ..Default::default()
        };
        assert!(build_produces(&krate, &build));
    }

    #[test]
    fn build_produces_true_for_explicit_binary() {
        let dir = crate_dir("[package]\nname = \"lib\"\nversion = \"0.0.0\"\n", false);
        let krate = krate_at("lib", dir.path().to_str().unwrap(), None);
        let build = BuildConfig {
            binary: Some("app".to_string()),
            ..Default::default()
        };
        assert!(build_produces(&krate, &build));
    }

    #[test]
    fn build_produces_true_for_declared_bin() {
        // src/main.rs + package name == crate name → declares a `--bin <crate>`.
        let dir = crate_dir("[package]\nname = \"app\"\nversion = \"0.0.0\"\n", true);
        let krate = krate_at("app", dir.path().to_str().unwrap(), None);
        let build = BuildConfig::default();
        assert!(build_produces(&krate, &build));
    }

    #[test]
    fn crate_target_list_empty_for_library_with_materialized_binary_none_build() {
        // A library crate that inherited a `defaults.builds` template carries a
        // build with `binary: None`; with no `--bin <crate>` target the gate
        // drops it, so the crate produces no targets.
        let dir = crate_dir("[package]\nname = \"lib\"\nversion = \"0.0.0\"\n", false);
        let krate = krate_at(
            "lib",
            dir.path().to_str().unwrap(),
            Some(vec![BuildConfig::default()]),
        );
        let defaults = vec!["x86_64-unknown-linux-gnu".to_string()];
        assert!(crate_target_list(&krate, &defaults).is_empty());
    }

    #[test]
    fn crate_target_list_uses_default_targets_for_declared_bin() {
        let dir = crate_dir("[package]\nname = \"app\"\nversion = \"0.0.0\"\n", true);
        let krate = krate_at("app", dir.path().to_str().unwrap(), None);
        let defaults = vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
        ];
        assert_eq!(crate_target_list(&krate, &defaults), defaults);
    }
}
