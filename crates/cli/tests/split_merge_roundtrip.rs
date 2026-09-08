//! `release --split` followed by `release --merge` in one `dist/` must
//! round-trip whatever target-selection flags the split leg ran with.
//!
//! The split leg names its shard directory after the resolved partial
//! target (`dist/linux/` for a host build under `partial.by: os`, but
//! `dist/<triple>/` when `--single-target` or `TARGET=` pins an exact
//! triple), while `matrix.json` is keyed on the configured `partial.by`
//! axis. The merge leg must identify each shard by the targets it built,
//! through the same key function that wrote the matrix, so the two flag
//! combinations reconcile identically.

mod common;

use std::path::Path;
use std::process::Command;

use common::{bootstrap_minimal_cargo_repo, host_triple, tool_on_path};

fn run_anodizer(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_anodizer"))
        .args(args)
        .current_dir(dir)
        .env_remove("TARGET")
        .env_remove("ANODIZER_OS")
        .env_remove("ANODIZER_ARCH")
        .env_remove("GGOOS")
        .env_remove("GGOARCH")
        .env_remove("COSIGN_KEY")
        .env_remove("GPG_PRIVATE_KEY")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("ANODIZER_GITHUB_TOKEN")
        .output()
        .expect("invoke anodizer")
}

/// Split with `split_flags`, then merge, asserting the shard landed under
/// `expected_subdir` and the merge loaded it.
fn split_then_merge(split_flags: &[&str], expected_subdir: &str) {
    if !tool_on_path("cargo") {
        eprintln!("skipping: cargo not on PATH");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    bootstrap_minimal_cargo_repo(tmp.path(), "rt");

    let mut args = vec!["release", "--snapshot", "--split"];
    args.extend_from_slice(split_flags);
    let split = run_anodizer(tmp.path(), &args);
    let split_err = String::from_utf8_lossy(&split.stderr);
    assert!(
        split.status.success(),
        "release --split {split_flags:?} failed:\n{split_err}"
    );
    let context = tmp
        .path()
        .join("dist")
        .join(expected_subdir)
        .join("context.json");
    assert!(
        context.is_file(),
        "split {split_flags:?} must write dist/{expected_subdir}/context.json:\n{split_err}"
    );
    assert!(tmp.path().join("dist/matrix.json").is_file());

    let merge = run_anodizer(tmp.path(), &["release", "--snapshot", "--merge"]);
    let merge_err = String::from_utf8_lossy(&merge.stderr);
    assert!(
        merge.status.success(),
        "release --merge after --split {split_flags:?} failed:\n{merge_err}"
    );
    assert!(
        merge_err.contains("loaded 1 artifact(s) from 1 context(s)"),
        "merge must load the shard written by --split {split_flags:?}:\n{merge_err}"
    );
    assert!(
        !merge_err.contains("split-worker manifest mismatch"),
        "{merge_err}"
    );
}

#[test]
fn split_then_merge_round_trips_a_host_shard() {
    // Under the default `partial.by: os` a host build shards by OS name.
    let (os, _) = anodizer_core::target::map_target(&host_triple());
    split_then_merge(&[], &os);
}

#[test]
fn split_then_merge_round_trips_a_single_target_shard() {
    let triple = host_triple();
    split_then_merge(&["--single-target"], &triple);
}
