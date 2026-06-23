//! Shared helpers for the `crates/cli/tests/*.rs` integration suite.
//!
//! Cargo's integration-test layout treats each `tests/<name>.rs` as a
//! standalone binary, so helper code must live in a subdirectory
//! (`tests/common/mod.rs`) and be re-declared via `mod common;` from
//! every test file that needs it. The harness fixture-bootstrap helpers
//! (cargo workspace synthesis, git init, host triple detection,
//! recursive file walks, file hashing) used to be duplicated across
//! `check_determinism.rs` and `preserve_dist.rs`; consolidating them
//! here keeps the bootstrap in one place so a future contributor
//! editing the fixture shape doesn't have to chase N copies.
//!
//! Convention: every test file that uses these helpers declares
//!
//! ```ignore
//! mod common;
//! use common::*;
//! ```
//!
//! at the top. Cargo silently builds `common/mod.rs` per consumer
//! binary; that's normal for the `tests/common` pattern, not a
//! duplication signal.
//!
//! `#![allow(dead_code)]` at module scope: cargo builds this module
//! once per integration-test binary, but each binary only consumes
//! a subset of the helpers. The unused-warning fires in the binary
//! whose import surface doesn't reach a given helper — silencing it
//! at the module level is the canonical `tests/common/mod.rs` idiom.

#![allow(dead_code)]

use std::fs;
use std::path::Path;
use std::process::Command;

/// Cheap "is this tool on PATH?" probe. Used by the harness integration
/// tests to skip cleanly on minimal hosts (containers without rustup
/// installed, the documentation-build job, etc.) instead of failing
/// the whole suite with a `cargo: command not found`.
pub fn tool_on_path(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run `git <args>` in `dir`, panicking with the captured stderr on
/// failure. Used for fixture-repo setup — synthesizing a minimal
/// cargo workspace with `git init` / `commit`-style boilerplate.
pub fn run_git(dir: &Path, args: &[&str]) {
    let out = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(dir).args(args);
            cmd
        },
        "git",
    );
    assert!(
        out.status.success(),
        "git {:?} failed: stdout={} stderr={}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Detect the host's target triple via `rustc -vV`. Used by the
/// fixture bootstrap to populate `.anodizer.yaml`'s `targets:` list
/// with the host triple — otherwise the build stage tries to link a
/// `x86_64-unknown-linux-gnu` binary on macOS / Windows hosts that
/// have no matching toolchain.
pub fn host_triple() -> String {
    let out = Command::new("rustc")
        .args(["-vV"])
        .output()
        .expect("rustc -vV must succeed (cargo is on PATH; rustc is sibling)");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return host.trim().to_string();
        }
    }
    panic!("no `host:` line in `rustc -vV` output:\n{}", stdout);
}

/// Bootstrap a minimal cargo workspace at `dir` with the given crate
/// name. The workspace builds a no-deps `hello-world` binary so the
/// harness can exercise the full build → archive → sbom → sign →
/// checksum pipeline without the cargo registry / network. Init as
/// git repo + initial commit so `head_commit_*` resolvers work.
///
/// Caller-supplied `crate_name` keeps two test files using disjoint
/// fixture names (avoids cargo's per-workspace lock contention when
/// the tests share `TempDir` semantics).
pub fn bootstrap_minimal_cargo_repo(dir: &Path, crate_name: &str) {
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "{crate_name}"
path = "src/main.rs"
"#,
        ),
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();

    let host = host_triple();
    let yaml = format!(
        r#"crates:
  - name: {crate_name}
    path: .
    builds:
      - id: {crate_name}
        binary: {crate_name}
        targets:
          - {host}
"#,
    );
    fs::write(dir.join(".anodizer.yaml"), yaml).unwrap();

    run_git(dir, &["init", "-q", "-b", "master"]);
    run_git(dir, &["config", "user.email", "test@test.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    run_git(dir, &["config", "commit.gpgsign", "false"]);
    run_git(dir, &["add", "-A"]);
    run_git(dir, &["commit", "-q", "-m", "init"]);
}

/// Walk `<dir>` recursively and return a sorted list of `(relpath, abspath)`
/// for every regular file. Sorted by relpath so iteration order is
/// stable for assertion error messages.
///
/// `relpath` uses forward slashes regardless of platform — matches the
/// path normalization the harness applies in `context.json`.
pub fn walk_files(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    fn inner(root: &Path, dir: &Path, out: &mut Vec<(String, std::path::PathBuf)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                inner(root, &path, out);
            } else if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, path));
            }
        }
    }
    let mut out = Vec::new();
    inner(dir, dir, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// SHA-256 a file and return the `sha256:<hex>` shape the harness uses
/// for `ArtifactRow::hash` and `PreservedArtifact::sha256`. Reads the
/// whole file into memory — fine for the small fixtures the tests
/// exercise (no production artifact exceeds a few MB).
pub fn sha256_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("sha256:{:x}", hasher.finalize())
}
