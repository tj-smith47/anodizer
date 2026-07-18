use super::*;

#[test]
fn resolve_msi_tools_threads_resolved_wix_tool_from_config() {
    use anodizer_core::config::{Config, CrateConfig, MsiConfig};

    // The dispatcher must thread the WiX tool requirement resolved from
    // config into the harness gate — not a host-static guess. A `version:
    // v4` config resolves deterministically to the single `wix` CLI
    // (V4 is never downgraded), so this proves the config→tools wiring
    // independent of which WiX binaries the test host happens to carry.
    // (The host-aware v3 → candle+light path — the actual release-blocker
    // — is pinned by `anodizer_stage_msi`'s `required_msi_tools` tests.)
    let msi_cfg = MsiConfig {
        wxs: Some("app.wxs".to_string()),
        version: Some("v4".to_string()),
        ..Default::default()
    };
    let config = Config {
        project_name: "myapp".to_string(),
        crates: vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            msis: Some(vec![msi_cfg]),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(resolve_msi_tools(Some(&config)), vec!["wix".to_string()]);
}

#[test]
fn resolve_msi_tools_none_config_is_empty() {
    assert!(resolve_msi_tools(None).is_empty());
}

#[test]
fn resolve_upx_tools_threads_enabled_binary_from_config() {
    use anodizer_core::config::{Config, StringOrBool, UpxConfig};

    // The dispatcher must thread each enabled `upx:` entry's binary into
    // the harness gate so `--require-tools` can hard-fail a host-default
    // upx run whose binary is absent. A disabled entry contributes nothing.
    let config = Config {
        project_name: "myapp".to_string(),
        upx: vec![
            UpxConfig {
                enabled: Some(StringOrBool::Bool(true)),
                binary: "upx".to_string(),
                ..Default::default()
            },
            UpxConfig {
                enabled: Some(StringOrBool::Bool(false)),
                binary: "other-upx".to_string(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert_eq!(resolve_upx_tools(Some(&config)), vec!["upx".to_string()]);
}

#[test]
fn resolve_upx_tools_none_config_is_empty() {
    assert!(resolve_upx_tools(None).is_empty());
}

#[test]
fn resolve_upx_tools_force_requires_templated_enabled() {
    use anodizer_core::config::{Config, StringOrBool, UpxConfig};

    // A context-dependent `enabled:` can render false in the bare gate
    // context yet true in the `--snapshot` child. Because the upx stage
    // WARN-SKIPS a missing binary (silent false coverage), the gate must
    // over-require: a templated `enabled` forces its binary in even when
    // the bare-context render is false. This template renders literally
    // `false` here (no vars), so `required_upx_tools` alone would drop it —
    // proving the conservative pass, not the SSOT, is what adds it.
    let config = Config {
        project_name: "myapp".to_string(),
        upx: vec![UpxConfig {
            enabled: Some(StringOrBool::String(
                "{{ if false }}true{{ else }}false{{ end }}".to_string(),
            )),
            binary: "upx".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(resolve_upx_tools(Some(&config)), vec!["upx".to_string()]);
}

#[test]
fn resolve_upx_tools_omits_literal_false_enabled() {
    use anodizer_core::config::{Config, StringOrBool, UpxConfig};

    // The conservative pass must fire ONLY for templates: a literal
    // `enabled: false` is context-free, so the gate trusts the SSOT and
    // carries no requirement (no spurious hard-fail under --require-tools).
    let config = Config {
        project_name: "myapp".to_string(),
        upx: vec![UpxConfig {
            enabled: Some(StringOrBool::Bool(false)),
            binary: "upx".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(resolve_upx_tools(Some(&config)).is_empty());
}

#[test]
fn parse_stages_default_returns_host_native_partition() {
    // No `--stages` resolves to the OS-native partition (the encoded
    // `det_stages` that used to live per-shard in determinism.yml), not a
    // minimal subset that would silently under-cover the release.
    let stages = parse_stages(None).expect("None is always Ok");
    assert_eq!(stages, default_stages_for_host());
    // The common base is present on every OS.
    for base in [
        StageId::Build,
        StageId::Source,
        StageId::Upx,
        StageId::Archive,
        StageId::Sbom,
        StageId::Sign,
        StageId::Checksum,
    ] {
        assert!(stages.contains(&base), "base stage {base:?} missing");
    }
}

#[test]
fn default_stages_for_host_includes_os_native_producers() {
    let stages = default_stages_for_host();
    // cargo-package is harness-only and stays opt-in on every OS.
    assert!(
        !stages.contains(&StageId::CargoPackage),
        "cargo-package must never be in the host default"
    );
    #[cfg(target_os = "linux")]
    for s in [
        StageId::Nfpm,
        StageId::Makeself,
        StageId::Snapcraft,
        StageId::Srpm,
        StageId::Docker,
        StageId::Appimage,
        StageId::Flatpak,
    ] {
        assert!(stages.contains(&s), "linux default missing {s:?}");
    }
    #[cfg(target_os = "macos")]
    for s in [StageId::Appbundle, StageId::Dmg, StageId::Pkg] {
        assert!(stages.contains(&s), "macos default missing {s:?}");
    }
    #[cfg(target_os = "windows")]
    for s in [StageId::Msi, StageId::Nsis] {
        assert!(stages.contains(&s), "windows default missing {s:?}");
    }
}

#[test]
fn derived_stage_sets_are_subsets_of_stage_id_vocabulary() {
    // Both derived sets must draw only from the `StageId` enum, and every
    // configured-producer token must round-trip back to a variant — the
    // guarantee that a producer can never be byte-verified on a host
    // default yet be unselectable via `--stages` (or vice versa).
    let vocab: std::collections::HashSet<StageId> = StageId::iter().collect();
    for s in default_stages_for_host() {
        assert!(
            vocab.contains(&s),
            "host-default stage {s:?} not in StageId vocabulary"
        );
    }

    use anodizer_core::config::{Config, CrateConfig, NfpmConfig, SrpmConfig};
    let config = Config {
        crates: vec![CrateConfig {
            name: "x".to_string(),
            path: ".".to_string(),
            nfpms: Some(vec![NfpmConfig::default()]),
            ..Default::default()
        }],
        srpms: Some(SrpmConfig::default()),
        ..Default::default()
    };
    let configured = anodizer_core::env_preflight::configured_producer_stages(&config);
    // The representative config configures exactly nfpm + srpm; both must
    // map back to a `StageId` variant.
    assert!(configured.contains("nfpm") && configured.contains("srpm"));
    for tok in &configured {
        let s = StageId::from_token(tok)
            .unwrap_or_else(|| panic!("producer token `{tok}` has no StageId variant"));
        assert!(vocab.contains(&s));
    }

    // The derived host default matches the previously-hardcoded per-OS
    // expectation for the build host.
    let host: std::collections::HashSet<StageId> = default_stages_for_host().into_iter().collect();
    #[cfg(target_os = "linux")]
    for s in [
        StageId::Nfpm,
        StageId::Makeself,
        StageId::Snapcraft,
        StageId::Srpm,
        StageId::Docker,
        StageId::Appimage,
        StageId::Flatpak,
    ] {
        assert!(host.contains(&s), "linux derived default missing {s:?}");
    }
    #[cfg(target_os = "macos")]
    for s in [StageId::Appbundle, StageId::Dmg, StageId::Pkg] {
        assert!(host.contains(&s), "macos derived default missing {s:?}");
    }
    #[cfg(target_os = "windows")]
    for s in [StageId::Msi, StageId::Nsis] {
        assert!(host.contains(&s), "windows derived default missing {s:?}");
    }
    // cargo-package is harness-only and never enters any host default.
    assert!(!host.contains(&StageId::CargoPackage));
}

#[test]
fn parse_stages_accepts_appimage_and_flatpak() {
    let stages = parse_stages(Some("appimage,flatpak")).expect("both are known stages");
    assert_eq!(stages, vec![StageId::Appimage, StageId::Flatpak]);
}

/// A minimal config (one crate, no producer blocks) must resolve the
/// `--stages`-absent default to the always-on base ONLY — every config-
/// gated producer is pruned, so `--require-tools` cannot hard-fail on a
/// tool for an artifact this project never builds.
#[test]
fn host_default_excludes_unconfigured_producers() {
    use anodizer_core::config::{Config, CrateConfig};
    let config = Config {
        project_name: "minimal".to_string(),
        crates: vec![CrateConfig {
            name: "minimal".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let stages = host_default_for_config(Some(&config));
    // Base stays unconditionally.
    for base in ALWAYS_ON_STAGES {
        assert!(stages.contains(base), "base stage {base:?} must remain");
    }
    // No config-gated producer survives on any OS.
    for gated in [
        StageId::Nfpm,
        StageId::Makeself,
        StageId::Snapcraft,
        StageId::Srpm,
        StageId::Docker,
        StageId::Appimage,
        StageId::Flatpak,
        StageId::Appbundle,
        StageId::Dmg,
        StageId::Pkg,
        StageId::Msi,
        StageId::Nsis,
    ] {
        assert!(
            !stages.contains(&gated),
            "unconfigured producer {gated:?} must be pruned from the default"
        );
    }
}

/// A config that DOES configure the Linux producers must keep them in the
/// resolved default (so they are byte-verified, and `--require-tools`
/// legitimately requires their tools). Mixes per-crate blocks (nfpm /
/// snapcraft / flatpak / docker) and top-level blocks (appimage / makeself
/// / srpm) to cover both detection paths.
#[cfg(target_os = "linux")]
#[test]
fn host_default_includes_configured_linux_producers() {
    use anodizer_core::config::{
        AppImageConfig, Config, CrateConfig, DockerV2Config, FlatpakConfig, MakeselfConfig,
        NfpmConfig, SnapcraftConfig, SrpmConfig,
    };
    let config = Config {
        project_name: "full".to_string(),
        crates: vec![CrateConfig {
            name: "full".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            nfpms: Some(vec![NfpmConfig::default()]),
            snapcrafts: Some(vec![SnapcraftConfig::default()]),
            flatpaks: Some(vec![FlatpakConfig::default()]),
            dockers_v2: Some(vec![DockerV2Config::default()]),
            ..Default::default()
        }],
        appimages: vec![AppImageConfig::default()],
        makeselfs: vec![MakeselfConfig::default()],
        srpms: Some(SrpmConfig::default()),
        ..Default::default()
    };
    let stages = host_default_for_config(Some(&config));
    for producer in [
        StageId::Nfpm,
        StageId::Makeself,
        StageId::Snapcraft,
        StageId::Srpm,
        StageId::Docker,
        StageId::Appimage,
        StageId::Flatpak,
    ] {
        assert!(
            stages.contains(&producer),
            "configured producer {producer:?} must stay in the default"
        );
    }
}

/// `None` config (load failed) falls back to the full OS partition — the
/// conservative "do not silently under-verify" choice.
#[test]
fn host_default_none_config_is_full_partition() {
    assert_eq!(host_default_for_config(None), default_stages_for_host());
}

/// The real false-coverage guard: a GENUINE render error in a declared
/// `dockers_v2` entry (malformed `dockerfile` template) must PROPAGATE from
/// `resolve_docker_configs` (the `?` path), never be swallowed into an empty
/// result. The dispatcher then hard-fails it under `--require-tools` /
/// explicit `--stages=docker`. Contrast with the harness's
/// `docker_stage_declared_but_all_skipped_warns_not_errors_even_when_explicit`,
/// which pins the LEGITIMATE-skip (empty, no error) case.
#[test]
fn resolve_docker_configs_propagates_render_error() {
    use anodizer_core::config::{Config, CrateConfig, DockerV2Config};
    let config = Config {
        project_name: "full".to_string(),
        crates: vec![CrateConfig {
            name: "full".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            dockers_v2: Some(vec![DockerV2Config {
                // Unknown filter → hard render error regardless of strict mode.
                dockerfile: "{{ Version | this_filter_does_not_exist }}".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    let err = resolve_docker_configs(Some(&config), Some("full"), true, &log)
        .expect_err("a malformed dockerfile template must propagate, not resolve to empty");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("dockerfile"),
        "the propagated error must be the dockerfile render failure (not a swallowed empty \
             result nor an unrelated setup error): {msg}"
    );
}

/// Production parity at the resolver: a declared entry with a truthy `skip:`
/// resolves to an EMPTY set with NO error (mirrors production's
/// `prepare_v2_config` `return Ok(())`). Combined with the harness's
/// warn-skip on `declared && empty`, the all-skipped case never hard-fails.
#[test]
fn resolve_docker_configs_truthy_skip_resolves_empty_without_error() {
    use anodizer_core::config::{Config, CrateConfig, DockerV2Config, StringOrBool};
    let config = Config {
        project_name: "full".to_string(),
        crates: vec![CrateConfig {
            name: "full".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            dockers_v2: Some(vec![DockerV2Config {
                dockerfile: "Dockerfile.release".to_string(),
                skip: Some(StringOrBool::Bool(true)),
                ..Default::default()
            }]),
            ..Default::default()
        }],
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    let resolved = resolve_docker_configs(Some(&config), Some("full"), true, &log)
        .expect("a truthy skip must resolve cleanly (Ok), mirroring production");
    assert!(
        resolved.is_empty(),
        "a skipped entry must produce no ResolvedDockerConfig: {resolved:?}"
    );
}

/// Regression for the v0.14.0 release-blocker: on a RELEASE-mode probe
/// (`child_snapshot=false`, the tagged-HEAD path the CI determinism shard
/// actually takes) `setup_env`'s release token gate must be skipped for a
/// `release:`-configured crate, mirroring the credential-less
/// determinism child build. `child_snapshot=true` (the existing tests
/// above) never exercised this: untagged-HEAD local runs take the
/// snapshot escape hatch and never reach the token gate at all.
///
/// This drives `setup_env` with a `Context` built the SAME way
/// `resolve_docker_configs` now builds its own (identical
/// `ContextOptions`, including `skip_stages`), routed through a
/// hermetic empty `MapEnvSource` so the assertion holds in every
/// environment — CI or local — regardless of whether a real GitHub
/// token happens to be set in the process env. `resolve_docker_configs`
/// itself cannot be driven end-to-end here with `child_snapshot=false`:
/// it also calls `resolve_git_context`, which independently requires
/// HEAD to sit exactly at a matching git tag (the real determinism
/// child build's premise — it runs against the just-cut release tag),
/// a fixture this unit test would have to fabricate by mutating this
/// repository's own tags, which is neither hermetic nor safe. Isolating
/// `setup_env` pins precisely the mechanism this fix restores without
/// that unrelated git-state coupling.
#[test]
fn setup_env_release_mode_skips_token_gate_with_determinism_skip_stages() {
    use anodizer_core::config::{Config, CrateConfig, ReleaseConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let skip_stages: Vec<String> = anodizer_core::determinism_runner::SIDE_EFFECT_STAGES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let config = Config {
        project_name: "full".to_string(),
        crates: vec![CrateConfig {
            name: "full".to_string(),
            path: ".".to_string(),
            release: Some(ReleaseConfig::default()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let opts = ContextOptions {
        snapshot: false,
        skip_stages,
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), opts);
    ctx.set_env_source(anodizer_core::env_source::MapEnvSource::new());
    assert!(
        ctx.should_skip("release"),
        "resolve_docker_configs's skip_stages must mirror the determinism child \
             build's SIDE_EFFECT_STAGES"
    );
    let log = StageLogger::new("test", Verbosity::Quiet);
    crate::commands::helpers::setup_env(&mut ctx, &config, &log).expect(
        "a release-mode (tagged-HEAD) probe with skip_stages mirroring the \
             determinism child build must not demand a GitHub token",
    );
}

/// A resolution ERROR on a HOST-DEFAULT run must warn and reflect the crate
/// as NOT declared, so the harness routes to its quiet skip rather than the
/// (now factually wrong) "declares dockers_v2 but all entries were skipped"
/// warn. This is the LOW-1 invariant: an errored host-default resolve must
/// not surface a misleading legit-skip message downstream.
#[test]
fn classify_docker_stage_state_host_default_error_forces_not_declared() {
    let log = StageLogger::new("test", Verbosity::Quiet);
    let (configs, declared) = classify_docker_stage_state(
        Err(anyhow::anyhow!("boom: could not resolve")),
        true,  // crate DOES declare dockers_v2 ...
        false, // ... but this is a host-default run (not explicit)
        &log,
    )
    .expect("a host-default resolve error must warn-and-skip, not hard-fail");
    assert!(configs.is_empty(), "an errored resolve carries no configs");
    assert!(
        !declared,
        "an errored host-default resolve must be reflected as NOT declared so the harness \
             quiet-skips instead of emitting the misleading legit-skip warn"
    );
}

/// The same resolution ERROR under an EXPLICIT request
/// (`--require-tools` / explicit `--stages=docker`) must hard-fail — silently
/// skipping a stage the caller asked to byte-verify is false coverage.
#[test]
fn classify_docker_stage_state_explicit_error_hard_fails() {
    let log = StageLogger::new("test", Verbosity::Quiet);
    let res = classify_docker_stage_state(
        Err(anyhow::anyhow!("boom: could not resolve")),
        true,
        true, // explicit request
        &log,
    );
    assert!(
        res.is_err(),
        "an explicit docker request whose resolve errored must hard-fail, not skip"
    );
}

/// A successful resolve carries its configs and leaves `declared` untouched
/// (the errored-host-default reconciliation applies ONLY to the error arm).
#[test]
fn classify_docker_stage_state_ok_preserves_declared_and_configs() {
    let log = StageLogger::new("test", Verbosity::Quiet);
    let resolved = vec![ResolvedDockerConfig {
        dockerfile: "FROM scratch\n".to_string(),
        extra_files: Vec::new(),
        build_args: Vec::new(),
    }];
    let (configs, declared) =
        classify_docker_stage_state(Ok(resolved), true, true, &log).expect("Ok must pass through");
    assert_eq!(configs.len(), 1, "resolved configs must be carried through");
    assert!(declared, "a successful resolve must not alter `declared`");
}

/// An EXPLICIT `--stages` is the operator's typed intent and ignores the
/// config intersection entirely — `--stages=nfpm` resolves to `[nfpm]`
/// even when the config configures no nfpm.
#[test]
fn resolve_stages_explicit_ignores_config() {
    use anodizer_core::config::{Config, CrateConfig};
    let bare = Config {
        crates: vec![CrateConfig {
            name: "x".to_string(),
            path: ".".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let stages = resolve_stages(Some("nfpm"), Some(&bare)).expect("nfpm is a known stage");
    assert_eq!(stages, vec![StageId::Nfpm]);
}

#[test]
fn is_explicit_stage_selection_matches_nonblank_token_only() {
    // The single predicate behind both the stage-set resolution and the
    // explicit-stages hard-fail set: a real token is explicit; absent or
    // all-blank is the host default. Drift between the two call sites would
    // let a stage hard-fail in one path and warn-skip in the other.
    assert!(is_explicit_stage_selection(Some("msi")));
    assert!(is_explicit_stage_selection(Some(" archive , checksum ")));
    assert!(!is_explicit_stage_selection(None));
    assert!(!is_explicit_stage_selection(Some("")));
    assert!(!is_explicit_stage_selection(Some(" , , ")));
}

#[test]
fn parse_stages_subset_filters_to_named_set() {
    let stages = parse_stages(Some("archive,checksum")).expect("all known stages");
    assert_eq!(
        stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        vec!["archive", "checksum"]
    );
}

#[test]
fn parse_stages_accepts_full_byte_stable_set() {
    // Every stage name reachable from anodizer-action's per-OS
    // determinism-stages default must parse cleanly. Drift between
    // this parser and the action's expanded default surfaces as
    // "unknown stage(s): makeself, snapcraft" in CI. This test pins
    // the parser to the action's current Linux default CSV.
    let stages = parse_stages(Some(
        "build,source,upx,archive,nfpm,makeself,snapcraft,sbom,sign,checksum",
    ))
    .expect("all stages in the action's Linux default must parse");
    assert_eq!(
        stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        vec![
            "build",
            "source",
            "upx",
            "archive",
            "nfpm",
            "makeself",
            "snapcraft",
            "sbom",
            "sign",
            "checksum"
        ]
    );
}

#[test]
fn parse_stages_errors_on_unknown_token() {
    // Typos like `--stages=archve,checksum` previously filtered to
    // just `checksum` and quietly under-verified. The unknown token
    // must surface as an error naming the bad token and the legal
    // vocabulary.
    let err =
        parse_stages(Some(" archive , bogus, checksum ")).expect_err("unknown token must error");
    assert!(
        err.contains("bogus") && err.contains("Known stages"),
        "error must name the bad token and the legal vocabulary: {err}"
    );
    // Multiple unknowns are reported together rather than failing on
    // the first — the operator gets a complete picture in one shot.
    let err = parse_stages(Some("archve,nope")).expect_err("multiple unknowns must error");
    assert!(
        err.contains("archve") && err.contains("nope"),
        "all unknown tokens must be named: {err}"
    );
}

#[test]
fn parse_stages_tolerates_trailing_comma_and_whitespace() {
    // Empty / whitespace-only tokens (trailing comma, double comma,
    // surrounding spaces) are noise rather than typos.
    let stages = parse_stages(Some("archive,checksum,")).expect("trailing comma tolerated");
    assert_eq!(
        stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        vec!["archive", "checksum"]
    );
    let stages = parse_stages(Some(" archive , , checksum ")).expect("empty middle tolerated");
    assert_eq!(
        stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        vec!["archive", "checksum"]
    );
}

#[test]
fn parse_stages_installers_umbrella_expands_to_full_set() {
    // `--stages=installers` is the operator-facing shorthand for
    // every installer-family stage. The expansion must include
    // nfpm + makeself + srpm + msi + nsis + dmg + pkg in the same
    // order `installer_stages()` advertises so the harness gate
    // and the parser stay aligned.
    let stages = parse_stages(Some("installers")).expect("umbrella token must parse");
    assert_eq!(
        stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        vec!["nfpm", "makeself", "srpm", "msi", "nsis", "dmg", "pkg"]
    );
}

#[test]
fn parse_stages_installers_dedupes_against_individual_members() {
    // `--stages=installers,msi` must not double-list `msi` in the
    // report's `stages_under_test`. First mention wins so the
    // operator's typed order is preserved.
    let stages = parse_stages(Some("installers,msi")).expect("umbrella + individual must parse");
    let names: Vec<&str> = stages.iter().map(|s| s.as_str()).collect();
    assert_eq!(names.iter().filter(|n| **n == "msi").count(), 1);
}

#[test]
fn parse_stages_accepts_each_individual_installer_token() {
    // Every individual installer stage token must parse in
    // isolation so operators can narrow the harness to a single
    // family (`--stages=msi`) without invoking the umbrella.
    for token in ["msi", "nsis", "dmg", "pkg", "srpm"] {
        let stages =
            parse_stages(Some(token)).unwrap_or_else(|e| panic!("token `{token}` must parse: {e}"));
        assert_eq!(
            stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec![token]
        );
    }
}

#[test]
fn parse_stages_accepts_appbundle_token() {
    // `appbundle` is pure file assembly (no tool) but must be a
    // first-class stage token: a `dmg`/`pkg` stage with `use:
    // appbundle` finds no source artifact unless `appbundle` is kept
    // out of the harness's child `--skip=` complement, which requires
    // it to be requestable here.
    let stages = parse_stages(Some("appbundle,dmg")).expect("appbundle token must parse");
    assert_eq!(
        stages.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        vec!["appbundle", "dmg"]
    );
}

#[test]
fn parse_stages_empty_string_falls_back_to_default() {
    // An empty / all-whitespace selection picks the OS-native host
    // partition so `--stages=""` doesn't degrade into a no-op.
    let expected = default_stages_for_host();
    let stages = parse_stages(Some("")).expect("empty list returns default");
    assert_eq!(stages, expected);
    let stages = parse_stages(Some(" , , ")).expect("whitespace-only returns default");
    assert_eq!(stages, expected);
}

#[test]
fn parse_targets_default_is_none() {
    assert_eq!(parse_targets(None).unwrap(), None);
}

#[test]
fn parse_targets_subset_filters_to_named_list() {
    let got = parse_targets(Some("x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu"))
        .expect("ascii triples accepted");
    assert_eq!(
        got,
        Some(vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "aarch64-unknown-linux-gnu".to_string(),
        ])
    );
}

#[test]
fn parse_targets_tolerates_trailing_comma_and_whitespace() {
    let got = parse_targets(Some(" x86_64-apple-darwin , aarch64-apple-darwin , "))
        .expect("trailing comma + spaces tolerated");
    assert_eq!(
        got,
        Some(vec![
            "x86_64-apple-darwin".to_string(),
            "aarch64-apple-darwin".to_string(),
        ])
    );
}

#[test]
fn parse_targets_errors_on_all_empty_csv() {
    // Operator typed `--targets=""` or `--targets=", , "` — they
    // clearly meant to pass *something* but gave nothing. Silent
    // fallback to "no filter" would mask the typo and cross-compile
    // every configured target (the very bug Option B exists to
    // prevent).
    let err = parse_targets(Some("")).expect_err("empty CSV must error");
    assert!(
        err.contains("at least one entry"),
        "error must explain the requirement: {err}"
    );
    let err = parse_targets(Some(" , , ")).expect_err("whitespace-only CSV must error");
    assert!(
        err.contains("at least one entry"),
        "error must explain the requirement: {err}"
    );
}

#[test]
fn commit_short_truncates_to_seven_chars() {
    assert_eq!(commit_short("abcdef1234567890"), "abcdef1");
}

#[test]
fn commit_short_keeps_short_commit_as_is() {
    assert_eq!(commit_short("abc"), "abc");
}

/// The harness body is exercised by the integration test at
/// `crates/cli/tests/check_determinism.rs`. Argument-plumbing
/// behavior is covered by the unit tests above.
#[test]
fn dispatcher_args_are_consumed() {
    // Sanity guard: if the CheckDeterminismArgs surface grows new
    // required fields, this test fails to compile and forces the
    // dispatcher above to pick up the new field explicitly.
    let _args = CheckDeterminismArgs {
        runs: 2,
        stages: None,
        targets: None,
        report: None,
        snapshot: false,
        no_snapshot: false,
        inject_drift: None,
        preserve_dist: None,
        crate_name: None,
        require_tools: false,
    };
}

// ── resolve_child_snapshot ────────────────────────────────────────────

#[test]
fn resolve_child_snapshot_auto_off_when_head_at_tag() {
    // Tagged HEAD = cutting a release → produce-stages emit
    // release-named artifacts (no `-SNAPSHOT-<sha>` suffix). The
    // workflow's preserved-dist payload must be immediately
    // shippable via `--publish-only`.
    assert!(!resolve_child_snapshot(false, false, true));
}

#[test]
fn resolve_child_snapshot_auto_on_when_head_not_at_tag() {
    // Untagged HEAD = local rehearsal → produce-stages emit
    // `-SNAPSHOT-<sha>`-suffixed artifacts so the bytes can't be
    // mistaken for a release build.
    assert!(resolve_child_snapshot(false, false, false));
}

#[test]
fn resolve_child_snapshot_explicit_snapshot_beats_auto() {
    // `--snapshot` on a tagged HEAD: operator deliberately wants
    // snapshot-style artifacts even though HEAD is tagged. Auto
    // would say off; explicit must beat auto.
    assert!(resolve_child_snapshot(true, false, true));
    assert!(resolve_child_snapshot(true, false, false));
}

#[test]
fn resolve_child_snapshot_explicit_no_snapshot_beats_auto() {
    // `--no-snapshot` on an untagged HEAD: legacy workflow override
    // — operator forces release-style artifact names even though
    // we're not at a tag. Auto would say on; explicit must beat
    // auto.
    assert!(!resolve_child_snapshot(false, true, false));
    assert!(!resolve_child_snapshot(false, true, true));
}

// ── read_project_version ──────────────────────────────────────────────

#[test]
fn read_project_version_returns_none_when_cargo_toml_missing() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(read_project_version(tmp.path()), None);
}

#[test]
fn read_project_version_reads_workspace_package_version() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        r#"[workspace]
members = ["crates/*"]

[workspace.package]
version = "1.2.3-test"
edition = "2021"
"#,
    )
    .unwrap();
    assert_eq!(
        read_project_version(tmp.path()),
        Some("1.2.3-test".to_string())
    );
}

#[test]
fn read_project_version_reads_package_version() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        r#"[package]
name = "demo"
version = "0.4.2"
edition = "2021"
"#,
    )
    .unwrap();
    assert_eq!(read_project_version(tmp.path()), Some("0.4.2".to_string()));
}

#[test]
fn read_project_version_prefers_workspace_when_both_present() {
    // Workspace inheritance: the root `[workspace.package].version`
    // is the authoritative version and `[package].version` is
    // usually `version.workspace = true`. When both literal values
    // are present we still prefer the workspace key because that's
    // what `cargo` itself would propagate via inheritance.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        r#"[workspace.package]
version = "9.9.9"

[package]
name = "root-crate"
version = "0.0.1"
"#,
    )
    .unwrap();
    assert_eq!(read_project_version(tmp.path()), Some("9.9.9".to_string()));
}

#[test]
fn read_project_version_returns_none_on_malformed_toml() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Cargo.toml"), "not valid \x00 toml ===").unwrap();
    assert_eq!(read_project_version(tmp.path()), None);
}

#[test]
fn signature_allowlist_derives_custom_cosign_bundle_suffix() {
    use anodizer_core::config::{Config, SignConfig};
    // Mirrors cfgd: a checksum-signing cosign entry with a custom
    // `.cosign.bundle` signature template plus a default-`.sig` entry.
    let cfg = Config {
        signs: vec![
            SignConfig {
                signature: Some("{{ .Artifact }}.cosign.bundle".into()),
                ..Default::default()
            },
            SignConfig::default(), // default template → `.sig`
        ],
        ..Default::default()
    };
    let entries = signature_allowlist_entries_from_config(&cfg);
    let patterns: Vec<&str> = entries.iter().map(|e| e.artifact.as_str()).collect();
    assert!(
        patterns.contains(&"*.cosign.bundle"),
        "custom signature suffix must be allow-listed, got {patterns:?}"
    );
    assert!(patterns.contains(&"*.sig"), "got {patterns:?}");
    // Every derived pattern is a concrete extension anchor, never a
    // bare `*` (which would suppress all drift).
    assert!(entries.iter().all(|e| e.artifact != "*"));
}

#[test]
fn signature_allowlist_derives_keyless_certificate_suffix() {
    use anodizer_core::config::{Config, SignConfig};
    // A cosign keyless-mode `certificate:` template is per-invocation
    // (Fulcio mints a fresh short-lived cert every sign) just like the
    // signature itself, so it must drift-allowlist the same way.
    let cfg = Config {
        signs: vec![SignConfig {
            certificate: Some("{{ .Artifact }}.pem".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let entries = signature_allowlist_entries_from_config(&cfg);
    let patterns: Vec<&str> = entries.iter().map(|e| e.artifact.as_str()).collect();
    assert!(
        patterns.contains(&"*.pem"),
        "configured certificate suffix must be allow-listed, got {patterns:?}"
    );
}

/// Regression for the cfgd v0.4.0 determinism failure: the build was
/// reproducible, but 18 signature/SBOM artifacts drifted and counted,
/// failing the release. Every one of those exact names must now resolve
/// to an allow-list reason through the canonical matcher — the SBOM
/// documents via the compile-time list, the cosign bundles via the
/// config-derived signature suffix.
#[test]
fn cfgd_v040_drift_set_is_fully_allowlisted() {
    use anodizer_core::DeterminismState;
    use anodizer_core::config::{Config, SignConfig};

    // cfgd's signing surface: a cosign checksum signer emitting
    // `.cosign.bundle`, plus default-`.sig` gpg/cosign entries.
    let cfg = Config {
        signs: vec![
            SignConfig {
                signature: Some("{{ .Artifact }}.cosign.bundle".into()),
                ..Default::default()
            },
            SignConfig::default(),
        ],
        binary_signs: vec![SignConfig::default()],
        ..Default::default()
    };

    let mut state = DeterminismState::seed_from_commit(0).expect("non-negative");
    for entry in signature_allowlist_entries_from_config(&cfg) {
        state.append_runtime(entry.artifact, entry.reason);
    }

    // The exact artifact set that drifted in run 26675983133.
    let drifted = [
        "cfgd-0.4.0-linux-amd64-installer.run.sha256.cosign.bundle",
        "cfgd-0.4.0-linux-amd64.tar.gz.cdx.json",
        "cfgd-0.4.0-linux-amd64.tar.gz.cdx.json.sha256",
        "cfgd-0.4.0-linux-amd64.tar.gz.cdx.json.sha256.cosign.bundle",
        "cfgd-0.4.0-linux-amd64.tar.gz.sha256.cosign.bundle",
        "cfgd-0.4.0-linux-arm64-installer.run.sha256.cosign.bundle",
        "cfgd-0.4.0-linux-arm64.tar.gz.cdx.json",
        "cfgd-0.4.0-linux-arm64.tar.gz.cdx.json.sha256",
        "cfgd-0.4.0-linux-arm64.tar.gz.cdx.json.sha256.cosign.bundle",
        "cfgd-0.4.0-linux-arm64.tar.gz.sha256.cosign.bundle",
        "cfgd-0.4.0-source.tar.gz.sha256.cosign.bundle",
        "cfgd_0.4.0_linux_amd64.apk.sha256.cosign.bundle",
        "cfgd_0.4.0_linux_amd64.deb.sha256.cosign.bundle",
        "cfgd_0.4.0_linux_amd64.rpm.sha256.cosign.bundle",
        "cfgd_0.4.0_linux_arm64.apk.sha256.cosign.bundle",
        "cfgd_0.4.0_linux_arm64.deb.sha256.cosign.bundle",
        "cfgd_0.4.0_linux_arm64.rpm.sha256.cosign.bundle",
        "install.sh.sha256.cosign.bundle",
        // macOS shard drift set (darwin universal + per-arch). NOTE:
        // `artifacts.json` is intentionally NOT here — it is no longer
        // blanket-allow-listed (that masked drift in gated members). The
        // determinism harness now judges it via the aggregate registry's
        // transitive-derivation rule, member by member.
        "cfgd-0.4.0-darwin-all.tar.gz.cdx.json",
        "cfgd-0.4.0-darwin-all.tar.gz.cdx.json.sha256",
        "cfgd-0.4.0-darwin-all.tar.gz.cdx.json.sha256.cosign.bundle",
        "cfgd-0.4.0-darwin-all.tar.gz.sha256.cosign.bundle",
        "cfgd-0.4.0-darwin-amd64.tar.gz.cdx.json",
        "cfgd-0.4.0-darwin-arm64.tar.gz.cdx.json.sha256.cosign.bundle",
        // cfgd-csi shard: a combined-checksums cosign bundle.
        "cfgd_0.4.0_checksums.txt.cosign.bundle",
    ];
    for name in drifted {
        assert!(
            state.resolve_reason(name).is_some(),
            "{name} drifted v0.4.0 and must now be allow-listed"
        );
    }

    // Negative control: a real build output must NOT be allow-listed,
    // so genuine binary drift still fails the harness.
    assert!(
        state
            .resolve_reason("cfgd-0.4.0-linux-amd64.tar.gz")
            .is_none(),
        "archive bytes must still be drift-checked"
    );
    assert!(
        state.resolve_reason("cfgd").is_none(),
        "raw binary must still be drift-checked"
    );
}

#[test]
fn signature_allowlist_collects_per_workspace_signs() {
    use anodizer_core::config::{Config, SignConfig, WorkspaceConfig};
    let cfg = Config {
        workspaces: Some(vec![WorkspaceConfig {
            name: "member".into(),
            binary_signs: vec![SignConfig {
                signature: Some("{{ .Artifact }}.bundle".into()),
                ..Default::default()
            }],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let patterns: Vec<String> = signature_allowlist_entries_from_config(&cfg)
        .into_iter()
        .map(|e| e.artifact)
        .collect();
    assert!(
        patterns.contains(&"*.bundle".to_string()),
        "per-workspace signature suffix must be collected, got {patterns:?}"
    );
}
