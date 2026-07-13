//! Unit and integration tests for the cargo publisher.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

use super::*;

/// Literal `version = "X.Y.Z"` in [package] is read verbatim.
#[test]
fn read_cargo_toml_version_literal_in_package() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"foo\"\nversion = \"1.2.3\"\n",
    )
    .unwrap();
    assert_eq!(
        read_cargo_toml_version(dir.path().to_str().unwrap()),
        Some("1.2.3".into())
    );
}

/// `version.workspace = true` resolves via the workspace root's
/// `[workspace.package].version`. Without this resolution the
/// publish path falls back to the release-context version, which
/// is wrong for any multi-cadence workspace.
#[test]
fn read_cargo_toml_version_workspace_dot_form() {
    let ws_root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        ws_root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/leaf\"]\n\n[workspace.package]\nversion = \"4.5.6\"\n",
    )
    .unwrap();
    let leaf = ws_root.path().join("crates").join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::write(
        leaf.join("Cargo.toml"),
        "[package]\nname = \"leaf\"\nversion.workspace = true\n",
    )
    .unwrap();
    assert_eq!(
        read_cargo_toml_version(leaf.to_str().unwrap()),
        Some("4.5.6".into())
    );
}

/// `version = { workspace = true }` (inline-table form) resolves
/// the same way as the dotted form.
#[test]
fn read_cargo_toml_version_workspace_inline_table_form() {
    let ws_root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        ws_root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"leaf\"]\n[workspace.package]\nversion = \"0.9.0\"\n",
    )
    .unwrap();
    let leaf = ws_root.path().join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::write(
        leaf.join("Cargo.toml"),
        "[package]\nname = \"leaf\"\nversion = { workspace = true }\n",
    )
    .unwrap();
    assert_eq!(
        read_cargo_toml_version(leaf.to_str().unwrap()),
        Some("0.9.0".into())
    );
}

/// No version anywhere yields None (publish path falls back to the
/// release-context version, preserving prior behavior for
/// version-less manifests).
#[test]
fn read_cargo_toml_version_returns_none_when_absent() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    assert_eq!(read_cargo_toml_version(dir.path().to_str().unwrap()), None);
}

#[test]
fn test_topo_sort_simple() {
    let order = vec![
        ("cfgd-core".to_string(), vec![]),
        ("cfgd".to_string(), vec!["cfgd-core".to_string()]),
    ];
    let sorted = topological_sort(&order);
    assert_eq!(sorted, vec!["cfgd-core", "cfgd"]);
}

#[test]
fn test_topo_sort_no_deps() {
    let order = vec![("a".to_string(), vec![]), ("b".to_string(), vec![])];
    let sorted = topological_sort(&order);
    assert_eq!(sorted.len(), 2);
}

#[test]
fn test_publish_command_default() {
    // No config block — historical behaviour preserved (--allow-dirty on).
    let cmd = publish_command("my-crate", None);
    assert_eq!(
        cmd,
        vec![
            "cargo".to_string(),
            "publish".to_string(),
            "-p".to_string(),
            "my-crate".to_string(),
            "--allow-dirty".to_string(),
        ]
    );
}

#[test]
fn test_publish_command_full_flag_surface() {
    let cfg = CargoPublishConfig {
        registry: Some("alt-registry".to_string()),
        index: Some("https://example.com/idx".to_string()),
        no_verify: Some(true),
        allow_dirty: Some(true),
        features: Some(vec!["a".to_string(), "b".to_string()]),
        all_features: Some(true),
        no_default_features: Some(true),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        target_dir: Some(std::path::PathBuf::from("/tmp/td")),
        jobs: Some(4),
        keep_going: Some(true),
        manifest_path: Some(std::path::PathBuf::from("./Cargo.toml")),
        locked: Some(true),
        offline: Some(true),
        frozen: Some(true),
        ..Default::default()
    };
    let cmd = publish_command("my-crate", Some(&cfg));

    // Helper: assert the flag is present and (for value-bearing flags)
    // the immediately-next argv slot holds the expected value. Catches
    // bugs where two adjacent flag/value pairs swap.
    let assert_value = |flag: &str, expected: &str| {
        let pos = cmd
            .iter()
            .position(|s| s == flag)
            .unwrap_or_else(|| panic!("missing flag {flag}: {cmd:?}"));
        assert_eq!(
            cmd[pos + 1],
            expected,
            "{flag} value mismatch (full cmd: {cmd:?})"
        );
    };
    let assert_present = |flag: &str| {
        assert!(
            cmd.iter().any(|s| s == flag),
            "missing flag {flag}: {cmd:?}"
        );
    };

    // Value-bearing flags — assert flag + adjacent value at pos+1.
    assert_value("--registry", "alt-registry");
    assert_value("--index", "https://example.com/idx");
    assert_value("--features", "a,b"); // features are comma-joined
    assert_value("--target", "x86_64-unknown-linux-gnu");
    assert_value("--target-dir", "/tmp/td");
    assert_value("--jobs", "4");
    assert_value("--manifest-path", "./Cargo.toml");

    // Boolean flags — only need to assert presence (no following value).
    for flag in [
        "--no-verify",
        "--allow-dirty",
        "--all-features",
        "--no-default-features",
        "--keep-going",
        "--locked",
        "--offline",
        "--frozen",
    ] {
        assert_present(flag);
    }
}

#[test]
fn test_publish_command_allow_dirty_explicit_false() {
    let cfg = CargoPublishConfig {
        allow_dirty: Some(false),
        ..Default::default()
    };
    let cmd = publish_command("my-crate", Some(&cfg));
    assert!(
        !cmd.iter().any(|s| s == "--allow-dirty"),
        "explicit allow_dirty=false should suppress the flag: {cmd:?}"
    );
}

fn crate_with_deps(name: &str, deps: &[&str]) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

#[test]
fn test_expand_transitive_deps_includes_direct_dep() {
    // --crate cfgd should expand to [cfgd, cfgd-core] so cfgd-core
    // gets published before cfgd tries to reference it on crates.io.
    let crates = vec![
        crate_with_deps("cfgd-core", &[]),
        crate_with_deps("cfgd", &["cfgd-core"]),
    ];
    let selection = vec!["cfgd".to_string()];
    let expanded = expand_with_transitive_deps(&crates, &selection);
    assert!(expanded.contains(&"cfgd".to_string()));
    assert!(expanded.contains(&"cfgd-core".to_string()));
    assert_eq!(expanded.len(), 2);
}

#[test]
fn test_expand_transitive_deps_chains_through_multiple_levels() {
    let crates = vec![
        crate_with_deps("a", &[]),
        crate_with_deps("b", &["a"]),
        crate_with_deps("c", &["b"]),
    ];
    let expanded = expand_with_transitive_deps(&crates, &["c".to_string()]);
    assert!(expanded.contains(&"a".to_string()));
    assert!(expanded.contains(&"b".to_string()));
    assert!(expanded.contains(&"c".to_string()));
}

#[test]
fn test_expand_transitive_deps_dedupes_shared_ancestors() {
    // diamond: d depends on both b and c, which both depend on a.
    let crates = vec![
        crate_with_deps("a", &[]),
        crate_with_deps("b", &["a"]),
        crate_with_deps("c", &["a"]),
        crate_with_deps("d", &["b", "c"]),
    ];
    let expanded = expand_with_transitive_deps(&crates, &["d".to_string()]);
    assert_eq!(
        expanded.len(),
        4,
        "expected all 4 crates once: {:?}",
        expanded
    );
}

#[test]
fn test_expand_transitive_deps_ignores_external_deps() {
    // Deps on names not present in the config (i.e. external crates.io
    // crates) are silently dropped — cargo verifies them against the
    // real registry, not our workspace.
    let crates = vec![crate_with_deps("cfgd", &["cfgd-core", "serde"])];
    let expanded = expand_with_transitive_deps(&crates, &["cfgd".to_string()]);
    assert!(expanded.contains(&"cfgd".to_string()));
    // cfgd-core isn't in the config, so it won't appear
    assert!(!expanded.contains(&"cfgd-core".to_string()));
    assert!(!expanded.contains(&"serde".to_string()));
}

// -----------------------------------------------------------------------
// crates.io idempotency (C-new-11 / C-new-13)
//
// The hash-match short-circuit in publish_to_cargo (cf. cargo.rs
// ~line 489) avoids redundant `cargo publish` calls — and the bogus
// 422-with-stale-bytes problem they create — when the version already
// exists on crates.io and the local .crate cksum matches the index. The
// tests below pin (a) the sparse-index URL shape so we hit the same
// path cargo itself uses, and (b) the JSONL parser so we keep treating
// "version present, no cksum" as a fall-back-to-skip rather than a
// silently-missed publish.
// -----------------------------------------------------------------------

/// Sparse-index URL must follow the cargo registry layout:
/// 1-char names live under `/1/<name>`, 2-char under `/2/<name>`,
/// 3-char under `/3/<first>/<name>`, 4+ under `/<first2>/<next2>/<name>`.
/// Mismatch here means we'd query a URL that always 404s and silently
/// re-publish every release.
#[test]
fn test_sparse_index_url_shape() {
    // 1-char crate name.
    assert_eq!(sparse_index_url("a"), "https://index.crates.io/1/a");
    // 2-char.
    assert_eq!(sparse_index_url("ab"), "https://index.crates.io/2/ab");
    // 3-char — `/3/<first>/<name>`.
    assert_eq!(sparse_index_url("abc"), "https://index.crates.io/3/a/abc");
    // 4-char — `/<first2>/<next2>/<name>`.
    assert_eq!(
        sparse_index_url("abcd"),
        "https://index.crates.io/ab/cd/abcd"
    );
    // Real-world case (5+ char): `cfgd-core`.
    assert_eq!(
        sparse_index_url("cfgd-core"),
        "https://index.crates.io/cf/gd/cfgd-core"
    );
    // Uppercase normalises to lowercase per cargo registry spec.
    assert_eq!(
        sparse_index_url("MyTool"),
        "https://index.crates.io/my/to/mytool"
    );
}

/// Parser returns the cksum only when a line matches the requested
/// version; mismatched-version lines and absent fields short-circuit
/// to None/empty respectively.
#[test]
fn test_parse_index_cksum_for_version_matches_requested_version() {
    // Two versions on the index; only 1.2.3's cksum should come back.
    let body = r#"{"name":"foo","vers":"1.2.2","cksum":"old","yanked":false}
{"name":"foo","vers":"1.2.3","cksum":"newhash","yanked":false}
{"name":"foo","vers":"1.2.4","cksum":"newer","yanked":false}"#;
    assert_eq!(
        parse_index_cksum_for_version(body, "1.2.3"),
        Some("newhash".to_string())
    );
}

#[test]
fn test_parse_index_cksum_for_version_returns_none_when_absent() {
    // Index has 1.2.2 but caller asked for 1.2.3 — must return None so
    // publish_to_cargo proceeds with the publish.
    let body = r#"{"name":"foo","vers":"1.2.2","cksum":"old","yanked":false}"#;
    assert_eq!(parse_index_cksum_for_version(body, "1.2.3"), None);
}

#[test]
fn test_parse_index_cksum_for_version_empty_string_when_cksum_missing() {
    // Index entry has the requested version but no `cksum` field
    // (malformed/legacy entry). Returning Some("") signals "present but
    // drift undetectable" so the caller falls back to the historical
    // skip behaviour rather than mis-treating it as "not published".
    let body = r#"{"name":"foo","vers":"1.2.3","yanked":false}"#;
    assert_eq!(
        parse_index_cksum_for_version(body, "1.2.3"),
        Some(String::new())
    );
}

#[test]
fn test_parse_index_cksum_for_version_empty_body() {
    // Defensive: an empty/whitespace body parses to None (the function
    // is invoked after a 200-OK status but before further validation,
    // so we mustn't panic on malformed bodies).
    assert_eq!(parse_index_cksum_for_version("", "1.0.0"), None);
    assert_eq!(parse_index_cksum_for_version("   \n  ", "1.0.0"), None);
}

#[test]
fn test_parse_index_cksum_for_version_skips_garbage_lines() {
    // A non-JSON line in the middle must not abort the scan — cargo's
    // own client tolerates trailing newlines and similar.
    let body = "not-json\n{\"name\":\"foo\",\"vers\":\"1.2.3\",\"cksum\":\"abcd\"}\n";
    assert_eq!(
        parse_index_cksum_for_version(body, "1.2.3"),
        Some("abcd".to_string())
    );
}

// ---- content-vs-version guard decision unit tests --------------------

/// Build an in-memory `.crate` tarball (a gzip-compressed tar) with the
/// given `(in-tar path, content)` entries — for `crates_equal_modulo_vcs`
/// and `decide_already_published` fixtures that need real archive bytes
/// rather than opaque cksum labels.
fn make_crate_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;

    let mut builder = tar::Builder::new(Vec::new());
    for (path, content) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, *content)
            .expect("append tar entry");
    }
    let tar_bytes = builder.into_inner().expect("finish tar");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar_bytes).expect("gzip write");
    gz.finish().expect("gzip finish")
}

/// Like [`make_crate_tarball`] but writes each entry's path directly into
/// the header's raw name bytes instead of going through
/// `tar::Builder::append_data`. `append_data` normalizes the path via
/// `tar`'s `copy_path_into_inner`, which deliberately drops a leading
/// `./` (`Component::CurDir`) — so it can't produce the `./`-prefixed
/// root entry the leading-CurDir hardening test below needs to prove
/// against. `tar::Builder::append` (unlike `append_data`) writes the
/// header as-is with no path processing.
fn make_crate_tarball_raw_paths(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;

    let mut builder = tar::Builder::new(Vec::new());
    for (path, content) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        let path_bytes = path.as_bytes();
        let name_slot = &mut header.as_old_mut().name;
        assert!(
            path_bytes.len() < name_slot.len(),
            "raw path fixture '{path}' too long for the tar header name field"
        );
        name_slot[..path_bytes.len()].copy_from_slice(path_bytes);
        header.set_cksum();
        builder
            .append(&header, *content)
            .expect("append raw tar entry");
    }
    let tar_bytes = builder.into_inner().expect("finish tar");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&tar_bytes).expect("gzip write");
    gz.finish().expect("gzip finish")
}

/// Minimal `.cargo_vcs_info.json` body: `{"git":{"sha1":"<sha>"},"path_in_vcs":"<vcs_path>"}`.
fn vcs_info_json(sha1: &str, path_in_vcs: &str) -> Vec<u8> {
    format!(r#"{{"git":{{"sha1":"{sha1}"}},"path_in_vcs":"{path_in_vcs}"}}"#).into_bytes()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    anodizer_core::hashing::hex_lower(&sha2::Sha256::digest(bytes))
}

#[test]
fn crates_equal_modulo_vcs_identical_archives_match() {
    let bytes = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("deadbeef", "."),
        ),
    ]);
    let m = crates_equal_modulo_vcs(&bytes, &bytes, false).expect("compare");
    assert!(matches!(m, CrateContentMatch::Equivalent { .. }));
}

#[test]
fn crates_equal_modulo_vcs_differs_only_in_vcs_sha1_matches() {
    let local = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
    ]);
    let published = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_b", "."),
        ),
    ]);
    let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
    assert!(
        matches!(m, CrateContentMatch::Equivalent { .. }),
        "a git.sha1-only delta is a same-source re-cut"
    );
}

#[test]
fn crates_equal_modulo_vcs_differs_in_src_file_reports_path() {
    let local = make_crate_tarball(&[
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
    ]);
    let published = make_crate_tarball(&[
        ("c-1.0.0/src/lib.rs", b"fn a() { /* changed */ }"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
    ]);
    let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
    match m {
        CrateContentMatch::Differs(paths) => {
            assert_eq!(paths, vec!["c-1.0.0/src/lib.rs".to_string()]);
        }
        CrateContentMatch::Equivalent { .. } => panic!("a real source edit must be flagged"),
    }
}

#[test]
fn crates_equal_modulo_vcs_differs_in_vcs_non_sha_field_reports_path() {
    let local = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
    ]);
    let published = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "subdir"),
        ),
    ]);
    let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
    match m {
        CrateContentMatch::Differs(paths) => {
            assert_eq!(paths, vec!["c-1.0.0/.cargo_vcs_info.json".to_string()]);
        }
        CrateContentMatch::Equivalent { .. } => {
            panic!("a path_in_vcs change is structural drift, not just the commit stamp")
        }
    }
}

#[test]
fn crates_equal_modulo_vcs_extra_entry_reports_path() {
    let local = make_crate_tarball(&[("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n")]);
    let published = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        ("c-1.0.0/src/extra.rs", b"// only in published"),
    ]);
    let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
    match m {
        CrateContentMatch::Differs(paths) => {
            assert_eq!(paths, vec!["c-1.0.0/src/extra.rs".to_string()]);
        }
        CrateContentMatch::Equivalent { .. } => {
            panic!("an entry present in only one archive must be flagged")
        }
    }
}

#[test]
fn crates_equal_modulo_vcs_nested_decoy_is_byte_compared() {
    let local = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
        (
            "c-1.0.0/tests/data/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
    ]);
    let published = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
        (
            "c-1.0.0/tests/data/.cargo_vcs_info.json",
            &vcs_info_json("commit_b", "."),
        ),
    ]);
    let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
    match m {
        CrateContentMatch::Differs(paths) => {
            assert_eq!(
                paths,
                vec!["c-1.0.0/tests/data/.cargo_vcs_info.json".to_string()]
            );
        }
        CrateContentMatch::Equivalent { .. } => {
            panic!("a nested .cargo_vcs_info.json is ordinary source, not the root vcs stamp")
        }
    }
}

#[test]
fn crates_equal_modulo_vcs_root_vcs_info_still_normalized() {
    let local = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
    ]);
    let published = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_b", "."),
        ),
    ]);
    let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
    assert!(
        matches!(m, CrateContentMatch::Equivalent { .. }),
        "the root .cargo_vcs_info.json's git.sha1 is still normalized"
    );
}

#[test]
fn crates_equal_modulo_vcs_root_vcs_info_dot_slash_prefixed_still_normalized() {
    let local = make_crate_tarball_raw_paths(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "./c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
    ]);
    let published = make_crate_tarball_raw_paths(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "./c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_b", "."),
        ),
    ]);
    let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
    assert!(
        matches!(m, CrateContentMatch::Equivalent { .. }),
        "a leading `./` (a CurDir component) must not inflate the root gate's \
             component count and misclassify the crate-root vcs-info as nested source"
    );
}

#[test]
fn crates_equal_modulo_vcs_root_vcs_info_missing_on_one_side_differs() {
    let local = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
    ]);
    let published = make_crate_tarball(&[("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n")]);
    let m = crates_equal_modulo_vcs(&local, &published, false).expect("compare");
    match m {
        CrateContentMatch::Differs(paths) => {
            assert_eq!(paths, vec!["c-1.0.0/.cargo_vcs_info.json".to_string()]);
        }
        CrateContentMatch::Equivalent { .. } => {
            panic!("a root vcs-info present on only one side is an unambiguous divergence")
        }
    }
}

#[test]
fn targets_crates_io_true_for_default_and_false_for_custom() {
    assert!(targets_crates_io(None), "no cfg ⇒ crates.io");
    assert!(
        targets_crates_io(Some(&CargoPublishConfig::default())),
        "empty cfg ⇒ crates.io"
    );
    let custom_reg = CargoPublishConfig {
        registry: Some("corp".into()),
        ..Default::default()
    };
    assert!(!targets_crates_io(Some(&custom_reg)), "registry= ⇒ custom");
    let custom_idx = CargoPublishConfig {
        index: Some("https://example/index".into()),
        ..Default::default()
    };
    assert!(!targets_crates_io(Some(&custom_idx)), "index= ⇒ custom");
}

/// Fetch closure that panics if invoked — for tests proving a code path
/// never reaches the slow-path download.
fn fetch_panics(_: &str, _: &str) -> Result<Vec<u8>> {
    panic!("fetch_published must not run on this path")
}

#[test]
fn decide_already_published_empty_index_cksum_fails_closed() {
    // An empty cksum on a returned index entry cannot prove content
    // identity. Skipping it would reopen the poison hole, so the guard
    // fails closed WITHOUT invoking the local computer or the fetcher.
    let cfg = CrateConfig::default();
    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let local_panics =
        |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| -> Result<Option<LocalCrate>> {
            panic!("local cksum must not run when index cksum is empty")
        };
    let err = decide_already_published(
        "c",
        "1.0.0",
        "",
        &cfg,
        None,
        false,
        local_panics,
        fetch_panics,
        &log,
    )
    .expect_err("empty cksum ⇒ fail closed, never skip");
    assert!(
        err.to_string().contains("carries no cksum"),
        "actionable empty-cksum error: {err}"
    );
}

#[test]
fn decide_already_published_local_none_fails_closed() {
    // Ok(None) means no local digest for a crates.io-targeting crate — an
    // unverifiable state the main loop should never reach, so the guard
    // refuses to skip rather than silently pass a possibly-drifted version.
    let cfg = CrateConfig::default();
    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let local_none = |_: &str,
                      _: &CrateConfig,
                      _: Option<&CargoPublishConfig>|
     -> Result<Option<LocalCrate>> { Ok(None) };
    let err = decide_already_published(
        "c",
        "1.0.0",
        "abcd",
        &cfg,
        None,
        false,
        local_none,
        fetch_panics,
        &log,
    )
    .expect_err("local None ⇒ fail closed, never skip");
    assert!(
        err.to_string().contains("content identity is unverifiable"),
        "actionable local-None error: {err}"
    );
}

#[test]
fn decide_already_published_match_is_case_insensitive_skip() {
    // Fast path: local sha256 == index cksum (case-insensitive) ⇒ Skip
    // WITHOUT ever invoking the (panicking) fetch closure.
    let cfg = CrateConfig::default();
    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let local = |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: "ABCD".to_string(),
            bytes: Vec::new(),
        }))
    };
    let d = decide_already_published(
        "c",
        "1.0.0",
        "abcd",
        &cfg,
        None,
        false,
        local,
        fetch_panics,
        &log,
    )
    .expect("case-insensitive match ⇒ Skip, no download");
    assert_eq!(d, CargoSkipDecision::Skip);
}

#[test]
fn decide_already_published_slow_path_identical_modulo_vcs_skips() {
    // Local sha256 != index cksum (the fast path misses), but the
    // fetched published .crate is identical to the local one except for
    // .cargo_vcs_info.json's git.sha1 — the same-source-re-cut case the
    // whole slow path exists for.
    let local_bytes = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_new", "."),
        ),
    ]);
    let published_bytes = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", b"[package]\nname = \"c\"\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_old", "."),
        ),
    ]);
    let index_cksum = sha256_hex(&published_bytes);
    let local_cksum = sha256_hex(&local_bytes);
    assert_ne!(local_cksum, index_cksum, "fixture must miss the fast path");

    let cfg = CrateConfig::default();
    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let local_bytes_clone = local_bytes.clone();
    let local = move |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: local_cksum.clone(),
            bytes: local_bytes_clone.clone(),
        }))
    };
    let published_bytes_clone = published_bytes.clone();
    let fetch = move |_: &str, _: &str| Ok(published_bytes_clone.clone());
    let d = decide_already_published(
        "c",
        "1.0.0",
        &index_cksum,
        &cfg,
        None,
        false,
        local,
        fetch,
        &log,
    )
    .expect("same-source re-cut (vcs-only delta) ⇒ Skip");
    assert_eq!(d, CargoSkipDecision::Skip);
}

#[test]
fn decide_already_published_slow_path_real_drift_hard_fails() {
    // Local sha256 != index cksum, and the fetched published .crate has a
    // GENUINE content difference (not just the vcs stamp) ⇒ hard fail,
    // naming the differing path.
    let local_bytes = make_crate_tarball(&[
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
    ]);
    let published_bytes = make_crate_tarball(&[
        ("c-1.0.0/src/lib.rs", b"fn a() { /* poisoned */ }"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_a", "."),
        ),
    ]);
    let index_cksum = sha256_hex(&published_bytes);
    let local_cksum = sha256_hex(&local_bytes);
    assert_ne!(local_cksum, index_cksum, "fixture must miss the fast path");

    let cfg = CrateConfig::default();
    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let local_bytes_clone = local_bytes.clone();
    let local = move |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: local_cksum.clone(),
            bytes: local_bytes_clone.clone(),
        }))
    };
    let published_bytes_clone = published_bytes.clone();
    let fetch = move |_: &str, _: &str| Ok(published_bytes_clone.clone());
    let err = decide_already_published(
        "c",
        "1.0.0",
        &index_cksum,
        &cfg,
        None,
        false,
        local,
        fetch,
        &log,
    )
    .expect_err("real content drift ⇒ hard fail");
    let msg = format!("{err:#}");
    assert!(msg.contains("DIFFERENT content"), "{msg}");
    assert!(
        msg.contains("c-1.0.0/src/lib.rs"),
        "error must name the differing path: {msg}"
    );
}

#[test]
fn decide_already_published_published_fetch_err_fails_closed() {
    // The fast path misses; fetching the published .crate to run the
    // slow-path comparison fails (network) ⇒ fail closed, never skip a
    // version whose content identity couldn't be confirmed either way.
    let cfg = CrateConfig::default();
    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let local = |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: "local_sha".to_string(),
            bytes: Vec::new(),
        }))
    };
    let fetch_err =
        |_: &str, _: &str| -> Result<Vec<u8>> { Err(anyhow::anyhow!("connection refused")) };
    let err = decide_already_published(
        "c",
        "1.0.0",
        "index_sha",
        &cfg,
        None,
        false,
        local,
        fetch_err,
        &log,
    )
    .expect_err("published fetch failure ⇒ fail closed");
    assert!(
        format!("{err:#}").contains("could not be fetched"),
        "{err:#}"
    );
}

#[test]
fn decide_already_published_published_sha_mismatch_fails_closed() {
    // The fast path misses; the fetched "published" bytes don't actually
    // hash to the index cksum — a mismatched download is not a valid
    // comparison basis ⇒ fail closed rather than trust it either way.
    let cfg = CrateConfig::default();
    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let local = |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: "local_sha".to_string(),
            bytes: Vec::new(),
        }))
    };
    let fetch = |_: &str, _: &str| Ok(b"not the real published bytes".to_vec());
    let err = decide_already_published(
        "c",
        "1.0.0",
        "index_sha_that_wont_match",
        &cfg,
        None,
        false,
        local,
        fetch,
        &log,
    )
    .expect_err("published-sha mismatch ⇒ fail closed");
    assert!(format!("{err:#}").contains("does NOT match"));
}

/// Normalized lib-only packaged manifest, as `cargo package` writes it
/// (explicit `[lib]`, no `[[bin]]`).
const LIB_ONLY_MANIFEST: &[u8] =
    b"[package]\nname = \"c\"\nversion = \"1.0.0\"\n\n[lib]\npath = \"src/lib.rs\"\n";

/// Normalized packaged manifest carrying an explicit `[[bin]]` target.
const BIN_MANIFEST: &[u8] = b"[package]\nname = \"c\"\nversion = \"1.0.0\"\n\n[[bin]]\nname = \"c\"\npath = \"src/main.rs\"\n";

#[test]
fn changelog_provenance_recorded_matches_marker_and_fails_closed() {
    use anodizer_core::test_helpers::TestContextBuilder;
    let tmp = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = std::process::Command::new("git");
                cmd.args(args)
                    .current_dir(tmp.path())
                    .env("GIT_AUTHOR_NAME", "t")
                    .env("GIT_AUTHOR_EMAIL", "t@t.com")
                    .env("GIT_COMMITTER_NAME", "t")
                    .env("GIT_COMMITTER_EMAIL", "t@t.com");
                cmd
            },
            "git",
        );
        assert!(out.status.success(), "git {args:?} failed");
    };
    run(&["init"]);
    run(&["config", "user.email", "t@t.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(tmp.path().join("CHANGELOG.md"), "notes").unwrap();
    run(&["add", "."]);
    let msg = format!(
        "chore(release): bump workspace → 1.0.0\n\n{}",
        anodizer_core::git::changelog_regenerated_marker("c", "1.0.0")
    );
    run(&["commit", "-m", &msg]);

    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let ctx = TestContextBuilder::new()
        .project_root(tmp.path().to_path_buf())
        .build();
    let rel = crate_changelog_rel_path(".");
    assert!(changelog_provenance_recorded(
        &ctx, "c", "1.0.0", &rel, &log
    ));
    assert!(!changelog_provenance_recorded(
        &ctx, "c", "1.0.1", &rel, &log
    ));
    // The marker binds to its crate — a sibling at the same version has
    // no provenance of its own.
    assert!(!changelog_provenance_recorded(
        &ctx, "other", "1.0.0", &rel, &log
    ));

    // A later hand-edit makes an unmarked commit the file's last toucher —
    // provenance is withdrawn, the guard reverts to byte-strict.
    std::fs::write(tmp.path().join("CHANGELOG.md"), "notes\nedited").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "docs: tweak changelog"]);
    assert!(!changelog_provenance_recorded(
        &ctx, "c", "1.0.0", &rel, &log
    ));

    // A non-repo project root cannot prove provenance — fail closed.
    let bare = tempfile::tempdir().unwrap();
    let ctx = TestContextBuilder::new()
        .project_root(bare.path().to_path_buf())
        .build();
    assert!(!changelog_provenance_recorded(
        &ctx, "c", "1.0.0", &rel, &log
    ));
}

#[test]
fn crate_changelog_rel_path_handles_root_and_nested_crates() {
    assert_eq!(crate_changelog_rel_path("."), "CHANGELOG.md");
    assert_eq!(crate_changelog_rel_path(""), "CHANGELOG.md");
    assert_eq!(crate_changelog_rel_path("./"), "CHANGELOG.md");
    assert_eq!(
        crate_changelog_rel_path("crates/core"),
        "crates/core/CHANGELOG.md"
    );
    assert_eq!(
        crate_changelog_rel_path("crates/core/"),
        "crates/core/CHANGELOG.md"
    );
}

#[test]
fn decide_already_published_recut_changelog_and_lockfile_skips_with_provenance() {
    // The exact cfgd-crd@0.5.0 scenario: a re-cut of a partially-published
    // workspace release where the published crate and the local re-cut
    // differ in exactly two crate-root files — CHANGELOG.md (regenerated
    // by anodizer, proven by the bump-commit provenance marker) and
    // Cargo.lock (the workspace lockfile moved via an unrelated dependency
    // bump) — on a lib-only crate. Sources identical ⇒ safe idempotent
    // Skip.
    let local_bytes = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        (
            "c-1.0.0/CHANGELOG.md",
            b"# Changelog\n\n## 1.0.0 (re-cut)\n",
        ),
        ("c-1.0.0/Cargo.lock", b"# lockfile v2\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_new", "."),
        ),
    ]);
    let published_bytes = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        ("c-1.0.0/CHANGELOG.md", b"# Changelog\n\n## 1.0.0\n"),
        ("c-1.0.0/Cargo.lock", b"# lockfile v1\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_old", "."),
        ),
    ]);
    let index_cksum = sha256_hex(&published_bytes);
    let local_cksum = sha256_hex(&local_bytes);
    assert_ne!(local_cksum, index_cksum, "fixture must miss the fast path");

    let cfg = CrateConfig::default();
    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let local_bytes_clone = local_bytes.clone();
    let local = move |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: local_cksum.clone(),
            bytes: local_bytes_clone.clone(),
        }))
    };
    let published_bytes_clone = published_bytes.clone();
    let fetch = move |_: &str, _: &str| Ok(published_bytes_clone.clone());
    let d = decide_already_published(
        "c",
        "1.0.0",
        &index_cksum,
        &cfg,
        None,
        true,
        local,
        fetch,
        &log,
    )
    .expect("changelog+lockfile-only re-cut of a lib crate ⇒ Skip");
    assert_eq!(d, CargoSkipDecision::Skip);
}

#[test]
fn decide_already_published_changelog_drift_without_provenance_hard_fails() {
    // Same CHANGELOG.md delta, but no bump commit records a
    // changelog-provenance marker for this crate@version: there is no
    // proof anodizer regenerated the file, so the drift is real and the
    // guard must hard-fail, naming the file AND the why. This is also
    // what closes the old any-drift-forgiven hole — an active changelog
    // stage in the current run proves nothing about who wrote the file.
    let local_bytes = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        (
            "c-1.0.0/CHANGELOG.md",
            b"# Changelog\n\n## 1.0.0 (edited)\n",
        ),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_new", "."),
        ),
    ]);
    let published_bytes = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/CHANGELOG.md", b"# Changelog\n\n## 1.0.0\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_old", "."),
        ),
    ]);
    let index_cksum = sha256_hex(&published_bytes);
    let local_cksum = sha256_hex(&local_bytes);

    let cfg = CrateConfig::default();
    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let local_bytes_clone = local_bytes.clone();
    let local = move |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: local_cksum.clone(),
            bytes: local_bytes_clone.clone(),
        }))
    };
    let published_bytes_clone = published_bytes.clone();
    let fetch = move |_: &str, _: &str| Ok(published_bytes_clone.clone());
    let err = decide_already_published(
        "c",
        "1.0.0",
        &index_cksum,
        &cfg,
        None,
        false,
        local,
        fetch,
        &log,
    )
    .expect_err("CHANGELOG.md drift with no provenance marker ⇒ hard fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("c-1.0.0/CHANGELOG.md"),
        "must name the file: {msg}"
    );
    assert!(
        msg.contains("carries no `changelog regenerated for <crate>@<version>`"),
        "must say why the file was not treated as equivalent: {msg}"
    );
    // The remedies must be actionable: re-cut with provenance, or unshallow
    // the checkout so an existing bump commit becomes visible.
    assert!(
        msg.contains("anodizer tag --changelog") && msg.contains("fetch-depth: 0"),
        "must name both remedies: {msg}"
    );
}

#[test]
fn decide_already_published_lockfile_drift_on_binary_crate_hard_fails() {
    // Root Cargo.lock delta on a crate WITH a [[bin]] target: the packaged
    // lockfile is consumer-visible via `cargo install --locked`, so it
    // stays byte-strict — hard fail naming the file AND the why.
    let local_bytes = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", BIN_MANIFEST),
        ("c-1.0.0/src/main.rs", b"fn main() {}"),
        ("c-1.0.0/Cargo.lock", b"# lockfile v2\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_new", "."),
        ),
    ]);
    let published_bytes = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", BIN_MANIFEST),
        ("c-1.0.0/src/main.rs", b"fn main() {}"),
        ("c-1.0.0/Cargo.lock", b"# lockfile v1\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_old", "."),
        ),
    ]);
    let index_cksum = sha256_hex(&published_bytes);
    let local_cksum = sha256_hex(&local_bytes);

    let cfg = CrateConfig::default();
    let log = StageLogger::new("t", anodizer_core::log::Verbosity::Normal);
    let local_bytes_clone = local_bytes.clone();
    let local = move |_: &str, _: &CrateConfig, _: Option<&CargoPublishConfig>| {
        Ok(Some(LocalCrate {
            cksum: local_cksum.clone(),
            bytes: local_bytes_clone.clone(),
        }))
    };
    let published_bytes_clone = published_bytes.clone();
    let fetch = move |_: &str, _: &str| Ok(published_bytes_clone.clone());
    let err = decide_already_published(
        "c",
        "1.0.0",
        &index_cksum,
        &cfg,
        None,
        true,
        local,
        fetch,
        &log,
    )
    .expect_err("Cargo.lock drift on a binary crate ⇒ hard fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("c-1.0.0/Cargo.lock"),
        "must name the file: {msg}"
    );
    assert!(
        msg.contains("binary or example targets") && msg.contains("cargo install --locked"),
        "must say why the lockfile stayed byte-strict: {msg}"
    );
}

#[test]
fn crates_equal_modulo_vcs_source_drift_beside_normalizable_files_differs() {
    // A real src/lib.rs edit rides along with the forgivable metadata
    // deltas: the source drift must still be flagged — the normalizations
    // never mask a genuine content change.
    let local = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        ("c-1.0.0/CHANGELOG.md", b"# Changelog (re-cut)\n"),
        ("c-1.0.0/Cargo.lock", b"# lockfile v2\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_new", "."),
        ),
    ]);
    let published = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/src/lib.rs", b"fn a() { /* poisoned */ }"),
        ("c-1.0.0/CHANGELOG.md", b"# Changelog\n"),
        ("c-1.0.0/Cargo.lock", b"# lockfile v1\n"),
        (
            "c-1.0.0/.cargo_vcs_info.json",
            &vcs_info_json("commit_old", "."),
        ),
    ]);
    let m = crates_equal_modulo_vcs(&local, &published, true).expect("compare");
    match m {
        CrateContentMatch::Differs(paths) => {
            assert_eq!(paths, vec!["c-1.0.0/src/lib.rs".to_string()]);
        }
        CrateContentMatch::Equivalent { .. } => {
            panic!("a real source edit must be flagged even beside forgivable metadata")
        }
    }
}

#[test]
fn crates_equal_modulo_vcs_nested_changelog_and_lockfile_are_byte_compared() {
    // Root-only discipline: CHANGELOG.md / Cargo.lock at 3+ Normal
    // components are ordinary packaged source (e.g. test fixtures) and
    // must be byte-compared, never normalized.
    let local = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/tests/data/CHANGELOG.md", b"fixture a"),
        ("c-1.0.0/tests/data/Cargo.lock", b"fixture lock a"),
    ]);
    let published = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/tests/data/CHANGELOG.md", b"fixture b"),
        ("c-1.0.0/tests/data/Cargo.lock", b"fixture lock b"),
    ]);
    let m = crates_equal_modulo_vcs(&local, &published, true).expect("compare");
    match m {
        CrateContentMatch::Differs(paths) => {
            assert_eq!(
                paths,
                vec![
                    "c-1.0.0/tests/data/CHANGELOG.md".to_string(),
                    "c-1.0.0/tests/data/Cargo.lock".to_string(),
                ]
            );
        }
        CrateContentMatch::Equivalent { .. } => {
            panic!("nested CHANGELOG.md / Cargo.lock are ordinary source, never normalized")
        }
    }
}

#[test]
fn crates_equal_modulo_vcs_cargo_toml_drift_always_differs() {
    // Cargo.toml is NEVER in the equivalence set — a manifest delta is
    // real drift regardless of the changelog-stage flag.
    let local = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
    ]);
    let published = make_crate_tarball(&[
        (
            "c-1.0.0/Cargo.toml",
            b"[package]\nname = \"c\"\nversion = \"1.0.0\"\nedition = \"2024\"\n".as_slice(),
        ),
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
    ]);
    let m = crates_equal_modulo_vcs(&local, &published, true).expect("compare");
    match m {
        CrateContentMatch::Differs(paths) => {
            assert_eq!(paths, vec!["c-1.0.0/Cargo.toml".to_string()]);
        }
        CrateContentMatch::Equivalent { .. } => {
            panic!("a Cargo.toml delta must always be flagged")
        }
    }
}

#[test]
fn packaged_crate_has_bin_targets_reads_the_normalized_manifest() {
    let lib_only = read_crate_entries(&make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
    ]))
    .expect("unpack");
    assert_eq!(packaged_crate_has_bin_targets(&lib_only), Some(false));

    let with_bin = read_crate_entries(&make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", BIN_MANIFEST),
        ("c-1.0.0/src/main.rs", b"fn main() {}"),
    ]))
    .expect("unpack");
    assert_eq!(packaged_crate_has_bin_targets(&with_bin), Some(true));

    // Conventional bin sources count even without an explicit [[bin]]
    // (belt-and-braces against implicit target auto-discovery).
    let implicit_bin = read_crate_entries(&make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/src/main.rs", b"fn main() {}"),
    ]))
    .expect("unpack");
    assert_eq!(packaged_crate_has_bin_targets(&implicit_bin), Some(true));

    // No root Cargo.toml ⇒ indeterminate ⇒ caller fails closed.
    let no_manifest = read_crate_entries(&make_crate_tarball(&[(
        "c-1.0.0/src/lib.rs",
        b"fn a() {}".as_slice(),
    )]))
    .expect("unpack");
    assert_eq!(packaged_crate_has_bin_targets(&no_manifest), None);
}

#[test]
fn packaged_crate_examples_count_as_installable_targets() {
    // `cargo install --example` consumes the packaged lockfile just like
    // a bin install, so an explicit [[example]] disqualifies the crate
    // from the lib-only Cargo.lock forgiveness.
    const EXAMPLE_MANIFEST: &[u8] = b"[package]\nname = \"c\"\nversion = \"1.0.0\"\n\n[lib]\nname = \"c\"\npath = \"src/lib.rs\"\n\n[[example]]\nname = \"demo\"\npath = \"examples/demo.rs\"\n";
    let with_example = read_crate_entries(&make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", EXAMPLE_MANIFEST),
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        ("c-1.0.0/examples/demo.rs", b"fn main() {}"),
    ]))
    .expect("unpack");
    assert_eq!(packaged_crate_has_bin_targets(&with_example), Some(true));

    // Conventional examples/ sources count even without an explicit
    // [[example]] (belt-and-braces against implicit auto-discovery).
    let implicit_example = read_crate_entries(&make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        ("c-1.0.0/examples/demo.rs", b"fn main() {}"),
    ]))
    .expect("unpack");
    assert_eq!(
        packaged_crate_has_bin_targets(&implicit_example),
        Some(true)
    );
}

#[test]
fn crates_equal_modulo_vcs_lockfile_drift_on_example_crate_differs() {
    // Lockfile drift on a crate carrying examples must NOT be forgiven:
    // the packaged lockfile ships to `cargo install --example` consumers.
    let local = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        ("c-1.0.0/examples/demo.rs", b"fn main() {}"),
        ("c-1.0.0/Cargo.lock", b"# lockfile v2\n"),
    ]);
    let published = make_crate_tarball(&[
        ("c-1.0.0/Cargo.toml", LIB_ONLY_MANIFEST),
        ("c-1.0.0/src/lib.rs", b"fn a() {}"),
        ("c-1.0.0/examples/demo.rs", b"fn main() {}"),
        ("c-1.0.0/Cargo.lock", b"# lockfile v1\n"),
    ]);
    match crates_equal_modulo_vcs(&local, &published, false).expect("compare") {
        CrateContentMatch::Differs(files) => {
            assert!(
                files
                    .iter()
                    .any(|f| f.contains("Cargo.lock") && f.contains("binary or example targets")),
                "lockfile drift must be flagged with the install-visibility \
                     rationale: {files:?}"
            );
        }
        other => panic!("example crate lockfile drift must differ, got {other:?}"),
    }
}

// ---- retry plumbing through is_already_published_at ------------------
//
// Pin: the sparse-index GET must route through retry_http_blocking so
// transient 5xx / 429 / network failures retry per the user's policy.
// 404 (crate never published) must remain Ok(None) — preserved via the
// HttpError(404)-from-Break catch in is_already_published_at.

use anodizer_core::test_helpers::responder::spawn_oneshot_http_responder;

fn fast_retry_policy() -> anodizer_core::retry::RetryPolicy {
    anodizer_core::retry::RetryPolicy {
        max_attempts: 3,
        base_delay: std::time::Duration::from_millis(1),
        max_delay: std::time::Duration::from_millis(2),
    }
}

#[test]
fn is_already_published_at_retries_5xx_then_succeeds() {
    use std::sync::atomic::Ordering;

    let body = r#"{"name":"foo","vers":"1.2.3","cksum":"abc123","yanked":false}"#.to_string();
    let body_len = body.len();
    let ok_resp: &'static str = Box::leak(
        format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
    );
    let (addr, calls) = spawn_oneshot_http_responder(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        ok_resp,
    ]);

    let url = format!("http://{addr}/3/f/foo");
    let result = is_already_published_at(
        &url,
        "foo",
        "1.2.3",
        &fast_retry_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("retries 5xx then parses");
    assert_eq!(result, Some("abc123".to_string()));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one 503 retry then success"
    );
}

#[test]
fn is_already_published_at_404_maps_to_ok_none() {
    // A 404 must NOT retry and must surface as Ok(None) — preserving
    // the "crate never published" signal that the publish pipeline
    // relies on to skip the drift check.
    use std::sync::atomic::Ordering;

    let (addr, calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let url = format!("http://{addr}/3/f/foo");
    let result = is_already_published_at(
        &url,
        "foo",
        "1.2.3",
        &fast_retry_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("404 is Ok(None)");
    assert_eq!(result, None);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "404 must NOT retry");
}

/// Defense-in-depth: a crates.io sparse-index 4xx response that echoes
/// our `Authorization: Bearer <PAT>` header back must not leak the token
/// into the user-visible error chain. The sparse index is unauthenticated
/// in production, so this is paranoia — but mirror/proxy registries can
/// gateway through an auth proxy.
#[test]
fn is_already_published_at_redacts_bearer_in_error_body() {
    let leaky = "Authorization: Bearer ghp_FAKETOKEN1234567890abcdefg denied";
    let body_len = leaky.len();
    // 401 fast-fails (4xx) so a single response suffices.
    let resp: &'static str = Box::leak(
        format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {body_len}\r\n\r\n{leaky}")
            .into_boxed_str(),
    );
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
    let url = format!("http://{addr}/3/f/foo");
    let err = is_already_published_at(
        &url,
        "foo",
        "1.2.3",
        &fast_retry_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect_err("401 must fast-fail");
    let chain = format!("{err:#}");
    assert!(
        !chain.contains("ghp_FAKETOKEN1234567890abcdefg"),
        "bearer token leaked into error chain: {chain}"
    );
    assert!(
        chain.contains("<redacted>"),
        "expected `<redacted>` marker in error chain: {chain}"
    );
}

/// Version-exists on crates.io must skip without comparing bytes.
/// Pre-seed a sparse-index response that returns a valid version entry;
/// the publisher loop must emit "skipped" and NOT attempt to POST.
#[test]
fn skip_on_version_exists_no_cksum_comparison() {
    use std::sync::atomic::Ordering;

    // Serve a JSONL body that says version 1.2.3 is published (with a cksum).
    let body = r#"{"name":"myapp","vers":"1.2.3","cksum":"deadbeef","yanked":false}"#;
    let body_len = body.len();
    let ok_resp: &'static str = Box::leak(
        format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
    );
    let (addr, calls) = spawn_oneshot_http_responder(vec![ok_resp]);
    let url = format!("http://{addr}/3/m/myapp");

    // is_already_published_at should return Some(_), signalling skip.
    let result = is_already_published_at(
        &url,
        "myapp",
        "1.2.3",
        &fast_retry_policy(),
        anodizer_core::test_helpers::test_logger(),
    )
    .expect("index check succeeds");
    assert!(
        result.is_some(),
        "index returned a version entry, expected Some"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one HTTP request");

    // The important invariant: Some(_) from is_already_published now
    // unconditionally skips — the caller must NOT call
    // compute_local_crate_cksum or bail.  We verify that by checking
    // the value is discarded (any Some triggers skip regardless of content).
    let cksum = result.unwrap();
    // Non-empty cksum in index body: old code would have compared it and
    // potentially bailed; new code ignores the value entirely.
    assert_eq!(cksum, "deadbeef");
}

// The per-crate index confirmation is propagation progress, not a RESULT:
// it fires once per crate with dependents, so it rides at verbose, leaving
// `published crate '<name>'` as the only default-level per-crate output.
#[test]
fn index_confirmation_rides_at_verbose_not_default() {
    use anodizer_core::log::LogLevel;

    let body = r#"{"name":"myapp","vers":"1.2.3","cksum":"deadbeef","yanked":false}"#;
    let body_len = body.len();
    let ok_resp: &'static str = Box::leak(
        format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
    );
    let (addr, _calls) = spawn_oneshot_http_responder(vec![ok_resp]);
    let url = format!("http://{addr}/3/m/myapp");

    let (log, cap) =
        StageLogger::with_capture("publish-test", anodizer_core::log::Verbosity::Normal);
    poll_crates_io_index_at(&url, "myapp", "1.2.3", 5, std::time::Duration::ZERO, &log)
        .expect("version present ⇒ confirmed");

    let confirmed = "crates.io index confirmed myapp-1.2.3";
    let status: Vec<String> = cap
        .all_messages()
        .into_iter()
        .filter(|(lvl, _)| *lvl == LogLevel::Status)
        .map(|(_, m)| m)
        .collect();
    let verbose: Vec<String> = cap
        .all_messages()
        .into_iter()
        .filter(|(lvl, _)| *lvl == LogLevel::Verbose)
        .map(|(_, m)| m)
        .collect();
    assert!(
        !status.iter().any(|m| m == confirmed),
        "confirmation must NOT appear at default: {status:?}"
    );
    assert!(
        verbose.iter().any(|m| m == confirmed),
        "confirmation must ride at verbose: {verbose:?}"
    );
}

// -----------------------------------------------------------------------
// sparse-index propagation retry on cargo publish
//
// Defense in depth on top of poll_crates_io_index: even after our wait
// sees the just-published dep on the sparse index, cargo's own resolution
// may hit a stale Fastly edge a beat later. run_cargo_publish_with_retry
// narrows retry exclusively to the propagation-shaped error signatures
// so real failures (auth, packaging, network) still fast-fail.
// -----------------------------------------------------------------------

/// Discriminator: every known propagation-style cargo stderr must match
/// so the retry harness recognises it; non-propagation failures must NOT
/// match so retry doesn't mask genuine errors.
#[test]
fn is_index_propagation_failure_matches_known_signatures() {
    // Historical signature from anodizer's older topo-sort era.
    assert!(is_index_propagation_failure(
        "error: no matching package named `cfgd-core` found"
    ));
    // Stale-edge resolution failure: cargo found the crate on the
    // sparse index but not the just-published version it depends on.
    assert!(is_index_propagation_failure(
        "error: failed to select a version for the requirement \
             `anodizer-stage-publish = \"^0.3.0\"`"
    ));
    // Sparse-index transport variant.
    assert!(is_index_propagation_failure(
        "error: failed to load source for dependency `anodizer-core`"
    ));
}

#[test]
fn is_index_propagation_failure_rejects_unrelated_errors() {
    // Auth failure — must NOT retry (token won't appear by waiting).
    assert!(!is_index_propagation_failure(
        "error: failed to publish to registry: 401 Unauthorized"
    ));
    // Validation failure — must NOT retry (broken Cargo.toml stays broken).
    assert!(!is_index_propagation_failure(
        "error: invalid character `_` in crate name `bad_name`"
    ));
    // Network failure — caller has its own transport retries; the
    // propagation-retry path shouldn't double-count those.
    assert!(!is_index_propagation_failure(
        "error: failed to send HTTP request: connection refused"
    ));
    // Empty stderr (cargo crashed without saying anything) — don't retry.
    assert!(!is_index_propagation_failure(""));
}

#[test]
fn is_transient_network_failure_matches_known_signatures() {
    // The exact v0.11.3 makeself abort, verbatim from the run log.
    assert!(is_transient_network_failure(
        "    Updating crates.io index\nerror: download of ti/ny/tinystr failed\n\
             Caused by:\n  [16] Error in the HTTP2 framing layer"
    ));
    // libcurl transport faults.
    assert!(is_transient_network_failure(
        "error: failed to send HTTP request: connection refused"
    ));
    assert!(is_transient_network_failure("Connection reset by peer"));
    assert!(is_transient_network_failure(
        "error: could not resolve host: static.crates.io"
    ));
    // cargo's own wording + CDN 5xx / rate-limit.
    assert!(is_transient_network_failure(
        "warning: spurious network error (3 tries remaining)"
    ));
    assert!(is_transient_network_failure(
        "error: failed to get successful HTTP response: 503 Service Unavailable"
    ));
    assert!(is_transient_network_failure("429 Too Many Requests"));
    // Case-insensitive: curl/cargo vary casing across versions.
    assert!(is_transient_network_failure(
        "ERROR IN THE HTTP2 FRAMING LAYER"
    ));
}

#[test]
fn is_transient_network_failure_rejects_unrelated_errors() {
    // Auth — a retry will not conjure a token. Must fast-fail.
    assert!(!is_transient_network_failure(
        "error: failed to publish to registry: 401 Unauthorized"
    ));
    // Packaging/validation — a broken Cargo.toml stays broken.
    assert!(!is_transient_network_failure(
        "error: invalid character `_` in crate name `bad_name`"
    ));
    // Already-published is handled upstream (idempotent skip), not by retry.
    assert!(!is_transient_network_failure(
        "error: crate version `0.11.3` is already uploaded"
    ));
    // A missing/yanked dependency surfaces "failed to download" too, but it
    // is NOT transient — retrying cannot conjure the version. The bare
    // phrase is deliberately excluded so this hard error fast-fails.
    assert!(!is_transient_network_failure(
        "error: failed to download `foo v1.2.3`\n  no matching package named `foo` found"
    ));
    // Empty stderr — don't retry.
    assert!(!is_transient_network_failure(""));
}

/// Pin the cargo major.minor version against which the discriminator
/// substrings in [`is_index_propagation_failure`] were last verified.
///
/// If CI upgrades to a different cargo major.minor this test fails,
/// signalling that a maintainer must re-run `cargo publish` against a
/// fixture that triggers each error substring and confirm the wording
/// matches before bumping `VERIFIED_CARGO_MINOR` below.
///
/// The substrings were last verified against cargo 1.97.x (rust-lang/cargo
/// branch rust-1.97.0 source, 2026-07-10). Bump `VERIFIED_CARGO_MINOR` only after
/// manually confirming all three substrings still appear verbatim in
/// the new cargo's publish output.
#[test]
fn cargo_version_matches_pinned_discriminator_strings() {
    // Last-verified cargo minor. Update together with re-verification.
    const VERIFIED_CARGO_MINOR: u64 = 97;

    // Resolve cargo via the `CARGO` env var — the absolute path cargo
    // exports when it spawns the test binary — not PATH: a peer `#[serial]`
    // test prepends a stub-cargo dir to the process-global PATH, and a
    // PATH-resolved spawn here would race it and read the stub's version.
    let cargo_bin = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = std::process::Command::new(cargo_bin)
        .arg("--version")
        // Pin cwd: a peer test that deletes the process-global cwd would
        // otherwise make this forked `cargo --version` abort on getcwd.
        .current_dir(anodizer_core::path_util::probe_dir())
        .output()
        .expect("cargo --version must succeed");
    let version_str = String::from_utf8_lossy(&output.stdout);
    // Format: "cargo X.Y.Z (hash date)"
    let minor: Option<u64> = version_str
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.split('.').nth(1))
        .and_then(|s| s.parse().ok());
    let minor = minor.unwrap_or_else(|| panic!("could not parse cargo minor from: {version_str}"));
    assert_eq!(
        minor, VERIFIED_CARGO_MINOR,
        "cargo minor version changed from {VERIFIED_CARGO_MINOR} to {minor}. \
             Re-verify the is_index_propagation_failure substrings against \
             `cargo publish` output on the new version, then bump \
             VERIFIED_CARGO_MINOR in this test."
    );
}

/// End-to-end retry behaviour: stub `cargo` with a shell script that
/// fails twice with a propagation-style stderr, then succeeds. The
/// retry harness must persist through the failures and surface success.
///
/// Uses a counter file under tempdir so successive invocations of the
/// same script select different exit paths — keeps the test
/// deterministic without needing a global mutex.
#[cfg(unix)]
#[test]
fn run_cargo_publish_with_retry_recovers_from_propagation_lag() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let counter = tmp.path().join("counter");
    let stub = tmp.path().join("cargo");
    let script = format!(
        "#!/bin/sh\n\
             n=$(cat {counter} 2>/dev/null || echo 0)\n\
             n=$((n+1))\n\
             echo $n > {counter}\n\
             if [ $n -lt 3 ]; then\n\
             echo 'error: failed to select a version for the requirement `dep = \"^1.0.0\"`' >&2\n\
             exit 101\n\
             fi\n\
             echo 'published ok'\n\
             exit 0\n",
        counter = counter.display(),
    );
    std::fs::write(&stub, script).expect("write stub");

    // Run the stub via `sh` instead of exec'ing it directly. A freshly
    // written executable that another test thread forks across in the
    // window before its write fd is closed trips ETXTBSY ("Text file
    // busy") on execve; `sh` is a long-lived binary and the stub is only
    // read, so the race cannot occur. When the test itself execs the
    // stub, use
    // `anodizer_core::test_helpers::fake_tool::output_retrying_etxtbsy`
    // instead of sh-routing.
    let cmd = vec![
        "sh".to_string(),
        stub.display().to_string(),
        "publish".to_string(),
    ];
    let log =
        anodizer_core::log::StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    // Use a tiny backoff so the retry path exercises the full counter/sleep/error
    // envelope without incurring real wall-clock cost.
    let result = run_cargo_publish_with_retry(
        &cmd,
        "stub publish",
        &log,
        std::time::Duration::from_millis(1),
        None,
    )
    .expect("retry harness must succeed after propagation lag");
    assert!(result.status.success(), "final attempt must succeed");

    // Counter file confirms the harness invoked the stub 3 times
    // (initial + 2 retries).
    let n: u32 = std::fs::read_to_string(&counter)
        .expect("counter")
        .trim()
        .parse()
        .expect("u32");
    assert_eq!(n, 3, "expected 3 invocations (initial + 2 retries)");
}

/// End-to-end retry on a transient network fault: the v0.11.3 regression.
/// The stub fails twice with the exact `HTTP2 framing layer` stderr cargo
/// emitted when makeself's publish died, then succeeds. The harness must
/// persist through the transport blips rather than burning the re-cut.
#[cfg(unix)]
#[test]
fn run_cargo_publish_with_retry_recovers_from_transient_network() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let counter = tmp.path().join("counter");
    let stub = tmp.path().join("cargo");
    let script = format!(
        "#!/bin/sh\n\
             n=$(cat {counter} 2>/dev/null || echo 0)\n\
             n=$((n+1))\n\
             echo $n > {counter}\n\
             if [ $n -lt 3 ]; then\n\
             echo '    Updating crates.io index' >&2\n\
             echo 'error: [16] Error in the HTTP2 framing layer' >&2\n\
             exit 101\n\
             fi\n\
             echo 'published ok'\n\
             exit 0\n",
        counter = counter.display(),
    );
    std::fs::write(&stub, script).expect("write stub");

    // Route through `sh` to dodge the ETXTBSY race exec'ing a
    // freshly-written stub under parallel tests (see the propagation test).
    let cmd = vec![
        "sh".to_string(),
        stub.display().to_string(),
        "publish".to_string(),
    ];
    let log =
        anodizer_core::log::StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    let result = run_cargo_publish_with_retry(
        &cmd,
        "stub publish",
        &log,
        std::time::Duration::from_millis(1),
        None,
    )
    .expect("retry harness must recover from a transient network blip");
    assert!(result.status.success(), "final attempt must succeed");

    let n: u32 = std::fs::read_to_string(&counter)
        .expect("counter")
        .trim()
        .parse()
        .expect("u32");
    assert_eq!(n, 3, "expected 3 invocations (initial + 2 retries)");
}

/// Fast-fail behaviour: a non-propagation failure (auth) must NOT
/// trigger retry. The stub fails with a 401-style stderr; harness must
/// surface immediately without further invocations.
#[cfg(unix)]
#[test]
fn run_cargo_publish_with_retry_does_not_retry_unrelated_failure() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let counter = tmp.path().join("counter");
    let stub = tmp.path().join("cargo");
    let script = format!(
        "#!/bin/sh\n\
             n=$(cat {counter} 2>/dev/null || echo 0)\n\
             n=$((n+1))\n\
             echo $n > {counter}\n\
             echo 'error: failed to publish: 401 Unauthorized' >&2\n\
             exit 101\n",
        counter = counter.display(),
    );
    std::fs::write(&stub, script).expect("write stub");

    // See the recovery test above: route through `sh` to dodge the
    // ETXTBSY race exec'ing a freshly-written stub under parallel tests.
    let cmd = vec![
        "sh".to_string(),
        stub.display().to_string(),
        "publish".to_string(),
    ];
    let log =
        anodizer_core::log::StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    let err = run_cargo_publish_with_retry(
        &cmd,
        "stub publish",
        &log,
        std::time::Duration::from_millis(1),
        None,
    )
    .expect_err("non-propagation failure must surface");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("401") || chain.contains("Unauthorized") || chain.contains("exit code"),
        "expected upstream error in chain: {chain}"
    );

    let n: u32 = std::fs::read_to_string(&counter)
        .expect("counter")
        .trim()
        .parse()
        .expect("u32");
    assert_eq!(n, 1, "non-propagation failure must NOT retry");
}

/// Cross-platform variant of the retry recovery test. Instead of a shell
/// script stub (unix-only), this variant compiles a minimal Rust binary
/// whose behaviour is controlled by a counter file — same contract as the
/// unix shell stub, but works on Windows CI where /bin/sh is absent.
///
/// Gated on `cfg(not(unix))` so only one of the two variants runs per
/// platform; the shell-script path is preferred on unix (faster compile).
#[cfg(not(unix))]
#[test]
#[serial_test::serial(stub_counter)]
fn run_cargo_publish_with_retry_recovers_from_propagation_lag_windows() {
    // Build the counter stub from an in-test source string. We write
    // a tiny Rust program to a tempdir and compile it with `rustc`.
    let tmp = tempfile::tempdir().expect("tempdir");
    let counter = tmp.path().join("counter.txt");
    let src_path = tmp.path().join("stub.rs");
    let exe_path = if cfg!(windows) {
        tmp.path().join("stub.exe")
    } else {
        tmp.path().join("stub")
    };

    // Counter file path passed via env var so the compiled binary can
    // locate it at runtime without baking in a temp path at compile time.
    let src = r#"
use std::fs;

fn main() {
    let counter_path = std::env::var("STUB_COUNTER").expect("STUB_COUNTER not set");
    let n: u32 = fs::read_to_string(&counter_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
        + 1;
    fs::write(&counter_path, n.to_string()).expect("write counter");
    if n < 3 {
        eprintln!("error: failed to select a version for the requirement `dep = \"^1.0.0\"`");
        std::process::exit(101);
    }
    println!("published ok");
}
"#;
    std::fs::write(&src_path, src).expect("write stub source");

    let compile = std::process::Command::new("rustc")
        .arg(&src_path)
        .arg("-o")
        .arg(&exe_path)
        .output()
        .expect("rustc spawn");
    if !compile.status.success() {
        panic!(
            "stub compile failed: {}",
            String::from_utf8_lossy(&compile.stderr)
        );
    }

    let cmd = vec![exe_path.display().to_string(), "publish".to_string()];
    let log =
        anodizer_core::log::StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    // STUB_COUNTER points the spawned stub at this test's own tempdir
    // counter file; the env-var NAME is shared, so the sibling
    // `..._unrelated_failure_windows` test races the set/remove pair
    // without serialization. The `#[serial(stub_counter)]` annotation on
    // the test guarantees no other stub_counter test runs concurrently.
    // SAFETY: serialised by `#[serial(stub_counter)]`; pair set / remove.
    // env-ok: STUB_COUNTER under #[serial(stub_counter)]; per-test tempdir counter file
    unsafe { std::env::set_var("STUB_COUNTER", counter.display().to_string()) };
    let result = run_cargo_publish_with_retry(
        &cmd,
        "stub publish",
        &log,
        std::time::Duration::from_millis(1),
        None,
    )
    .expect("retry harness must succeed after propagation lag");
    // SAFETY: serialised by `#[serial(stub_counter)]`; pair with set.
    // env-ok: STUB_COUNTER under #[serial(stub_counter)]; per-test tempdir counter file
    unsafe { std::env::remove_var("STUB_COUNTER") };
    assert!(result.status.success(), "final attempt must succeed");

    let n: u32 = std::fs::read_to_string(&counter)
        .expect("counter")
        .trim()
        .parse()
        .expect("u32");
    assert_eq!(n, 3, "expected 3 invocations (initial + 2 retries)");
}

/// Cross-platform fast-fail variant: non-propagation failure must NOT
/// retry. Windows CI exercises this path because the unix shell-script
/// variants are excluded on non-unix platforms.
#[cfg(not(unix))]
#[test]
#[serial_test::serial(stub_counter)]
fn run_cargo_publish_with_retry_does_not_retry_unrelated_failure_windows() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let counter = tmp.path().join("counter.txt");
    let src_path = tmp.path().join("stub_auth.rs");
    let exe_path = if cfg!(windows) {
        tmp.path().join("stub_auth.exe")
    } else {
        tmp.path().join("stub_auth")
    };

    let src = r#"
fn main() {
    let counter_path = std::env::var("STUB_COUNTER").expect("STUB_COUNTER not set");
    let n: u32 = std::fs::read_to_string(&counter_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
        + 1;
    std::fs::write(&counter_path, n.to_string()).expect("write counter");
    eprintln!("error: failed to publish: 401 Unauthorized");
    std::process::exit(101);
}
"#;
    std::fs::write(&src_path, src).expect("write stub source");
    let compile = std::process::Command::new("rustc")
        .arg(&src_path)
        .arg("-o")
        .arg(&exe_path)
        .output()
        .expect("rustc spawn");
    if !compile.status.success() {
        panic!(
            "stub compile failed: {}",
            String::from_utf8_lossy(&compile.stderr)
        );
    }

    let cmd = vec![exe_path.display().to_string(), "publish".to_string()];
    let log =
        anodizer_core::log::StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    // Serialized by `#[serial(stub_counter)]` — see the sibling
    // `..._recovers_from_propagation_lag_windows` test for the
    // race this guards against.
    // SAFETY: serialised by `#[serial(stub_counter)]`; pair set / remove.
    // env-ok: STUB_COUNTER under #[serial(stub_counter)]; per-test tempdir counter file
    unsafe { std::env::set_var("STUB_COUNTER", counter.display().to_string()) };
    let err = run_cargo_publish_with_retry(
        &cmd,
        "stub publish",
        &log,
        std::time::Duration::from_millis(1),
        None,
    )
    .expect_err("non-propagation failure must surface");
    // SAFETY: serialised by `#[serial(stub_counter)]`; pair with set.
    // env-ok: STUB_COUNTER under #[serial(stub_counter)]; per-test tempdir counter file
    unsafe { std::env::remove_var("STUB_COUNTER") };
    let chain = format!("{err:#}");
    assert!(
        chain.contains("401") || chain.contains("Unauthorized") || chain.contains("exit code"),
        "expected upstream error in chain: {chain}"
    );

    let n: u32 = std::fs::read_to_string(&counter)
        .expect("counter")
        .trim()
        .parse()
        .expect("u32");
    assert_eq!(n, 1, "non-propagation failure must NOT retry");
}

// -----------------------------------------------------------------------
// wait_for_workspace_deps — pre-publish gate
//
// Pin the manifest parser shape and the polling-success path. The
// sparse-index URL math is exercised by `test_sparse_index_url_shape`
// above; the gate reuses that helper unchanged.
// -----------------------------------------------------------------------

fn write_manifest(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let p = dir.join("Cargo.toml");
    std::fs::write(&p, body).expect("write Cargo.toml");
    p
}

/// Bare-string dep (`name = "1.2.3"`) and inline-table dep
/// (`name = { path = "...", version = "..." }`) are both parsed as
/// version pins; deps not in the workspace name set are filtered out.
#[test]
fn workspace_deps_for_crate_picks_up_pinned_workspace_deps() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(
        tmp.path(),
        r#"
[package]
name = "cfgd-operator"
version = "1.0.0"

[dependencies]
cfgd-core = { path = "../core", version = "0.4.0" }
cfgd-shared = "0.5.0"
serde = "1.0"
tokio = { version = "1.0", features = ["full"] }
"#,
    );
    let ws_names: HashSet<&str> = ["cfgd-core", "cfgd-shared", "cfgd-operator"]
        .iter()
        .copied()
        .collect();
    let mut deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    deps.sort();
    assert_eq!(
        deps,
        vec![
            ("cfgd-core".to_string(), "0.4.0".to_string()),
            ("cfgd-shared".to_string(), "0.5.0".to_string()),
        ]
    );
}

/// `dev-dependencies` and `build-dependencies` participate alongside
/// `dependencies` — version_sync rewrites all three, and a downstream
/// publish of an integration-test fixture would race the same way.
#[test]
fn workspace_deps_for_crate_includes_dev_and_build_sections() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(
        tmp.path(),
        r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies]
core-lib = { path = "../core", version = "0.4.0" }

[dev-dependencies]
test-fixtures = { path = "../fixtures", version = "0.2.0" }

[build-dependencies]
build-tools = { path = "../build", version = "0.3.0" }
"#,
    );
    let ws_names: HashSet<&str> = ["core-lib", "test-fixtures", "build-tools", "leaf"]
        .iter()
        .copied()
        .collect();
    let mut deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    deps.sort();
    assert_eq!(
        deps,
        vec![
            ("build-tools".to_string(), "0.3.0".to_string()),
            ("core-lib".to_string(), "0.4.0".to_string()),
            ("test-fixtures".to_string(), "0.2.0".to_string()),
        ]
    );
}

/// `target.'cfg(...)'.dependencies` (and dev/build target variants)
/// must also be scanned — version_sync rewrites them; missing them
/// would leave a publish racing the index on platform-specific deps.
#[test]
fn workspace_deps_for_crate_scans_target_specific_sections() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(
        tmp.path(),
        r#"
[package]
name = "leaf"
version = "1.0.0"

[target.'cfg(unix)'.dependencies]
unix-helper = { path = "../unix", version = "0.1.0" }

[target.'cfg(windows)'.build-dependencies]
win-build = { path = "../win", version = "0.2.0" }
"#,
    );
    let ws_names: HashSet<&str> = ["unix-helper", "win-build", "leaf"]
        .iter()
        .copied()
        .collect();
    let mut deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    deps.sort();
    assert_eq!(
        deps,
        vec![
            ("unix-helper".to_string(), "0.1.0".to_string()),
            ("win-build".to_string(), "0.2.0".to_string()),
        ]
    );
}

/// Deps with no crates.io-queryable pin anywhere — git deps, path-only
/// entries, and `workspace = true` inherits with no root version pin —
/// are skipped (returning them would either timeout or false-confirm
/// against an unrelated version). The explicit root manifest pins
/// nothing: "inherited" resolves to a path-only root entry and
/// "unrooted" has no root entry at all.
#[test]
fn workspace_deps_for_crate_skips_deps_without_resolvable_pin() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"leaf\", \"inherited\"]\n\n\
             [workspace.dependencies]\ninherited = { path = \"inherited\" }\n",
    )
    .expect("write workspace root");
    let leaf_dir = tmp.path().join("leaf");
    std::fs::create_dir_all(&leaf_dir).expect("mkdir leaf");
    let manifest = write_manifest(
        &leaf_dir,
        r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies]
inherited = { workspace = true }
unrooted = { workspace = true }
git-only = { git = "https://example.com/foo" }
path-only = { path = "../foo" }
pinned = { path = "../bar", version = "0.5.0" }
"#,
    );
    let ws_names: HashSet<&str> = [
        "inherited",
        "unrooted",
        "git-only",
        "path-only",
        "pinned",
        "leaf",
    ]
    .iter()
    .copied()
    .collect();
    let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    assert_eq!(deps, vec![("pinned".to_string(), "0.5.0".to_string())]);
}

/// The same package may appear in several sections with different specs;
/// a version-less sighting (here an inherit whose root entry has no pin)
/// must not shadow a pinned occurrence in a later section.
#[test]
fn workspace_deps_for_crate_backfills_version_from_later_section() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"leaf\", \"lib\"]\n\n\
             [workspace.dependencies]\nlib = { path = \"lib\" }\n",
    )
    .expect("write workspace root");
    let leaf_dir = tmp.path().join("leaf");
    std::fs::create_dir_all(&leaf_dir).expect("mkdir leaf");
    let manifest = write_manifest(
        &leaf_dir,
        r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies]
lib = { workspace = true }

[build-dependencies]
lib = { path = "../lib", version = "0.3.0" }
"#,
    );
    let ws_names: HashSet<&str> = ["lib", "leaf"].iter().copied().collect();
    let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    assert_eq!(
        deps,
        vec![("lib".to_string(), "0.3.0".to_string())],
        "one entry, carrying the pinned version from the later section"
    );
}

/// A package pinned in two sections collapses to one wait entry; the
/// first pin wins.
#[test]
fn workspace_deps_for_crate_dedupes_across_sections() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(
        tmp.path(),
        r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies]
lib = { path = "../lib", version = "0.4.0" }

[dev-dependencies]
lib = { path = "../lib", version = "0.9.9" }
"#,
    );
    let ws_names: HashSet<&str> = ["lib", "leaf"].iter().copied().collect();
    let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    assert_eq!(
        deps,
        vec![("lib".to_string(), "0.4.0".to_string())],
        "duplicate pins collapse to one entry, first pin wins"
    );
}

/// One run can touch crates from two distinct cargo workspaces (a nested
/// standalone `[workspace]`); a shared cache must resolve each crate's
/// inherits against its OWN root, not whichever root was parsed first.
#[test]
fn workspace_deps_root_cache_is_keyed_per_workspace_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Outer workspace: pins shared@1.1.1.
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\"]\n\n\
             [workspace.dependencies]\nshared = { path = \"shared\", version = \"1.1.1\" }\n",
    )
    .expect("write outer root");
    let app_dir = tmp.path().join("app");
    std::fs::create_dir_all(&app_dir).expect("mkdir app");
    let app_manifest = write_manifest(
        &app_dir,
        "[package]\nname = \"app\"\nversion = \"1.0.0\"\n\n\
             [dependencies]\nshared.workspace = true\n",
    );
    // Nested standalone workspace: pins shared@2.2.2.
    let nested = tmp.path().join("nested");
    std::fs::create_dir_all(&nested).expect("mkdir nested");
    std::fs::write(
        nested.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app2\"]\n\n\
             [workspace.dependencies]\nshared = { path = \"shared\", version = \"2.2.2\" }\n",
    )
    .expect("write nested root");
    let app2_dir = nested.join("app2");
    std::fs::create_dir_all(&app2_dir).expect("mkdir app2");
    let app2_manifest = write_manifest(
        &app2_dir,
        "[package]\nname = \"app2\"\nversion = \"1.0.0\"\n\n\
             [dependencies]\nshared.workspace = true\n",
    );

    let ws_names: HashSet<&str> = ["shared", "app", "app2"].iter().copied().collect();
    let mut cache = RootDepCache::new();
    assert_eq!(
        workspace_deps_for_crate(&app_manifest, &ws_names, &mut cache),
        vec![("shared".to_string(), "1.1.1".to_string())],
        "outer crate resolves against the outer root"
    );
    assert_eq!(
        workspace_deps_for_crate(&app2_manifest, &ws_names, &mut cache),
        vec![("shared".to_string(), "2.2.2".to_string())],
        "nested crate must resolve against its own root, not the cached outer one"
    );
}

/// Full-table form rename (`[dependencies.core]` with `package = ...`)
/// resolves like the inline form.
#[test]
fn workspace_deps_for_crate_resolves_full_table_rename() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(
        tmp.path(),
        r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies.core]
package = "anodizer-core"
path = "../core"
version = "0.8.0"
"#,
    );
    let ws_names: HashSet<&str> = ["anodizer-core", "core", "leaf"].iter().copied().collect();
    let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    assert_eq!(
        deps,
        vec![("anodizer-core".to_string(), "0.8.0".to_string())],
        "full-table rename must be waited on under the real package name"
    );
}

/// Standard-table form (`[dependencies.name]\nversion = "..."`) is
/// accepted alongside inline-table / bare-string forms.
#[test]
fn workspace_deps_for_crate_handles_standard_table_form() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(
        tmp.path(),
        r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies.cfgd-core]
path = "../core"
version = "0.4.0"
features = ["extra"]
"#,
    );
    let ws_names: HashSet<&str> = ["cfgd-core", "leaf"].iter().copied().collect();
    let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    assert_eq!(deps, vec![("cfgd-core".to_string(), "0.4.0".to_string())]);
}

/// A renamed dep (`alias = { package = "real", ... }`) must be waited on
/// under its real package name — that is the name cargo resolves against
/// the index. The alias key must NOT be matched, even when a workspace
/// member shares the alias's name ("core" below).
#[test]
fn workspace_deps_for_crate_resolves_package_renamed_dep() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(
        tmp.path(),
        r#"
[package]
name = "leaf"
version = "1.0.0"

[dependencies]
core = { package = "anodizer-core", path = "../core", version = "0.8.0" }
"#,
    );
    let ws_names: HashSet<&str> = ["anodizer-core", "core", "leaf"].iter().copied().collect();
    let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    assert_eq!(
        deps,
        vec![("anodizer-core".to_string(), "0.8.0".to_string())],
        "wait set must carry the real package name, not the alias"
    );
}

/// A rename declared on the workspace root entry — the only place cargo
/// accepts `package =` for an inherited dep — with the leaf inheriting
/// via `core.workspace = true`. The wait set must carry the real package
/// name at the root-pinned version.
#[test]
fn workspace_deps_for_crate_resolves_inherited_renamed_dep() {
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
    let manifest = write_manifest(
        &app_dir,
        r#"
[package]
name = "app"
version = "0.8.0"

[dependencies]
core.workspace = true
"#,
    );
    let ws_names: HashSet<&str> = ["anodizer-core", "app"].iter().copied().collect();
    let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    assert_eq!(
        deps,
        vec![("anodizer-core".to_string(), "0.8.0".to_string())],
        "inherited rename must be waited on under its real package name"
    );
}

/// A plain `<dep>.workspace = true` inherit whose version pin lives on
/// the workspace root entry must be waited on at that version — the same
/// propagation race exists whether the pin is on the leaf or the root.
#[test]
fn workspace_deps_for_crate_resolves_inherited_dep_version_from_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"lib\"]\n\n\
             [workspace.dependencies]\nlib = { path = \"lib\", version = \"0.7.0\" }\n",
    )
    .expect("write workspace root");
    let app_dir = tmp.path().join("app");
    std::fs::create_dir_all(&app_dir).expect("mkdir app");
    let manifest = write_manifest(
        &app_dir,
        r#"
[package]
name = "app"
version = "0.7.0"

[dependencies]
lib.workspace = true
"#,
    );
    let ws_names: HashSet<&str> = ["lib", "app"].iter().copied().collect();
    let deps = workspace_deps_for_crate(&manifest, &ws_names, &mut RootDepCache::new());
    assert_eq!(
        deps,
        vec![("lib".to_string(), "0.7.0".to_string())],
        "root-pinned inherit must be waited on at the root version"
    );
}

/// Disabled gate is a no-op even when deps are present — the master
/// switch protects single-crate workspaces (anodize itself) from the
/// always-on polling cost.
#[test]
fn wait_for_workspace_deps_no_op_when_disabled() {
    let cfg = WaitForWorkspaceDepsConfig {
        enabled: Some(false),
        ..Default::default()
    };
    let log =
        anodizer_core::log::StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    let deps = vec![("would-block".to_string(), "9.9.9".to_string())];
    wait_for_workspace_deps_to_appear("dummy", &deps, &cfg, &log)
        .expect("disabled gate must short-circuit before any HTTP");
}

/// Empty dep list is a no-op even when the gate is enabled — keeps
/// the publisher from paying HTTP-client-construction cost on every
/// crate even after deps have been filtered down to zero.
#[test]
fn wait_for_workspace_deps_no_op_when_no_deps() {
    let cfg = WaitForWorkspaceDepsConfig {
        enabled: Some(true),
        ..Default::default()
    };
    let log =
        anodizer_core::log::StageLogger::new("publish-test", anodizer_core::log::Verbosity::Normal);
    wait_for_workspace_deps_to_appear("dummy", &[], &cfg, &log)
        .expect("empty deps must short-circuit");
}

/// End-to-end: a local HTTP responder serves a populated sparse-index
/// response on first call, so the gate breaks out of its poll loop
/// after exactly one probe. Exercises `probe_dep_on_index` +
/// `parse_index_cksum_for_version` integration without hitting the
/// real crates.io.
#[test]
fn probe_dep_on_index_returns_true_when_version_present() {
    let body = r#"{"name":"cfgd-core","vers":"0.4.0","cksum":"abc","yanked":false}"#;
    let body_len = body.len();
    let resp: &'static str = Box::leak(
        format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
    );
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
    let client =
        anodizer_core::http::blocking_client(std::time::Duration::from_secs(2)).expect("client");
    let url = format!("http://{addr}/cf/gd/cfgd-core");
    let found = probe_dep_on_index(&client, &url, "0.4.0").expect("probe ok");
    assert!(found, "version should be detected as present");
}

/// A 200 with a body that lacks the requested version returns
/// false — the gate must loop and retry, not treat any 2xx as
/// "dep present."
#[test]
fn probe_dep_on_index_returns_false_when_version_absent() {
    // Index has 0.3.0 but we're waiting for 0.4.0.
    let body = r#"{"name":"cfgd-core","vers":"0.3.0","cksum":"old","yanked":false}"#;
    let body_len = body.len();
    let resp: &'static str = Box::leak(
        format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n{body}").into_boxed_str(),
    );
    let (addr, _calls) = spawn_oneshot_http_responder(vec![resp]);
    let client =
        anodizer_core::http::blocking_client(std::time::Duration::from_secs(2)).expect("client");
    let url = format!("http://{addr}/cf/gd/cfgd-core");
    let found = probe_dep_on_index(&client, &url, "0.4.0").expect("probe ok");
    assert!(!found, "missing version must return false, not error");
}

/// A 404 response (crate has never been published) returns false —
/// the gate keeps polling rather than bailing, because the dep's
/// upstream Release.yml run may still be in flight.
#[test]
fn probe_dep_on_index_returns_false_on_404() {
    let (addr, _calls) =
        spawn_oneshot_http_responder(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
    let client =
        anodizer_core::http::blocking_client(std::time::Duration::from_secs(2)).expect("client");
    let url = format!("http://{addr}/cf/gd/cfgd-core");
    let found = probe_dep_on_index(&client, &url, "0.4.0").expect("404 is not an error");
    assert!(!found);
}

// -----------------------------------------------------------------------
// Operator-facing log message helpers.
// -----------------------------------------------------------------------

#[test]
fn run_start_and_done_messages_carry_counts() {
    assert_eq!(
        run_start_message(3),
        "starting cargo publish — processing 3 selected crate(s)"
    );
    assert_eq!(
        run_per_crate_start_message("cfgd-core"),
        "starting per-crate cargo publish for 'cfgd-core'"
    );
    assert_eq!(
        run_done_message(2),
        "finished cargo publish — 2 selected crate(s) processed"
    );
}

#[test]
fn run_no_eligible_crates_warning_names_the_total() {
    let w = run_no_eligible_crates_warning(5);
    assert!(w.starts_with("cargo publisher registered but 0 of 5 effective crate(s)"));
    assert!(w.contains("--crate / --all"));
}

// -----------------------------------------------------------------------
// strip_key_prefix — key-boundary check guarding `version` scans.
// -----------------------------------------------------------------------

#[test]
fn strip_key_prefix_accepts_boundary_chars_only() {
    // Whitespace, `=`, and `.` are valid boundaries after the key.
    assert_eq!(
        strip_key_prefix("version = \"1.0\"", "version"),
        Some(" = \"1.0\"")
    );
    assert_eq!(
        strip_key_prefix("version= \"1.0\"", "version"),
        Some("= \"1.0\"")
    );
    assert_eq!(
        strip_key_prefix("version.workspace = true", "version"),
        Some(".workspace = true")
    );
    // A non-boundary continuation (`versioned`, `versions`) is rejected.
    assert_eq!(strip_key_prefix("versioned = 1", "version"), None);
    assert_eq!(strip_key_prefix("versions = []", "version"), None);
    // Bare key with nothing after it is rejected (not a key=value line).
    assert_eq!(strip_key_prefix("version", "version"), None);
}

// -----------------------------------------------------------------------
// scan_section_version — section scoping + literal/workspace/none.
// -----------------------------------------------------------------------

#[test]
fn scan_section_version_reads_literal_and_strips_inline_comment() {
    let body = "[package]\nname = \"x\"\nversion = \"1.2.3\" # pinned\n";
    assert_eq!(
        scan_section_version(body, "[package]"),
        CargoVersionRef::Literal("1.2.3".to_string())
    );
}

#[test]
fn scan_section_version_detects_dot_and_inline_workspace_inherit() {
    let dot = "[package]\nversion.workspace = true\n";
    assert_eq!(
        scan_section_version(dot, "[package]"),
        CargoVersionRef::Workspace
    );
    let inline = "[package]\nversion = { workspace = true }\n";
    assert_eq!(
        scan_section_version(inline, "[package]"),
        CargoVersionRef::Workspace
    );
}

#[test]
fn scan_section_version_stops_at_sibling_section_but_not_subtable() {
    // The version lives only in a SIBLING section -> None (scan stops at
    // `[dependencies]`, never reaching it).
    let sibling = "[package]\nname = \"x\"\n[dependencies]\nversion = \"9.9.9\"\n";
    assert_eq!(
        scan_section_version(sibling, "[package]"),
        CargoVersionRef::None
    );

    // A sub-table of the logical block does NOT end the scan: the version
    // after `[workspace.package.metadata.x]` is still found.
    let subtable = concat!(
        "[workspace.package]\n",
        "[workspace.package.metadata.docs]\n",
        "foo = 1\n",
        "version = \"7.7.7\"\n",
    );
    assert_eq!(
        scan_section_version(subtable, "[workspace.package]"),
        CargoVersionRef::Literal("7.7.7".to_string())
    );
}

#[test]
fn scan_section_version_skips_comment_lines() {
    let body = "# comment\n[package]\n# version = \"0.0.0\"\nversion = \"4.5.6\"\n";
    assert_eq!(
        scan_section_version(body, "[package]"),
        CargoVersionRef::Literal("4.5.6".to_string())
    );
}

// -----------------------------------------------------------------------
// find_workspace_root_manifest — anchored [workspace] header walk.
// -----------------------------------------------------------------------

/// Walks up from a leaf crate dir to the manifest carrying `[workspace]`.
#[test]
fn find_workspace_root_manifest_walks_up_to_workspace() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/leaf\"]\n",
    )
    .unwrap();
    let leaf = root.path().join("crates").join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::write(
        leaf.join("Cargo.toml"),
        "[package]\nname = \"leaf\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    let found = find_workspace_root_manifest(&leaf).expect("workspace root found");
    assert_eq!(
        std::fs::canonicalize(found).unwrap(),
        std::fs::canonicalize(root.path().join("Cargo.toml")).unwrap()
    );
}

/// A bare `[workspace.package.metadata.docs.rs]` sub-table in a leaf
/// manifest must NOT be mistaken for a workspace root (anchored exact
/// header match, not `starts_with`).
#[test]
fn find_workspace_root_manifest_ignores_metadata_subtable() {
    let root = tempfile::tempdir().expect("tempdir");
    // Leaf-only manifest with a metadata sub-table but no real [workspace].
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"solo\"\n[workspace.package.metadata.docs.rs]\nall-features = true\n",
    )
    .unwrap();
    assert_eq!(find_workspace_root_manifest(root.path()), None);
}

// -----------------------------------------------------------------------
// publish_to_cargo — end-to-end orchestration in dry-run mode.
//
// Dry-run takes the early `ctx.is_dry_run()` branch: it builds the same
// expanded selection, eligibility map (skip/if gating), and topological
// `sorted_names` the live path uses, then emits per-crate start +
// `(dry-run) would run: <cmd>` status lines instead of shelling out. The
// captured status stream is therefore a faithful witness of the ordering
// and gating decisions WITHOUT any network or subprocess. Covers all
// three config modes — single-crate, workspace-lockstep, workspace
// per-crate — for the publish-graph walk.
// -----------------------------------------------------------------------

use anodizer_core::config::{PublishConfig, WorkspaceConfig};
// `Verbosity` / `LogLevel` are not in the file-level imports `super::*`
// re-exports; `StageLogger` is, but an explicit re-import of a glob item
// is permitted (explicit binding wins, same resolved path — no conflict).
use anodizer_core::log::{LogLevel, StageLogger, Verbosity};
use anodizer_core::test_helpers::TestContextBuilder;

/// A crate with a `publish.cargo` block (eligible for the cargo
/// publisher) plus the given workspace-internal `depends_on` edges.
fn cargo_crate(name: &str, deps: &[&str]) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
        publish: Some(PublishConfig {
            cargo: Some(CargoPublishConfig::default()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A crate with the given `publish.cargo` config (so `skip:` / `if:`
/// can be exercised) and `depends_on` edges.
fn cargo_crate_with_cfg(name: &str, deps: &[&str], cfg: CargoPublishConfig) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
        publish: Some(PublishConfig {
            cargo: Some(cfg),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A crate with NO `publish.cargo` block — present in the config (so it
/// participates in `depends_on` resolution) but not eligible to publish.
fn plain_crate(name: &str, deps: &[&str]) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

/// Run `publish_to_cargo` in dry-run mode with a capturing logger and
/// return the ordered list of crate names whose per-crate-start line was
/// emitted — i.e. the order `publish_to_cargo` walked the publish graph.
fn dry_run_publish_order(ctx: &mut Context) -> Vec<String> {
    let (log, cap) = StageLogger::with_capture("publish-test", Verbosity::Normal);
    let selected = ctx.options.selected_crates.clone();
    let mut record = Vec::new();
    publish_to_cargo(ctx, &selected, &log, &mut record).expect("dry-run publish must succeed");
    // Each crate emits `run_per_crate_start_message(name)` exactly once
    // (at verbose), in topological order, before its `(dry-run) would
    // run` line.
    cap.all_messages()
        .into_iter()
        .filter(|(lvl, _)| *lvl == LogLevel::Verbose)
        .filter_map(|(_, m)| {
            m.strip_prefix("starting per-crate cargo publish for '")
                .and_then(|rest| rest.strip_suffix('\''))
                .map(str::to_string)
        })
        .collect()
}

/// Single-crate mode: one eligible crate with no deps publishes itself
/// and only itself. The expanded selection is exactly `[the crate]`.
#[test]
fn publish_to_cargo_single_crate_mode_publishes_the_one_crate() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![cargo_crate("solo", &[])])
        .selected_crates(vec!["solo".to_string()])
        .dry_run(true)
        .build();
    assert_eq!(dry_run_publish_order(&mut ctx), vec!["solo"]);
}

/// Workspace-lockstep mode: every crate lives under top-level
/// `crates:` and a single `--crate cfgd` selection expands transitively
/// to its dependency chain, published dependencies-first.
#[test]
fn publish_to_cargo_lockstep_orders_dependency_before_dependent() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            cargo_crate("cfgd", &["cfgd-core"]),
            cargo_crate("cfgd-core", &[]),
        ])
        // Select only the leaf binary; the dependency must be pulled in
        // by expand_with_transitive_deps and published FIRST.
        .selected_crates(vec!["cfgd".to_string()])
        .dry_run(true)
        .build();
    assert_eq!(dry_run_publish_order(&mut ctx), vec!["cfgd-core", "cfgd"]);
}

/// Workspace-lockstep, three-level chain: a→b→c must publish c, b, a in
/// strict topological order regardless of declaration order.
#[test]
fn publish_to_cargo_lockstep_orders_three_level_chain() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            cargo_crate("a", &["b"]),
            cargo_crate("b", &["c"]),
            cargo_crate("c", &[]),
        ])
        .selected_crates(vec!["a".to_string()])
        .dry_run(true)
        .build();
    assert_eq!(dry_run_publish_order(&mut ctx), vec!["c", "b", "a"]);
}

/// Workspace per-crate mode: crates live under `workspaces:` (NOT
/// top-level `crates:`). `all_crates` overlays the workspace members,
/// and a cross-member dep is still ordered dependency-first.
#[test]
fn publish_to_cargo_per_crate_workspace_orders_across_members() {
    let core_ws = WorkspaceConfig {
        name: "core-ws".to_string(),
        crates: vec![cargo_crate("cfgd-core", &[])],
        ..Default::default()
    };
    let app_ws = WorkspaceConfig {
        name: "app-ws".to_string(),
        crates: vec![cargo_crate("cfgd", &["cfgd-core"])],
        ..Default::default()
    };
    let mut ctx = TestContextBuilder::new()
        .workspaces(vec![core_ws, app_ws])
        .selected_crates(vec!["cfgd".to_string()])
        .dry_run(true)
        .build();
    // cfgd-core lives in a DIFFERENT workspace than cfgd, yet the cross-
    // workspace depends_on edge still forces it published first.
    assert_eq!(dry_run_publish_order(&mut ctx), vec!["cfgd-core", "cfgd"]);
}

/// A dependency without its own `publish.cargo` block is pulled into the
/// graph for ordering but is itself NOT published — only cargo-eligible
/// crates appear in the emitted order, and the eligible dependent still
/// publishes.
#[test]
fn publish_to_cargo_skips_dep_lacking_cargo_block() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            cargo_crate("app", &["helper"]),
            plain_crate("helper", &[]),
        ])
        .selected_crates(vec!["app".to_string()])
        .dry_run(true)
        .build();
    // `helper` has no publish.cargo → not in cargo_cfgs → filtered out of
    // `publishable`; only `app` is published.
    assert_eq!(dry_run_publish_order(&mut ctx), vec!["app"]);
}

/// `publish.cargo.skip: true` removes the crate from the eligible set
/// even though it carries a cargo block — the other eligible crate still
/// publishes.
#[test]
fn publish_to_cargo_honors_skip_true() {
    let skipped = cargo_crate_with_cfg(
        "skipme",
        &[],
        CargoPublishConfig {
            skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
            ..Default::default()
        },
    );
    let mut ctx = TestContextBuilder::new()
        .crates(vec![skipped, cargo_crate("keepme", &[])])
        .selected_crates(vec!["skipme".to_string(), "keepme".to_string()])
        .dry_run(true)
        .build();
    assert_eq!(dry_run_publish_order(&mut ctx), vec!["keepme"]);
}

/// `publish.cargo.if: "false"` (a falsy `if` condition) gates the crate
/// out of the eligible set — the live path renders the template and
/// drops the crate when it evaluates falsy.
#[test]
fn publish_to_cargo_honors_falsy_if_condition() {
    let gated = cargo_crate_with_cfg(
        "gated",
        &[],
        CargoPublishConfig {
            if_condition: Some("false".to_string()),
            ..Default::default()
        },
    );
    let mut ctx = TestContextBuilder::new()
        .crates(vec![gated, cargo_crate("open", &[])])
        .selected_crates(vec!["gated".to_string(), "open".to_string()])
        .dry_run(true)
        .build();
    assert_eq!(dry_run_publish_order(&mut ctx), vec!["open"]);
}

/// `if: "true"` keeps the crate eligible — the truthy branch of the
/// `if` gate is the complement of the falsy test above.
#[test]
fn publish_to_cargo_keeps_crate_when_if_condition_truthy() {
    let gated = cargo_crate_with_cfg(
        "gated",
        &[],
        CargoPublishConfig {
            if_condition: Some("true".to_string()),
            ..Default::default()
        },
    );
    let mut ctx = TestContextBuilder::new()
        .crates(vec![gated])
        .selected_crates(vec!["gated".to_string()])
        .dry_run(true)
        .build();
    assert_eq!(dry_run_publish_order(&mut ctx), vec!["gated"]);
}

/// The `--skip=cargo` stage gate short-circuits `publish_to_cargo`
/// before any per-crate work: no crate-start lines are emitted even
/// though an eligible crate is selected.
#[test]
fn publish_to_cargo_short_circuits_when_stage_skipped() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![cargo_crate("solo", &[])])
        .selected_crates(vec!["solo".to_string()])
        .skip_stages(vec!["cargo".to_string()])
        .dry_run(true)
        .build();
    assert!(
        dry_run_publish_order(&mut ctx).is_empty(),
        "--skip=cargo must publish nothing"
    );
}

/// The dry-run command line for each crate reflects its per-crate
/// `publish.cargo` config (here `--no-verify` + the implicit
/// `--allow-dirty`), proving the cfg→argv wiring survives the
/// orchestration, not just the unit `publish_command` call.
#[test]
fn publish_to_cargo_dry_run_emits_configured_flags() {
    let crate_cfg = cargo_crate_with_cfg(
        "flagged",
        &[],
        CargoPublishConfig {
            no_verify: Some(true),
            ..Default::default()
        },
    );
    let mut ctx = TestContextBuilder::new()
        .crates(vec![crate_cfg])
        .selected_crates(vec!["flagged".to_string()])
        .dry_run(true)
        .build();
    let (log, cap) = StageLogger::with_capture("publish-test", Verbosity::Normal);
    let selected = ctx.options.selected_crates.clone();
    let mut record = Vec::new();
    publish_to_cargo(&mut ctx, &selected, &log, &mut record).expect("dry-run ok");
    let dry_line = cap
        .all_messages()
        .into_iter()
        .find_map(|(_, m)| m.strip_prefix("(dry-run) would run: ").map(str::to_string))
        .expect("dry-run command line emitted");
    assert!(
        dry_line.contains("cargo publish -p flagged"),
        "missing publish target: {dry_line}"
    );
    assert!(
        dry_line.contains("--no-verify"),
        "configured --no-verify not threaded into dry-run cmd: {dry_line}"
    );
    assert!(
        dry_line.contains("--allow-dirty"),
        "implicit --allow-dirty missing: {dry_line}"
    );
}

/// Diamond graph (d depends on b and c, both depend on a) publishes `a`
/// first and `d` last; the two middle crates appear in the
/// deterministic alphabetical seed order the topo-sort guarantees.
#[test]
fn publish_to_cargo_orders_diamond_dependency_graph() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![
            cargo_crate("d", &["b", "c"]),
            cargo_crate("b", &["a"]),
            cargo_crate("c", &["a"]),
            cargo_crate("a", &[]),
        ])
        .selected_crates(vec!["d".to_string()])
        .dry_run(true)
        .build();
    let order = dry_run_publish_order(&mut ctx);
    assert_eq!(order.first().map(String::as_str), Some("a"), "root first");
    assert_eq!(order.last().map(String::as_str), Some("d"), "sink last");
    // b and c are independent middles — deterministic alpha seed order.
    assert_eq!(order, vec!["a", "b", "c", "d"]);
}

// -----------------------------------------------------------------------
// cargo_publish_plan — the #25 single-source-of-truth extraction.
//
// Asserts the resolved plan directly (order + per-crate cfgs + per-crate
// versions) rather than only the dry-run log, so a regression in the
// version/cfg resolution surfaces even if the ordering stays correct.
// Covered across all three config modes per the all-modes requirement.
// -----------------------------------------------------------------------

/// Quiet logger for plan resolution — the plan emits skip/if status
/// lines we don't inspect here, so a non-capturing logger suffices.
fn quiet_log() -> StageLogger {
    StageLogger::new("publish-test", Verbosity::Normal)
}

/// Write a `[package]` manifest pinning `version` under a fresh subdir of
/// `root` and return a cargo-eligible `CrateConfig` rooted there, so the
/// plan's per-crate version resolution reads a REAL on-disk version
/// instead of the cwd manifest. `cfg` controls the publish.cargo block.
fn disk_crate(
    root: &std::path::Path,
    name: &str,
    version: &str,
    deps: &[&str],
    cfg: CargoPublishConfig,
) -> CrateConfig {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("mkdir crate dir");
    std::fs::write(
        dir.join("Cargo.toml"),
        format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\n"),
    )
    .expect("write manifest");
    CrateConfig {
        name: name.to_string(),
        path: dir.display().to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        depends_on: Some(deps.iter().map(|s| s.to_string()).collect()),
        publish: Some(PublishConfig {
            cargo: Some(cfg),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Single-crate mode: the plan resolves exactly the one selected crate,
/// carries its cargo cfg, and reads the crate's own on-disk version.
#[test]
fn cargo_publish_plan_single_crate_resolves_order_cfg_and_version() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let solo = disk_crate(
        tmp.path(),
        "solo",
        "1.2.3",
        &[],
        CargoPublishConfig {
            no_verify: Some(true),
            ..Default::default()
        },
    );
    let mut ctx = TestContextBuilder::new()
        .tag("v9.9.9") // release version differs from the on-disk version
        .crates(vec![solo])
        .selected_crates(vec!["solo".to_string()])
        .build();
    let plan =
        cargo_publish_plan(&mut ctx, &["solo".to_string()], &quiet_log()).expect("plan resolves");

    assert_eq!(plan.order, vec!["solo"]);
    // cfg survives into the plan map verbatim.
    assert_eq!(plan.cfgs.get("solo").and_then(|c| c.no_verify), Some(true));
    // Version is read from the crate's own manifest, not the release tag.
    assert_eq!(plan.versions.get("solo").map(String::as_str), Some("1.2.3"));
}

/// Workspace-lockstep mode: a `--crate` selection of the leaf expands
/// transitively, the plan orders the dependency first, and EACH crate's
/// own on-disk version is resolved (mixed cadence: 0.4.0 vs 0.4.1).
#[test]
fn cargo_publish_plan_lockstep_orders_deps_and_resolves_both_versions() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let core = disk_crate(
        tmp.path(),
        "cfgd-core",
        "0.4.0",
        &[],
        CargoPublishConfig::default(),
    );
    let app = disk_crate(
        tmp.path(),
        "cfgd",
        "0.4.1",
        &["cfgd-core"],
        CargoPublishConfig::default(),
    );
    let mut ctx = TestContextBuilder::new()
        .tag("v0.4.0")
        .crates(vec![app, core])
        .selected_crates(vec!["cfgd".to_string()])
        .build();
    let plan =
        cargo_publish_plan(&mut ctx, &["cfgd".to_string()], &quiet_log()).expect("plan resolves");

    assert_eq!(plan.order, vec!["cfgd-core", "cfgd"]);
    assert_eq!(
        plan.versions.get("cfgd-core").map(String::as_str),
        Some("0.4.0")
    );
    // Distinct per-crate version proves the plan reads each manifest.
    assert_eq!(plan.versions.get("cfgd").map(String::as_str), Some("0.4.1"));
    // Both eligible crates have a (default) cargo cfg recorded.
    assert!(plan.cfgs.contains_key("cfgd-core"));
    assert!(plan.cfgs.contains_key("cfgd"));
}

/// Workspace per-crate mode: members live under `workspaces:` and the
/// plan overlays them into `all_crates`, orders a cross-member dep
/// first, and records each member's cfg/version from disk.
#[test]
fn cargo_publish_plan_per_crate_workspace_overlays_members() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let core = disk_crate(
        tmp.path(),
        "cfgd-core",
        "0.3.0",
        &[],
        CargoPublishConfig::default(),
    );
    let app = disk_crate(
        tmp.path(),
        "cfgd",
        "2.0.0",
        &["cfgd-core"],
        CargoPublishConfig::default(),
    );
    let core_ws = WorkspaceConfig {
        name: "core-ws".to_string(),
        crates: vec![core],
        ..Default::default()
    };
    let app_ws = WorkspaceConfig {
        name: "app-ws".to_string(),
        crates: vec![app],
        ..Default::default()
    };
    let mut ctx = TestContextBuilder::new()
        .tag("v2.0.0")
        .workspaces(vec![core_ws, app_ws])
        .selected_crates(vec!["cfgd".to_string()])
        .build();
    let plan =
        cargo_publish_plan(&mut ctx, &["cfgd".to_string()], &quiet_log()).expect("plan resolves");

    assert_eq!(plan.order, vec!["cfgd-core", "cfgd"]);
    // `all_crates` is the overlay both members are drawn from.
    let names: HashSet<&str> = plan.all_crates.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains("cfgd-core") && names.contains("cfgd"));
    // Cross-member crates resolve their distinct on-disk versions.
    assert_eq!(
        plan.versions.get("cfgd-core").map(String::as_str),
        Some("0.3.0")
    );
    assert_eq!(plan.versions.get("cfgd").map(String::as_str), Some("2.0.0"));
}

/// A `skip: true` crate is dropped from BOTH the cfg map and the order —
/// the plan is the single source of truth, so the skip must not leave a
/// dangling cfg entry that a later consumer could publish.
#[test]
fn cargo_publish_plan_skip_true_removes_from_cfgs_and_order() {
    let skipped = cargo_crate_with_cfg(
        "skipme",
        &[],
        CargoPublishConfig {
            skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
            ..Default::default()
        },
    );
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![skipped, cargo_crate("keepme", &[])])
        .selected_crates(vec!["skipme".to_string(), "keepme".to_string()])
        .build();
    let plan = cargo_publish_plan(
        &mut ctx,
        &["skipme".to_string(), "keepme".to_string()],
        &quiet_log(),
    )
    .expect("plan resolves");

    assert_eq!(plan.order, vec!["keepme"]);
    assert!(
        !plan.cfgs.contains_key("skipme"),
        "skip=true must drop the cfg entry too: {:?}",
        plan.cfgs.keys().collect::<Vec<_>>()
    );
}

/// A falsy `if:` condition drops the crate from the plan; the surviving
/// crate keeps its cfg + order. Complements the skip test (separate gate).
#[test]
fn cargo_publish_plan_falsy_if_drops_crate() {
    let gated = cargo_crate_with_cfg(
        "gated",
        &[],
        CargoPublishConfig {
            if_condition: Some("false".to_string()),
            ..Default::default()
        },
    );
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![gated, cargo_crate("open", &[])])
        .selected_crates(vec!["gated".to_string(), "open".to_string()])
        .build();
    let plan = cargo_publish_plan(
        &mut ctx,
        &["gated".to_string(), "open".to_string()],
        &quiet_log(),
    )
    .expect("plan resolves");
    assert_eq!(plan.order, vec!["open"]);
    assert!(!plan.cfgs.contains_key("gated"));
}

/// Empty selection (no `--crate`) means "all eligible crates": every
/// crate with a publish.cargo block lands in the plan, ordered topo.
#[test]
fn cargo_publish_plan_empty_selection_takes_all_eligible() {
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![cargo_crate("app", &["lib"]), cargo_crate("lib", &[])])
        .build();
    let plan = cargo_publish_plan(&mut ctx, &[], &quiet_log()).expect("plan resolves");
    assert_eq!(plan.order, vec!["lib", "app"]);
}

/// A malformed `if:` template (unterminated Tera expression) propagates
/// the render error out of plan resolution rather than silently keeping
/// or dropping the crate.
#[test]
fn cargo_publish_plan_propagates_if_render_error() {
    let bad = cargo_crate_with_cfg(
        "bad",
        &[],
        CargoPublishConfig {
            // Unbalanced delimiters — Tera render must error.
            if_condition: Some("{{ unterminated".to_string()),
            ..Default::default()
        },
    );
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .crates(vec![bad])
        .selected_crates(vec!["bad".to_string()])
        .build();
    // CargoPublishPlan is not Debug, so match rather than expect_err.
    let chain = match cargo_publish_plan(&mut ctx, &["bad".to_string()], &quiet_log()) {
        Ok(_) => panic!("malformed if template must surface as Err"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        chain.contains("if") || chain.contains("template") || chain.contains("render"),
        "expected an if-template render error in the chain: {chain}"
    );
}

// -----------------------------------------------------------------------
// publish_to_cargo — empty-plan early return + no-eligible publisher run.
// -----------------------------------------------------------------------

/// When the expanded selection matches no cargo-eligible crate, the plan
/// is empty and `publish_to_cargo` returns Ok without emitting any
/// per-crate start line (the empty-`sorted_names` early return).
#[test]
fn publish_to_cargo_empty_plan_is_clean_noop() {
    let mut ctx = TestContextBuilder::new()
        .crates(vec![cargo_crate("real", &[])])
        // Select a name that doesn't exist → expanded selection is empty
        // of any eligible crate → plan order is empty.
        .selected_crates(vec!["ghost".to_string()])
        .dry_run(true)
        .build();
    assert!(
        dry_run_publish_order(&mut ctx).is_empty(),
        "no eligible crate selected ⇒ no per-crate work"
    );
}

/// `CargoPublisher::run` with zero cargo-configured crates emits the
/// canonical no-eligible warn and returns empty evidence (the
/// `eligible == 0` short-circuit), without delegating into the loop.
#[test]
fn cargo_publisher_run_warns_when_no_cargo_crate_configured() {
    use anodizer_core::Publisher;
    // A crate with NO publish.cargo block ⇒ count_cargo_configured == 0.
    let mut ctx = TestContextBuilder::new()
        .crates(vec![plain_crate("plain", &[])])
        .selected_crates(vec!["plain".to_string()])
        .dry_run(true)
        .build();
    let ev = CargoPublisher::new().run(&mut ctx).expect("run ok");
    assert_eq!(ev.publisher, "cargo");
    // No crate published ⇒ no recorded yank targets, no primary ref.
    assert!(decode_cargo_yank_targets(&ev.extra).is_empty());
    assert!(ev.primary_ref.is_none());
}

/// `skips_on_nightly` is true for the cargo publisher — nightly/snapshot
/// builds carry a non-publishable version and must not hit crates.io.
#[test]
fn cargo_publisher_skips_on_nightly() {
    use anodizer_core::Publisher;
    assert!(CargoPublisher::new().skips_on_nightly());
}

/// `decode_cargo_yank_targets` returns an empty vec for any non-Cargo
/// evidence variant, so rollback treats a foreign-evidence run as
/// "nothing published" and no-ops instead of panicking.
#[test]
fn decode_cargo_yank_targets_empty_for_non_cargo_variant() {
    // `PublishEvidenceExtra::None` is the default/empty variant — any
    // non-Cargo variant must decode to an empty target list.
    let extra = anodizer_core::PublishEvidenceExtra::default();
    assert!(decode_cargo_yank_targets(&extra).is_empty());
}

/// `programmatic_rollback_on_failure` is gated on a non-empty recorded
/// target set: a run that published nothing stays inert (no rollback),
/// while a run that recorded a yank target opts into rollback.
#[test]
fn programmatic_rollback_gated_on_recorded_targets() {
    use anodizer_core::Publisher;
    let p = CargoPublisher::new();

    let mut empty = anodizer_core::PublishEvidence::new("cargo");
    empty.extra = encode_cargo_yank_targets(&[]);
    assert!(
        !p.programmatic_rollback_on_failure(&empty),
        "empty record ⇒ no rollback"
    );

    let mut nonempty = anodizer_core::PublishEvidence::new("cargo");
    nonempty.extra = encode_cargo_yank_targets(&[CargoYankTarget {
        name: "x".into(),
        version: "1.0.0".into(),
        registry: None,
        index: None,
    }]);
    assert!(
        p.programmatic_rollback_on_failure(&nonempty),
        "recorded target ⇒ rollback"
    );
}

/// Dry-run rollback takes the `is_dry_run` branch: it returns Ok WITHOUT
/// spawning `cargo`. "No spawn" is proven by shadowing `cargo` with the
/// argv-recording stub: any reached `cargo yank` would land in the argv
/// log, so an empty log witnesses the dry-run short-circuit firing
/// before the loop. The stub is PREPENDED to PATH (never a wholesale
/// replacement, which would make every concurrent PATH-resolved spawn
/// in this binary flaky). Gated unix: mutates PATH and uses unix paths.
#[cfg(unix)]
#[test]
fn rollback_dry_run_returns_ok_without_spawning_cargo() {
    use anodizer_core::Publisher;
    let tmp = tempfile::tempdir().expect("tempdir");
    let argv_log = tmp.path().join("argv.log");
    let new_path = super::partial_rollback_tests::install_cargo_stub(tmp.path(), &argv_log, "none");
    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .dry_run(true)
        .build();
    // Two recorded targets so the loop WOULD spawn twice if reached.
    let targets = vec![
        CargoYankTarget {
            name: "a".into(),
            version: "1.0.0".into(),
            registry: None,
            index: None,
        },
        CargoYankTarget {
            name: "b".into(),
            version: "2.0.0".into(),
            registry: None,
            index: None,
        },
    ];
    let mut evidence = anodizer_core::PublishEvidence::new("cargo");
    evidence.extra = encode_cargo_yank_targets(&targets);

    let _g = anodizer_core::test_helpers::env::env_mutex()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("PATH").ok();
    // SAFETY: serialised by env_mutex; paired with the restore below.
    // env-ok: PATH stub prepend under env_mutex (serializes all PATH mutators); restored on drop
    unsafe { std::env::set_var("PATH", &new_path) };
    let rb = CargoPublisher::new().rollback(&mut ctx, &evidence);
    // SAFETY: restore PATH (paired with the set above).
    unsafe {
        match prev {
            // env-ok: PATH stub prepend under env_mutex (serializes all PATH mutators); restored on drop
            Some(p) => std::env::set_var("PATH", p),
            // env-ok: PATH stub prepend under env_mutex (serializes all PATH mutators); restored on drop
            None => std::env::remove_var("PATH"),
        }
    }
    rb.expect("dry-run rollback must short-circuit to Ok before spawning");
    assert!(
        super::partial_rollback_tests::read_argv_log(&argv_log).is_empty(),
        "dry-run rollback must never spawn cargo"
    );
}

// -----------------------------------------------------------------------
// extract_version_pin — the three TOML dep shapes + the None branches.
//
// workspace_deps_for_crate tests above exercise the happy paths end to
// end; these pin the helper directly so each early-return branch (bare
// string, inline-table workspace-inherit, inline-table version, standard
// table workspace-inherit, standard table version, no-version) is
// observable in isolation.
// -----------------------------------------------------------------------

fn dep_item(toml_body: &str, key: &str) -> toml_edit::Item {
    let doc = toml_body.parse::<toml_edit::DocumentMut>().expect("parse");
    doc["dependencies"][key].clone()
}

#[test]
fn extract_version_pin_bare_string() {
    let item = dep_item("[dependencies]\nfoo = \"1.2.3\"\n", "foo");
    assert_eq!(extract_version_pin(&item), Some("1.2.3".to_string()));
}

#[test]
fn extract_version_pin_inline_table_version() {
    let item = dep_item(
        "[dependencies]\nfoo = { path = \"../foo\", version = \"4.5.6\" }\n",
        "foo",
    );
    assert_eq!(extract_version_pin(&item), Some("4.5.6".to_string()));
}

#[test]
fn extract_version_pin_inline_table_workspace_inherit_is_none() {
    let item = dep_item("[dependencies]\nfoo = { workspace = true }\n", "foo");
    assert_eq!(extract_version_pin(&item), None);
}

#[test]
fn extract_version_pin_inline_table_no_version_is_none() {
    // path-only inline table — nothing to poll for.
    let item = dep_item("[dependencies]\nfoo = { path = \"../foo\" }\n", "foo");
    assert_eq!(extract_version_pin(&item), None);
}

#[test]
fn extract_version_pin_standard_table_version() {
    let item = dep_item(
        "[dependencies.foo]\npath = \"../foo\"\nversion = \"7.8.9\"\n",
        "foo",
    );
    assert_eq!(extract_version_pin(&item), Some("7.8.9".to_string()));
}

#[test]
fn extract_version_pin_standard_table_workspace_inherit_is_none() {
    let item = dep_item("[dependencies.foo]\nworkspace = true\n", "foo");
    assert_eq!(extract_version_pin(&item), None);
}

#[test]
fn extract_version_pin_standard_table_no_version_is_none() {
    let item = dep_item("[dependencies.foo]\npath = \"../foo\"\n", "foo");
    assert_eq!(extract_version_pin(&item), None);
}

// -----------------------------------------------------------------------
// workspace_deps_for_crate — degraded-input branches (unreadable /
// unparseable manifest) must return an empty vec so the gate no-ops
// rather than erroring out an otherwise-valid publish.
// -----------------------------------------------------------------------

#[test]
fn workspace_deps_for_crate_missing_manifest_returns_empty() {
    let ws: HashSet<&str> = ["a"].iter().copied().collect();
    let nonexistent = std::path::Path::new("/nonexistent/dir/does/not/exist/Cargo.toml");
    assert!(workspace_deps_for_crate(nonexistent, &ws, &mut RootDepCache::new()).is_empty());
}

#[test]
fn workspace_deps_for_crate_unparseable_manifest_returns_empty() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(tmp.path(), "this is = = not valid toml [[[");
    let ws: HashSet<&str> = ["a"].iter().copied().collect();
    assert!(workspace_deps_for_crate(&manifest, &ws, &mut RootDepCache::new()).is_empty());
}

/// A `[target.<cfg>]` whose value is not a dependency table (e.g. a
/// stray scalar) is skipped without panicking — the recursion guards
/// against malformed target sections.
#[test]
fn workspace_deps_for_crate_skips_non_table_target_value() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let manifest = write_manifest(
        tmp.path(),
        r#"
[package]
name = "leaf"
version = "1.0.0"

[target]
"cfg(unix)" = "not-a-table"

[dependencies]
real = { path = "../real", version = "1.0.0" }
"#,
    );
    let ws: HashSet<&str> = ["real", "leaf"].iter().copied().collect();
    // The malformed target scalar is skipped; the normal dep is still found.
    assert_eq!(
        workspace_deps_for_crate(&manifest, &ws, &mut RootDepCache::new()),
        vec![("real".to_string(), "1.0.0".to_string())]
    );
}

// -----------------------------------------------------------------------
// scan_section_version — workspace-inherit branches inside the scan that
// the read_cargo_toml_version tests reach only indirectly.
// -----------------------------------------------------------------------

/// `version.workspace = true` immediately followed by another value on
/// the same logical line is classified Workspace (the dot-form branch).
#[test]
fn scan_section_version_dot_workspace_true() {
    let body = "[package]\nname = \"x\"\nversion.workspace = true\n";
    assert_eq!(
        scan_section_version(body, "[package]"),
        CargoVersionRef::Workspace
    );
}

/// A workspace-inherit manifest whose workspace root has NO
/// `[workspace.package].version` resolves to None (the `_ => None` arm
/// in read_cargo_toml_version) — the publish path then falls back to the
/// release version.
#[test]
fn read_cargo_toml_version_workspace_root_without_version_is_none() {
    let ws_root = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        ws_root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"leaf\"]\n[workspace.package]\nedition = \"2021\"\n",
    )
    .unwrap();
    let leaf = ws_root.path().join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::write(
        leaf.join("Cargo.toml"),
        "[package]\nname = \"leaf\"\nversion.workspace = true\n",
    )
    .unwrap();
    // [workspace.package] exists but carries no `version` ⇒ None.
    assert_eq!(read_cargo_toml_version(leaf.to_str().unwrap()), None);
}

// -----------------------------------------------------------------------
// run_cargo_publish_with_retry — exhaustion path (all retries fail).
//
// The recovery + fast-fail paths are covered above; this pins the third
// arm: a propagation-style failure that NEVER clears must retry the full
// PUBLISH_PROPAGATION_RETRIES budget, then surface the last failure.
// -----------------------------------------------------------------------

/// A stub that emits a propagation-style stderr on EVERY invocation must
/// be retried exactly `PUBLISH_PROPAGATION_RETRIES` times (initial + the
/// rest) and then surface the failure — never loop forever, never
/// succeed.
#[cfg(unix)]
#[test]
fn run_cargo_publish_with_retry_exhausts_then_surfaces() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let counter = tmp.path().join("counter");
    let stub = tmp.path().join("cargo");
    // Always fail with a propagation-shaped stderr; bump the counter so
    // we can assert the exact attempt count.
    let script = format!(
        "#!/bin/sh\n\
             n=$(cat {counter} 2>/dev/null || echo 0)\n\
             n=$((n+1))\n\
             echo $n > {counter}\n\
             echo 'error: no matching package named `dep` found' >&2\n\
             exit 101\n",
        counter = counter.display(),
    );
    std::fs::write(&stub, script).expect("write stub");

    // Route through `sh` to dodge the ETXTBSY race (see the recovery
    // test above for the rationale).
    let cmd = vec![
        "sh".to_string(),
        stub.display().to_string(),
        "publish".to_string(),
    ];
    let log = StageLogger::new("publish-test", Verbosity::Normal);
    let err = run_cargo_publish_with_retry(
        &cmd,
        "stub publish",
        &log,
        std::time::Duration::from_millis(1),
        None,
    )
    .expect_err("persistent propagation failure must surface after exhaustion");
    let chain = format!("{err:#}");
    assert!(
        chain.contains("no matching package") || chain.contains("exit code"),
        "expected last failure in chain: {chain}"
    );

    let n: u32 = std::fs::read_to_string(&counter)
        .expect("counter")
        .trim()
        .parse()
        .expect("u32");
    assert_eq!(
        n, PUBLISH_PROPAGATION_RETRIES,
        "must retry the full budget before surfacing"
    );
}
