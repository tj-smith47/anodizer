use super::super::generate::{NixParams, generate_nix_expression};
use super::*;
use crate::util;
use anodizer_core::config::{
    ArchiveConfig, ArchivesConfig, BuildConfig, CrateConfig, NixConfig, NixDependency,
    WrapInDirectory,
};
use anodizer_core::context::Context;
use anodizer_core::log::{StageLogger, Verbosity};

fn quiet_log() -> StageLogger {
    StageLogger::new("publish", Verbosity::Quiet)
}

#[test]
fn commit_outcome_is_pushed() {
    assert!(util::CommitOutcome::Pushed.is_pushed());
    assert!(!util::CommitOutcome::NoChanges.is_pushed());
}

// -----------------------------------------------------------------
// unique_dep_args — declaration order preserved, dupes collapsed.
// -----------------------------------------------------------------

#[test]
fn unique_dep_args_empty_returns_empty() {
    assert!(unique_dep_args(&[]).is_empty());
}

#[test]
fn unique_dep_args_dedupes_preserving_first_occurrence_order() {
    let deps = vec![
        NixDependency {
            name: "openssl".to_string(),
            os: Some("linux".to_string()),
        },
        NixDependency {
            name: "openssl".to_string(),
            os: Some("darwin".to_string()),
        },
        NixDependency {
            name: "git".to_string(),
            os: None,
        },
        NixDependency {
            name: "openssl".to_string(),
            os: None,
        },
    ];
    assert_eq!(
        unique_dep_args(&deps),
        vec!["openssl".to_string(), "git".to_string()]
    );
}

// -----------------------------------------------------------------
// collect_binary_names — pulled from builds, falls back to name.
// -----------------------------------------------------------------

#[test]
fn collect_binary_names_falls_back_to_derivation_name_when_no_builds() {
    let cc = CrateConfig {
        builds: None,
        ..Default::default()
    };
    assert_eq!(collect_binary_names(&cc, "mytool"), vec!["mytool"]);
}

#[test]
fn collect_binary_names_falls_back_when_builds_have_no_binary() {
    let cc = CrateConfig {
        builds: Some(vec![BuildConfig {
            binary: None,
            ..Default::default()
        }]),
        ..Default::default()
    };
    assert_eq!(collect_binary_names(&cc, "fallback"), vec!["fallback"]);
}

#[test]
fn collect_binary_names_extracts_and_dedupes_preserving_order() {
    let cc = CrateConfig {
        builds: Some(vec![
            BuildConfig {
                binary: Some("alpha".to_string()),
                ..Default::default()
            },
            BuildConfig {
                binary: Some("beta".to_string()),
                ..Default::default()
            },
            BuildConfig {
                binary: Some("alpha".to_string()),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    assert_eq!(
        collect_binary_names(&cc, "ignored"),
        vec!["alpha".to_string(), "beta".to_string()]
    );
}

// -----------------------------------------------------------------
// build_wrap_program_line — partitioned by `os:` filter.
// -----------------------------------------------------------------

#[test]
fn build_wrap_program_line_returns_none_when_deps_empty() {
    assert!(build_wrap_program_line(&[], "mytool").is_none());
}

#[test]
fn build_wrap_program_line_all_os_emits_unconditional_list() {
    let deps = vec![
        NixDependency {
            name: "git".to_string(),
            os: None,
        },
        NixDependency {
            name: "curl".to_string(),
            os: None,
        },
    ];
    let line = build_wrap_program_line(&deps, "mytool").expect("should emit");
    assert!(line.contains("wrapProgram $out/bin/mytool"));
    assert!(line.contains("[ git curl ]"));
    assert!(!line.contains("isDarwin"));
    assert!(!line.contains("isLinux"));
}

#[test]
fn build_wrap_program_line_partitions_by_os() {
    let deps = vec![
        NixDependency {
            name: "darwin_dep".to_string(),
            os: Some("darwin".to_string()),
        },
        NixDependency {
            name: "linux_dep".to_string(),
            os: Some("linux".to_string()),
        },
        NixDependency {
            name: "git".to_string(),
            os: None,
        },
    ];
    let line = build_wrap_program_line(&deps, "mytool").expect("should emit");
    assert!(line.contains("lib.optionals stdenvNoCC.isDarwin [ darwin_dep ]"));
    assert!(line.contains("lib.optionals stdenvNoCC.isLinux [ linux_dep ]"));
    assert!(line.contains("[ git ]"));
    // Darwin must precede linux which must precede all-OS bucket.
    let darwin_pos = line.find("isDarwin").unwrap();
    let linux_pos = line.find("isLinux").unwrap();
    assert!(darwin_pos < linux_pos);
}

#[test]
fn build_wrap_program_line_unknown_os_string_is_dropped() {
    let deps = vec![NixDependency {
        name: "freebsd_dep".to_string(),
        os: Some("freebsd".to_string()),
    }];
    assert!(build_wrap_program_line(&deps, "mytool").is_none());
}

// -----------------------------------------------------------------
// build_install_lines — custom install vs auto-generated.
// -----------------------------------------------------------------

#[test]
fn build_install_lines_custom_install_overrides_auto_block() {
    let nix_cfg = NixConfig {
        install: Some("custom-line-1\ncustom-line-2".to_string()),
        ..Default::default()
    };
    let cc = CrateConfig::default();
    let lines = build_install_lines(&nix_cfg, &cc, "mytool", &[], false);
    assert_eq!(lines, vec!["custom-line-1", "custom-line-2"]);
}

#[test]
fn build_install_lines_custom_install_appends_extra_install() {
    let nix_cfg = NixConfig {
        install: Some("base".to_string()),
        extra_install: Some("extra-1\nextra-2".to_string()),
        ..Default::default()
    };
    let cc = CrateConfig::default();
    let lines = build_install_lines(&nix_cfg, &cc, "mytool", &[], false);
    assert_eq!(lines, vec!["base", "extra-1", "extra-2"]);
}

#[test]
fn build_install_lines_auto_generates_mkdir_and_cp_per_binary() {
    let nix_cfg = NixConfig::default();
    let cc = CrateConfig {
        builds: Some(vec![BuildConfig {
            binary: Some("mytool".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let lines = build_install_lines(&nix_cfg, &cc, "mytool", &[], false);
    assert_eq!(lines[0], "mkdir -p $out/bin");
    assert!(lines.iter().any(|l| l == "cp -vr ./mytool $out/bin/mytool"));
    assert!(lines.iter().any(|l| l == "chmod +x $out/bin/mytool"));
}

#[test]
fn build_install_lines_appends_wrap_program_when_needed() {
    let nix_cfg = NixConfig::default();
    let cc = CrateConfig::default();
    let deps = vec![NixDependency {
        name: "git".to_string(),
        os: None,
    }];
    let lines = build_install_lines(&nix_cfg, &cc, "mytool", &deps, true);
    let wrap = lines
        .iter()
        .find(|l| l.starts_with("wrapProgram"))
        .expect("wrap line must be appended");
    assert!(wrap.contains("[ git ]"));
}

#[test]
fn build_install_lines_skips_wrap_program_when_deps_filter_to_empty() {
    // needs_make_wrapper=true but every dep is OS-filtered to an
    // unknown OS — build_wrap_program_line returns None, no wrap appended.
    let nix_cfg = NixConfig::default();
    let cc = CrateConfig::default();
    let deps = vec![NixDependency {
        name: "x".to_string(),
        os: Some("plan9".to_string()),
    }];
    let lines = build_install_lines(&nix_cfg, &cc, "mytool", &deps, true);
    assert!(!lines.iter().any(|l| l.starts_with("wrapProgram")));
}

// -----------------------------------------------------------------
// build_archive_tuples — sha256 guard, url_template, hash conversion.
// -----------------------------------------------------------------

fn os_artifact(os: &str, arch: &str, url: &str, sha256: &str) -> util::OsArtifact {
    // Synthesize a representative genuine triple so `is_macos`-based nix
    // eligibility treats a "darwin" os as real macOS. Apple-but-not-macOS
    // targets (watchos/tvos) also map to os="darwin" but carry a different
    // triple — see `nix_system_for_artifact_excludes_apple_non_macos`.
    let target = match os {
        "darwin" => "aarch64-apple-darwin",
        "linux" => "x86_64-unknown-linux-gnu",
        "windows" => "x86_64-pc-windows-msvc",
        _ => "",
    };
    util::OsArtifact {
        url: url.to_string(),
        sha256: sha256.to_string(),
        os: os.to_string(),
        arch: arch.to_string(),
        target: target.to_string(),
        ..Default::default()
    }
}

/// Build an artifact carrying an explicit triple, so tests can drive the
/// Apple-but-not-macOS eligibility path (`os` alone cannot express it).
fn os_artifact_with_target(
    os: &str,
    arch: &str,
    target: &str,
    url: &str,
    sha256: &str,
) -> util::OsArtifact {
    util::OsArtifact {
        url: url.to_string(),
        sha256: sha256.to_string(),
        os: os.to_string(),
        arch: arch.to_string(),
        target: target.to_string(),
        ..Default::default()
    }
}

#[test]
fn build_archive_tuples_empty_artifact_list_bails() {
    let cfg = NixConfig::default();
    let err =
        build_archive_tuples(&[], &cfg, "mytool", "1.0.0", &quiet_log()).expect_err("no arts");
    assert!(format!("{err}").contains("no Linux/Darwin archive"));
}

#[test]
fn nix_system_for_artifact_excludes_apple_non_macos() {
    let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    // Genuine macOS (and Linux) stay nix-eligible.
    assert_eq!(
        nix_system_for_artifact(&os_artifact_with_target(
            "darwin",
            "arm64",
            "aarch64-apple-darwin",
            "u",
            sha,
        )),
        Some("aarch64-darwin".to_string()),
    );
    assert_eq!(
        nix_system_for_artifact(&os_artifact_with_target(
            "linux",
            "amd64",
            "x86_64-unknown-linux-gnu",
            "u",
            sha,
        )),
        Some("x86_64-linux".to_string()),
    );
    // map_target folds watchos/tvos into os="darwin"; these carry no
    // nix-installable binary and must NOT become a darwin nix system.
    for target in [
        "aarch64-apple-watchos",
        "aarch64-apple-tvos",
        "aarch64-apple-ios",
    ] {
        assert_eq!(
            nix_system_for_artifact(&os_artifact_with_target(
                "darwin", "arm64", target, "u", sha,
            )),
            None,
            "{target} is Apple-but-not-macOS — must be nix-ineligible",
        );
    }
}

#[test]
fn build_archive_tuples_excludes_watchos_darwin_keeps_linux() {
    // A watchOS archive maps to os="darwin" (map_target's broad apple rule)
    // but is not a real macOS binary; it must be dropped, leaving only the
    // genuine linux system in the tuples — never emitted as aarch64-darwin.
    let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let arts = vec![
        os_artifact_with_target(
            "darwin",
            "arm64",
            "aarch64-apple-watchos",
            "https://example.com/watch.tar.gz",
            sha,
        ),
        os_artifact_with_target(
            "linux",
            "amd64",
            "x86_64-unknown-linux-gnu",
            "https://example.com/linux.tar.gz",
            sha,
        ),
    ];
    let cfg = NixConfig::default();
    let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log()).unwrap();
    assert_eq!(tuples.len(), 1);
    assert_eq!(tuples[0].0, "x86_64-linux");
    assert!(
        !tuples.iter().any(|(sys, _, _)| sys.contains("darwin")),
        "watchOS archive must never surface as a darwin nix system"
    );
}

#[test]
fn build_archive_tuples_only_apple_non_macos_bails_as_no_archive() {
    // A full build whose only Apple archive is tvOS has no nix-installable
    // system: build_archive_tuples must bail (failure surfaced), not emit a
    // bogus aarch64-darwin package.
    let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let arts = vec![os_artifact_with_target(
        "darwin",
        "arm64",
        "aarch64-apple-tvos",
        "https://example.com/tv.tar.gz",
        sha,
    )];
    let cfg = NixConfig::default();
    let err = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log())
        .expect_err("tvOS-only must bail");
    assert!(format!("{err}").contains("no Linux/Darwin archive"));
}

#[test]
fn build_archive_tuples_missing_sha256_for_nix_system_bails() {
    let arts = vec![os_artifact(
        "linux",
        "amd64",
        "https://example.com/x.tar.gz",
        "",
    )];
    let cfg = NixConfig::default();
    let err = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log())
        .expect_err("empty sha256 must bail");
    let msg = format!("{err}");
    assert!(msg.contains("sha256"));
    assert!(msg.contains("mytool"));
}

#[test]
fn build_archive_tuples_skips_non_nix_systems_silently() {
    // Windows artifact has no nix_system mapping; sha256-empty guard
    // should not trigger for it.
    let arts = vec![
        os_artifact("windows", "amd64", "https://example.com/x.zip", ""),
        os_artifact(
            "linux",
            "amd64",
            "https://example.com/x.tar.gz",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
    ];
    let cfg = NixConfig::default();
    let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log()).unwrap();
    assert_eq!(tuples.len(), 1);
    assert_eq!(tuples[0].0, "x86_64-linux");
}

#[test]
fn build_archive_tuples_converts_hex_to_nix_base32() {
    let arts = vec![os_artifact(
        "linux",
        "amd64",
        "https://example.com/x.tar.gz",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    )];
    let cfg = NixConfig::default();
    let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log()).unwrap();
    assert_eq!(tuples[0].2.len(), 52, "nix base32 must be 52 chars");
    assert_ne!(
        tuples[0].2, arts[0].sha256,
        "must convert, not pass hex through"
    );
}

#[test]
fn build_archive_tuples_falls_back_to_raw_hex_on_bad_sha256() {
    // 64-char string that is NOT valid hex — base32 conversion fails,
    // warn-and-pass-through path runs (still yields a tuple).
    let bad = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
    let arts = vec![os_artifact(
        "linux",
        "amd64",
        "https://example.com/x.tar.gz",
        bad,
    )];
    let cfg = NixConfig::default();
    let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log()).unwrap();
    assert_eq!(tuples[0].2, bad, "fallback must preserve raw hex");
}

#[test]
fn build_archive_tuples_applies_url_template() {
    let arts = vec![os_artifact(
        "linux",
        "amd64",
        "https://original/url.tar.gz",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    )];
    let cfg = NixConfig {
        url_template: Some(
            "https://mirror.example.com/{{ name }}-{{ version }}-{{ os }}-{{ arch }}.tar.gz"
                .to_string(),
        ),
        ..Default::default()
    };
    let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.2.3", &quiet_log()).unwrap();
    assert_eq!(
        tuples[0].1,
        "https://mirror.example.com/mytool-1.2.3-linux-amd64.tar.gz"
    );
}

#[test]
fn build_archive_tuples_dedupes_by_nix_system() {
    // Both an Archive and an UploadableBinary for the same target collapse
    // to one nix system (x86_64-linux). Without source dedup the pipeline
    // carries N tuples per system, triplicating meta.platforms AND emitting
    // an ambiguous urlMap/shaMap whose `selectSystem` winner is BTreeMap
    // last-writer-wins. Source dedup must keep exactly one tuple per system.
    let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let arts = vec![
        os_artifact("linux", "amd64", "https://example.com/a.tar.gz", sha),
        os_artifact("linux", "amd64", "https://example.com/a.bin", sha),
        os_artifact("linux", "amd64", "https://example.com/a2.tar.gz", sha),
        os_artifact("darwin", "arm64", "https://example.com/d.tar.gz", sha),
    ];
    let cfg = NixConfig::default();
    let tuples = build_archive_tuples(&arts, &cfg, "mytool", "1.0.0", &quiet_log()).unwrap();
    let systems: Vec<&str> = tuples.iter().map(|(s, _, _)| s.as_str()).collect();
    assert_eq!(
        systems,
        vec!["x86_64-linux", "aarch64-darwin"],
        "one tuple per nix system, first occurrence kept, insertion order preserved"
    );
    // First occurrence wins so the urlMap winner is the first-seen archive,
    // not a BTreeMap last-writer-wins surprise.
    assert_eq!(tuples[0].1, "https://example.com/a.tar.gz");
}

#[test]
fn generate_nix_expression_emits_each_platform_once() {
    // Even if a caller somehow passes duplicate-system tuples, the rendered
    // meta.platforms must list each platform exactly once (deterministic,
    // sorted) — the historical bug rendered 12 entries for 4 platforms.
    let sha = "0bv1xkjqlf06hjyl3z7xj9zyq2k0q0k0q0k0q0k0q0k0q0k0q0k0";
    let archives = vec![
        (
            "x86_64-linux".to_string(),
            "https://e/a".to_string(),
            sha.to_string(),
        ),
        (
            "x86_64-linux".to_string(),
            "https://e/b".to_string(),
            sha.to_string(),
        ),
        (
            "x86_64-linux".to_string(),
            "https://e/c".to_string(),
            sha.to_string(),
        ),
        (
            "aarch64-darwin".to_string(),
            "https://e/d".to_string(),
            sha.to_string(),
        ),
    ];
    let expr = generate_nix_expression(&NixParams {
        name: "mytool",
        version: "1.0.0",
        description: "",
        long_description: "",
        homepage: "",
        changelog: "",
        license_expr: "",
        maintainers: &[],
        main_program: "",
        archives: &archives,
        install_lines: &[],
        post_install_lines: &[],
        needs_unzip: false,
        needs_make_wrapper: false,
        dep_args: &[],
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();
    let platforms_line = expr
        .lines()
        .find(|l| l.trim_start().starts_with("platforms ="))
        .expect("platforms line present");
    assert_eq!(
        platforms_line.matches("\"x86_64-linux\"").count(),
        1,
        "x86_64-linux must appear exactly once in: {platforms_line}"
    );
    assert_eq!(
        platforms_line.matches("\"aarch64-darwin\"").count(),
        1,
        "aarch64-darwin must appear exactly once in: {platforms_line}"
    );
}

// -----------------------------------------------------------------
// resolve_source_roots — single-root collapse vs per-system map.
// -----------------------------------------------------------------

#[test]
fn resolve_source_roots_no_artifacts_yields_dot_default() {
    let cc = CrateConfig::default();
    let (single, map) = resolve_source_roots(&cc, &[], "mytool", "1.0.0");
    assert_eq!(single.as_deref(), Some("."));
    assert!(map.is_none());
}

#[test]
fn resolve_source_roots_uniform_root_collapses_to_single() {
    let arts = vec![
        os_artifact("linux", "amd64", "u1", "h1"),
        os_artifact("darwin", "arm64", "u2", "h2"),
    ];
    let cc = CrateConfig {
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            wrap_in_directory: Some(WrapInDirectory::Bool(true)),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let (single, map) = resolve_source_roots(&cc, &arts, "mytool", "1.0.0");
    assert_eq!(single.as_deref(), Some("mytool-1.0.0"));
    assert!(map.is_none());
}

#[test]
fn resolve_source_roots_disabled_archives_falls_back_to_dot() {
    let arts = vec![os_artifact("linux", "amd64", "u1", "h1")];
    let cc = CrateConfig {
        archives: ArchivesConfig::Disabled,
        ..Default::default()
    };
    let (single, map) = resolve_source_roots(&cc, &arts, "mytool", "1.0.0");
    assert_eq!(single.as_deref(), Some("."));
    assert!(map.is_none());
}

#[test]
fn resolve_source_roots_divergent_per_id_emits_per_system_map() {
    let mut linux = os_artifact("linux", "amd64", "u1", "h1");
    linux.id = Some("linux-archive".to_string());
    let mut darwin = os_artifact("darwin", "arm64", "u2", "h2");
    darwin.id = Some("darwin-archive".to_string());
    let cc = CrateConfig {
        archives: ArchivesConfig::Configs(vec![
            ArchiveConfig {
                id: Some("linux-archive".to_string()),
                wrap_in_directory: Some(WrapInDirectory::Bool(true)),
                ..Default::default()
            },
            ArchiveConfig {
                id: Some("darwin-archive".to_string()),
                wrap_in_directory: Some(WrapInDirectory::Bool(false)),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let (single, map) = resolve_source_roots(&cc, &[linux, darwin], "mytool", "1.0.0");
    assert!(single.is_none());
    let entries = map.expect("per-system map must be emitted");
    assert_eq!(entries.len(), 2);
    // Sorted by system identifier.
    assert!(entries[0].system < entries[1].system);
    let roots: std::collections::HashMap<&str, &str> = entries
        .iter()
        .map(|e| (e.system.as_str(), e.root.as_str()))
        .collect();
    assert_eq!(roots.get("x86_64-linux"), Some(&"mytool-1.0.0"));
    assert_eq!(roots.get("aarch64-darwin"), Some(&"."));
}

#[test]
fn resolve_source_roots_single_unidentified_cfg_matches_id_bearing_artifact() {
    // The artifact carries an `id`, but the lone archive config has
    // `id: None`. The `(_, None) if archive_cfgs.len() == 1` fallback
    // matches it, so the custom wrap directory is applied to the system.
    let mut art = os_artifact("linux", "amd64", "u1", "h1");
    art.id = Some("some-archive-id".to_string());
    let cc = CrateConfig {
        archives: ArchivesConfig::Configs(vec![ArchiveConfig {
            id: None,
            wrap_in_directory: Some(WrapInDirectory::Name("custom-root".to_string())),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let (single, map) = resolve_source_roots(&cc, &[art], "mytool", "1.0.0");
    assert_eq!(single.as_deref(), Some("custom-root"));
    assert!(map.is_none());
}

#[test]
fn build_install_lines_auto_block_appends_extra_install() {
    // No custom `install`, so the auto mkdir/cp block runs; `extra_install`
    // must be appended after the generated cp/chmod lines.
    let nix_cfg = NixConfig {
        extra_install: Some("install -m644 LICENSE $out/share/LICENSE".to_string()),
        ..Default::default()
    };
    let cc = CrateConfig::default();
    let lines = build_install_lines(&nix_cfg, &cc, "mytool", &[], false);
    assert_eq!(lines[0], "mkdir -p $out/bin");
    assert!(lines.iter().any(|l| l == "cp -vr ./mytool $out/bin/mytool"));
    assert_eq!(
        lines.last().map(String::as_str),
        Some("install -m644 LICENSE $out/share/LICENSE"),
        "extra_install must be the final appended line on the auto path"
    );
}

// -----------------------------------------------------------------
// detect_dynamically_linked — build-stage metadata flag short-circuit.
// -----------------------------------------------------------------

fn ctx_with_binary_metadata(crate_name: &str, flag: Option<&str>) -> Context {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::test_helpers::TestContextBuilder;
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![CrateConfig {
            name: crate_name.to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }])
        .build();
    let mut metadata = std::collections::HashMap::new();
    if let Some(v) = flag {
        metadata.insert("DynamicallyLinked".to_string(), v.to_string());
    }
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        // A path that does NOT exist on disk — proving the metadata flag
        // short-circuits before any ELF inspection of `path`.
        path: std::path::PathBuf::from("/nonexistent/anodizer-test-binary"),
        name: crate_name.to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: crate_name.to_string(),
        metadata,
        size: None,
    });
    ctx
}

#[test]
fn detect_dynamically_linked_true_from_metadata_flag() {
    let ctx = ctx_with_binary_metadata("mytool", Some("true"));
    assert!(
        detect_dynamically_linked(&ctx, "mytool").unwrap(),
        "DynamicallyLinked=true metadata must report dynamic linkage \
             without touching the (nonexistent) binary path"
    );
}

#[test]
fn detect_dynamically_linked_false_from_metadata_flag() {
    let ctx = ctx_with_binary_metadata("mytool", Some("false"));
    assert!(
        !detect_dynamically_linked(&ctx, "mytool").unwrap(),
        "DynamicallyLinked=false metadata must report static linkage \
             without falling through to ELF inspection of a missing path"
    );
}

// -----------------------------------------------------------------
// resolve_nix_metadata — license resolution + meta.* render.
// -----------------------------------------------------------------

fn meta_ctx() -> Context {
    use anodizer_core::test_helpers::TestContextBuilder;
    TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }])
        .build()
}

/// A bare crate config (no release repo, no archives) for the
/// `resolve_nix_metadata` unit tests — they exercise description / homepage
/// / license / main_program resolution, none of which need build artifacts.
fn meta_crate_cfg() -> CrateConfig {
    CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        ..Default::default()
    }
}

#[test]
fn resolve_nix_metadata_resolves_spdx_license_to_nix_attr() {
    let ctx = meta_ctx();
    let cfg = NixConfig {
        description: Some("a demo".to_string()),
        homepage: Some("https://example.com".to_string()),
        license: Some("Apache-2.0".to_string()),
        main_program: Some("mytool".to_string()),
        ..Default::default()
    };
    let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
        .expect("resolve");
    assert_eq!(meta.description, "a demo");
    assert_eq!(meta.homepage, "https://example.com");
    // SPDX `Apache-2.0` maps to the nix `lib.licenses.asl20` attribute.
    assert_eq!(meta.license_expr, "lib.licenses.asl20");
    assert_eq!(meta.main_program, "mytool");
}

#[test]
fn resolve_nix_metadata_passes_through_raw_nix_license_attr() {
    let ctx = meta_ctx();
    let cfg = NixConfig {
        license: Some("mit".to_string()),
        ..Default::default()
    };
    let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
        .expect("resolve");
    assert_eq!(
        meta.license_expr, "lib.licenses.mit",
        "a valid nix attr passes through verbatim"
    );
}

#[test]
fn resolve_nix_metadata_empty_license_suppressed_not_resolved() {
    let ctx = meta_ctx();
    // No license configured and no project metadata fallback — the empty
    // value resolves to no `meta.license` attribute at all.
    let cfg = NixConfig::default();
    let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
        .expect("resolve");
    assert_eq!(
        meta.license_expr, "",
        "empty license must stay empty, not error"
    );
    assert_eq!(meta.description, "");
    assert_eq!(meta.main_program, "");
}

#[test]
fn resolve_nix_metadata_invalid_license_degrades_to_string() {
    let ctx = meta_ctx();
    let cfg = NixConfig {
        license: Some("not-a-real-license-xyz".to_string()),
        ..Default::default()
    };
    // An unmappable license no longer aborts the release; it degrades to
    // the verbatim quoted-string form (always valid in `meta`).
    let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
        .expect("unmappable license must degrade, not bail");
    assert_eq!(meta.license_expr, "\"not-a-real-license-xyz\"");
}

#[test]
fn resolve_nix_metadata_falls_back_to_project_metadata() {
    use anodizer_core::config::MetadataConfig;
    let mut ctx = meta_ctx();
    ctx.config.metadata = Some(MetadataConfig {
        description: Some("project-level description".to_string()),
        homepage: Some("https://project.example".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    });
    // NixConfig supplies none of these, so each must fall through to the
    // project `metadata.*` value (and the SPDX `MIT` resolves to nix `mit`).
    let cfg = NixConfig::default();
    let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
        .expect("resolve");
    assert_eq!(meta.description, "project-level description");
    assert_eq!(meta.homepage, "https://project.example");
    assert_eq!(meta.license_expr, "lib.licenses.mit");
}

#[test]
fn resolve_nix_metadata_config_overrides_project_metadata() {
    use anodizer_core::config::MetadataConfig;
    let mut ctx = meta_ctx();
    ctx.config.metadata = Some(MetadataConfig {
        description: Some("project-level".to_string()),
        homepage: Some("https://project.example".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    });
    let cfg = NixConfig {
        description: Some("nix-level".to_string()),
        homepage: Some("https://nix.example".to_string()),
        license: Some("Apache-2.0".to_string()),
        ..Default::default()
    };
    let meta = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
        .expect("resolve");
    assert_eq!(
        meta.description, "nix-level",
        "nix config wins over metadata"
    );
    assert_eq!(meta.homepage, "https://nix.example");
    assert_eq!(
        meta.license_expr, "lib.licenses.asl20",
        "Apache-2.0 resolves to asl20"
    );
}

#[test]
fn resolve_nix_metadata_bad_homepage_template_bails() {
    let ctx = meta_ctx();
    let cfg = NixConfig {
        // Unterminated Tera expression — render must surface an Err that the
        // `with_context("render homepage template …")` wrapper carries up.
        homepage: Some("https://x/{{ unclosed".to_string()),
        ..Default::default()
    };
    let err = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
        .expect_err("malformed homepage template must bail");
    assert!(format!("{err:#}").contains("homepage"));
}

#[test]
fn resolve_nix_metadata_bad_main_program_template_bails() {
    let ctx = meta_ctx();
    let cfg = NixConfig {
        main_program: Some("{{ unclosed".to_string()),
        ..Default::default()
    };
    let err = resolve_nix_metadata(&ctx, &meta_crate_cfg(), &cfg, "mytool", &quiet_log())
        .expect_err("malformed main_program template must bail");
    assert!(format!("{err:#}").contains("main_program"));
}

// -----------------------------------------------------------------
// render_license_expr — RHS rendering for each NixLicense shape.
// -----------------------------------------------------------------

#[test]
fn render_license_expr_single_attr() {
    assert_eq!(render_license_expr("MIT"), "lib.licenses.mit");
}

#[test]
fn render_license_expr_dual_or_is_with_list() {
    assert_eq!(
        render_license_expr("MIT OR Apache-2.0"),
        "with lib.licenses; [ mit asl20 ]"
    );
}

#[test]
fn render_license_expr_unknown_is_quoted_string() {
    assert_eq!(render_license_expr("Weird-9.9"), "\"Weird-9.9\"");
}

#[test]
fn render_license_expr_compound_with_is_quoted_string() {
    assert_eq!(
        render_license_expr("Apache-2.0 WITH LLVM-exception"),
        "\"Apache-2.0 WITH LLVM-exception\""
    );
}

#[test]
fn render_license_expr_empty_is_empty() {
    assert_eq!(render_license_expr(""), "");
}

// -----------------------------------------------------------------
// resolve_nix_changelog — explicit override + release-repo derivation.
// -----------------------------------------------------------------

fn crate_cfg_with_github_release(owner: &str, repo: &str) -> CrateConfig {
    use anodizer_core::config::{ReleaseConfig, ScmRepoConfig};
    CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        release: Some(ReleaseConfig {
            github: Some(ScmRepoConfig {
                owner: owner.to_string(),
                name: repo.to_string(),
                token: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn changelog_derived_from_release_repo_and_tag() {
    let mut ctx = meta_ctx();
    ctx.template_vars_mut().set("Tag", "v1.4.2");
    let cc = crate_cfg_with_github_release("BurntSushi", "ripgrep");
    let cfg = NixConfig::default();
    let got = resolve_nix_changelog(&ctx, &cc, &cfg, &quiet_log()).expect("changelog");
    assert_eq!(
        got,
        "https://github.com/BurntSushi/ripgrep/releases/tag/v1.4.2"
    );
}

#[test]
fn changelog_explicit_override_wins_and_templates() {
    let mut ctx = meta_ctx();
    ctx.template_vars_mut().set("Tag", "v1.4.2");
    let cc = crate_cfg_with_github_release("BurntSushi", "ripgrep");
    let cfg = NixConfig {
        changelog: Some(
            "https://github.com/BurntSushi/ripgrep/blob/{{ Tag }}/CHANGELOG.md".to_string(),
        ),
        ..Default::default()
    };
    let got = resolve_nix_changelog(&ctx, &cc, &cfg, &quiet_log()).expect("changelog");
    assert_eq!(
        got,
        "https://github.com/BurntSushi/ripgrep/blob/v1.4.2/CHANGELOG.md"
    );
}

#[test]
fn changelog_empty_without_release_repo() {
    let ctx = meta_ctx();
    let cc = meta_crate_cfg();
    let cfg = NixConfig::default();
    let got = resolve_nix_changelog(&ctx, &cc, &cfg, &quiet_log()).expect("changelog");
    assert_eq!(
        got, "",
        "no release repo + no override → suppress changelog"
    );
}

#[test]
fn changelog_falls_back_to_v_version_when_no_tag_var() {
    let mut ctx = meta_ctx();
    // Model an in-memory/snapshot render where no resolved git `Tag` exists
    // but a `Version` does — the URL falls back to `v<version>`.
    ctx.template_vars_mut().unset("Tag");
    ctx.template_vars_mut().set("Version", "2.0.0");
    let cc = crate_cfg_with_github_release("me", "tool");
    let cfg = NixConfig::default();
    let got = resolve_nix_changelog(&ctx, &cc, &cfg, &quiet_log()).expect("changelog");
    assert_eq!(got, "https://github.com/me/tool/releases/tag/v2.0.0");
}

// -----------------------------------------------------------------
// build_completion_install_lines / build_manpage_install_lines —
// gated on archive config; emit installShellCompletion / installManPage.
// -----------------------------------------------------------------

fn crate_cfg_with_archive(archive: anodizer_core::config::ArchiveConfig) -> CrateConfig {
    use anodizer_core::config::ArchivesConfig;
    CrateConfig {
        name: "mytool".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        archives: ArchivesConfig::Configs(vec![archive]),
        ..Default::default()
    }
}

#[test]
fn no_completion_lines_when_archive_has_no_completions() {
    let cc = crate_cfg_with_archive(anodizer_core::config::ArchiveConfig::default());
    assert!(build_completion_install_lines(&cc, &["mytool".to_string()]).is_empty());
    assert!(build_manpage_install_lines(&cc).is_empty());
}

#[test]
fn completion_lines_emitted_when_archive_bundles_completions() {
    use anodizer_core::config::{ArchiveConfig, CompletionsConfig};
    let archive = ArchiveConfig {
        completions: Some(CompletionsConfig {
            generate: Some("{{ ArtifactPath }} completions {{ Shell }}".to_string()),
            shells: Some(vec![
                "bash".to_string(),
                "zsh".to_string(),
                "fish".to_string(),
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let cc = crate_cfg_with_archive(archive);
    let lines = build_completion_install_lines(&cc, &["rg".to_string()]);
    assert_eq!(
        lines.len(),
        1,
        "one installShellCompletion line; got {lines:?}"
    );
    // clap filenames per shell under the default `completions/` dir.
    assert_eq!(
        lines[0],
        "installShellCompletion --cmd rg --bash completions/rg --zsh completions/_rg --fish completions/rg.fish"
    );
}

#[test]
fn completion_lines_skip_shells_without_install_flag() {
    use anodizer_core::config::{ArchiveConfig, CompletionsConfig};
    // powershell/elvish have no `installShellCompletion` flag — they stay
    // bundled in the archive but are not install-flagged.
    let archive = ArchiveConfig {
        completions: Some(CompletionsConfig {
            generate: Some("x".to_string()),
            shells: Some(vec!["bash".to_string(), "powershell".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let cc = crate_cfg_with_archive(archive);
    let lines = build_completion_install_lines(&cc, &["rg".to_string()]);
    assert_eq!(
        lines,
        vec!["installShellCompletion --cmd rg --bash completions/rg"]
    );
}

#[test]
fn manpage_line_emitted_when_archive_bundles_manpages() {
    use anodizer_core::config::{ArchiveConfig, ManpagesConfig};
    let archive = ArchiveConfig {
        manpages: Some(ManpagesConfig {
            generate: Some("{{ ArtifactPath }} --man".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let cc = crate_cfg_with_archive(archive);
    let lines = build_manpage_install_lines(&cc);
    assert_eq!(lines, vec!["installManPage man/man1/*"]);
}

// -----------------------------------------------------------------
// resolve_repo_coords — owner/name resolution + render.
// -----------------------------------------------------------------

#[test]
fn resolve_repo_coords_renders_owner_and_name_templates() {
    use anodizer_core::config::RepositoryConfig;
    let ctx = meta_ctx();
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("acme-{{ ProjectName }}".to_string()),
            name: Some("nix-overlay".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let coords = resolve_repo_coords(&ctx, &cfg, "mytool", &quiet_log()).expect("coords");
    assert_eq!(
        coords.repo_owner, "acme-demo",
        "owner template must render {{ ProjectName }} -> demo"
    );
    assert_eq!(coords.repo_name, "nix-overlay");
}

#[test]
fn resolve_repo_coords_missing_repository_bails() {
    let ctx = meta_ctx();
    let cfg = NixConfig::default();
    let err = resolve_repo_coords(&ctx, &cfg, "mytool", &quiet_log())
        .expect_err("absent repository config must bail");
    let msg = format!("{err}");
    assert!(msg.contains("no repository config"), "{msg}");
    assert!(msg.contains("mytool"), "{msg}");
}

// -----------------------------------------------------------------
// render_nix_for_validation + crate_has_nix_archive — the in-memory
// render twins (no clone, no subprocess). An Archive-kind artifact
// never registers as a Binary, so detect_dynamically_linked finds no
// binary artifacts and never touches disk — keeping these ungated.
// -----------------------------------------------------------------

fn archive_artifact(target: &str, url: &str, sha256: &str) -> anodizer_core::artifact::Artifact {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("url".to_string(), url.to_string());
    metadata.insert("sha256".to_string(), sha256.to_string());
    metadata.insert("format".to_string(), "tar.gz".to_string());
    Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from(format!("dist/{target}.tar.gz")),
        name: format!("mytool-{target}.tar.gz"),
        target: Some(target.to_string()),
        crate_name: "mytool".to_string(),
        metadata,
        size: None,
    }
}

fn validation_ctx(nix: NixConfig, artifacts: Vec<anodizer_core::artifact::Artifact>) -> Context {
    use anodizer_core::config::PublishConfig;
    use anodizer_core::test_helpers::TestContextBuilder;
    let mut ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig {
                nix: Some(nix),
                ..Default::default()
            }),
            ..Default::default()
        }])
        .build();
    for a in artifacts {
        ctx.artifacts.add(a);
    }
    ctx
}

const VALID_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[test]
fn render_nix_for_validation_renders_expression_without_clone() {
    let arts = vec![
        archive_artifact(
            "x86_64-unknown-linux-gnu",
            "https://e/x-linux.tar.gz",
            VALID_SHA,
        ),
        archive_artifact(
            "aarch64-apple-darwin",
            "https://e/x-darwin.tar.gz",
            VALID_SHA,
        ),
    ];
    let cfg = NixConfig {
        description: Some("demo tool".to_string()),
        ..Default::default()
    };
    let ctx = validation_ctx(cfg, arts);
    let render = render_nix_for_validation(&ctx, "mytool", &quiet_log())
        .expect("render ok")
        .expect("not skipped");
    assert_eq!(render.name, "mytool");
    assert!(
        render.expr.contains("pname = \"mytool\";"),
        "{}",
        render.expr
    );
    assert!(
        render.expr.contains("version = \"1.2.3\";"),
        "{}",
        render.expr
    );
    assert!(
        render.expr.contains("https://e/x-linux.tar.gz"),
        "linux archive url must be embedded: {}",
        render.expr
    );
    // Both systems mapped to a (system, url, hash) tuple.
    let systems: std::collections::HashSet<&str> =
        render.archives.iter().map(|(s, _, _)| s.as_str()).collect();
    assert!(systems.contains("x86_64-linux"));
    assert!(systems.contains("aarch64-darwin"));
}

#[test]
fn render_nix_for_validation_returns_none_when_skipped() {
    use anodizer_core::config::StringOrBool;
    let arts = vec![archive_artifact(
        "x86_64-unknown-linux-gnu",
        "https://e/x.tar.gz",
        VALID_SHA,
    )];
    let cfg = NixConfig {
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let ctx = validation_ctx(cfg, arts);
    let render = render_nix_for_validation(&ctx, "mytool", &quiet_log()).expect("ok");
    assert!(
        render.is_none(),
        "skip:true validation render must yield None"
    );
}

#[test]
fn render_nix_for_validation_missing_nix_config_bails() {
    use anodizer_core::config::PublishConfig;
    use anodizer_core::test_helpers::TestContextBuilder;
    // Crate has a publish block but no `nix` publisher configured.
    let ctx = TestContextBuilder::new()
        .project_name("demo")
        .crates(vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        }])
        .build();
    let err = render_nix_for_validation(&ctx, "mytool", &quiet_log())
        .expect_err("absent nix config must bail");
    assert!(format!("{err}").contains("no nix config"));
}

#[test]
fn crate_has_nix_archive_true_when_nix_system_maps() {
    let arts = vec![archive_artifact(
        "x86_64-unknown-linux-gnu",
        "https://e/x.tar.gz",
        VALID_SHA,
    )];
    let cfg = NixConfig::default();
    let ctx = validation_ctx(cfg.clone(), arts);
    assert!(
        crate_has_nix_archive(&ctx, &cfg, "mytool").expect("ok"),
        "a linux archive maps to x86_64-linux"
    );
}

#[test]
fn crate_has_nix_archive_false_when_only_non_nix_systems() {
    // A windows archive (valid sha256) maps to no nix system: genuine
    // absence, NOT an error — Ok(false), not Err.
    let arts = vec![archive_artifact(
        "x86_64-pc-windows-msvc",
        "https://e/x.zip",
        VALID_SHA,
    )];
    let cfg = NixConfig::default();
    let ctx = validation_ctx(cfg.clone(), arts);
    assert!(
        !crate_has_nix_archive(&ctx, &cfg, "mytool").expect("absence is Ok(false)"),
        "windows-only artifacts map to no nix system"
    );
}

#[test]
fn crate_has_nix_archive_errors_on_present_but_sha_less_artifact() {
    // A matched artifact missing its sha256 is present-but-broken: the
    // collect step bails so the publisher surfaces it rather than skipping.
    let arts = vec![archive_artifact(
        "x86_64-unknown-linux-gnu",
        "https://e/x.tar.gz",
        "",
    )];
    let cfg = NixConfig::default();
    let ctx = validation_ctx(cfg.clone(), arts);
    let err = crate_has_nix_archive(&ctx, &cfg, "mytool")
        .expect_err("missing sha256 on a present artifact must error, not skip");
    assert!(format!("{err}").contains("sha256"));
}

#[test]
fn render_nix_for_validation_bails_on_sha_less_nix_artifact() {
    let arts = vec![archive_artifact(
        "x86_64-unknown-linux-gnu",
        "https://e/x.tar.gz",
        "",
    )];
    let cfg = NixConfig::default();
    let ctx = validation_ctx(cfg, arts);
    let err = render_nix_for_validation(&ctx, "mytool", &quiet_log())
        .expect_err("sha-less nix artifact must bail before rendering");
    assert!(format!("{err}").contains("sha256"));
}

#[test]
fn render_nix_for_validation_bad_if_template_bails() {
    // A malformed `if` condition makes `check_skip_guards` -> the shared
    // `evaluate_if_condition` propagate an Err rather than a skip boolean.
    let arts = vec![archive_artifact(
        "x86_64-unknown-linux-gnu",
        "https://e/x.tar.gz",
        VALID_SHA,
    )];
    let cfg = NixConfig {
        if_condition: Some("{{ unclosed".to_string()),
        ..Default::default()
    };
    let ctx = validation_ctx(cfg, arts);
    let err = render_nix_for_validation(&ctx, "mytool", &quiet_log())
        .expect_err("malformed `if` template must propagate an error");
    assert!(format!("{err:#}").contains("nix publisher for crate 'mytool'"));
}

#[test]
fn render_nix_for_validation_bad_skip_template_bails() {
    // A malformed `skip` template surfaces through the first guard's
    // `with_context("render skip template …")` wrapper.
    let arts = vec![archive_artifact(
        "x86_64-unknown-linux-gnu",
        "https://e/x.tar.gz",
        VALID_SHA,
    )];
    let cfg = NixConfig {
        skip: Some(anodizer_core::config::StringOrBool::String(
            "{{ unclosed".to_string(),
        )),
        ..Default::default()
    };
    let ctx = validation_ctx(cfg, arts);
    let err = render_nix_for_validation(&ctx, "mytool", &quiet_log())
        .expect_err("malformed `skip` template must propagate an error");
    assert!(format!("{err:#}").contains("skip template"));
}

// =================================================================
// Subprocess-driven paths: formatter (alejandra/nixfmt) + the full
// clone -> write -> flake -> commit -> push pipeline. Every test here
// spawns `git` (and a fake formatter) and mutates `PATH`/env, so the
// whole module is `#[cfg(unix)]`-gated (precedent: npm/tests.rs,
// homebrew/publish_formula.rs). Coverage is measured on ubuntu, so the
// gate costs nothing while keeping Windows builds warning-free.
// =================================================================
#[cfg(unix)]
mod subprocess {
    use super::*;
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{
        CommitAuthorConfig, GitRepoConfig, PublishConfig, ReleaseConfig, RepositoryConfig,
        StringOrBool,
    };
    use anodizer_core::context::Context;
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;
    use serial_test::serial;
    use std::path::Path;
    use std::process::Command;

    const SAMPLE_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn git_ok(dir: &Path, args: &[&str]) {
        anodizer_core::test_helpers::git_test_ok(dir, args)
    }

    fn git_stdout(dir: &Path, args: &[&str]) -> String {
        anodizer_core::test_helpers::git_test_stdout(dir, args)
    }

    /// Bare overlay repo seeded with one commit on `branch`, usable as a
    /// local `git clone` URL. The publisher clones it, writes the
    /// derivation + flake, commits, and pushes back — the bare repo is the
    /// assertion surface (inspect the landed `default.nix` / `flake.nix`).
    fn make_bare_repo(branch: &str) -> (String, tempfile::TempDir) {
        let bare = tempfile::tempdir().expect("bare tempdir");
        let seed = tempfile::tempdir().expect("seed tempdir");
        git_ok(bare.path(), &["init", "--bare", "-b", branch]);
        git_ok(seed.path(), &["init", "-b", branch]);
        git_ok(seed.path(), &["config", "user.email", "t@example.invalid"]);
        git_ok(seed.path(), &["config", "user.name", "T"]);
        git_ok(seed.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(seed.path().join("README"), "overlay\n").unwrap();
        git_ok(seed.path(), &["add", "README"]);
        git_ok(seed.path(), &["commit", "-m", "seed overlay"]);
        assert!(
            anodizer_core::test_helpers::output_with_spawn_retry(
                || {
                    let mut cmd = Command::new("git");
                    cmd.args(["remote", "add", "origin"])
                        .arg(bare.path())
                        .current_dir(seed.path());
                    cmd
                },
                "git",
            )
            .status
            .success(),
            "git remote add origin failed"
        );
        git_ok(seed.path(), &["push", "-u", "origin", branch]);
        (bare.path().to_string_lossy().into_owned(), bare)
    }

    /// Read a file's content as landed on the bare repo's `branch` ref.
    fn show(bare: &Path, branch: &str, path: &str) -> String {
        git_stdout(bare, &["show", &format!("{branch}:{path}")])
    }

    fn archive(target: &str, url: &str, sha: &str) -> Artifact {
        let mut metadata = std::collections::HashMap::new();
        metadata.insert("url".to_string(), url.to_string());
        metadata.insert("sha256".to_string(), sha.to_string());
        metadata.insert("format".to_string(), "tar.gz".to_string());
        Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from(format!("/tmp/{target}.tar.gz")),
            name: format!("mytool-{target}.tar.gz"),
            target: Some(target.to_string()),
            crate_name: "mytool".to_string(),
            metadata,
            size: None,
        }
    }

    fn nix_cfg_local(bare_url: &str, branch: &str) -> NixConfig {
        NixConfig {
            repository: Some(RepositoryConfig {
                owner: Some("myorg".to_string()),
                name: Some("nix-overlay".to_string()),
                branch: Some(branch.to_string()),
                git: Some(GitRepoConfig {
                    url: Some(bare_url.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn ctx_for(nix: NixConfig, artifacts: Vec<Artifact>) -> Context {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![CrateConfig {
                name: "mytool".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                release: Some(ReleaseConfig {
                    github: Some(anodizer_core::config::ScmRepoConfig {
                        owner: "myorg".to_string(),
                        name: "mytool".to_string(),
                        token: None,
                    }),
                    ..Default::default()
                }),
                publish: Some(PublishConfig {
                    nix: Some(nix),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .build();
        for a in artifacts {
            ctx.artifacts.add(a);
        }
        ctx
    }

    fn two_archives() -> Vec<Artifact> {
        vec![
            archive(
                "x86_64-unknown-linux-gnu",
                "https://e/mytool-linux-x64.tar.gz",
                SAMPLE_SHA,
            ),
            archive(
                "aarch64-apple-darwin",
                "https://e/mytool-darwin-arm64.tar.gz",
                SAMPLE_SHA,
            ),
        ]
    }

    // -------------------------------------------------------------
    // run_formatter — mandatory-format matrix. Formatting is opt-in
    // (None = no-op, matches GR) but once a formatter is configured it
    // is MANDATORY in EVERY mode (no --strict gate): a missing binary,
    // a non-zero exit, or an unknown name each bail so an unformatted
    // derivation is never pushed. INTENTIONALLY stricter than GR.
    // -------------------------------------------------------------

    #[test]
    fn run_formatter_none_is_noop() {
        let cfg = NixConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("default.nix");
        std::fs::write(&f, "{}\n").unwrap();
        run_formatter(&cfg, &f, &quiet_log()).expect("no formatter is Ok");
    }

    #[test]
    #[serial]
    fn run_formatter_runs_configured_alejandra_with_file_arg() {
        let tools = FakeToolDir::new();
        tools.tool("alejandra").install();
        let _path = tools.activate();
        let cfg = NixConfig {
            formatter: Some("alejandra".to_string()),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("default.nix");
        std::fs::write(&f, "{}\n").unwrap();
        run_formatter(&cfg, &f, &quiet_log()).expect("formatter success is Ok");
        // The version-flag probe (`tool_detect::runs`) and the format run
        // each invoke the fake tool once.
        let calls = tools.calls("alejandra");
        assert_eq!(
            calls.last().expect("alejandra invoked"),
            &vec![f.to_string_lossy().to_string()],
            "formatter receives the generated file path as its sole arg"
        );
    }

    #[test]
    #[serial]
    fn run_formatter_nonzero_exit_bails_even_in_lenient_mode() {
        // No --strict set: the bail must fire regardless, so an
        // unformatted derivation is never pushed. The stub answers the
        // presence probe (`--version`) with exit 0 but fails the actual
        // format invocation with exit 3.
        let tools = FakeToolDir::new();
        tools
            .tool("nixfmt")
            .script(
                "case \"$1\" in --version) exit 0 ;; *) echo 'parse error' 1>&2; exit 3 ;; esac",
            )
            .install();
        let _path = tools.activate();
        let cfg = NixConfig {
            formatter: Some("nixfmt".to_string()),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("default.nix");
        std::fs::write(&f, "{}\n").unwrap();
        let err = run_formatter(&cfg, &f, &quiet_log())
            .expect_err("non-zero formatter exit must bail in lenient mode too");
        let msg = format!("{err}");
        assert!(msg.contains("nixfmt formatting failed"), "{msg}");
        assert!(
            msg.contains("refusing to push an unformatted derivation"),
            "{msg}"
        );
        assert!(msg.contains("exit 3"), "{msg}");
    }

    #[test]
    #[serial]
    fn run_formatter_missing_binary_bails_with_install_remedy() {
        // A FakeToolDir that installs NO formatter: `alejandra` is a
        // recognized name but absent from PATH, so the presence probe
        // fails and run_formatter bails (no --strict needed). Prepending
        // (rather than emptying) PATH keeps git/sh available for other
        // concurrently-running tests and avoids a process-wide PATH race.
        let tools = FakeToolDir::new();
        let _path = tools.activate();
        let cfg = NixConfig {
            formatter: Some("alejandra".to_string()),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("default.nix");
        std::fs::write(&f, "{}\n").unwrap();
        let err = run_formatter(&cfg, &f, &quiet_log())
            .expect_err("missing formatter binary must bail in lenient mode");
        let msg = format!("{err}");
        assert!(
            msg.contains("formatter 'alejandra' not found on PATH"),
            "{msg}"
        );
        assert!(msg.contains("install it"), "{msg}");
    }

    #[test]
    fn run_formatter_unknown_name_bails_in_lenient_mode() {
        let cfg = NixConfig {
            formatter: Some("rustfmt".to_string()),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("default.nix");
        std::fs::write(&f, "{}\n").unwrap();
        let err = run_formatter(&cfg, &f, &quiet_log())
            .expect_err("unrecognized formatter must bail in lenient mode");
        let msg = format!("{err}");
        assert!(msg.contains("unknown formatter 'rustfmt'"), "{msg}");
        assert!(msg.contains("alejandra or nixfmt"), "{msg}");
    }

    // -------------------------------------------------------------
    // publish_to_nix — full clone/write/flake/commit/push pipeline.
    // -------------------------------------------------------------

    #[test]
    fn publish_to_nix_direct_push_lands_derivation_and_flake() {
        let (bare_url, bare) = make_bare_repo("main");
        let nix = nix_cfg_local(&bare_url, "main");
        let mut ctx = ctx_for(nix, two_archives());

        let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        assert!(pushed, "a real push must return Ok(true)");

        let bare_path = Path::new(&bare_url);
        // Default path is pkgs/<name>/default.nix.
        let drv = show(bare_path, "main", "pkgs/mytool/default.nix");
        assert!(drv.contains("pname = \"mytool\";"), "{drv}");
        assert!(drv.contains("version = \"1.2.3\";"), "{drv}");
        assert!(
            drv.contains("https://e/mytool-linux-x64.tar.gz"),
            "linux archive url must be embedded: {drv}"
        );
        assert!(
            drv.contains("x86_64-linux") && drv.contains("aarch64-darwin"),
            "both nix systems must be mapped: {drv}"
        );
        // The root flake referencing the package is written too.
        let flake = show(bare_path, "main", "flake.nix");
        assert!(
            flake.contains("mytool"),
            "flake must reference package: {flake}"
        );

        let subject = git_stdout(bare_path, &["log", "-1", "--pretty=%s", "main"]);
        assert!(
            subject.contains("mytool") && subject.contains("1.2.3"),
            "commit subject must name package + version; got: {subject}"
        );
        drop(bare);
    }

    #[test]
    #[serial]
    fn publish_to_nix_formatter_absent_errors_and_pushes_nothing() {
        // A configured formatter that is absent from PATH must abort the
        // crate's nix publish BEFORE flake write / commit / push — nothing
        // lands on the overlay branch. The FakeToolDir installs NO
        // alejandra (prepend, not empty, so the file-URL clone's git still
        // resolves), so the presence probe fails after clone+write.
        let (bare_url, bare) = make_bare_repo("main");
        let mut nix = nix_cfg_local(&bare_url, "main");
        nix.formatter = Some("alejandra".to_string());
        let mut ctx = ctx_for(nix, two_archives());

        let bare_path = Path::new(&bare_url);
        let before = git_stdout(bare_path, &["rev-parse", "main"]);

        let tools = FakeToolDir::new();
        let _path = tools.activate();
        let res = publish_to_nix(&mut ctx, "mytool", &quiet_log());
        drop(_path);

        let err = res.expect_err("missing formatter must abort the publish");
        assert!(format!("{err}").contains("not found on PATH"), "{err}");
        // The overlay branch is untouched: same tip, no default.nix landed.
        let after = git_stdout(bare_path, &["rev-parse", "main"]);
        assert_eq!(before, after, "no commit must reach the overlay branch");
        let drv_present = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["cat-file", "-e", "main:pkgs/mytool/default.nix"])
                    .current_dir(bare_path);
                cmd
            },
            "git",
        )
        .status
        .success();
        assert!(!drv_present, "no unformatted derivation must be pushed");
        drop(bare);
    }

    #[test]
    fn publish_to_nix_honors_custom_path_and_commit_author() {
        let (bare_url, bare) = make_bare_repo("main");
        let mut nix = nix_cfg_local(&bare_url, "main");
        nix.path = Some("packages/mytool.nix".to_string());
        nix.commit_author = Some(CommitAuthorConfig {
            name: Some("Nix Bot".to_string()),
            email: Some("nix-bot@example.invalid".to_string()),
            ..Default::default()
        });
        let mut ctx = ctx_for(nix, two_archives());

        let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        assert!(pushed);

        let bare_path = Path::new(&bare_url);
        let drv = show(bare_path, "main", "packages/mytool.nix");
        assert!(drv.contains("pname = \"mytool\";"), "{drv}");
        // The default pkgs/<name>/default.nix path must NOT exist.
        let default_path = anodizer_core::test_helpers::output_with_spawn_retry(
            || {
                let mut cmd = Command::new("git");
                cmd.args(["cat-file", "-e", "main:pkgs/mytool/default.nix"])
                    .current_dir(bare_path);
                cmd
            },
            "git",
        )
        .status;
        assert!(
            !default_path.success(),
            "derivation must live at the configured path, not the default"
        );
        // The configured commit_author must drive the landed author —
        // proving the identity is applied via the GIT_AUTHOR_* child env
        // (which overrides inherited env + repo config), not via
        // `-c user.name=` (which git's precedence defeats whenever an
        // ambient GIT_AUTHOR_NAME is present).
        let author = git_stdout(bare_path, &["log", "-1", "--pretty=%an", "main"]);
        assert_eq!(author, "Nix Bot", "configured commit author must drive %an");
        let author_email = git_stdout(bare_path, &["log", "-1", "--pretty=%ae", "main"]);
        assert_eq!(
            author_email, "nix-bot@example.invalid",
            "configured commit author email must drive %ae over the ambient GIT_AUTHOR_EMAIL"
        );
        drop(bare);
    }

    #[test]
    fn publish_to_nix_second_run_is_noop_no_extra_commit() {
        let (bare_url, bare) = make_bare_repo("main");
        let nix = nix_cfg_local(&bare_url, "main");
        let mut ctx = ctx_for(nix.clone(), two_archives());
        publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("first publish");
        let bare_path = Path::new(&bare_url);
        let head1 = git_stdout(bare_path, &["rev-parse", "main"]);

        let mut ctx2 = ctx_for(nix, two_archives());
        let pushed2 = publish_to_nix(&mut ctx2, "mytool", &quiet_log()).expect("second publish");
        let head2 = git_stdout(bare_path, &["rev-parse", "main"]);
        assert!(!pushed2, "an unchanged re-publish must report no push");
        assert_eq!(head1, head2, "no new commit when nothing changed");
        drop(bare);
    }

    #[test]
    fn publish_to_nix_dry_run_makes_no_commit() {
        let (bare_url, bare) = make_bare_repo("main");
        let nix = nix_cfg_local(&bare_url, "main");
        let mut ctx = ctx_for(nix, two_archives());
        ctx.options.dry_run = true;
        let bare_path = Path::new(&bare_url);
        let head_before = git_stdout(bare_path, &["rev-parse", "main"]);

        let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("dry-run ok");
        assert!(!pushed, "dry-run must not push");
        let head_after = git_stdout(bare_path, &["rev-parse", "main"]);
        assert_eq!(
            head_before, head_after,
            "dry-run must leave the repo untouched"
        );
        drop(bare);
    }

    #[test]
    fn publish_to_nix_skip_true_returns_false_without_clone() {
        // `skip: true` short-circuits before any repo coordinate is even
        // resolved; an invalid bare URL would error if a clone were
        // attempted, so a clean Ok(false) proves the skip gate fired first.
        let mut nix = nix_cfg_local("/nonexistent/not-a-repo", "main");
        nix.skip = Some(StringOrBool::Bool(true));
        let mut ctx = ctx_for(nix, two_archives());
        let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("skip ok");
        assert!(!pushed, "skip:true must return Ok(false) and not clone");
    }

    #[test]
    fn publish_to_nix_if_condition_falsy_returns_false_without_clone() {
        let mut nix = nix_cfg_local("/nonexistent/not-a-repo", "main");
        nix.if_condition = Some("false".to_string());
        let mut ctx = ctx_for(nix, two_archives());
        let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("if-falsy ok");
        assert!(!pushed, "falsy `if` must return Ok(false) and not clone");
    }

    #[test]
    fn publish_to_nix_skip_upload_returns_false_without_clone() {
        let mut nix = nix_cfg_local("/nonexistent/not-a-repo", "main");
        nix.skip_upload = Some(StringOrBool::Bool(true));
        let mut ctx = ctx_for(nix, two_archives());
        let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("skip_upload ok");
        assert!(!pushed, "skip_upload must return Ok(false) and not clone");
    }

    #[test]
    fn publish_to_nix_pull_request_enabled_records_outcome() {
        // With `pull_request.enabled = true`, finalize_publish drives
        // maybe_submit_pr, which yields Some(outcome) and is recorded on
        // the context. The direct push still lands; the PR attempt (no gh
        // resolvable against a fake fork) surfaces a recorded outcome —
        // proving the `if let Some(pr_outcome)` branch ran (a non-PR
        // publish records nothing).
        use anodizer_core::config::PullRequestConfig;
        let (bare_url, bare) = make_bare_repo("main");
        let mut nix = nix_cfg_local(&bare_url, "main");
        if let Some(repo) = nix.repository.as_mut() {
            repo.pull_request = Some(PullRequestConfig {
                enabled: Some(true),
                ..Default::default()
            });
        }
        let mut ctx = ctx_for(nix, two_archives());
        let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        assert!(pushed, "the direct push to the overlay branch still lands");
        assert!(
            ctx.take_pending_outcome().is_some(),
            "an enabled pull_request must record a publisher outcome"
        );
        // The landed derivation is still correct.
        let drv = show(Path::new(&bare_url), "main", "pkgs/mytool/default.nix");
        assert!(drv.contains("pname = \"mytool\";"), "{drv}");
        drop(bare);
    }

    #[test]
    #[serial]
    fn publish_to_nix_runs_configured_formatter_on_generated_file() {
        // A configured formatter is invoked against the written derivation
        // before commit; the fake formatter records its argv so we can
        // assert the generated default.nix path was handed to it.
        let tools = FakeToolDir::new();
        tools.tool("nixfmt").install();
        let _path = tools.activate();
        let (bare_url, bare) = make_bare_repo("main");
        let mut nix = nix_cfg_local(&bare_url, "main");
        nix.formatter = Some("nixfmt".to_string());
        let mut ctx = ctx_for(nix, two_archives());
        let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        assert!(pushed);
        // run_formatter probes presence (`--version`) then formats; assert
        // the generated default.nix path was handed to the format call.
        let calls = tools.calls("nixfmt");
        let formatted = calls.iter().any(|c| {
            c.last()
                .is_some_and(|p| p.ends_with("pkgs/mytool/default.nix"))
        });
        assert!(
            formatted,
            "formatter must receive the generated derivation path: {calls:?}"
        );
        drop(bare);
    }

    #[test]
    fn publish_to_nix_embeds_post_install_and_custom_install_lines() {
        // Exercises the install_lines / post_install_lines plumbing of
        // render_nix_derivation_inner end-to-end: both land verbatim in
        // the rendered derivation.
        let (bare_url, bare) = make_bare_repo("main");
        let mut nix = nix_cfg_local(&bare_url, "main");
        nix.install = Some("mkdir -p $out/bin\ncp ./mytool $out/bin/".to_string());
        nix.post_install = Some("echo done >$out/.installed".to_string());
        let mut ctx = ctx_for(nix, two_archives());
        publish_to_nix(&mut ctx, "mytool", &quiet_log()).expect("publish ok");
        let drv = show(Path::new(&bare_url), "main", "pkgs/mytool/default.nix");
        assert!(
            drv.contains("cp ./mytool $out/bin/"),
            "custom install line must be embedded: {drv}"
        );
        assert!(
            drv.contains("echo done >$out/.installed"),
            "post_install line must be embedded: {drv}"
        );
        drop(bare);
    }

    /// A `nix.description` template that fails to render (undefined
    /// field) falls back to its raw `{{ }}` text via `render_or_warn` and
    /// lands in the derivation — `guard_no_unrendered` must hard-fail the
    /// real publish before anything is written to the overlay branch.
    #[test]
    fn publish_residual_description_template_errors_before_push() {
        let (bare_url, bare) = make_bare_repo("main");
        let mut nix = nix_cfg_local(&bare_url, "main");
        nix.description = Some("{{ .NoSuchField }}".to_string());
        let mut ctx = ctx_for(nix, two_archives());

        let bare_path = Path::new(&bare_url);
        let before = git_stdout(bare_path, &["rev-parse", "main"]);

        let err = publish_to_nix(&mut ctx, "mytool", &quiet_log())
            .expect_err("residual {{ }} in the derivation must hard-fail");
        assert!(
            format!("{err:#}").contains("nix derivation"),
            "error must name the manifest label; got: {err:#}"
        );
        let after = git_stdout(bare_path, &["rev-parse", "main"]);
        assert_eq!(
            before, after,
            "a residual-delimiter bail must leave the overlay branch untouched"
        );
        drop(bare);
    }

    /// The same residual `nix.description` template stays lenient in
    /// dry-run: `publish_to_nix` early-returns before the derivation
    /// render (and therefore before the guard), so the call must still
    /// report `Ok(false)` rather than surface the residual as an error.
    #[test]
    fn publish_residual_description_template_dry_run_stays_lenient() {
        let (bare_url, bare) = make_bare_repo("main");
        let mut nix = nix_cfg_local(&bare_url, "main");
        nix.description = Some("{{ .NoSuchField }}".to_string());
        let mut ctx = TestContextBuilder::new()
            .crates(vec![CrateConfig {
                name: "mytool".to_string(),
                path: ".".to_string(),
                tag_template: Some("v{{ .Version }}".to_string()),
                release: Some(ReleaseConfig {
                    github: Some(anodizer_core::config::ScmRepoConfig {
                        owner: "myorg".to_string(),
                        name: "mytool".to_string(),
                        token: None,
                    }),
                    ..Default::default()
                }),
                publish: Some(PublishConfig {
                    nix: Some(nix),
                    ..Default::default()
                }),
                ..Default::default()
            }])
            .dry_run(true)
            .build();
        for a in two_archives() {
            ctx.artifacts.add(a);
        }

        let pushed = publish_to_nix(&mut ctx, "mytool", &quiet_log())
            .expect("dry-run must stay lenient on a residual template");
        assert!(!pushed, "dry-run must report no push");
        drop(bare);
    }
}
