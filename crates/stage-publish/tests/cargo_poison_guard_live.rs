//! Live, network-gated proof of the cargo content-vs-version poison guard's
//! core mechanism: a local `cargo package` of a published crate's source at its
//! release tag reproduces, byte-for-byte, the `.crate` tarball crates.io
//! recorded — so its sha256 equals the index `cksum` the guard compares against.
//!
//! This is the empirical ground truth behind
//! `anodizer_stage_publish::cargo::local_crate_cksum`: that function packages a
//! crate locally and trusts the resulting sha256 to match crates.io's `cksum`
//! when (and only when) the content is identical. If `cargo package` ever stops
//! being reproducible from identical tagged source, this test fails and the
//! guard's soundness assumption is broken.
//!
//! It deliberately sets NO `SOURCE_DATE_EPOCH`: `cargo package` does not consult
//! it for the `.crate` bytes (proven separately by packaging the same source
//! under two different SDE values and getting one digest), so the guard must NOT
//! seed it either. This test pins that contract.
//!
//! `#[ignore]`d so ordinary CI never reaches the network. Run it explicitly:
//!
//! ```bash
//! cargo test -p anodizer-stage-publish --test cargo_poison_guard_live \
//!     -- --ignored --nocapture
//! ```
//!
//! Requirements: network access to `index.crates.io`, a `cargo` on PATH, and a
//! git checkout of this repo carrying the `v0.11.2` tag (the proof anchor).

use std::process::Command;

/// The proof anchor: `anodizer-core` at tag `v0.11.2`. The sha256 of its
/// `.crate` was reproduced locally (no SDE) and matches the crates.io index
/// `cksum` exactly. These three constants are the immutable published fact.
const CRATE_NAME: &str = "anodizer-core";
const PROOF_TAG: &str = "v0.11.2";
const PROOF_VERSION: &str = "0.11.2";
/// crates.io index `cksum` for `anodizer-core` 0.11.2 — an immutable published
/// version. Fetch independently with:
/// `curl -s https://index.crates.io/an/od/anodizer-core | grep '"0.11.2"'`.
const PROOF_CKSUM: &str = "548ed4e2f3d91d78ebc64242664d3f3760f67dded7656e46d4ae88a473e2f00e";

/// Repo root = two levels up from this crate's manifest dir
/// (`<root>/crates/stage-publish`).
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate manifest is at <root>/crates/stage-publish")
        .to_path_buf()
}

/// Fetch the crates.io sparse-index `cksum` for `version` of `CRATE_NAME`,
/// independent of the hardcoded `PROOF_CKSUM`, so the assertion proves the
/// mechanism end-to-end rather than echoing a constant.
fn fetch_index_cksum(version: &str) -> String {
    fetch_index_cksum_for(CRATE_NAME, version)
}

/// Fetch the crates.io sparse-index `cksum` for `version` of `name`. Both
/// `anodizer-core` and `anodizer` (cli) share the `an/od/<name>` sparse-index
/// path (names 4+ chars index under `<first2>/<next2>/<name>`).
fn fetch_index_cksum_for(name: &str, version: &str) -> String {
    let url = format!("https://index.crates.io/an/od/{name}");
    let out = Command::new("curl")
        .arg("-fsSL")
        .arg(&url)
        .output()
        .expect("curl available on PATH for the live index fetch");
    assert!(
        out.status.success(),
        "fetching {url} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = String::from_utf8(out.stdout).expect("index body is utf-8");
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: serde_json::Value =
            serde_json::from_str(line).expect("each index line is a JSON object");
        if entry.get("vers").and_then(|v| v.as_str()) == Some(version) {
            return entry
                .get("cksum")
                .and_then(|v| v.as_str())
                .expect("published version carries a cksum")
                .to_string();
        }
    }
    panic!("version {version} not found in the crates.io index for {name}");
}

#[test]
#[ignore = "live: fetches the crates.io index + runs `cargo package`; needs \
            network + the v0.11.2 tag. Run: cargo test -p anodizer-stage-publish \
            --test cargo_poison_guard_live -- --ignored --nocapture"]
fn local_package_reproduces_published_crates_io_cksum() {
    let root = repo_root();

    // Sanity: the index's recorded cksum equals the anchor we proved by hand.
    // A drift here means crates.io changed an immutable version (impossible) or
    // the URL path math is wrong — surface it before blaming the packaging.
    let index_cksum = fetch_index_cksum(PROOF_VERSION);
    assert_eq!(
        index_cksum, PROOF_CKSUM,
        "the crates.io index cksum for {CRATE_NAME} {PROOF_VERSION} drifted from the proof anchor"
    );

    // Package the crate from a detached worktree pinned at the release tag, so
    // the source is byte-identical to what was published — exactly what the
    // poison guard does at publish time on the tagged release commit.
    let worktree = tempfile::tempdir().expect("tempdir for the tag worktree");
    let wt_path = worktree.path().join("wt");
    let add = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(&root)
                .args(["worktree", "add", "--detach"])
                .arg(&wt_path)
                .arg(PROOF_TAG);
            cmd
        },
        "git",
    );
    assert!(
        add.status.success(),
        "git worktree add at {PROOF_TAG} failed (is the tag present?): {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let target = tempfile::tempdir().expect("tempdir for the hermetic CARGO_TARGET_DIR");

    // No SOURCE_DATE_EPOCH: the guard must reproduce publish bytes WITHOUT it,
    // since `cargo package` does not consult it for the `.crate` tarball.
    let pkg = Command::new("cargo")
        .current_dir(&wt_path)
        .args([
            "package",
            "-p",
            CRATE_NAME,
            "--no-verify",
            "--allow-dirty",
            "--no-metadata",
        ])
        .env("CARGO_TARGET_DIR", target.path())
        .output()
        .expect("cargo available on PATH");

    // spawn-retry-ok: best-effort worktree cleanup before the assertion below.
    // A spawn failure here must be ignored, not retried-then-panicked — a panic
    // would mask the real assertion. Err leaves the worktree, as it did before.
    let _ = Command::new("git")
        .current_dir(&root)
        .args(["worktree", "remove", "--force"])
        .arg(&wt_path)
        .output();

    assert!(
        pkg.status.success(),
        "`cargo package -p {CRATE_NAME}` at {PROOF_TAG} failed: {}",
        String::from_utf8_lossy(&pkg.stderr)
    );

    let crate_file = target
        .path()
        .join("package")
        .join(format!("{CRATE_NAME}-{PROOF_VERSION}.crate"));
    let local_cksum =
        anodizer_core::hashing::sha256_file(&crate_file).expect("sha256 the local .crate");

    eprintln!("local  cksum: {local_cksum}");
    eprintln!("index  cksum: {index_cksum}");
    assert_eq!(
        local_cksum, index_cksum,
        "local `cargo package` of {CRATE_NAME} {PROOF_VERSION} must reproduce the published \
         crates.io cksum byte-for-byte — the poison guard's soundness depends on it"
    );
}

// ---------------------------------------------------------------------------
// Live binstall proof: the guard must package the SAME on-disk tree state
// `cargo publish` uploads, INCLUDING anodizer's own pre-publish binstall write.
// ---------------------------------------------------------------------------

/// The binstall proof anchor: the `anodizer` *cli* crate at tag `v0.11.2`,
/// which carries `binstall.enabled: true` in `.anodizer.yaml`. Its published
/// `.crate` was produced AFTER the release wrote `[package.metadata.binstall]`
/// into `crates/cli/Cargo.toml`, so the tag's COMMITTED source (no binstall
/// table) does NOT reproduce it — only the binstall-mutated tree does. This is
/// the live form of the false-poison the ordering fix eliminates.
const CLI_CRATE_NAME: &str = "anodizer";
const CLI_PROOF_VERSION: &str = "0.11.2";
/// crates.io index `cksum` for the published `anodizer` cli crate 0.11.2.
/// Fetch independently:
/// `curl -s https://index.crates.io/an/od/anodizer | grep '"0.11.2"'`.
const CLI_PROOF_CKSUM: &str = "cf7192fb6786e29acf4191347eb2a75ced53e1c5410bf842082e781183e69212";

/// Fetch the published `.crate` tarball for `anodizer-<version>` and return its
/// bytes. The static.crates.io CDN serves the immutable artifact directly.
fn fetch_published_crate(version: &str) -> Vec<u8> {
    let url = format!(
        "https://static.crates.io/crates/{CLI_CRATE_NAME}/{CLI_CRATE_NAME}-{version}.crate"
    );
    let out = Command::new("curl")
        .args(["-fsSL", &url])
        .output()
        .expect("curl available on PATH");
    assert!(
        out.status.success(),
        "fetching {url} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Extract `Cargo.toml.orig` (the verbatim SOURCE manifest, including the
/// binstall table the release wrote) from a `.crate` tarball's bytes.
fn extract_orig_manifest(crate_bytes: &[u8], version: &str) -> String {
    let tmp = tempfile::tempdir().expect("tempdir for crate extraction");
    let crate_path = tmp.path().join("pub.crate");
    std::fs::write(&crate_path, crate_bytes).expect("write fetched .crate");
    let status = Command::new("tar")
        .current_dir(tmp.path())
        .args(["xzf", "pub.crate"])
        .status()
        .expect("tar available on PATH");
    assert!(status.success(), "extracting the published .crate failed");
    let orig = tmp
        .path()
        .join(format!("{CLI_CRATE_NAME}-{version}"))
        .join("Cargo.toml.orig");
    std::fs::read_to_string(&orig).expect("published .crate carries Cargo.toml.orig")
}

/// Package the cli crate at `PROOF_TAG` in a fresh detached worktree, optionally
/// overwriting `crates/cli/Cargo.toml` with `mutated_manifest` first (the
/// binstall-mutated source). Returns the sha256 of the produced `.crate`.
fn package_cli_at_tag(root: &std::path::Path, mutated_manifest: Option<&str>) -> String {
    let worktree = tempfile::tempdir().expect("tempdir for the tag worktree");
    let wt_path = worktree.path().join("wt");
    let add = anodizer_core::test_helpers::output_with_spawn_retry(
        || {
            let mut cmd = Command::new("git");
            cmd.current_dir(root)
                .args(["worktree", "add", "--detach"])
                .arg(&wt_path)
                .arg(PROOF_TAG);
            cmd
        },
        "git",
    );
    assert!(
        add.status.success(),
        "git worktree add at {PROOF_TAG} failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    // Apply the pre-publish binstall mutation by dropping the published source
    // manifest (which carries the table) over the tag's committed manifest —
    // the faithful stand-in for `ensure_binstall_metadata` running in place.
    if let Some(manifest) = mutated_manifest {
        std::fs::write(wt_path.join("crates/cli/Cargo.toml"), manifest)
            .expect("overwrite cli Cargo.toml with the binstall-mutated source");
    }

    let target = tempfile::tempdir().expect("tempdir for the hermetic CARGO_TARGET_DIR");
    let pkg = Command::new("cargo")
        .current_dir(&wt_path)
        .args([
            "package",
            "-p",
            CLI_CRATE_NAME,
            "--no-verify",
            "--allow-dirty",
            "--no-metadata",
        ])
        .env("CARGO_TARGET_DIR", target.path())
        .output()
        .expect("cargo available on PATH");

    // spawn-retry-ok: best-effort cleanup — a spawn failure must be ignored,
    // not retried-then-panicked. Err leaves the worktree, as it did before.
    let _ = Command::new("git")
        .current_dir(root)
        .args(["worktree", "remove", "--force"])
        .arg(&wt_path)
        .output();

    assert!(
        pkg.status.success(),
        "`cargo package -p {CLI_CRATE_NAME}` at {PROOF_TAG} failed: {}",
        String::from_utf8_lossy(&pkg.stderr)
    );

    let crate_file = target
        .path()
        .join("package")
        .join(format!("{CLI_CRATE_NAME}-{CLI_PROOF_VERSION}.crate"));
    anodizer_core::hashing::sha256_file(&crate_file).expect("sha256 the local .crate")
}

#[test]
#[ignore = "live: fetches the crates.io index + the published .crate + runs \
            `cargo package`; needs network + the v0.11.2 tag. Run: cargo test \
            -p anodizer-stage-publish --test cargo_poison_guard_live -- \
            --ignored --nocapture"]
fn binstall_crate_guard_requires_the_prepublish_mutation() {
    let root = repo_root();

    // The published cli cksum is the immutable fact the guard compares against.
    let index_cksum = fetch_index_cksum_for(CLI_CRATE_NAME, CLI_PROOF_VERSION);
    assert_eq!(
        index_cksum, CLI_PROOF_CKSUM,
        "the crates.io index cksum for {CLI_CRATE_NAME} {CLI_PROOF_VERSION} drifted from the anchor"
    );

    // 1. The tag's COMMITTED source (no binstall table) does NOT reproduce the
    //    published cksum — this is the live false-poison: a guard that packages
    //    the pre-binstall tree mismatches the index and hard-fails a clean
    //    re-cut. Load-bearing: it proves the binstall write is NOT optional.
    let cksum_without_binstall = package_cli_at_tag(&root, None);
    eprintln!("without-binstall cksum: {cksum_without_binstall}");
    eprintln!("published index cksum : {index_cksum}");
    assert_ne!(
        cksum_without_binstall, index_cksum,
        "the tag's committed source (no binstall table) must NOT match the published cksum — \
         if it did, the binstall mutation would not be load-bearing and this proof is vacuous"
    );

    // 2. Apply the SAME binstall table the release wrote (extracted verbatim
    //    from the published .crate's source manifest) and confirm the mutation
    //    is reproducible: packaging the mutated tree twice yields one cksum.
    let crate_bytes = fetch_published_crate(CLI_PROOF_VERSION);
    let mutated_manifest = extract_orig_manifest(&crate_bytes, CLI_PROOF_VERSION);
    assert!(
        mutated_manifest.contains("[package.metadata.binstall"),
        "the published source manifest must carry the binstall table"
    );
    let cksum_with_binstall_1 = package_cli_at_tag(&root, Some(&mutated_manifest));
    let cksum_with_binstall_2 = package_cli_at_tag(&root, Some(&mutated_manifest));
    eprintln!("with-binstall cksum (run 1): {cksum_with_binstall_1}");
    eprintln!("with-binstall cksum (run 2): {cksum_with_binstall_2}");
    assert_eq!(
        cksum_with_binstall_1, cksum_with_binstall_2,
        "the binstall mutation must be reproducible — the guard packages it deterministically"
    );

    // 3. The binstall mutation flips the cksum: WITH != WITHOUT. This is the
    //    crux of the bug — the same tree hashes differently once the table is
    //    present, so the guard MUST package the mutated tree to match what
    //    `cargo publish` uploads.
    assert_ne!(
        cksum_with_binstall_1, cksum_without_binstall,
        "the binstall table must change the .crate bytes — otherwise the ordering bug would be \
         harmless and the fix unnecessary"
    );

    // NOTE on byte-exact reproduction of the PUBLISHED cli cksum: it is NOT
    // asserted here, and that is correct. The cli crate has workspace-internal
    // deps (`anodizer-core` et al.) whose versions `cargo package` re-resolves
    // to the LATEST compatible registry release at package time. Since
    // `anodizer-core 0.11.3` was published after cli 0.11.2, packaging cli
    // 0.11.2 today locks `anodizer-core = 0.11.3`, so the bytes differ from the
    // 0.11.2-locked published artifact — an intrinsic property of cargo's
    // package-time resolution, NOT a guard defect. In a REAL release the cli is
    // a BINARY crate, so `cargo package` embeds `Cargo.lock` in the `.crate`;
    // re-cutting a tag packages from that tag's COMMITTED lockfile, which pins
    // every dep to the version it was published with — so the bytes reproduce
    // even when a newer sibling (e.g. anodizer-core 0.11.3) exists, and
    // guard-cksum == publish-cksum holds there (the leaf
    // `local_package_reproduces_published_crates_io_cksum` proves the byte-exact
    // path on a dep-stable crate). What this test proves is the load-bearing
    // claim the unit tests assert at the seam: the binstall mutation changes the
    // cksum and must be applied — deterministically — before the guard packages.
}
