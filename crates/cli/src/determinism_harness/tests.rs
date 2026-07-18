use super::artifacts::{
    HEAD_SAMPLE_BYTES, TAIL_SAMPLE_BYTES, infer_stage_from_path, should_capture_full,
};
use super::*;
use anodizer_core::AllowListEntry;

#[test]
fn stage_id_from_token_round_trips_every_variant() {
    for s in StageId::iter() {
        assert_eq!(
            StageId::from_token(s.as_str()),
            Some(s),
            "from_token is not the inverse of as_str for {s:?}"
        );
    }
    // An unknown token resolves to None rather than a silent default.
    assert_eq!(StageId::from_token("not-a-stage"), None);
    assert_eq!(StageId::from_token(""), None);
}

#[test]
fn require_c_toolchain_errors_on_msvc_target_without_clang_cl() {
    let targets = vec!["x86_64-pc-windows-msvc".to_string()];
    let err = require_c_toolchain(&targets, false, |_| false).unwrap_err();
    assert!(
        err.to_string().contains("clang-cl"),
        "error must name clang-cl as the missing tool: {err}"
    );
}

#[test]
fn require_c_toolchain_ok_on_msvc_target_with_clang_cl_present() {
    let targets = vec!["aarch64-pc-windows-msvc".to_string()];
    require_c_toolchain(&targets, false, |_| true).unwrap();
}

#[test]
fn require_c_toolchain_ok_on_non_msvc_target_without_clang_cl() {
    let targets = vec!["x86_64-unknown-linux-gnu".to_string()];
    require_c_toolchain(&targets, false, |_| false).unwrap();
}

#[test]
fn require_c_toolchain_errors_on_empty_targets_when_host_is_msvc() {
    require_c_toolchain(&[], true, |_| false).unwrap_err();
}

#[test]
fn require_c_toolchain_ok_on_empty_targets_when_host_is_not_msvc() {
    require_c_toolchain(&[], false, |_| false).unwrap();
}

#[test]
fn require_c_toolchain_ok_on_mixed_targets_with_clang_cl_present() {
    let targets = vec![
        "x86_64-pc-windows-msvc".to_string(),
        "x86_64-unknown-linux-gnu".to_string(),
    ];
    require_c_toolchain(&targets, false, |_| true).unwrap();
}

fn empty_harness() -> Harness {
    Harness {
        repo_root: PathBuf::from("/tmp/unused"),
        commit: "deadbeef".into(),
        stages: vec![StageId::Archive, StageId::Checksum],
        explicit_stages: vec![StageId::Archive, StageId::Checksum],
        require_tools: false,
        runs: 2,
        sde: 1_715_000_000,
        allowlist: AllowList::default(),
        report_path: PathBuf::from("/tmp/unused/report.json"),
        inject_drift: None,
        targets: None,
        preserve_dist: None,
        version_hint: String::new(),
        child_snapshot: true,
        docker_backend_hint: None,
        docker_configs: Vec::new(),
        docker_declared: false,
        crate_name: None,
        verbosity: Verbosity::Normal,
        config_tools: BTreeMap::new(),
        disk_abs_floor_bytes: anodizer_core::disk::DEFAULT_ABS_FLOOR_BYTES,
        disk_safety_factor: anodizer_core::disk::DEFAULT_SAFETY_FACTOR,
    }
}

/// A harness whose compile-time allow-list excuses the given glob
/// patterns (e.g. `*.deb`) — the intrinsically-non-deterministic members
/// the transitive-derivation rule should excuse.
fn harness_with_allow(patterns: &[&str]) -> Harness {
    let mut h = empty_harness();
    h.allowlist = AllowList {
        compile_time: patterns
            .iter()
            .map(|p| AllowListEntry {
                artifact: (*p).to_string(),
                reason: format!("test: {p} is intrinsically non-deterministic"),
            })
            .collect(),
        runtime: Vec::new(),
    };
    h
}

fn run_with_files(
    h: &Harness,
    runs: Vec<Vec<(&str, &[u8])>>,
) -> Vec<BTreeMap<String, ArtifactInfo>> {
    // Synthesize per-run hash maps as if the child build pipeline
    // had emitted each file. Bypasses the actual subprocess so unit
    // tests don't depend on cargo / rustup / git.
    let _ = h;
    runs.into_iter()
        .map(|files| {
            let mut map = BTreeMap::new();
            for (name, bytes) in files {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(bytes);
                let digest = format!("sha256:{:x}", hasher.finalize());
                let head_len = bytes.len().min(HEAD_SAMPLE_BYTES);
                let tail_sample = if bytes.len() > HEAD_SAMPLE_BYTES + TAIL_SAMPLE_BYTES {
                    bytes[bytes.len() - TAIL_SAMPLE_BYTES..].to_vec()
                } else {
                    Vec::new()
                };
                // Mirror production's full-byte retention so the
                // transitive-derivation rule (incl. marker-renamed combined
                // files) can reconstruct members.
                let full = if should_capture_full(name, bytes) {
                    Some(bytes.to_vec())
                } else {
                    None
                };
                map.insert(
                    name.into(),
                    ArtifactInfo {
                        hash: digest,
                        size_bytes: bytes.len() as u64,
                        relative_path: format!("dist/{}", name),
                        stage: infer_stage_from_path(name),
                        head_sample: bytes[..head_len].to_vec(),
                        tail_sample,
                        full,
                    },
                );
            }
            map
        })
        .collect()
}

#[test]
fn harness_report_shape_serializes_correctly() {
    let h = empty_harness();
    let runs = run_with_files(
        &h,
        vec![
            vec![("anodizer_0.2.1.tar.gz", b"hello")],
            vec![("anodizer_0.2.1.tar.gz", b"hello")],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(report.schema_version, 1);
    assert_eq!(report.runs, 2);
    assert_eq!(report.commit, "deadbeef");
    assert_eq!(report.stages_under_test, vec!["archive", "checksum"]);
    assert_eq!(report.drift_count, 0);
    assert_eq!(report.artifacts.len(), 1);
    assert!(report.artifacts[0].deterministic);
    assert!(report.artifacts[0].hash.is_some());
    assert!(report.artifacts[0].hashes.is_empty());

    // Round-trip JSON.
    let s = serde_json::to_string_pretty(&report).unwrap();
    let back: DeterminismReport = serde_json::from_str(&s).unwrap();
    assert_eq!(back, report);
}

#[test]
fn harness_diffs_artifacts_by_sha256() {
    let h = empty_harness();
    let runs = run_with_files(
        &h,
        vec![
            vec![("stable.tar.gz", b"hello"), ("drifting.tar.gz", b"first")],
            vec![("stable.tar.gz", b"hello"), ("drifting.tar.gz", b"second")],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(report.drift_count, 1);
    assert_eq!(report.drift.len(), 1);
    assert_eq!(report.drift[0].artifact, "drifting.tar.gz");
    assert_eq!(report.drift[0].hashes.len(), 2);
    assert_ne!(report.drift[0].hashes[0], report.drift[0].hashes[1]);
    // Diagnostic: the drift row must carry a `differing_bytes_summary`
    // so future fix-cycles aren't blind.
    let summary = report.drift[0]
        .differing_bytes_summary
        .as_deref()
        .expect("drift row must populate differing_bytes_summary");
    assert!(
        summary.contains("offset 0x0"),
        "summary should point at byte 0 for diverging single-byte prefixes. got={summary}"
    );

    // Both artifacts appear in `artifacts`, with the stable one
    // marked deterministic and the drifting one marked not.
    let stable = report
        .artifacts
        .iter()
        .find(|a| a.name == "stable.tar.gz")
        .unwrap();
    let drifting = report
        .artifacts
        .iter()
        .find(|a| a.name == "drifting.tar.gz")
        .unwrap();
    assert!(stable.deterministic);
    assert!(!drifting.deterministic);
    assert!(drifting.hash.is_none());
    assert_eq!(drifting.hashes.len(), 2);
}

// --- Transitive-derivation rule for aggregate artifacts --------------

/// No false positive: a combined checksums file drifts solely because an
/// allow-listed member (`*.deb`, signed) changed its line. Every
/// differing member is allow-listed ⇒ the aggregate is excused ⇒
/// `drift_count == 0`.
#[test]
fn aggregate_excused_when_only_allowlisted_member_drifts() {
    let h = harness_with_allow(&["*.deb"]);
    let run0 = b"hashA  bar.tar.gz\ndeb000  app_1.0_amd64.deb\n" as &[u8];
    let run1 = b"hashA  bar.tar.gz\ndeb111  app_1.0_amd64.deb\n" as &[u8];
    let runs = run_with_files(
        &h,
        vec![
            vec![("app_checksums.txt", run0), ("bar.tar.gz", b"stable")],
            vec![("app_checksums.txt", run1), ("bar.tar.gz", b"stable")],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 0,
        "aggregate drift caused only by an allow-listed member must not fail"
    );
    let agg = report
        .artifacts
        .iter()
        .find(|a| a.name == "app_checksums.txt")
        .expect("checksums row present");
    assert!(!agg.deterministic);
    assert!(
        agg.nondeterministic_reason
            .as_deref()
            .is_some_and(|r| r.contains("app_1.0_amd64.deb")),
        "excuse must name the differing allow-listed member: {:?}",
        agg.nondeterministic_reason
    );
}

/// No masking: a GATED (supposedly byte-reproducible) member's line
/// changed in the aggregate. Even with NO separate row for that member
/// (only the aggregate is emitted), the aggregate must FAIL and name the
/// offending member.
#[test]
fn aggregate_fails_when_gated_member_drifts_even_if_member_row_suppressed() {
    let h = harness_with_allow(&["*.deb"]);
    // Only the checksums file is emitted — the gated `bar.tar.gz` member
    // has no independent row, so the aggregate is the sole signal.
    let run0 = b"t000  bar.tar.gz\ndeb000  app_1.0_amd64.deb\n" as &[u8];
    let run1 = b"t111  bar.tar.gz\ndeb111  app_1.0_amd64.deb\n" as &[u8];
    let runs = run_with_files(
        &h,
        vec![
            vec![("app_checksums.txt", run0)],
            vec![("app_checksums.txt", run1)],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 1,
        "a gated member drifting inside the aggregate must surface as drift"
    );
    assert!(
        report
            .drift
            .iter()
            .any(|d| d.artifact.contains("bar.tar.gz")),
        "the offending gated member must be named: {:?}",
        report.drift
    );
    // The allow-listed deb that ALSO changed must NOT be reported.
    assert!(
        !report.drift.iter().any(|d| d.artifact.contains(".deb")),
        "allow-listed member must not be reported as a regression"
    );
}

/// A member appearing (added) is judged by its own allow-list status: a
/// new GATED member fails; a removed ALLOW-LISTED member is excused.
#[test]
fn aggregate_judges_additions_and_removals_by_member_status() {
    // Addition of a gated member ⇒ fail.
    let h = harness_with_allow(&["*.deb"]);
    let add0 = b"a000  a.tar.gz\ndeb000  x_1.0_amd64.deb\n" as &[u8];
    let add1 = b"a000  a.tar.gz\ndeb000  x_1.0_amd64.deb\nb000  b.tar.gz\n" as &[u8];
    let runs = run_with_files(
        &h,
        vec![
            vec![("c_checksums.txt", add0)],
            vec![("c_checksums.txt", add1)],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(report.drift_count, 1, "added gated member must fail");
    assert!(report.drift.iter().any(|d| d.artifact.contains("b.tar.gz")));

    // Removal of an allow-listed member ⇒ excused.
    let rem0 = b"a000  a.tar.gz\ndeb000  x_1.0_amd64.deb\n" as &[u8];
    let rem1 = b"a000  a.tar.gz\n" as &[u8];
    let runs = run_with_files(
        &h,
        vec![
            vec![("c_checksums.txt", rem0)],
            vec![("c_checksums.txt", rem1)],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 0,
        "removing an allow-listed member must be excused"
    );
}

/// Fail-closed: an aggregate that drifts but cannot be parsed (or whose
/// drift is structural, with no member unit changing) is treated as real
/// drift, never excused.
#[test]
fn aggregate_fails_closed_on_unparseable_or_structural_drift() {
    let h = harness_with_allow(&["*.deb"]);
    // Structural drift: identical member set, only line ORDER changed.
    let s0 = b"a  a.tar.gz\nd  x_1.0_amd64.deb\n" as &[u8];
    // Reordered but same lines ⇒ parsed unit sets are identical ⇒ no
    // member differs ⇒ fail closed (cannot attribute the byte drift).
    let s1 = b"d  x_1.0_amd64.deb\na  a.tar.gz\nz  z\n" as &[u8];
    let runs = run_with_files(
        &h,
        vec![vec![("s_checksums.txt", s0)], vec![("s_checksums.txt", s1)]],
    );
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 1,
        "an aggregate whose drift cannot be attributed must fail closed"
    );
}

/// The artifacts.json manifest aggregate is judged member-by-member: a
/// gated archive whose recorded digest changed fails; the same change to
/// an allow-listed deb is excused.
#[test]
fn artifacts_manifest_transitive_rule() {
    let h = harness_with_allow(&["*.deb"]);
    let gated0 = br#"[
          {"kind":"archive","path":"./dist/a.tar.gz","name":"a.tar.gz","metadata":{"sha256":"aaaa"}},
          {"kind":"linux_package","path":"./dist/a_1.0_amd64.deb","name":"a_1.0_amd64.deb","metadata":{"sha256":"dddd"}}
        ]"# as &[u8];
    // The gated archive's recorded digest changed ⇒ regression.
    let gated1 = br#"[
          {"kind":"archive","path":"./dist/a.tar.gz","name":"a.tar.gz","metadata":{"sha256":"bbbb"}},
          {"kind":"linux_package","path":"./dist/a_1.0_amd64.deb","name":"a_1.0_amd64.deb","metadata":{"sha256":"dddd"}}
        ]"#;
    let runs = run_with_files(
        &h,
        vec![
            vec![("artifacts.json", gated0)],
            vec![("artifacts.json", gated1)],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 1,
        "gated archive digest drift must fail"
    );
    assert!(report.drift.iter().any(|d| d.artifact.contains("a.tar.gz")));

    // Only the allow-listed deb digest changed ⇒ excused.
    let deb1 = br#"[
          {"kind":"archive","path":"./dist/a.tar.gz","name":"a.tar.gz","metadata":{"sha256":"aaaa"}},
          {"kind":"linux_package","path":"./dist/a_1.0_amd64.deb","name":"a_1.0_amd64.deb","metadata":{"sha256":"eeee"}}
        ]"#;
    let runs = run_with_files(
        &h,
        vec![
            vec![("artifacts.json", gated0)],
            vec![("artifacts.json", deb1)],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 0,
        "deb-only digest drift must be excused"
    );
}

/// Finding 1: the combined-checksums aggregate is recognized by the
/// `combined = "true"` manifest marker, not the filename suffix. An
/// operator-renamed `SHA512SUMS` (which the suffix heuristic misses) is
/// still subject to the transitive-derivation rule: excused when only an
/// allow-listed member line drifts, failed when a gated member line drifts.
#[test]
fn marker_named_combined_file_obeys_transitive_rule() {
    let h = harness_with_allow(&["*.deb"]);
    // Manifest flags `SHA512SUMS` as combined; identical across runs so the
    // manifest itself doesn't drift — only the SHA512SUMS file does.
    let manifest = br#"[
          {"kind":"archive","path":"./dist/bar.tar.gz","name":"bar.tar.gz","metadata":{"sha256":"barbar"}},
          {"kind":"linux_package","path":"./dist/app_1.0_amd64.deb","name":"app_1.0_amd64.deb","metadata":{"sha256":"debdeb"}},
          {"kind":"checksum","path":"./dist/SHA512SUMS","name":"SHA512SUMS","metadata":{"combined":"true"}}
        ]"# as &[u8];
    // The suffix heuristic alone does NOT recognize this file.
    assert!(anodizer_core::determinism::aggregate_kind_for("SHA512SUMS").is_none());

    // Excused: only the allow-listed deb line drifts.
    let sums0 = b"barbar  bar.tar.gz\ndeb000  app_1.0_amd64.deb\n" as &[u8];
    let sums1 = b"barbar  bar.tar.gz\ndeb111  app_1.0_amd64.deb\n" as &[u8];
    let runs = run_with_files(
        &h,
        vec![
            vec![
                ("artifacts.json", manifest),
                ("SHA512SUMS", sums0),
                ("bar.tar.gz", b"stable"),
            ],
            vec![
                ("artifacts.json", manifest),
                ("SHA512SUMS", sums1),
                ("bar.tar.gz", b"stable"),
            ],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 0,
        "marker-named combined file drift from an allow-listed member must be excused: {:?}",
        report.drift
    );
    let agg = report
        .artifacts
        .iter()
        .find(|a| a.name == "SHA512SUMS")
        .expect("SHA512SUMS classified as an aggregate row");
    assert!(!agg.deterministic);
    assert!(
        agg.nondeterministic_reason
            .as_deref()
            .is_some_and(|r| r.contains("app_1.0_amd64.deb")),
        "excuse must name the drifting allow-listed member: {:?}",
        agg.nondeterministic_reason
    );

    // Fail: the gated archive line drifts inside the same renamed file.
    let g0 = b"bar000  bar.tar.gz\ndeb000  app_1.0_amd64.deb\n" as &[u8];
    let g1 = b"bar111  bar.tar.gz\ndeb000  app_1.0_amd64.deb\n" as &[u8];
    let runs = run_with_files(
        &h,
        vec![
            vec![
                ("artifacts.json", manifest),
                ("SHA512SUMS", g0),
                ("bar.tar.gz", b"stable"),
            ],
            vec![
                ("artifacts.json", manifest),
                ("SHA512SUMS", g1),
                ("bar.tar.gz", b"stable"),
            ],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 1,
        "a gated member drifting inside the renamed combined file must fail"
    );
    assert!(
        report
            .drift
            .iter()
            .any(|d| d.artifact.contains("bar.tar.gz")),
        "the gated member must be named: {:?}",
        report.drift
    );
}

/// Finding 2 (realistic permutation): a cfgd-style recut where the archive
/// is byte-stable (gated) but the SBOM, cosign bundle, and detached
/// signature drift. Their drift is excused (compile-time `*.cdx.json` +
/// runtime `*.cosign.bundle` / `*.sig`), so `artifacts.json` — whose
/// recorded digests for those members moved — is excused too. Flipping the
/// gated archive then proves a real regression still surfaces.
#[test]
fn artifacts_manifest_recut_excuses_only_nondeterministic_members() {
    let mut h = empty_harness();
    h.allowlist = AllowList {
        compile_time: vec![
            AllowListEntry {
                artifact: "*.cdx.json".into(),
                reason: "CycloneDX SBOM carries a random serial UUID".into(),
            },
            AllowListEntry {
                artifact: "*.deb".into(),
                reason: "GPG-signed nfpm deb".into(),
            },
        ],
        runtime: vec![
            AllowListEntry {
                artifact: "*.cosign.bundle".into(),
                reason: "cosign ECDSA random nonce".into(),
            },
            AllowListEntry {
                artifact: "*.sig".into(),
                reason: "cosign detached signature".into(),
            },
        ],
    };
    let manifest = |arch: &str, sbom: &str, bundle: &str, sig: &str| {
        format!(
                r#"[
  {{"kind":"archive","path":"./dist/app.tar.gz","name":"app.tar.gz","metadata":{{"sha256":"{arch}"}}}},
  {{"kind":"sbom","path":"./dist/app.cdx.json","name":"app.cdx.json","metadata":{{"sha256":"{sbom}"}}}},
  {{"kind":"signature","path":"./dist/app.tar.gz.cosign.bundle","name":"app.tar.gz.cosign.bundle","metadata":{{"sha256":"{bundle}"}}}},
  {{"kind":"signature","path":"./dist/app.tar.gz.sig","name":"app.tar.gz.sig","metadata":{{"sha256":"{sig}"}}}},
  {{"kind":"checksum","path":"./dist/checksums.txt","name":"checksums.txt","metadata":{{"combined":"true"}}}},
  {{"kind":"metadata","path":"./dist/metadata.json","name":"metadata.json","metadata":{{}}}},
  {{"kind":"uploadable_file","path":"./dist/install.sh","name":"install.sh","metadata":{{"sha256":"inst"}}}}
]"#
            )
            .into_bytes()
    };
    let checksums = b"arch  app.tar.gz\n" as &[u8];
    let files = |m: &[u8], sbom: &[u8], bundle: &[u8], sig: &[u8]| -> Vec<(String, Vec<u8>)> {
        vec![
            ("artifacts.json".into(), m.to_vec()),
            ("app.tar.gz".into(), b"archive-stable".to_vec()),
            ("app.cdx.json".into(), sbom.to_vec()),
            ("app.tar.gz.cosign.bundle".into(), bundle.to_vec()),
            ("app.tar.gz.sig".into(), sig.to_vec()),
            ("checksums.txt".into(), checksums.to_vec()),
            ("metadata.json".into(), b"meta-stable".to_vec()),
            ("install.sh".into(), b"#!/bin/sh\n".to_vec()),
        ]
    };
    fn borrow(v: &[(String, Vec<u8>)]) -> Vec<(&str, &[u8])> {
        v.iter().map(|(n, b)| (n.as_str(), b.as_slice())).collect()
    }

    // Recut: archive stable, SBOM + bundle + sig all drift.
    let r0 = files(
        &manifest("ARCH", "SB0", "BUN0", "SIG0"),
        b"sbom-0",
        b"bundle-0",
        b"sig-0",
    );
    let r1 = files(
        &manifest("ARCH", "SB1", "BUN1", "SIG1"),
        b"sbom-1",
        b"bundle-1",
        b"sig-1",
    );
    let runs = run_with_files(&h, vec![borrow(&r0), borrow(&r1)]);
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 0,
        "a recut that only moves SBOM/cosign-bundle/sig must be fully excused: {:?}",
        report.drift
    );

    // Regression: the gated archive itself drifts (file + recorded digest).
    let g0 = files(
        &manifest("ARCH0", "SB0", "BUN0", "SIG0"),
        b"sbom-0",
        b"bundle-0",
        b"sig-0",
    );
    let mut g1 = files(
        &manifest("ARCH1", "SB0", "BUN0", "SIG0"),
        b"sbom-0",
        b"bundle-0",
        b"sig-0",
    );
    // Flip the archive bytes too so the file-level row also drifts.
    for entry in &mut g1 {
        if entry.0 == "app.tar.gz" {
            entry.1 = b"archive-DRIFTED".to_vec();
        }
    }
    let runs = run_with_files(&h, vec![borrow(&g0), borrow(&g1)]);
    let report = h.build_report(runs);
    assert!(
        report.drift_count >= 1,
        "a gated archive regression must surface"
    );
    assert!(
        report
            .drift
            .iter()
            .any(|d| d.artifact.contains("app.tar.gz")),
        "the gated archive must be named in drift: {:?}",
        report.drift
    );
}

/// Finding 2 (nested recursion): `artifacts.json` lists `checksums.txt` as
/// a combined member; the inner `checksums.txt` drifts. The transitive rule
/// recurses — excused when the inner drift is an allow-listed member,
/// failed when the inner drift is a gated member.
#[test]
fn nested_aggregate_recursion_judges_inner_members() {
    let h = harness_with_allow(&["*.cdx.json"]);
    // Manifest records no digest for checksums.txt, so its content token is
    // the whole entry; bumping `size` makes the `checksums.txt` member of
    // artifacts.json drift, forcing the recursion path.
    let manifest = |size: u32| {
        format!(
                r#"[
  {{"kind":"archive","path":"./dist/app.tar.gz","name":"app.tar.gz","metadata":{{"sha256":"AAAA"}}}},
  {{"kind":"sbom","path":"./dist/app.cdx.json","name":"app.cdx.json","metadata":{{"sha256":"SB{size}"}}}},
  {{"kind":"checksum","path":"./dist/checksums.txt","name":"checksums.txt","metadata":{{"combined":"true"}},"size":{size}}}
]"#
            )
            .into_bytes()
    };

    // Excused: the inner checksums.txt drifts only at its allow-listed SBOM
    // line.
    let ck0 = b"arch  app.tar.gz\nsbom0  app.cdx.json\n" as &[u8];
    let ck1 = b"arch  app.tar.gz\nsbom1  app.cdx.json\n" as &[u8];
    let m0 = manifest(100);
    let m1 = manifest(101);
    let runs = run_with_files(
        &h,
        vec![
            vec![("artifacts.json", m0.as_slice()), ("checksums.txt", ck0)],
            vec![("artifacts.json", m1.as_slice()), ("checksums.txt", ck1)],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 0,
        "nested aggregate excused when inner drift is allow-listed: {:?}",
        report.drift
    );

    // Fail: the inner checksums.txt drifts at its GATED archive line.
    let bad0 = b"arch0  app.tar.gz\nsbom0  app.cdx.json\n" as &[u8];
    let bad1 = b"arch1  app.tar.gz\nsbom0  app.cdx.json\n" as &[u8];
    let runs = run_with_files(
        &h,
        vec![
            vec![("artifacts.json", m0.as_slice()), ("checksums.txt", bad0)],
            vec![("artifacts.json", m1.as_slice()), ("checksums.txt", bad1)],
        ],
    );
    let report = h.build_report(runs);
    assert!(
        report.drift_count >= 1,
        "nested aggregate must fail when an inner gated member drifts"
    );
    assert!(
        report
            .drift
            .iter()
            .any(|d| d.artifact.contains("checksums.txt") || d.artifact.contains("app.tar.gz")),
        "the failing nested member chain must be named: {:?}",
        report.drift
    );
}

/// Every file a normal run emits classifies (zero `Unclassified`), and a
/// genuinely unregistered file is a hard fail even when byte-stable.
#[test]
fn unclassified_gates_on_byte_drift_not_on_classification() {
    let h = harness_with_allow(&["*.flatpak"]);
    // artifacts.json declares `install.sh` so it classifies as a tracked
    // primary via manifest membership (its `.sh` extension is unknown).
    let manifest = br#"[
          {"kind":"archive","path":"./dist/foo.tar.gz","name":"foo.tar.gz","metadata":{"sha256":"aaaa"}},
          {"kind":"uploadable_file","path":"./dist/install.sh","name":"install.sh","metadata":{"sha256":"bbbb"}},
          {"kind":"metadata","path":"./dist/metadata.json","name":"metadata.json","metadata":{}}
        ]"# as &[u8];
    let checksums = b"aaaa  foo.tar.gz\n" as &[u8];
    let files: Vec<(&str, &[u8])> = vec![
        ("foo.tar.gz", b"archive-bytes"),       // archive (infer)
        ("foo.tar.gz.sig", b"sig-bytes"),       // sidecar of a primary
        ("app_1.0_amd64.deb", b"deb-bytes"),    // nfpm (infer)
        ("app_1.0_amd64.flatpak", b"fp-bytes"), // allow-listed primary
        ("anodizer.1", b"man-bytes"),           // man page (infer)
        ("install.sh", b"sh-bytes"),            // manifest member primary
        ("metadata.json", b"meta-bytes"),       // explicit tracked primary
        ("app_checksums.txt", checksums),       // registered aggregate
        ("artifacts.json", manifest),           // registered aggregate
    ];
    let runs = run_with_files(&h, vec![files.clone(), files]);
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 0,
        "a fully-classified, byte-stable run must not fail. drift={:?}",
        report.drift
    );
    assert!(
        !report.drift.iter().any(|d| d
            .differing_bytes_summary
            .as_deref()
            .is_some_and(|s| s.contains("unclassified"))),
        "no file should be unclassified: {:?}",
        report.drift
    );

    // A genuinely unregistered file that is BYTE-STABLE passes: the gate
    // is byte-equality, not classification. A stable file cannot mask
    // member drift (identical aggregate bytes ⇒ identical members), so
    // there is nothing to fail.
    let stable: Vec<(&str, &[u8])> = vec![("mystery.xyz", b"same")];
    let report = h.build_report(run_with_files(&h, vec![stable.clone(), stable]));
    assert_eq!(
        report.drift_count, 0,
        "a byte-stable unclassified file must NOT fail: {:?}",
        report.drift
    );

    // The same unclassified file, now DRIFTING across runs, IS a hard
    // fail: no aggregate rule can excuse it, so it reads as a real
    // regression.
    let drifting: Vec<Vec<(&str, &[u8])>> =
        vec![vec![("mystery.xyz", b"one")], vec![("mystery.xyz", b"two")]];
    let report = h.build_report(run_with_files(&h, drifting));
    assert_eq!(
        report.drift_count, 1,
        "a drifting unclassified file must hard-fail: {:?}",
        report.drift
    );
    assert!(
        report.drift[0]
            .differing_bytes_summary
            .as_deref()
            .is_some_and(|s| s.contains("unclassified")),
        "the drift reason must flag the unclassified file: {:?}",
        report.drift
    );
}

#[test]
fn harness_excludes_allowlisted_artifacts_from_drift() {
    let mut h = empty_harness();
    // `.flatpak` is genuinely allow-listed (intrinsically non-reproducible
    // OSTree commit metadata); use it as the example so the fixture does
    // not model a now-gated format as non-deterministic.
    h.allowlist.compile_time.push(AllowListEntry {
        artifact: "*.flatpak".into(),
        reason: "flatpak build-bundle OSTree commit metadata not byte-stable".into(),
    });
    let runs = run_with_files(
        &h,
        vec![
            vec![("anodizer_0.2.1_linux_amd64.flatpak", b"flatpak-bytes-A")],
            vec![("anodizer_0.2.1_linux_amd64.flatpak", b"flatpak-bytes-B")],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(
        report.drift_count, 0,
        "allowlisted artifact must not bump drift_count"
    );
    let row = &report.artifacts[0];
    assert_eq!(row.name, "anodizer_0.2.1_linux_amd64.flatpak");
    assert!(!row.deterministic);
    assert_eq!(
        row.nondeterministic_reason.as_deref(),
        Some("flatpak build-bundle OSTree commit metadata not byte-stable")
    );
    assert_eq!(row.hashes.len(), 2);
}

#[test]
fn harness_treats_missing_artifact_in_one_run_as_drift() {
    let h = empty_harness();
    let runs = run_with_files(&h, vec![vec![("only-in-run-1.tar.gz", b"present")], vec![]]);
    let report = h.build_report(runs);
    assert_eq!(report.drift_count, 1);
    assert_eq!(report.drift[0].artifact, "only-in-run-1.tar.gz");
    assert!(report.drift[0].hashes.iter().any(|h| h == "<missing>"));
}

#[test]
fn matches_artifact_pattern_handles_glob_and_exact() {
    assert!(matches_artifact_pattern("*.crate", "foo.crate"));
    assert!(!matches_artifact_pattern("*.crate", "foo.tar.gz"));
    assert!(matches_artifact_pattern("exact.bin", "exact.bin"));
    assert!(!matches_artifact_pattern("exact.bin", "other.bin"));
}

#[test]
fn stage_id_round_trips_to_string() {
    assert_eq!(StageId::Build.as_str(), "build");
    assert_eq!(StageId::Archive.as_str(), "archive");
    assert_eq!(StageId::Sbom.as_str(), "sbom");
    assert_eq!(StageId::Sign.as_str(), "sign");
    assert_eq!(StageId::Checksum.as_str(), "checksum");
}

/// A minimal requested set (`build,archive,sbom,sign,checksum`) MUST
/// drive `compute_extra_skip` to emit produce-stages like `nfpm`,
/// `nsis`, `msi`, `dmg`, `pkg`, `snapcraft`, `source`, `flatpak`,
/// `appbundle`, `srpm`, `upx`, `makeself`. Without this, the child
/// release subprocess attempts e.g. `nfpm pkg --packager deb` on a
/// macOS shard and dies with `No such file or directory`. `notarize`
/// is NOT expected here — it is a `SIDE_EFFECT_STAGES` member, added
/// to the child `--skip=` unconditionally by `compute_skip_arg`, so
/// `compute_extra_skip` deliberately filters it out of the complement.
#[test]
fn harness_extra_skip_with_default_stages_includes_nfpm() {
    let stages = vec![
        StageId::Build,
        StageId::Archive,
        StageId::Sbom,
        StageId::Sign,
        StageId::Checksum,
    ];
    let extra = compute_extra_skip(&stages);
    for name in [
        "nfpm",
        "nsis",
        "msi",
        "dmg",
        "pkg",
        "snapcraft",
        "source",
        "flatpak",
        "appbundle",
        "srpm",
        "upx",
        "makeself",
    ] {
        assert!(
            extra.iter().any(|s| s == name),
            "compute_extra_skip(default-stages) missing `{name}`: {extra:?}"
        );
    }
    // notarize is a side-effect stage now; compute_extra_skip must not
    // double-list it (compute_skip_arg adds it from SIDE_EFFECT_STAGES).
    assert!(
        !extra.iter().any(|s| s == "notarize"),
        "notarize must not appear in the complement set: {extra:?}"
    );
}

/// PRESERVE_SET stages MUST never appear in the extra skip list,
/// regardless of whether the operator listed them via `--stages=`.
/// Skipping `validate` would let bad configs through; skipping
/// `before` would silently drop user hooks; skipping `templatefiles`
/// would leave downstream stages without their materialized inputs.
#[test]
fn harness_extra_skip_omits_preserve_set() {
    let stages = vec![StageId::Build, StageId::Archive];
    let extra = compute_extra_skip(&stages);
    for name in PRESERVE_SET {
        assert!(
            !extra.iter().any(|s| s == name),
            "compute_extra_skip emitted PRESERVE_SET stage `{name}`: {extra:?}"
        );
    }
}

/// `changelog` is NOT in PRESERVE_SET — its output isn't a built
/// artifact the harness diffs, `use=github-native` is inherently
/// non-deterministic (depends on remote API state), and the harness
/// env strips `GITHUB_TOKEN` for hermeticity so the stage would
/// bail on tag-push runs. The publish-only path still runs the
/// changelog stage with the real token, so the GitHub Release body
/// is unaffected.
#[test]
fn harness_extra_skip_includes_changelog() {
    let stages = vec![StageId::Build, StageId::Archive];
    let extra = compute_extra_skip(&stages);
    assert!(
        extra.iter().any(|s| s == "changelog"),
        "compute_extra_skip missing `changelog`: {extra:?}"
    );
}

/// If the operator names a produce-stage in `--stages=`, the harness
/// MUST NOT add it to the extra skip list — that would defeat the
/// whole point of asking for it.
#[test]
fn harness_extra_skip_omits_requested_stages() {
    let stages = vec![StageId::Build, StageId::Archive, StageId::Sign];
    let extra = compute_extra_skip(&stages);
    for name in ["build", "archive", "sign"] {
        assert!(
            !extra.iter().any(|s| s == name),
            "compute_extra_skip dropped requested stage `{name}`: {extra:?}"
        );
    }
}

/// An EXPLICIT binary-consuming subset (`--stages=appimage,flatpak`)
/// MUST keep `build` enabled in the child pipeline even though the
/// operator did not type `build`. Skipping it produces no binary, which
/// trips the binary-artifact guard (flatpak is guard-armed) and aborts
/// the run before either AppImage or flatpak is ever diffed.
#[test]
fn harness_extra_skip_retains_build_for_binary_consuming_subset() {
    for stages in [
        vec![StageId::Appimage, StageId::Flatpak],
        vec![StageId::Flatpak],
        vec![StageId::Nfpm],
        vec![StageId::Archive],
    ] {
        let extra = compute_extra_skip(&stages);
        assert!(
            !extra.iter().any(|s| s == "build"),
            "compute_extra_skip skipped `build` for binary-consuming subset {stages:?}: {extra:?}"
        );
    }
}

/// A source-only subset (`--stages=source`) needs no compiled binary,
/// so `build` stays a normal skip candidate — the harness must not pay
/// for a full release build it does not diff.
#[test]
fn harness_extra_skip_skips_build_for_source_only_subset() {
    for stages in [
        vec![StageId::Source],
        vec![StageId::CargoPackage],
        vec![StageId::Srpm],
    ] {
        let extra = compute_extra_skip(&stages);
        assert!(
            extra.iter().any(|s| s == "build"),
            "compute_extra_skip kept `build` for source-only subset {stages:?}: {extra:?}"
        );
    }
}

/// `SIDE_EFFECT_STAGES` entries are added back unconditionally by
/// the runner's `compute_skip_arg`, so the harness's complement set
/// shouldn't double-list them.
#[test]
fn harness_extra_skip_excludes_side_effect_stages() {
    use anodizer_core::determinism_runner::SIDE_EFFECT_STAGES;
    let stages = vec![StageId::Build];
    let extra = compute_extra_skip(&stages);
    for &name in SIDE_EFFECT_STAGES.iter() {
        assert!(
            !extra.iter().any(|s| s == name),
            "compute_extra_skip double-listed side-effect stage `{name}`: {extra:?}"
        );
    }
}

#[test]
fn report_drift_count_matches_drift_array_len() {
    let h = empty_harness();
    let runs = run_with_files(
        &h,
        vec![
            vec![("a.tar.gz", b"x"), ("b.tar.gz", b"y"), ("c.tar.gz", b"z")],
            vec![
                ("a.tar.gz", b"x"),
                ("b.tar.gz", b"y-different"),
                ("c.tar.gz", b"z-different"),
            ],
        ],
    );
    let report = h.build_report(runs);
    assert_eq!(report.drift.len() as u32, report.drift_count);
    assert_eq!(report.drift_count, 2);
}

/// No configured `dockers_v2` is an unconditional Ok no-op — there is
/// nothing to byte-compare, so it is never coverage loss, even when
/// the operator explicitly requested the docker stage.
#[test]
fn docker_stage_no_config_is_ok_even_when_explicit() {
    let tmp = tempfile::TempDir::new().unwrap();
    // A stray repo-root Dockerfile must NOT be built when no dockers_v2 is
    // configured — the empty-config skip is the gate, not the file.
    std::fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    let h = empty_harness();
    assert!(h.docker_configs.is_empty());
    let env = HashMap::new();
    assert!(
        h.run_docker_stage(tmp.path(), &env, true).is_ok(),
        "no dockers_v2 config must be a harmless no-op regardless of intent"
    );
}

/// Production parity: a crate that DECLARES `dockers_v2` but whose entries
/// are all LEGITIMATELY skipped in this context (truthy `skip:` — e.g.
/// `skip: "{{ .IsSnapshot }}"` under the harness's snapshot mode — or an
/// empty-rendered conditional dockerfile) must CLEANLY SKIP (warn, not
/// error) even under `--require-tools` / explicit `--stages=docker`. That
/// mirrors production (`DockerStage::run` builds nothing, no error);
/// hard-failing would be a false FAILURE that reddens every determinism run
/// of a skip-on-snapshot config. Resolution ERRORS, by contrast, hard-fail
/// upstream in `resolve_docker_configs` (see
/// `resolve_docker_configs_propagates_render_error`), so an empty set here
/// is never a swallowed error.
#[test]
fn docker_stage_declared_but_all_skipped_warns_not_errors_even_when_explicit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut h = empty_harness();
    h.docker_declared = true; // crate declares dockers_v2 ...
    h.docker_configs = Vec::new(); // ... but every entry was legitimately skipped
    let env = HashMap::new();
    // Explicit request (require-tools / --stages=docker) must NOT hard-fail
    // the all-skipped case — production cleanly skips it.
    assert!(
        h.run_docker_stage(tmp.path(), &env, true).is_ok(),
        "declared-but-all-skipped docker must warn-and-skip, not error, even under an \
             explicit request (production parity)"
    );
    // The host-default (non-explicit) path behaves identically.
    assert!(
        h.run_docker_stage(tmp.path(), &env, false).is_ok(),
        "declared-but-all-skipped docker must warn-and-skip on a host-default run too"
    );
}

/// The podman backend hint short-circuits before the buildx probe, so
/// this exercises the explicit-vs-auto fork deterministically on any
/// host (docker need not be installed).
///
/// Explicitly-requested (`--stages=…,docker`): the harness must HARD
/// ERROR rather than warn-and-skip. Silently skipping a stage the
/// caller asked it to byte-verify is false coverage — a
/// non-reproducible image could ship while the gate reports green.
#[test]
fn docker_stage_podman_explicit_request_is_hard_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    let mut h = empty_harness();
    h.docker_backend_hint = Some("podman".into());
    h.docker_configs = vec![ResolvedDockerConfig {
        dockerfile: "Dockerfile".into(),
        extra_files: Vec::new(),
        build_args: Vec::new(),
    }];
    let env = HashMap::new();
    let err = h
        .run_docker_stage(tmp.path(), &env, true)
        .expect_err("explicit docker request under podman must fail the run, not skip");
    let msg = err.to_string();
    assert!(
        msg.contains("podman") && msg.contains("Refusing"),
        "error must explain the false-coverage refusal: {msg}"
    );
}

/// An installer stage the operator explicitly typed into `--stages`
/// whose tool is absent must HARD ERROR at the harness gate, mirroring
/// the docker contract above. Silently warn-skipping a stage the
/// caller asked it to byte-verify is false coverage — a
/// non-reproducible installer could ship while the gate reports green.
///
/// Drives the real [`Harness::gate_installer_stages`] (the smallest
/// entry that invokes the gate `run()` itself calls) with an
/// always-absent probe, so the assertion holds regardless of which
/// installer tools the host has installed.
#[test]
fn installer_explicit_request_missing_tool_is_hard_error() {
    let mut h = empty_harness();
    h.stages = vec![StageId::Build, StageId::Nsis];
    // Operator typed these stages, so they enter the explicit set that the
    // hard-fail gate keys on.
    h.explicit_stages = h.stages.clone();
    let err = h
        .gate_installer_stages(&h.stages.clone(), |_tool| false)
        .expect_err("explicit installer request with a missing tool must fail the run, not skip");
    let msg = err.to_string();
    assert!(
        msg.contains("nsis") && msg.contains("makensis"),
        "error must name the missing stage and its tool: {msg}"
    );
    assert!(
        msg.contains("--stages"),
        "error must tell the operator how to opt out: {msg}"
    );
}

/// A non-explicit (auto-included) installer stage with a missing tool
/// must warn-and-drop, not error, so the gate's available set still
/// returns the non-installer stages. Pins the fork the hard-error
/// test's sibling branch depends on.
#[test]
fn installer_non_explicit_missing_tool_warns_and_drops() {
    // `stages` (the operator's explicit set) holds only Build, so the
    // Nsis stage reaching the gate is treated as non-explicit.
    let h = empty_harness();
    let effective = vec![StageId::Build, StageId::Nsis];
    let available = h
        .gate_installer_stages(&effective, |_tool| false)
        .expect("non-explicit missing tool must warn-and-drop, not error");
    assert_eq!(
        available,
        vec![StageId::Build],
        "missing-tool installer must be dropped; non-installer stages pass through"
    );
}

/// Under CI's `--require-tools` the SAME host-default (non-explicit) stage
/// that `installer_non_explicit_missing_tool_warns_and_drops` lets warn-skip
/// must instead HARD-FAIL — closing the silent under-coverage hole left by
/// removing the per-shard `det_stages` naming. `explicit_stages` stays empty
/// (the operator typed nothing); only `require_tools` flips the contract.
#[test]
fn require_tools_hard_fails_host_default_missing_tool() {
    let mut h = empty_harness();
    h.explicit_stages = Vec::new();
    h.require_tools = true;
    let effective = vec![StageId::Build, StageId::Nsis];
    let err = h
        .gate_installer_stages(&effective, |_tool| false)
        .expect_err("--require-tools must hard-fail a host-default missing tool");
    let msg = err.to_string();
    assert!(
        msg.contains("nsis") && msg.contains("makensis"),
        "error must name the missing host-default stage and its tool: {msg}"
    );
}

/// `--require-tools` must NOT punish a host-default stage whose tool IS
/// present — strict mode only fails on genuine absence, it does not force
/// every OS-native producer to exist regardless.
#[test]
fn require_tools_keeps_host_default_when_tool_present() {
    let mut h = empty_harness();
    h.explicit_stages = Vec::new();
    h.require_tools = true;
    let effective = vec![StageId::Build, StageId::Nsis];
    let available = h
        .gate_installer_stages(&effective, |_tool| true)
        .expect("present tool must pass even under --require-tools");
    assert_eq!(available, vec![StageId::Build, StageId::Nsis]);
}

/// Release-blocker regression: `upx` is a host-default producer whose
/// tool-presence was historically checked only by stage-upx's lenient
/// runtime guard, which warn-skips even under `--require-tools`. With its
/// resolved binary threaded into [`Harness::config_tools`], `--require-tools`
/// must HARD-FAIL a host-default upx run whose binary is absent — naming
/// the stage and the missing `upx` binary — instead of silently emitting
/// no compressed artifact (false determinism coverage). `explicit_stages`
/// stays empty (the operator typed nothing); only `require_tools` flips the
/// contract, exactly as it does for the installer family.
#[test]
fn require_tools_hard_fails_host_default_missing_upx() {
    let mut h = empty_harness();
    h.explicit_stages = Vec::new();
    h.require_tools = true;
    h.config_tools.insert(StageId::Upx, vec!["upx".to_string()]);
    let effective = vec![StageId::Build, StageId::Upx];
    let err = h
        .gate_installer_stages(&effective, |_tool| false)
        .expect_err("--require-tools must hard-fail a host-default missing upx");
    let msg = err.to_string();
    assert!(
        msg.contains("upx"),
        "error must name the missing upx stage and its tool: {msg}"
    );
}

/// The flip side: `--require-tools` must NOT punish a host-default upx run
/// whose binary IS present — the stage stays in the effective set and runs
/// in the child release subprocess.
#[test]
fn require_tools_keeps_host_default_upx_when_tool_present() {
    let mut h = empty_harness();
    h.explicit_stages = Vec::new();
    h.require_tools = true;
    h.config_tools.insert(StageId::Upx, vec!["upx".to_string()]);
    let effective = vec![StageId::Build, StageId::Upx];
    let available = h
        .gate_installer_stages(&effective, |_tool| true)
        .expect("present upx must pass even under --require-tools");
    assert_eq!(available, vec![StageId::Build, StageId::Upx]);
}

/// Dev mode (no `--require-tools`, upx not in `explicit_stages`): a missing
/// upx binary must warn-and-DROP, never error — so a dev box lacking upx
/// stays usable, mirroring the installer family's host-default warn-skip.
/// The stage's own lenient runtime guard still applies in the child; here
/// the harness gate simply removes it from the effective set.
#[test]
fn dev_mode_warn_skips_host_default_upx_when_tool_absent() {
    let mut h = empty_harness();
    h.explicit_stages = Vec::new();
    h.require_tools = false;
    h.config_tools.insert(StageId::Upx, vec!["upx".to_string()]);
    let effective = vec![StageId::Build, StageId::Upx];
    let available = h
        .gate_installer_stages(&effective, |_tool| false)
        .expect("dev-mode host-default missing upx must warn-and-drop, not error");
    assert_eq!(
        available,
        vec![StageId::Build],
        "missing-tool upx must be dropped in dev mode; non-gated stages pass through"
    );
}

/// Release-blocker regression: a `version: v3` MSI (candle+light) on a
/// Windows shard that HAS candle+light must NOT skip/hard-fail. Before
/// the fix the gate hardcoded `wix` (the v4 CLI) for `msi` on Windows, so
/// it probed an absent binary and hard-failed the whole Windows shard on
/// every release even though the build runs candle+light. Drives the real
/// gate with the resolved v3 tools present.
#[test]
fn msi_v3_gate_passes_when_candle_and_light_present() {
    let mut h = empty_harness();
    h.stages = vec![StageId::Build, StageId::Msi];
    // Resolved v3 tool set; both probe as present.
    h.config_tools.insert(
        StageId::Msi,
        vec!["candle".to_string(), "light".to_string()],
    );
    let available = h
        .gate_installer_stages(&h.stages.clone(), |tool| matches!(tool, "candle" | "light"))
        .expect("v3 msi with candle+light present must pass the gate");
    assert_eq!(
        available,
        vec![StageId::Build, StageId::Msi],
        "msi must stay in the effective set when its resolved tools are present"
    );
}

/// The flip side: when the resolved WiX tool is genuinely absent the gate
/// must still hard-fail an explicitly-requested msi stage, and name the
/// first missing tool — so a v3 shard missing `light` is caught.
#[test]
fn msi_v3_gate_hard_fails_when_a_resolved_tool_absent() {
    let mut h = empty_harness();
    h.stages = vec![StageId::Build, StageId::Msi];
    h.explicit_stages = h.stages.clone();
    h.config_tools.insert(
        StageId::Msi,
        vec!["candle".to_string(), "light".to_string()],
    );
    // `candle` present, `light` missing — v3 needs both, so msi skips.
    let err = h
        .gate_installer_stages(&h.stages.clone(), |tool| tool == "candle")
        .expect_err("v3 msi missing `light` must hard-fail the run");
    let msg = err.to_string();
    assert!(
        msg.contains("msi") && msg.contains("light"),
        "error must name msi and the first missing tool: {msg}"
    );
}

/// The docker staging step must lay each discovered per-triple binary
/// out at `<os>/<arch>/<bin>` (matching a dockerfile's
/// `COPY ${TARGETOS}/${TARGETARCH}/${BIN}`) and copy the CONFIGURED
/// dockerfile to the staging root, BEFORE any `docker buildx build`
/// spawns. This exercises the staging logic in isolation — no docker
/// required.
#[test]
fn docker_context_staging_lays_out_os_arch_bin_and_dockerfile() {
    let tmp = tempfile::TempDir::new().unwrap();
    let worktree = tmp.path();

    // Simulate the harness's discovered per-triple binaries: a linux
    // amd64 and an arm64 build under `.det-tmp/target/<triple>/release/`.
    for triple in ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
        let release = worktree
            .join(".det-tmp")
            .join("target")
            .join(triple)
            .join("release");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(release.join("anodizer"), b"fake-binary").unwrap();
        // A bare host build + scratch dirs must be ignored by staging.
    }
    let host_release = worktree.join(".det-tmp").join("target").join("release");
    std::fs::create_dir_all(&host_release).unwrap();
    std::fs::write(host_release.join("anodizer"), b"host-byproduct").unwrap();

    std::fs::write(worktree.join("Dockerfile"), "FROM scratch\nCOPY x x\n").unwrap();

    let cfg = ResolvedDockerConfig {
        dockerfile: "Dockerfile".into(),
        extra_files: Vec::new(),
        build_args: Vec::new(),
    };
    let context_dir = worktree.join(".det-tmp").join("docker-context");
    let log = StageLogger::new("test", Verbosity::Quiet);
    let staged = stage_docker_context(worktree, &context_dir, &cfg, &log).unwrap();

    // Both per-triple binaries staged; the bare host byproduct excluded.
    assert_eq!(staged, 2, "only per-triple binaries should be staged");
    assert!(
        context_dir
            .join("linux")
            .join("amd64")
            .join("anodizer")
            .is_file(),
        "amd64 binary must land at <context>/linux/amd64/anodizer"
    );
    assert!(
        context_dir
            .join("linux")
            .join("arm64")
            .join("anodizer")
            .is_file(),
        "arm64 binary must land at <context>/linux/arm64/anodizer"
    );
    assert!(
        context_dir.join("Dockerfile").is_file(),
        "configured dockerfile must be copied to the staging root"
    );

    // Re-running wipes stale bytes: stale content must not survive.
    std::fs::write(context_dir.join("stale.txt"), b"old").unwrap();
    let staged2 = stage_docker_context(worktree, &context_dir, &cfg, &log).unwrap();
    assert_eq!(staged2, 2);
    assert!(
        !context_dir.join("stale.txt").exists(),
        "re-run must wipe the prior staging dir so no bytes carry over"
    );
}

/// A THIN configured dockerfile (distinct from any repo-root `Dockerfile`)
/// plus `extra_files` must all land in the context: the RENDERED dockerfile
/// at the staging root, each per-triple binary at `<os>/<arch>/<bin>`, and
/// every extra_file at its structure-preserving relative path. This is the
/// regression guard for the harness building the wrong (fat repo-root)
/// dockerfile against an incomplete context.
#[test]
fn docker_context_staging_thin_dockerfile_and_extra_files() {
    let tmp = tempfile::TempDir::new().unwrap();
    let worktree = tmp.path();

    let release = worktree
        .join(".det-tmp")
        .join("target")
        .join("x86_64-unknown-linux-gnu")
        .join("release");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("cfgd"), b"fake-binary").unwrap();

    // A thin release dockerfile that is NOT named `Dockerfile`, alongside a
    // fat repo-root `Dockerfile` that must never be selected.
    std::fs::write(
        worktree.join("Dockerfile"),
        "FROM rust\nCOPY Cargo.lock .\n",
    )
    .unwrap();
    std::fs::write(
        worktree.join("Dockerfile.agent.release"),
        "FROM debian\nARG TARGETOS=linux\nARG TARGETARCH\n\
             COPY ${TARGETOS}/${TARGETARCH}/cfgd /usr/local/bin/cfgd\n\
             COPY entrypoint.sh /entrypoint.sh\n",
    )
    .unwrap();
    // An extra_file in a subdir — its relative structure must be preserved.
    std::fs::write(worktree.join("entrypoint.sh"), b"#!/bin/sh\n").unwrap();

    let cfg = ResolvedDockerConfig {
        dockerfile: "Dockerfile.agent.release".into(),
        extra_files: vec!["entrypoint.sh".into()],
        build_args: vec![("VERSION".into(), "1.2.3".into())],
    };
    let context_dir = worktree.join(".det-tmp").join("docker-context-0");
    let log = StageLogger::new("test", Verbosity::Quiet);
    let staged = stage_docker_context(worktree, &context_dir, &cfg, &log).unwrap();

    assert_eq!(staged, 1, "the single per-triple binary must be staged");
    assert!(
        context_dir
            .join("linux")
            .join("amd64")
            .join("cfgd")
            .is_file(),
        "binary must land at <context>/linux/amd64/cfgd"
    );
    // The RENDERED (thin) dockerfile — not the fat repo-root one — is the
    // copied `Dockerfile`.
    let staged_dockerfile = std::fs::read_to_string(context_dir.join("Dockerfile")).unwrap();
    assert!(
        staged_dockerfile.contains("COPY ${TARGETOS}/${TARGETARCH}/cfgd"),
        "the configured thin dockerfile must be staged, not the fat repo-root one: \
             {staged_dockerfile}"
    );
    assert!(
        !staged_dockerfile.contains("COPY Cargo.lock"),
        "the fat repo-root Dockerfile must never be staged: {staged_dockerfile}"
    );
    assert!(
        context_dir.join("entrypoint.sh").is_file(),
        "extra_files must be staged into the context"
    );
}

/// Auto-included (not explicitly typed): the podman/buildx-absent
/// path must remain a warn-and-skip so the harness stays harmless on
/// minimal hosts. `docker` is never auto-included today, but the fork
/// must preserve this branch for any future auto-inclusion path.
#[test]
fn docker_stage_podman_auto_included_warns_and_skips() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
    let mut h = empty_harness();
    h.docker_backend_hint = Some("podman".into());
    h.docker_configs = vec![ResolvedDockerConfig {
        dockerfile: "Dockerfile".into(),
        extra_files: Vec::new(),
        build_args: Vec::new(),
    }];
    let env = HashMap::new();
    assert!(
        h.run_docker_stage(tmp.path(), &env, false).is_ok(),
        "auto-included docker under podman must warn-and-skip, not error"
    );
}

/// The headroom guard must ABORT before a run when free space is below
/// the floor, and the error must carry the actionable numbers so a
/// recurrence is diagnosable from the log alone — never let the harness
/// limp into the opaque `hdiutil` ENOSPC.
#[test]
fn headroom_guard_aborts_below_floor_with_actionable_message() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let mut h = empty_harness();
    h.disk_abs_floor_bytes = 45 * GIB;
    let log = StageLogger::new("test", Verbosity::Quiet);
    let vol = std::path::Path::new("/Volumes/scratch");
    // run-0 (no prior peak), 30 GiB free, 45 GiB floor → abort.
    let err = h
        .guard_run_headroom(&log, 0, vol, Some(30 * GIB), None)
        .expect_err("below-floor free space must abort the run");
    let msg = err.to_string();
    assert!(msg.contains("determinism run 1"), "1-based run: {msg}");
    assert!(
        msg.contains(&format!("{}", 45 * GIB)),
        "exact required: {msg}"
    );
    assert!(
        msg.contains(&format!("{}", 30 * GIB)),
        "exact available: {msg}"
    );
    assert!(msg.contains("/Volumes/scratch"), "volume: {msg}");
    assert!(msg.contains("reclaim-disk"), "remedy hint: {msg}");
    assert!(
        msg.contains("absolute floor"),
        "run-0 must state the floor basis, not a peak guarantee: {msg}"
    );
}

/// Ample headroom → the guard proceeds. And run-1's MEASURED-peak gate
/// (the B1 fix) aborts when a prior run's peak × factor exceeds the
/// available space, where a net-delta guard would have wrongly proceeded.
#[test]
fn headroom_guard_proceeds_with_ample_space_and_gates_on_measured_peak() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let mut h = empty_harness();
    h.disk_abs_floor_bytes = 45 * GIB;
    h.disk_safety_factor = 1.3;
    let log = StageLogger::new("test", Verbosity::Quiet);
    let vol = std::path::Path::new("/scratch");
    // run-0 with 60 GiB free clears the 45 GiB floor.
    assert!(
        h.guard_run_headroom(&log, 0, vol, Some(60 * GIB), None)
            .is_ok(),
        "ample free space must proceed"
    );
    // run-1 gated on run-0's measured PEAK of 70 GiB; ×1.3 = 91 GiB
    // required. 71 GiB free → abort (a net delta would have seen ~30 and
    // proceeded into ENOSPC); 95 GiB free → proceed.
    let prior_peak = Some(70 * GIB);
    assert!(
        h.guard_run_headroom(&log, 1, vol, Some(71 * GIB), prior_peak)
            .is_err(),
        "71 GiB free under a 91 GiB peak-projected requirement must abort"
    );
    assert!(
        h.guard_run_headroom(&log, 1, vol, Some(95 * GIB), prior_peak)
            .is_ok(),
        "95 GiB free clears the 91 GiB peak-projected requirement"
    );
}

/// A probe gap (free space unknown) must degrade to a no-op — the guard
/// never manufactures a failure from missing disk data.
#[test]
fn headroom_guard_unknown_free_space_is_noop() {
    let h = empty_harness();
    let log = StageLogger::new("test", Verbosity::Quiet);
    let vol = std::path::Path::new("/scratch");
    assert!(
        h.guard_run_headroom(&log, 1, vol, None, None).is_ok(),
        "unknown free space must proceed (no manufactured abort)"
    );
}
