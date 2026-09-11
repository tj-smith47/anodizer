use super::*;
use anodizer_core::config::{CrateConfig, NightlyConfig, WorkspaceConfig};

// -----------------------------------------------------------------------
// install_verify_gate — proves `anodizer release` actually WIRES
// `run_asset_gate` into `ctx.verify_gate`, not just that
// `run_asset_gate` itself behaves correctly (already covered by
// stage-verify-release's own unit tests). Deleting the
// `install_verify_gate` call at the production call site, or swapping
// it for a decoy, must fail this test.
// -----------------------------------------------------------------------

#[test]
fn install_verify_gate_delegates_to_the_real_asset_gate() {
    use anodizer_core::config::{
        GitHubUrlsConfig, HumanDuration, RetryConfig, VerifyReleaseConfig,
    };
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::test_helpers::scripted_responder::spawn_scripted_responder;

    // A crate with a `release:` block but NO route served for it —
    // every request 404s. If `install_verify_gate` did not wire the
    // real `run_asset_gate` (deleted call, or a decoy always returning
    // `Ok(true)`), this test would observe a passing gate instead of
    // the real gate's `Ok(false)` on a missing release.
    let (addr, _log) = spawn_scripted_responder(Vec::new());
    let base = format!("http://{addr}");

    let krate: CrateConfig = serde_yaml_ng::from_str(
        "name: app\npath: .\ntag_template: \"v{{ .Version }}\"\n\
             release:\n  github: { owner: me, name: repo }\n",
    )
    .expect("valid crate yaml");

    let mut ctx = TestContextBuilder::new()
        .tag("v1.0.0")
        .token(Some("test-token".to_string()))
        .env("ANODIZER_GITHUB_API_BASE", &base)
        .crates(vec![krate])
        .build();
    ctx.config.github_urls = Some(GitHubUrlsConfig {
        api: Some(base.clone()),
        upload: Some(base.clone()),
        download: Some(base),
        skip_tls_verify: None,
    });
    ctx.config.retry = Some(RetryConfig {
        attempts: 1,
        delay: HumanDuration(std::time::Duration::from_millis(1)),
        max_delay: HumanDuration(std::time::Duration::from_millis(1)),
        max_elapsed: None,
    });
    ctx.config.verify_release = VerifyReleaseConfig {
        assert_landing: true,
        enabled: true,
        assert_assets: true,
        glibc_ceiling: None,
        install_smoke: None,
    };

    assert!(
        ctx.verify_gate.is_none(),
        "baseline: no gate installed before install_verify_gate runs"
    );
    install_verify_gate(&mut ctx);
    let gate = ctx
        .verify_gate
        .clone()
        .expect("install_verify_gate must set ctx.verify_gate");

    let passed = gate(&mut ctx).expect("run_asset_gate does not error on a missing release");
    assert!(
        !passed,
        "a missing GitHub release must fail the real run_asset_gate, proving delegation \
             rather than a stub"
    );
}

#[test]
fn custom_publishers_honor_operator_selection() {
    use anodizer_core::config::PublisherConfig;
    let named = |n: &str| PublisherConfig {
        name: Some(n.to_string()),
        ..Default::default()
    };
    let pubs = vec![named("minio-mirror"), PublisherConfig::default()];
    let log = StageLogger::new("test", Verbosity::Quiet);

    // Empty selectors (the main release job): everything runs.
    let ctx = Context::new(Config::default(), ContextOptions::default());
    assert_eq!(select_custom_publishers(&ctx, &pubs, &log).len(), 2);

    // `--publishers npm` (the npm job): neither custom entry is "npm", so
    // both deselect — the AWS-needing mirror never fires on the npm runner.
    let ctx = Context::new(
        Config::default(),
        ContextOptions {
            publisher_allowlist: vec!["npm".to_string()],
            ..Default::default()
        },
    );
    assert!(select_custom_publishers(&ctx, &pubs, &log).is_empty());

    // `--publishers minio-mirror`: the named entry runs; the nameless one
    // (index label `publisher[1]`) deselects.
    let ctx = Context::new(
        Config::default(),
        ContextOptions {
            publisher_allowlist: vec!["minio-mirror".to_string()],
            ..Default::default()
        },
    );
    let sel = select_custom_publishers(&ctx, &pubs, &log);
    assert_eq!(sel.len(), 1);
    assert_eq!(sel[0].name.as_deref(), Some("minio-mirror"));

    // `--skip minio-mirror`: the named entry deselects; the nameless runs.
    let ctx = Context::new(
        Config::default(),
        ContextOptions {
            skip_stages: vec!["minio-mirror".to_string()],
            ..Default::default()
        },
    );
    let sel = select_custom_publishers(&ctx, &pubs, &log);
    assert_eq!(sel.len(), 1);
    assert!(sel[0].name.is_none());
}

// -----------------------------------------------------------------------
// run_before_hooks — must fire in normal, --split, and --merge modes
// (before-hooks produce INPUTS for build (split) and archive/nfpm (merge)),
// and must skip in --publish-only / --announce-only (those operate on
// already-produced artifacts).
// -----------------------------------------------------------------------

/// Run `run_before_hooks` with a `before:` hook that `touch`es a unique
/// marker file, and report whether the marker was created (i.e. the hook
/// actually executed).
fn before_hook_fired(opts: &ReleaseOpts, skip: &[&str]) -> bool {
    use anodizer_core::config::{HookEntry, HooksConfig, StructuredHook};

    let dir = std::env::temp_dir().join(format!(
        "anodizer-before-hook-{}-{:p}",
        std::process::id(),
        opts as *const _
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("fired.marker");
    let _ = std::fs::remove_file(&marker);
    // sh -c mangles backslashes; forward-slash path resolves on Windows too.
    let marker_arg = marker.display().to_string().replace('\\', "/");

    let config = Config {
        before: Some(HooksConfig {
            hooks: Some(vec![HookEntry::Structured(StructuredHook {
                cmd: format!("touch {marker_arg}"),
                ..Default::default()
            })]),
            post: None,
        }),
        ..Default::default()
    };
    let ctx = Context::new(
        config.clone(),
        ContextOptions {
            skip_stages: skip.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        },
    );
    let log = StageLogger::new("test", Verbosity::Quiet);
    run_before_hooks(&ctx, &config, opts, &log).expect("run_before_hooks must not error");

    let fired = marker.exists();
    let _ = std::fs::remove_file(&marker);
    fired
}

#[test]
fn before_hooks_run_in_normal_split_and_merge_modes() {
    // Normal end-to-end run: hooks fire (baseline).
    assert!(
        before_hook_fired(&base_release_opts(), &[]),
        "before-hooks must run in a normal release"
    );

    // --split runs the build stage, which consumes hook-generated inputs
    // (e.g. a generated source file). Hooks MUST fire. This previously
    // FAILED — the old `!opts.split` clause codified the bug.
    let split = ReleaseOpts {
        split: true,
        ..base_release_opts()
    };
    assert!(
        before_hook_fired(&split, &[]),
        "before-hooks must run in --split mode (build consumes their inputs)"
    );

    // --merge runs archive/nfpm, which consume hook-generated inputs (e.g.
    // a generated man page the archive packages). Hooks MUST fire. This
    // previously FAILED — the old `!opts.merge` clause codified the bug.
    let merge = ReleaseOpts {
        merge: true,
        ..base_release_opts()
    };
    assert!(
        before_hook_fired(&merge, &[]),
        "before-hooks must run in --merge mode (archive/nfpm consume their inputs)"
    );
}

#[test]
fn before_hooks_skip_in_publish_only_and_announce_only() {
    // --publish-only operates on already-produced artifacts: no build, no
    // archive, so hook-generated inputs no longer apply.
    let publish_only = ReleaseOpts {
        publish_only: true,
        ..base_release_opts()
    };
    assert!(
        !before_hook_fired(&publish_only, &[]),
        "before-hooks must NOT run in --publish-only mode"
    );

    // --announce-only fires announcers against a finished release only.
    let announce_only = ReleaseOpts {
        announce_only: true,
        ..base_release_opts()
    };
    assert!(
        !before_hook_fired(&announce_only, &[]),
        "before-hooks must NOT run in --announce-only mode"
    );
}

#[test]
fn before_hooks_honor_skip_before() {
    // `--skip before` suppresses the hooks even in a mode that would run
    // them (here, a normal run).
    assert!(
        !before_hook_fired(&base_release_opts(), &["before"]),
        "`--skip before` must suppress before-hooks"
    );
}

fn make_crate(name: &str, deps: Option<Vec<&str>>) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some(format!("{}-v{{{{ .Version }}}}", name)),
        depends_on: deps.map(|d| d.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

// -----------------------------------------------------------------------
// Crate selection from tags at HEAD reads every crate's tag FAMILY, so a
// template-less crate is selectable by the `<name>-v` tag it releases under
// and a derived lockstep family selects the whole workspace.
// -----------------------------------------------------------------------

fn family_crate(name: &str, tag_template: Option<&str>) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: tag_template.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn a_lockstep_tag_selects_every_crate() {
    let log = StageLogger::new("test", Verbosity::Quiet);
    let lockstep: Vec<CrateConfig> = ["app", "core", "cli"]
        .iter()
        .map(|n| family_crate(n, Some("v{{ Version }}")))
        .collect();
    assert_eq!(
        select_crates_for_tags(&["v0.25.3".to_string()], &lockstep, &log),
        vec!["app".to_string(), "core".to_string(), "cli".to_string()],
        "one shared family: the pushed tag names all of them"
    );

    let template_less: Vec<CrateConfig> = ["app", "core", "cli"]
        .iter()
        .map(|n| family_crate(n, None))
        .collect();
    assert_eq!(
        select_crates_for_tags(&["app-v0.25.3".to_string()], &template_less, &log),
        vec!["app".to_string()],
        "a template-less crate is selected by its `<name>-v` family"
    );
}

/// `anodizer resolve-tag` and `--publish-only` crate selection must answer a
/// tag identically; a second prefix walk with its own fallback is how they
/// once disagreed.
#[test]
fn resolve_tag_command_and_release_selection_agree() {
    use crate::commands::resolve_tag::resolve_owner;

    let lockstep: Vec<CrateConfig> = ["app", "core"]
        .iter()
        .map(|n| family_crate(n, Some("v{{ Version }}")))
        .collect();
    let per_crate = vec![
        family_crate("app", Some("v{{ Version }}")),
        family_crate("vault", Some("vault-v{{ Version }}")),
        family_crate("tools", None),
    ];
    for (crates, tag) in [
        (&lockstep, "v0.25.3"),
        (&per_crate, "v1.0.0"),
        (&per_crate, "vault-v1.0.0"),
        (&per_crate, "tools-v1.0.0"),
        (&per_crate, "nope-1.0.0"),
    ] {
        let owner = resolve_owner(tag, crates).map(|c| c.name.as_str());
        let selected = resolve_tag_to_crates(tag, crates);
        assert_eq!(
            owner,
            selected.first().map(|c| c.name.as_str()),
            "tag '{tag}': resolve-tag and selection disagree"
        );
    }
    assert_eq!(
        resolve_owner("tools-v1.0.0", &per_crate).map(|c| c.name.as_str()),
        Some("tools"),
        "a template-less crate owns its `<name>-v` tag"
    );
}

// -----------------------------------------------------------------------
// resolve_host_targets (--host-targets) — all three config modes
// -----------------------------------------------------------------------

use anodizer_core::config::{BuildConfig, Defaults};

fn build_with_targets(binary: &str, targets: &[&str]) -> BuildConfig {
    BuildConfig {
        binary: Some(binary.to_string()),
        targets: Some(targets.iter().map(|s| s.to_string()).collect()),
        ..Default::default()
    }
}

fn crate_with_builds(name: &str, builds: Vec<BuildConfig>) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ .Version }}".to_string()),
        builds: Some(builds),
        ..Default::default()
    }
}

fn base_release_opts() -> ReleaseOpts {
    ReleaseOpts {
        crate_names: vec![],
        all: false,
        force: false,
        snapshot: false,
        nightly: false,
        dry_run: false,
        clean: false,
        skip: vec![],
        publishers: vec![],
        token: None,
        verbose: false,
        debug: false,
        quiet: false,
        config_override: None,
        parallelism: 1,
        single_target: None,
        targets: None,
        host_targets: false,
        release_notes: None,
        release_notes_tmpl: None,
        workspace: None,
        draft: false,
        release_header: None,
        release_header_tmpl: None,
        release_footer: None,
        release_footer_tmpl: None,
        fail_fast: false,
        split: false,
        merge: false,
        publish_only: false,
        strict: false,
        prepare: false,
        announce_only: false,
        resume_release: false,
        replace_existing: false,
        preflight: false,
        preflight_secrets: false,
        no_post_publish_poll: false,
        no_gate_submitter: false,
        simulate_failure: vec![],
        show_skipped: false,
        allow_nondeterministic: vec![],
        summary_json: None,
        allow_ai_failure: false,
        allow_snapshot_publish: false,
        no_env_preflight: false,
    }
}

#[test]
fn an_empty_release_footer_file_suppresses_the_attribution() {
    // `--release-footer <empty file>` is the documented opt-out from the
    // default attribution line. The empty file must land as an explicit empty
    // string: dropping it to `None` would read as "unset" downstream and
    // restore the very footer the operator asked to remove.
    let dir = tempfile::tempdir().unwrap();
    let footer = dir.path().join("footer.md");
    std::fs::write(&footer, "").unwrap();

    let mut config = Config::default();
    let mut opts = base_release_opts();
    opts.release_footer = Some(footer);

    apply_release_meta_overrides(&mut config, &opts).unwrap();

    let resolved = config
        .release
        .as_ref()
        .and_then(|r| r.footer.as_ref())
        .expect("the flag must write a release footer");
    assert!(
        matches!(resolved, anodizer_core::config::ContentSource::Inline(s) if s.is_empty()),
        "expected an explicit empty inline footer, got {resolved:?}"
    );
}

fn host_targets_opts() -> ReleaseOpts {
    ReleaseOpts {
        host_targets: true,
        snapshot: true,
        ..base_release_opts()
    }
}

// A fixed Linux host triple injected into the filter so the per-config-mode
// tests are deterministic regardless of what machine runs them (and never
// race on the process `TARGET`/`GGOOS` env vars `resolve_host_target` reads).
const LINUX_HOST: &str = "x86_64-unknown-linux-gnu";
const MAC_HOST: &str = "aarch64-apple-darwin";
const WINDOWS_HOST: &str = "x86_64-pc-windows-msvc";

/// The mixed-target / linux-host expectation, asserted identically in
/// every config mode: apple AND windows-msvc targets drop (neither
/// cross-builds from a linux host), linux + `*-windows-gnu` stay, and the
/// kept set is written to `opts.targets`. Host is injected (LINUX_HOST)
/// so the assertion holds on any test machine.
fn assert_linux_host_filter(config: &Config, selected: &[String]) {
    let log = StageLogger::new("test", Verbosity::Quiet);
    let mut opts = host_targets_opts();
    apply_host_targets_filter(&mut opts, config, selected, LINUX_HOST, &log).unwrap();
    let kept = opts.targets.expect("host_targets must set opts.targets");
    assert!(
        kept.contains(&"x86_64-unknown-linux-gnu".to_string()),
        "linux target kept: {kept:?}"
    );
    assert!(
        kept.contains(&"x86_64-pc-windows-gnu".to_string()),
        "windows-gnu target kept (cross-buildable via zig MinGW): {kept:?}"
    );
    assert!(
        !kept
            .iter()
            .any(|t| anodizer_core::target::is_windows_msvc(t)),
        "windows-msvc dropped on a non-windows host (needs MSVC SDK): {kept:?}"
    );
    assert!(
        !kept.iter().any(|t| anodizer_core::target::is_darwin(t)),
        "apple targets dropped on a non-apple host: {kept:?}"
    );
}

#[test]
fn host_targets_single_crate_mode() {
    let config = Config {
        project_name: "single".to_string(),
        crates: vec![crate_with_builds(
            "app",
            vec![build_with_targets(
                "app",
                &[
                    "x86_64-unknown-linux-gnu",
                    "x86_64-pc-windows-gnu",
                    "x86_64-apple-darwin",
                    "aarch64-apple-darwin",
                    "x86_64-pc-windows-msvc",
                ],
            )],
        )],
        ..Default::default()
    };
    assert_linux_host_filter(&config, &["app".to_string()]);
}

#[test]
fn host_targets_workspace_lockstep_mode() {
    // Lockstep: per-build `targets` omitted; the shared `defaults.targets`
    // supplies the target union for every crate.
    let mut config = Config {
        project_name: "lockstep".to_string(),
        crates: vec![
            crate_with_builds(
                "a",
                vec![BuildConfig {
                    binary: Some("a".to_string()),
                    ..Default::default()
                }],
            ),
            crate_with_builds(
                "b",
                vec![BuildConfig {
                    binary: Some("b".to_string()),
                    ..Default::default()
                }],
            ),
        ],
        ..Default::default()
    };
    config.defaults = Some(Defaults {
        targets: Some(
            [
                "x86_64-unknown-linux-gnu",
                "x86_64-pc-windows-gnu",
                "x86_64-apple-darwin",
                "aarch64-apple-darwin",
                "x86_64-pc-windows-msvc",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        ),
        ..Default::default()
    });
    assert_linux_host_filter(&config, &["a".to_string(), "b".to_string()]);
}

#[test]
fn host_targets_workspace_per_crate_mode() {
    // Per-crate: each crate declares its own per-build `targets`. The
    // union across crates is partitioned; apple drops, the rest stays.
    let config = Config {
        project_name: "per-crate".to_string(),
        crates: vec![
            crate_with_builds(
                "linux-svc",
                vec![build_with_targets(
                    "linux-svc",
                    &["x86_64-unknown-linux-gnu", "x86_64-apple-darwin"],
                )],
            ),
            crate_with_builds(
                "win-tool",
                vec![build_with_targets(
                    "win-tool",
                    &[
                        "x86_64-pc-windows-gnu",
                        "x86_64-pc-windows-msvc",
                        "aarch64-apple-darwin",
                    ],
                )],
            ),
        ],
        ..Default::default()
    };
    assert_linux_host_filter(&config, &["linux-svc".to_string(), "win-tool".to_string()]);
}

/// Mixed config (linux + apple + windows-gnu + windows-msvc), asserted on
/// every host. Returns the kept set so each host test asserts its own
/// expectation.
fn run_filter(host: &str) -> Vec<String> {
    let config = Config {
        project_name: "mixed".to_string(),
        crates: vec![crate_with_builds(
            "app",
            vec![build_with_targets(
                "app",
                &[
                    "x86_64-unknown-linux-gnu",
                    "x86_64-pc-windows-gnu",
                    "x86_64-apple-darwin",
                    "aarch64-apple-darwin",
                    "x86_64-pc-windows-msvc",
                ],
            )],
        )],
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    let mut opts = host_targets_opts();
    apply_host_targets_filter(&mut opts, &config, &["app".to_string()], host, &log).unwrap();
    opts.targets.expect("host_targets must set opts.targets")
}

#[test]
fn host_targets_apple_host_keeps_apple_still_skips_msvc() {
    // A macOS host builds apple + linux + windows-gnu, but windows-msvc
    // still needs a Windows host (the MSVC SDK isn't present on a Mac).
    let kept = run_filter(MAC_HOST);
    assert_eq!(
        kept,
        vec![
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ],
        "apple host keeps apple but not windows-msvc: {kept:?}"
    );
}

#[test]
fn host_targets_windows_host_keeps_msvc_skips_apple() {
    // A Windows host builds windows-msvc + linux + windows-gnu, but apple
    // still needs a macOS host.
    let kept = run_filter(WINDOWS_HOST);
    assert_eq!(
        kept,
        vec![
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-gnu",
            "x86_64-pc-windows-msvc",
        ],
        "windows host keeps windows-msvc but not apple: {kept:?}"
    );
}

#[test]
fn host_targets_empty_result_bails_apple_only_names_macos() {
    let config = Config {
        project_name: "darwin-only".to_string(),
        crates: vec![crate_with_builds(
            "app",
            vec![build_with_targets(
                "app",
                &["x86_64-apple-darwin", "aarch64-apple-darwin"],
            )],
        )],
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    let mut opts = host_targets_opts();
    let err = apply_host_targets_filter(&mut opts, &config, &["app".to_string()], LINUX_HOST, &log)
        .expect_err("apple-only config on a linux host must bail");
    let msg = err.to_string();
    assert!(
        msg.contains("none of the") && msg.contains("macOS host"),
        "empty-result guard names the cause + macOS remedy: {msg}"
    );
}

#[test]
fn host_targets_empty_result_bails_msvc_only_names_windows_not_macos() {
    // A windows-msvc-only config on a linux host must bail naming a
    // Windows host — NOT a hardcoded macOS remedy.
    let config = Config {
        project_name: "msvc-only".to_string(),
        crates: vec![crate_with_builds(
            "app",
            vec![build_with_targets("app", &["x86_64-pc-windows-msvc"])],
        )],
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    let mut opts = host_targets_opts();
    let err = apply_host_targets_filter(&mut opts, &config, &["app".to_string()], LINUX_HOST, &log)
        .expect_err("msvc-only config on a linux host must bail");
    let msg = err.to_string();
    assert!(
        msg.contains("none of the") && msg.contains("windows-msvc targets require a Windows host"),
        "empty-result guard names the Windows-host constraint: {msg}"
    );
    assert!(
        !msg.contains("macOS"),
        "msvc-only bail must not mention macOS: {msg}"
    );
}

#[test]
fn host_targets_no_builds_is_noop() {
    // A config with no build targets at all leaves opts.targets untouched.
    let config = Config {
        project_name: "no-builds".to_string(),
        crates: vec![CrateConfig {
            name: "lib".to_string(),
            path: ".".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    let mut opts = host_targets_opts();
    apply_host_targets_filter(&mut opts, &config, &["lib".to_string()], LINUX_HOST, &log).unwrap();
    assert!(
        opts.targets.is_none(),
        "no configured targets => no filter, opts.targets stays None"
    );
}

#[test]
fn test_topo_sort_selected_respects_order() {
    let all = vec![
        make_crate("a", None),
        make_crate("b", Some(vec!["a"])),
        make_crate("c", Some(vec!["b"])),
    ];
    let selected = vec!["c".to_string(), "b".to_string(), "a".to_string()];
    let sorted = topo_sort_selected(&all, &selected);
    assert_eq!(sorted, vec!["a", "b", "c"]);
}

#[test]
fn test_topo_sort_selected_partial() {
    let all = vec![
        make_crate("a", None),
        make_crate("b", Some(vec!["a"])),
        make_crate("c", None),
    ];
    // Only select b and c (not a)
    let selected = vec!["b".to_string(), "c".to_string()];
    let sorted = topo_sort_selected(&all, &selected);
    // b has no selected deps, c has no deps — both should appear
    assert!(sorted.contains(&"b".to_string()));
    assert!(sorted.contains(&"c".to_string()));
    assert!(!sorted.contains(&"a".to_string()));
}

#[test]
fn test_topo_sort_all_selected() {
    let all = vec![
        make_crate("core", None),
        make_crate("lib", Some(vec!["core"])),
        make_crate("cli", Some(vec!["lib", "core"])),
    ];
    let selected: Vec<String> = all.iter().map(|c| c.name.clone()).collect();
    let sorted = topo_sort_selected(&all, &selected);
    let core_pos = sorted.iter().position(|s| s == "core").unwrap();
    let lib_pos = sorted.iter().position(|s| s == "lib").unwrap();
    let cli_pos = sorted.iter().position(|s| s == "cli").unwrap();
    assert!(core_pos < lib_pos);
    assert!(core_pos < cli_pos);
    assert!(lib_pos < cli_pos);
}

/// Verify workspace overlay semantics:
/// - `env` merges additively (workspace env adds to / overrides top-level env)
/// - `signs` replaces top-level signs when workspace has its own
/// - `changelog` replaces top-level changelog when workspace has its own
#[test]
fn test_workspace_overlay_semantics() {
    use anodizer_core::config::{ChangelogConfig, SignConfig};

    // Build a top-level config with env, signs, and changelog
    let mut config = Config {
        project_name: "test".to_string(),
        crates: vec![make_crate("top-crate", None)],
        env: Some(vec![
            "SHARED=from-top".to_string(),
            "TOP_ONLY=top-value".to_string(),
        ]),
        signs: vec![SignConfig {
            cmd: Some("gpg".to_string()),
            ..Default::default()
        }],
        changelog: Some(ChangelogConfig {
            sort: Some("asc".to_string()),
            ..Default::default()
        }),
        workspaces: Some(vec![WorkspaceConfig {
            name: "ws".to_string(),
            crates: vec![make_crate("ws-crate", None)],
            env: Some(vec![
                "SHARED=from-ws".to_string(),
                "WS_ONLY=ws-value".to_string(),
            ]),
            signs: vec![SignConfig {
                cmd: Some("cosign".to_string()),
                ..Default::default()
            }],
            changelog: Some(ChangelogConfig {
                sort: Some("desc".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };

    // Apply the overlay using the shared helper
    let ws = config
        .workspaces
        .as_ref()
        .unwrap()
        .iter()
        .find(|w| w.name == "ws")
        .unwrap()
        .clone();
    helpers::apply_workspace_overlay(&mut config, &ws);

    // Verify crates were replaced
    assert_eq!(config.crates.len(), 1);
    assert_eq!(config.crates[0].name, "ws-crate");

    // Verify env merged additively: TOP_ONLY preserved, SHARED and WS_ONLY added from workspace
    let env = config.env.as_ref().unwrap();
    assert!(
        env.contains(&"TOP_ONLY=top-value".to_string()),
        "top-level-only key should be preserved"
    );
    assert!(
        env.contains(&"SHARED=from-ws".to_string()),
        "workspace SHARED entry should be present"
    );
    assert!(
        env.contains(&"WS_ONLY=ws-value".to_string()),
        "workspace-only key should be added"
    );

    // Verify signs were replaced (not merged)
    assert_eq!(config.signs.len(), 1);
    assert_eq!(
        config.signs[0].cmd.as_deref(),
        Some("cosign"),
        "signs should be replaced by workspace"
    );

    // Verify changelog was replaced
    let cl = config.changelog.as_ref().unwrap();
    assert_eq!(
        cl.sort.as_deref(),
        Some("desc"),
        "changelog should be replaced by workspace"
    );
}

// ---- depends_on propagation tests ----

#[test]
fn test_propagate_dependents_direct() {
    // B depends on A. If A changed, B should be included too.
    let crates = vec![
        make_crate("a", None),
        make_crate("b", Some(vec!["a"])),
        make_crate("c", None),
    ];
    let changed = vec!["a".to_string()];
    let result = propagate_dependents(&crates, changed);
    assert!(result.contains(&"a".to_string()));
    assert!(result.contains(&"b".to_string()));
    assert!(!result.contains(&"c".to_string()));
}

#[test]
fn test_propagate_dependents_transitive() {
    // C depends on B, B depends on A. If A changed, both B and C should be included.
    let crates = vec![
        make_crate("a", None),
        make_crate("b", Some(vec!["a"])),
        make_crate("c", Some(vec!["b"])),
    ];
    let changed = vec!["a".to_string()];
    let result = propagate_dependents(&crates, changed);
    assert!(result.contains(&"a".to_string()));
    assert!(result.contains(&"b".to_string()));
    assert!(result.contains(&"c".to_string()));
}

#[test]
fn test_propagate_dependents_no_deps() {
    let crates = vec![make_crate("a", None), make_crate("b", None)];
    let changed = vec!["a".to_string()];
    let result = propagate_dependents(&crates, changed);
    assert_eq!(result, vec!["a".to_string()]);
}

#[test]
fn test_propagate_dependents_preserves_order() {
    let crates = vec![
        make_crate("a", None),
        make_crate("b", Some(vec!["a"])),
        make_crate("c", Some(vec!["a"])),
    ];
    let changed = vec!["a".to_string()];
    let result = propagate_dependents(&crates, changed);
    // a should come first (from original changed), then b and c (propagated, in crate order)
    assert_eq!(result[0], "a");
    assert!(result.contains(&"b".to_string()));
    assert!(result.contains(&"c".to_string()));
}

// -----------------------------------------------------------------------
// CLI flag override tests
// -----------------------------------------------------------------------

#[test]
fn test_draft_flag_sets_release_config_draft() {
    // Start with a config that has no release config
    let mut config = Config {
        project_name: "test".to_string(),
        ..Default::default()
    };
    assert!(config.release.is_none());

    // Simulate what the release command does when --draft is true
    let release = config.release.get_or_insert_with(Default::default);
    release.draft = Some(true);

    assert_eq!(config.release.as_ref().unwrap().draft, Some(true));
}

#[test]
fn test_draft_flag_overrides_existing_config() {
    use anodizer_core::config::ReleaseConfig;

    // Start with a config that has draft=false
    let mut config = Config {
        project_name: "test".to_string(),
        release: Some(ReleaseConfig {
            draft: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };

    // Simulate --draft CLI override
    let release = config.release.get_or_insert_with(Default::default);
    release.draft = Some(true);

    assert_eq!(
        config.release.as_ref().unwrap().draft,
        Some(true),
        "CLI --draft should override config draft=false"
    );
}

// --- `--prepare` flag ---

#[test]
fn test_apply_prepare_mode_to_skip_from_empty() {
    let mut skip: Vec<String> = Vec::new();
    apply_prepare_mode_to_skip(&mut skip);
    assert_eq!(
        skip,
        anodizer_core::stages::UPSTREAM_STAGES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
        "--prepare on empty skip should add the full upstream-touching classification"
    );
    // Pin the members that historically leaked upstream from --prepare
    // (docker pushed images, docker-sign pushed signatures, and
    // verify-release fired live API calls against a release --prepare
    // never created) so the shared classification can't silently lose
    // them.
    for stage in ["docker", "docker-sign", "verify-release"] {
        assert!(
            skip.contains(&stage.to_string()),
            "--prepare must skip {stage}"
        );
    }
}

#[test]
fn test_apply_prepare_mode_to_skip_preserves_user_skip() {
    let mut skip = vec!["nfpm".to_string(), "sign".to_string()];
    apply_prepare_mode_to_skip(&mut skip);
    assert!(
        skip.contains(&"nfpm".to_string()) && skip.contains(&"sign".to_string()),
        "existing user skips must be preserved"
    );
    for &stage in anodizer_core::stages::UPSTREAM_STAGES {
        assert!(
            skip.contains(&stage.to_string()),
            "--prepare must add {stage} alongside user skips"
        );
    }
}

#[test]
fn test_apply_prepare_mode_to_skip_composes_with_snapshot_marker() {
    // `--prepare --snapshot` must produce a skip list that includes all
    // network-touching stages, independent of any snapshot-only entries a
    // caller may have pre-added. The augmentation is purely additive —
    // snapshot semantics remain owned by the snapshot flag.
    let mut skip = vec!["sign".to_string()];
    apply_prepare_mode_to_skip(&mut skip);
    for &stage in anodizer_core::stages::UPSTREAM_STAGES {
        assert!(
            skip.iter().any(|s| s == stage),
            "--prepare must add {stage} regardless of snapshot composition"
        );
    }
    assert!(
        skip.iter().any(|s| s == "sign"),
        "user-specified skip survives composition"
    );
}

#[test]
fn test_apply_prepare_mode_to_skip_is_idempotent() {
    let mut skip = vec![
        "release".to_string(),
        "publish".to_string(),
        "blob".to_string(),
    ];
    apply_prepare_mode_to_skip(&mut skip);
    // No duplicates for stages that were pre-populated.
    let release_count = skip.iter().filter(|s| s.as_str() == "release").count();
    let publish_count = skip.iter().filter(|s| s.as_str() == "publish").count();
    let blob_count = skip.iter().filter(|s| s.as_str() == "blob").count();
    assert_eq!(release_count, 1, "no duplicate release");
    assert_eq!(publish_count, 1, "no duplicate publish");
    assert_eq!(blob_count, 1, "no duplicate blob");
    assert!(skip.contains(&"announce".to_string()));
    assert!(skip.contains(&"snapcraft-publish".to_string()));
}

// ---- preflight auto-run gating ---------------------------------------

#[test]
fn should_run_preflight_auto_default_runs() {
    // No flag set → run. `--publish-only` is intentionally NOT a gate:
    // it is the one mode that actually crosses the one-way doors, so
    // the read-only publisher-state / credential probes must run there
    // by default.
    assert!(should_run_preflight_auto(false, false, false, false));
}

#[test]
fn should_run_preflight_auto_snapshot_skips() {
    assert!(!should_run_preflight_auto(true, false, false, false));
}

#[test]
fn should_run_preflight_auto_dry_run_skips() {
    assert!(!should_run_preflight_auto(false, true, false, false));
}

#[test]
fn should_run_preflight_auto_split_skips() {
    assert!(!should_run_preflight_auto(false, false, true, false));
}

#[test]
fn should_run_preflight_auto_publish_skipped_skips() {
    assert!(!should_run_preflight_auto(false, false, false, true));
}

/// The global `--strict` and `preflight.strict` fold into one effective
/// flag (`Context::preflight_is_strict`): either one turns on strict
/// per-publisher preflight promotion, neither leaves it off. Pin it
/// against the real combiner.
#[test]
fn strict_or_config_strict_promotes_preflight_to_strict() {
    let combine = |strict: bool, cfg_strict: bool| {
        let config = Config {
            preflight: anodizer_core::config::PreflightConfig { strict: cfg_strict },
            ..Default::default()
        };
        let ctx = Context::new(
            config,
            ContextOptions {
                strict,
                ..Default::default()
            },
        );
        ctx.preflight_is_strict()
    };
    assert!(!combine(false, false));
    assert!(combine(true, false));
    assert!(combine(false, true));
    assert!(combine(true, true));
}

// ---- gate_required_failures -----------------------------------------

/// Build a `Context` with a `publish_report` containing a single
/// publisher result with the given outcome and `required` flag.
fn ctx_with_report(
    name: &str,
    required: bool,
    outcome: anodizer_core::publish_report::PublisherOutcome,
    opts: ContextOptions,
) -> Context {
    use anodizer_core::publish_report::{PublishReport, PublisherGroup, PublisherResult};

    let mut ctx = Context::new(Config::default(), opts);
    let mut report = PublishReport::default();
    report.results.push(PublisherResult {
        name: name.to_string(),
        group: PublisherGroup::Manager,
        required,
        outcome,
        evidence: None,
        entry_skips: Vec::new(),
    });
    ctx.set_publish_report(report);
    ctx
}

#[test]
fn release_exits_nonzero_when_required_publisher_failed() {
    use anodizer_core::publish_report::PublisherOutcome;

    let ctx = ctx_with_report(
        "homebrew",
        true,
        PublisherOutcome::Failed("git push refused".into()),
        ContextOptions::default(),
    );
    let err = gate_required_failures(&ctx).expect_err("must error");
    let msg = format!("{err}");
    assert!(msg.contains("homebrew"), "error names publisher: {msg}");
    assert!(
        msg.contains("required publisher"),
        "error mentions required: {msg}"
    );
}

#[test]
fn release_exits_zero_when_no_required_failures() {
    use anodizer_core::publish_report::{
        PublishReport, PublisherGroup, PublisherOutcome, PublisherResult,
    };

    let mut ctx = Context::new(Config::default(), ContextOptions::default());
    let mut report = PublishReport::default();
    report.results.push(PublisherResult {
        name: "homebrew".to_string(),
        group: PublisherGroup::Manager,
        required: true,
        outcome: PublisherOutcome::Succeeded,
        evidence: None,
        entry_skips: Vec::new(),
    });
    // A *non*-required publisher that failed must NOT trip the gate.
    report.results.push(PublisherResult {
        name: "scoop".to_string(),
        group: PublisherGroup::Manager,
        required: false,
        outcome: PublisherOutcome::Failed("network".to_string()),
        evidence: None,
        entry_skips: Vec::new(),
    });
    ctx.set_publish_report(report);

    gate_required_failures(&ctx).expect("must succeed");
}

#[test]
fn release_required_failures_gate_skipped_in_snapshot() {
    use anodizer_core::publish_report::PublisherOutcome;

    let opts = ContextOptions {
        snapshot: true,
        ..Default::default()
    };
    let ctx = ctx_with_report(
        "homebrew",
        true,
        PublisherOutcome::Failed("boom".into()),
        opts,
    );
    // Snapshot mode skips the gate (defense-in-depth — publishers
    // shouldn't run in snapshot mode at all).
    gate_required_failures(&ctx).expect("snapshot must short-circuit gate");
}

#[test]
fn release_required_failures_gate_skipped_in_dry_run() {
    use anodizer_core::publish_report::PublisherOutcome;

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let ctx = ctx_with_report(
        "homebrew",
        true,
        PublisherOutcome::Failed("boom".into()),
        opts,
    );
    gate_required_failures(&ctx).expect("dry-run must short-circuit gate");
}

#[test]
fn release_required_failures_counts_rollback_failed() {
    use anodizer_core::publish_report::PublisherOutcome;

    // A rolled-back-failed required publisher leaves the operator
    // with a half-published surface — must also produce non-zero exit.
    let ctx = ctx_with_report(
        "homebrew",
        true,
        PublisherOutcome::RollbackFailed("manual cleanup required".into()),
        ContextOptions::default(),
    );
    let err = gate_required_failures(&ctx).expect_err("rollback-failed must error");
    let msg = format!("{err}");
    assert!(msg.contains("homebrew"), "names publisher: {msg}");
}

#[test]
fn release_exits_nonzero_when_required_publisher_blocked_by_verify_gate() {
    use anodizer_core::publish_report::{PublisherOutcome, SkipReason};

    // A transient verify-gate error/false-return blocks a REQUIRED
    // submitter (e.g. cargo on the OIDC leg) before it ever dispatches.
    // Skipped(_) is never is_required_release_failure(), so without the
    // dedicated blocked-names check this would exit 0 on an unpublished
    // required registry.
    let ctx = ctx_with_report(
        "cargo",
        true,
        PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked),
        ContextOptions::default(),
    );
    let err = gate_required_failures(&ctx)
        .expect_err("a required publisher blocked by the verify gate must error");
    let msg = format!("{err}");
    assert!(msg.contains("cargo"), "error names publisher: {msg}");
    assert!(
        msg.contains("verify-release"),
        "error names the verify-release gate: {msg}"
    );
}

#[test]
fn release_required_failures_ignores_optional_publisher_blocked_by_verify_gate() {
    use anodizer_core::publish_report::{PublisherOutcome, SkipReason};

    let ctx = ctx_with_report(
        "chocolatey",
        false,
        PublisherOutcome::Skipped(SkipReason::VerifyGateBlocked),
        ContextOptions::default(),
    );
    gate_required_failures(&ctx).expect("an optional blocked publisher must not gate");
}

#[test]
fn release_required_failures_ignored_when_not_required() {
    use anodizer_core::publish_report::PublisherOutcome;

    // `required: false` + Failed must NOT trip the gate.
    let ctx = ctx_with_report(
        "scoop",
        false,
        PublisherOutcome::Failed("boom".into()),
        ContextOptions::default(),
    );
    gate_required_failures(&ctx).expect("optional failure must not gate");
}

#[test]
fn release_required_failures_noop_without_report() {
    // No publish_report on the context at all (publish stage didn't
    // run, e.g. preflight-only) → gate is a no-op.
    let ctx = Context::new(Config::default(), ContextOptions::default());
    gate_required_failures(&ctx).expect("missing report must short-circuit");
}

#[test]
fn release_exits_nonzero_when_required_snapcraft_publish_failed() {
    // A snapcraft config that opts in via `required: true` must abort
    // the pipeline exit gate exactly like any other required publisher
    // — the same generic mechanism `SnapcraftConfig.required` wires
    // into via `derive_snapcraft_required`.
    use anodizer_core::publish_report::PublisherOutcome;

    let ctx = ctx_with_report(
        "snapcraft",
        true,
        PublisherOutcome::Failed("store rejected upload: dedup collision".into()),
        ContextOptions::default(),
    );
    let err = gate_required_failures(&ctx).expect_err("must error");
    let msg = format!("{err}");
    assert!(msg.contains("snapcraft"), "error names publisher: {msg}");
}

// ---- apply_nightly_template_vars ------------------------------------
//
// Nightly: `tag_name` accepts template syntax (e.g.
// `nightly-{{ .Version }}`) and is rendered AFTER `Version` /
// `RawVersion` / `IsNightly` are populated, so user templates that
// reference those vars resolve to the nightly-overridden values.

fn make_nightly_log() -> StageLogger {
    StageLogger::new("test-nightly", anodizer_core::log::Verbosity::Quiet)
}

/// Shared scaffolding for the `apply_nightly_template_vars` tests:
/// `project_name="myproj"` config (with the caller-supplied
/// `tag_name`, or no `nightly` block at all when `tag_name` is
/// `None`), a fresh `Context`, and `Version` / `ProjectName` /
/// `ShortCommit` pre-populated (the default version_template
/// references `ShortCommit`).
fn setup_nightly_ctx(tag_name: Option<&str>, version: &str) -> (Config, Context) {
    let config = Config {
        project_name: "myproj".to_string(),
        nightly: tag_name.map(|t| NightlyConfig {
            tag_name: Some(t.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = Context::new(
        config.clone(),
        ContextOptions {
            nightly: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", version);
    ctx.template_vars_mut().set("ProjectName", "myproj");
    ctx.template_vars_mut().set("ShortCommit", "abc123d");
    (config, ctx)
}

/// Two tracks so the seeded `Tag` has a family to belong to.
fn nightly_tracks() -> Vec<CrateConfig> {
    let track = |name: &str, tmpl: &str| CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some(tmpl.to_string()),
        ..Default::default()
    };
    vec![
        track("app", "v{{ Version }}"),
        track("operator", "operator-v{{ Version }}"),
    ]
}

/// `Tag` arrives holding the BASE tag the nightly version was derived FROM.
/// Leaving it there makes every pre-release stage — archive names, blob
/// directories, the release header — name the PREVIOUS release.
#[test]
fn nightly_renders_tag_as_the_minted_tag() {
    let (mut config, mut ctx) = setup_nightly_ctx(None, "1.2.3");
    config.crates = nightly_tracks();
    ctx.config = config.clone();
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
    assert_eq!(
        ctx.template_vars().get("Tag").map(String::as_str),
        Some("v1.2.4-abc123d-nightly"),
        "`Tag` must name the tag this nightly release is created on",
    );
}

/// The same seed on a SINGLE-family workspace — one crate, one track, which is
/// what anodizer and every lockstep repo actually are. The two-family pin above
/// cannot catch a seed that only works because a family prefix was there to
/// scope it.
#[test]
fn nightly_renders_tag_as_the_minted_tag_single_family() {
    let (mut config, mut ctx) = setup_nightly_ctx(None, "1.2.3");
    config.crates = vec![CrateConfig {
        name: "app".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ Version }}".to_string()),
        ..Default::default()
    }];
    ctx.config = config.clone();
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
    assert_eq!(
        ctx.template_vars().get("Tag").map(String::as_str),
        Some("v1.2.4-abc123d-nightly"),
    );
}

/// A single-family workspace takes `nightly.tag_name` VERBATIM: there is no
/// sibling track to disambiguate from, so prefixing it would invent a tag no
/// other surface looks for.
#[test]
fn nightly_tag_name_renders_tag_as_the_rolling_tag_single_family() {
    let (mut config, mut ctx) = setup_nightly_ctx(Some("edge"), "1.2.3");
    config.crates = vec![CrateConfig {
        name: "app".to_string(),
        path: ".".to_string(),
        tag_template: Some("v{{ Version }}".to_string()),
        ..Default::default()
    }];
    ctx.config = config.clone();
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
    assert_eq!(
        ctx.template_vars().get("Tag").map(String::as_str),
        Some("edge"),
    );
}

/// With `nightly.tag_name` set the release is created on the rolling tag, so
/// `{{ Tag }}` must be that tag — scoped to the covered crate's own family.
#[test]
fn nightly_tag_name_renders_tag_as_the_rolling_tag() {
    let (mut config, mut ctx) = setup_nightly_ctx(Some("edge"), "1.2.3");
    config.crates = nightly_tracks();
    ctx.config = config.clone();
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
    assert_eq!(
        ctx.template_vars().get("Tag").map(String::as_str),
        Some("vedge"),
    );

    let (mut config, mut ctx) = setup_nightly_ctx(Some("edge"), "1.2.3");
    config.crates = nightly_tracks();
    ctx.config = config.clone();
    ctx.options.selected_crates = vec!["operator".to_string()];
    apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
    assert_eq!(
        ctx.template_vars().get("Tag").map(String::as_str),
        Some("operator-vedge"),
        "the covered crate decides the family the run-wide tag lands in",
    );
}

/// `nightly.name_template` likewise belongs to the release stage. `ReleaseName`
/// was written here and read nowhere.
#[test]
fn nightly_does_not_set_a_release_name_var() {
    let (config, mut ctx) = setup_nightly_ctx(None, "1.2.3");
    apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
    assert_eq!(ctx.template_vars().get("ReleaseName"), None);
}

#[test]
fn nightly_default_version_uses_incpatch_and_short_commit() {
    let (config, mut ctx) = setup_nightly_ctx(None, "1.2.3");
    apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
    assert_eq!(
        ctx.template_vars().get("Version").map(String::as_str),
        Some("1.2.4-abc123d-nightly"),
        "GR-default nightly version: incpatch(1.2.3)-abc123d-nightly",
    );
    assert_eq!(
        ctx.template_vars().get("RawVersion").map(String::as_str),
        Some("1.2.4-abc123d-nightly"),
    );
}

#[test]
fn nightly_version_template_user_override() {
    let config = Config {
        project_name: "myproj".to_string(),
        nightly: Some(NightlyConfig {
            version_template: Some("{{ Version }}-edge-{{ ShortCommit }}".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    ctx.template_vars_mut().set("Version", "2.0.0");
    ctx.template_vars_mut().set("ProjectName", "myproj");
    ctx.template_vars_mut().set("ShortCommit", "deadbee");
    apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
    assert_eq!(
        ctx.template_vars().get("Version").map(String::as_str),
        Some("2.0.0-edge-deadbee"),
    );
}

#[test]
fn nightly_version_template_supports_nightly_build_and_base() {
    // nushell-style: <base>-nightly.<build>+<sha6>. NightlyBuild + Base
    // are set by populate_git_vars in production; set here
    // directly to prove the template references resolve.
    let config = Config {
        project_name: "myproj".to_string(),
        nightly: Some(NightlyConfig {
            version_template: Some(
                "{{ .Base }}-nightly.{{ .NightlyBuild }}+{{ .ShortCommit }}".to_string(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = Context::new(config.clone(), ContextOptions::default());
    ctx.template_vars_mut().set("Version", "0.103.0");
    ctx.template_vars_mut().set("Base", "0.103.0");
    // set_structured takes a serde_json::Value (converted to tera::Value
    // at render time by the engine adapter); a numeric injection here
    // matches populate_git_vars' own Value::Number injection shape.
    ctx.template_vars_mut()
        .set_structured("NightlyBuild", serde_json::Value::from(42u64));
    ctx.template_vars_mut().set("ProjectName", "myproj");
    ctx.template_vars_mut().set("ShortCommit", "a1b2c3");
    apply_nightly_template_vars(&mut ctx, &config, &make_nightly_log()).unwrap();
    assert_eq!(
        ctx.template_vars().get("Version").map(String::as_str),
        Some("0.103.0-nightly.42+a1b2c3"),
    );
}

// ---- map_head_tags_to_crates unit tests --------------------------------

fn make_log() -> StageLogger {
    StageLogger::new(
        "test",
        anodizer_core::log::Verbosity::from_flags(true, false, false),
    )
}

#[test]
fn map_head_tags_empty_returns_empty() {
    // No tags at HEAD → empty selection.
    let crates = vec![make_crate("app", None)];
    let log = make_log();
    // Simulate get_tags_at_head returning empty by calling with an empty list.
    // The core matching logic is exercised directly.
    let head_tags: &[String] = &[];
    let selected = run_tag_mapping(&crates, head_tags);
    assert!(selected.is_empty(), "no tags → empty selection");
    let _ = log;
}

#[test]
fn map_head_tags_single_tag_matches_single_crate() {
    let crates = vec![
        make_crate_with_template("core", "crates/core", "core-v{{ .Version }}"),
        make_crate_with_template("cli", "crates/cli", "v{{ .Version }}"),
    ];
    let head_tags = vec!["core-v1.2.3".to_string()];
    let selected = run_tag_mapping(&crates, &head_tags);
    assert_eq!(selected, vec!["core"]);
}

#[test]
fn map_head_tags_multiple_tags_maps_multiple_crates() {
    let crates = vec![
        make_crate_with_template("core", "crates/core", "core-v{{ .Version }}"),
        make_crate_with_template("cli", "crates/cli", "v{{ .Version }}"),
    ];
    let head_tags = vec!["core-v1.2.3".to_string(), "v1.2.3".to_string()];
    let selected = run_tag_mapping(&crates, &head_tags);
    assert!(selected.contains(&"core".to_string()));
    assert!(selected.contains(&"cli".to_string()));
    assert_eq!(selected.len(), 2);
}

#[test]
fn map_head_tags_longer_prefix_wins() {
    // "core-v" is more specific than "v"; only "core" should match.
    let crates = vec![
        make_crate_with_template("app", ".", "v{{ .Version }}"),
        make_crate_with_template("core", "crates/core", "core-v{{ .Version }}"),
    ];
    let head_tags = vec!["core-v0.5.0".to_string()];
    let selected = run_tag_mapping(&crates, &head_tags);
    assert_eq!(selected, vec!["core"], "longer prefix must win");
}

#[test]
fn map_head_tags_topo_sort_respects_depends_on() {
    // core → cli; both tags present; cli depends on core.
    // After topo_sort_selected, core must come before cli.
    let all = vec![
        make_crate_with_template("core", "crates/core", "core-v{{ .Version }}"),
        CrateConfig {
            name: "cli".to_string(),
            path: "crates/cli".to_string(),
            tag_template: Some("v{{ .Version }}".to_string()),
            depends_on: Some(vec!["core".to_string()]),
            ..Default::default()
        },
    ];
    let head_tags = vec!["v1.0.0".to_string(), "core-v1.0.0".to_string()];
    let selected = run_tag_mapping(&all, &head_tags);
    // Both should be selected.
    assert!(selected.contains(&"core".to_string()));
    assert!(selected.contains(&"cli".to_string()));
    let sorted = topo_sort_selected(&all, &selected);
    let core_pos = sorted.iter().position(|s| s == "core").unwrap();
    let cli_pos = sorted.iter().position(|s| s == "cli").unwrap();
    assert!(
        core_pos < cli_pos,
        "core must come before cli in topo order; got: {:?}",
        sorted
    );
}

#[test]
fn map_head_tags_lockstep_shared_template_selects_every_crate() {
    // Lockstep workspace: every crate shares `v{{ Version }}`, so the
    // single pushed tag `v1.0.0` matches them all with an equal-length
    // prefix. The publisher-owning binary crate is declared LAST, after a
    // library crate with no publisher block — the exact shape of
    // anodizer's own `.anodizer.yaml` (anodizer-core first, the `anodizer`
    // bin with scoop/chocolatey/winget/aur/nix last). Selecting only the
    // first-declared crate silently dropped the bin crate, no-op'ing every
    // artifact publisher. The whole tier must be selected.
    let all = vec![
        make_crate_with_template("anodizer-core", "crates/core", "v{{ .Version }}"),
        make_crate_with_template(
            "anodizer-stage-build",
            "crates/stage-build",
            "v{{ .Version }}",
        ),
        make_crate_with_template("anodizer", "crates/cli", "v{{ .Version }}"),
    ];
    let head_tags = vec!["v1.0.0".to_string()];
    let selected = run_tag_mapping(&all, &head_tags);
    assert!(
        selected.contains(&"anodizer".to_string()),
        "the publisher-owning bin crate must be selected; got: {selected:?}"
    );
    assert_eq!(
        selected.len(),
        3,
        "every lockstep crate sharing the tag must be selected; got: {selected:?}"
    );
}

#[test]
fn resolve_tag_to_crates_longer_prefix_tier_wins_exclusively() {
    // Distinct per-crate tags (independent-version workspace mode) must
    // NOT regress: a `core-v` crate (prefix len 6) wins exclusively over a
    // `v` sibling (len 1) for the tag `core-v0.5.0`, because the shorter
    // prefix is a different tier — only the longest-prefix tier is returned.
    let crates = vec![
        make_crate_with_template("app", ".", "v{{ .Version }}"),
        make_crate_with_template("core", "crates/core", "core-v{{ .Version }}"),
    ];
    let names: Vec<&str> = resolve_tag_to_crates("core-v0.5.0", &crates)
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, vec!["core"], "only the longest-prefix tier matches");
}

#[test]
fn map_head_tags_unrecognized_tag_is_ignored() {
    let crates = vec![make_crate_with_template("app", ".", "v{{ .Version }}")];
    let head_tags = vec!["nightly-20260527".to_string(), "v2.0.0".to_string()];
    let selected = run_tag_mapping(&crates, &head_tags);
    // nightly tag doesn't match any prefix → only "app" from v2.0.0.
    assert_eq!(selected, vec!["app"]);
}

#[test]
fn map_head_tags_no_tags_at_head_is_noop() {
    let crates = vec![make_crate_with_template("app", ".", "v{{ .Version }}")];
    let head_tags: Vec<String> = vec![];
    let selected = run_tag_mapping(&crates, &head_tags);
    assert!(selected.is_empty(), "no tags → no-op, empty selection");
}

/// Helper: drive the PRODUCTION selection pipeline
/// (`select_crates_for_head_tags` — filter + tag→crate mapping, the whole
/// body of `map_head_tags_to_crates` minus only the `git::get_tags_at_head()`
/// read). Tests pass a fixed tag list the way the wrapper would after reading
/// HEAD, so dropping the ignore/nightly filter from the production wiring
/// fails these tests.
fn run_tag_mapping(crates: &[CrateConfig], head_tags: &[String]) -> Vec<String> {
    let log = StageLogger::new("test", Verbosity::Quiet);
    select_crates_for_head_tags(head_tags, crates, None, &log)
}

#[test]
fn map_head_tags_stranded_nightly_tags_never_drive_selection() {
    // Stranded per-crate nightly tags at HEAD (failed nightly run) must not
    // scope a stable release to just those crates — with only nightly tags
    // present the selection is EMPTY (release no-op), not operator+csi.
    let crates = vec![
        make_crate_with_template("cfgd", ".", "v{{ .Version }}"),
        make_crate_with_template("operator", "crates/operator", "operator-v{{ .Version }}"),
        make_crate_with_template("csi", "crates/csi", "csi-v{{ .Version }}"),
    ];
    let head_tags = vec![
        "operator-v0.5.1-9a0d7ed0-nightly".to_string(),
        "csi-v0.5.1-9a0d7ed0-nightly".to_string(),
    ];
    assert!(run_tag_mapping(&crates, &head_tags).is_empty());

    // A real stable tag alongside the stranded ones selects normally.
    let head_tags = vec![
        "operator-v0.5.1-9a0d7ed0-nightly".to_string(),
        "v0.6.0".to_string(),
    ];
    assert_eq!(run_tag_mapping(&crates, &head_tags), vec!["cfgd"]);
}

fn make_crate_with_template(name: &str, path: &str, template: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: path.to_string(),
        tag_template: Some(template.to_string()),
        ..Default::default()
    }
}

// ---- project_root resolution -----------------------------------------

/// `resolve_project_root` must return the parent of a normal config
/// path so repo-relative file lookups resolve against the repo root
/// even when anodizer is invoked from a sibling directory with
/// `--config=<repo>/anodizer.yaml`.
#[test]
fn resolve_project_root_uses_config_parent() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).expect("create repo dir");
    let cfg_path = repo_dir.join("anodizer.yaml");
    std::fs::write(&cfg_path, "project_name: x\n").expect("write config");

    let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
    let resolved = resolve_project_root(&cfg_path, Some(&log)).expect("project_root resolved");
    let expected = std::fs::canonicalize(&repo_dir).expect("canonicalize repo dir");
    assert_eq!(resolved, expected);
    assert_eq!(
        cap.warn_count(),
        0,
        "a config path with a parent must NOT trigger the bare-filename warn"
    );
}

/// When the config path has no parent component (rare — bare filename),
/// the resolver must fall back to CWD so consumers still get *some*
/// anchor instead of `None`. CWD is process state a test cannot
/// override, so the assertion is that the field is populated rather
/// than a match on a specific path.
#[test]
fn resolve_project_root_falls_back_to_cwd_for_bare_filename() {
    let bare = std::path::Path::new("anodizer.yaml");
    let resolved = resolve_project_root(bare, None);
    assert!(
        resolved.is_some(),
        "bare-filename config should fall back to CWD, got None"
    );
}

/// Bare-filename `--config=anodizer.yaml` is almost always a
/// misconfiguration: every repo-relative consumer (snapcraft icon
/// lookup, extra-file globs, ...) will resolve against the process
/// CWD rather than the repo root. The resolver must surface a `warn`
/// so the misconfiguration is visible in CI logs without
/// hard-failing the release (which would break the legitimate
/// CWD == project-root case).
#[test]
fn resolve_project_root_warns_when_falling_back_for_bare_filename() {
    let bare = std::path::Path::new("anodizer.yaml");
    let (log, cap) = StageLogger::with_capture("test", Verbosity::Normal);
    let resolved = resolve_project_root(bare, Some(&log));
    assert!(resolved.is_some(), "fallback path still resolved");
    let warns = cap.warn_messages();
    assert!(
        warns.iter().any(|m| m.contains("bare filename")),
        "expected a bare-filename warn; got warns: {warns:?}"
    );
    assert!(
        warns
            .iter()
            .any(|m| m.contains("repo-relative file lookups")),
        "expected the warn to call out the load-bearing repo-relative file lookups; \
             got warns: {warns:?}"
    );
}

/// `build_context_options` must surface the `project_root` it was
/// handed verbatim onto `ContextOptions` so downstream stages that
/// resolve paths relative to the project root (e.g. the cargo
/// publisher's `target/` resolution, snapcraft icon paths) see the
/// real root rather than a hard-coded `None`.
#[test]
fn build_context_options_propagates_project_root() {
    let opts = base_release_opts();
    let root = std::path::PathBuf::from("/tmp/example-project");
    let ctx_opts = build_context_options(
        &opts,
        vec![],
        vec![],
        vec![],
        vec![],
        Some(root.clone()),
        None,
    );
    assert_eq!(
        ctx_opts.project_root,
        Some(root),
        "project_root must flow through build_context_options into ContextOptions"
    );
}

// -----------------------------------------------------------------------
// validate_strict_vs_allowlist
// -----------------------------------------------------------------------

#[test]
fn validate_strict_alone_is_ok() {
    let opts = ReleaseOpts {
        strict: true,
        ..base_release_opts()
    };
    assert!(validate_strict_vs_allowlist(&opts).is_ok());
}

#[test]
fn validate_strict_with_allowlist_is_mutually_exclusive_error() {
    let opts = ReleaseOpts {
        strict: true,
        allow_nondeterministic: vec!["sbom=flaky".to_string()],
        ..base_release_opts()
    };
    let err = validate_strict_vs_allowlist(&opts).unwrap_err().to_string();
    assert!(
        err.contains("mutually exclusive"),
        "must reject --strict + --allow-nondeterministic, got: {err}"
    );
}

#[test]
fn validate_allowlist_without_strict_is_ok() {
    let opts = ReleaseOpts {
        strict: false,
        allow_nondeterministic: vec!["sbom=flaky".to_string()],
        ..base_release_opts()
    };
    assert!(validate_strict_vs_allowlist(&opts).is_ok());
}

// -----------------------------------------------------------------------
// parse_allow_nondeterministic
// -----------------------------------------------------------------------

#[test]
fn parse_allow_nondeterministic_splits_name_and_reason() {
    let got = parse_allow_nondeterministic(&["sbom=upstream timestamp".to_string()]).unwrap();
    assert_eq!(
        got,
        vec![("sbom".to_string(), "upstream timestamp".to_string())]
    );
}

#[test]
fn parse_allow_nondeterministic_preserves_equals_in_reason() {
    // Only the FIRST `=` splits name from reason.
    let got = parse_allow_nondeterministic(&["docker=a=b reason".to_string()]).unwrap();
    assert_eq!(got, vec![("docker".to_string(), "a=b reason".to_string())]);
}

#[test]
fn parse_allow_nondeterministic_missing_equals_errors() {
    let err = parse_allow_nondeterministic(&["sbom".to_string()])
        .unwrap_err()
        .to_string();
    assert!(err.contains("must be NAME=REASON"));
}

#[test]
fn parse_allow_nondeterministic_empty_reason_errors() {
    let err = parse_allow_nondeterministic(&["sbom=   ".to_string()])
        .unwrap_err()
        .to_string();
    assert!(err.contains("reason cannot be empty"));
}

#[test]
fn parse_allow_nondeterministic_empty_input_yields_empty_vec() {
    assert!(parse_allow_nondeterministic(&[]).unwrap().is_empty());
}

// -----------------------------------------------------------------------
// compute_skip_stages
// -----------------------------------------------------------------------

#[test]
fn compute_skip_stages_merges_workspace_skip_without_duplicates() {
    let got = compute_skip_stages(
        vec!["docker".to_string()],
        &["docker".to_string(), "msi".to_string()],
        false,
    );
    assert_eq!(got, vec!["docker".to_string(), "msi".to_string()]);
}

#[test]
fn compute_skip_stages_snapshot_adds_upload_stages_and_announce() {
    let got = compute_skip_stages(vec![], &[], true);
    for stage in SNAPSHOT_AUTO_SKIP {
        assert!(
            got.contains(&stage.to_string()),
            "snapshot must auto-skip {stage}, got: {got:?}"
        );
    }
}

/// The snapshot auto-skip set is a deliberate NARROWING of
/// `UPSTREAM_STAGES` (see the const's doc for why the remaining
/// upstream stages self-gate); it must never contain a stage the
/// upstream classification doesn't — that would mean an auto-skip
/// entry with no upstream classification backing it.
#[test]
fn snapshot_auto_skip_is_subset_of_upstream_stages() {
    for stage in SNAPSHOT_AUTO_SKIP {
        assert!(
            anodizer_core::stages::UPSTREAM_STAGES.contains(stage),
            "SNAPSHOT_AUTO_SKIP entry '{stage}' is not an UPSTREAM_STAGES member"
        );
    }
}

#[test]
fn compute_skip_stages_skipping_publish_implies_announce() {
    let got = compute_skip_stages(vec!["publish".to_string()], &[], false);
    assert!(
        got.contains(&"announce".to_string()),
        "skipping publish must imply skipping announce, got: {got:?}"
    );
}

#[test]
fn compute_skip_stages_no_signals_is_passthrough() {
    let got = compute_skip_stages(vec!["msi".to_string()], &[], false);
    assert_eq!(got, vec!["msi".to_string()]);
}

// -----------------------------------------------------------------------
// crate-universe selection boundary
// -----------------------------------------------------------------------

/// `--crate X` / `--all` resolve against `Config::crate_universe` — a
/// workspace-only crate must be selectable exactly like a top-level one.
#[test]
fn selection_universe_unions_top_level_and_workspace_crates() {
    let config = Config {
        crates: vec![make_crate("top", None)],
        workspaces: Some(vec![WorkspaceConfig {
            crates: vec![make_crate("ws_a", None), make_crate("ws_b", None)],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let names: Vec<&str> = config
        .crate_universe()
        .into_iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, vec!["top", "ws_a", "ws_b"]);
}

#[test]
fn selection_universe_dedupes_by_name_keeping_top_level() {
    let config = Config {
        crates: vec![make_crate("dup", None)],
        workspaces: Some(vec![WorkspaceConfig {
            crates: vec![make_crate("dup", None), make_crate("unique", None)],
            ..Default::default()
        }]),
        ..Default::default()
    };
    let names: Vec<&str> = config
        .crate_universe()
        .into_iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, vec!["dup", "unique"]);
}

// -----------------------------------------------------------------------
// resolve_tag_to_crates (lockstep tie-tier + per-crate prefix resolution)
// -----------------------------------------------------------------------

fn tagged_crate(name: &str, template: &str) -> CrateConfig {
    CrateConfig {
        name: name.to_string(),
        path: ".".to_string(),
        tag_template: Some(template.to_string()),
        ..Default::default()
    }
}

/// Map a resolved tie-tier to its crate names for assertion.
fn resolved_names<'a>(tag: &str, crates: &'a [CrateConfig]) -> Vec<&'a str> {
    resolve_tag_to_crates(tag, crates)
        .iter()
        .map(|c| c.name.as_str())
        .collect()
}

#[test]
fn resolve_tag_to_crate_single_crate_v_prefix() {
    let crates = vec![tagged_crate("app", "v{{ .Version }}")];
    assert_eq!(resolved_names("v1.2.3", &crates), vec!["app"]);
}

#[test]
fn resolve_tag_to_crate_longest_prefix_wins_over_sibling() {
    // `core-v` must win over the shorter `v` for tag `core-v0.1.0`.
    let crates = vec![
        tagged_crate("base", "v{{ .Version }}"),
        tagged_crate("core", "core-v{{ .Version }}"),
    ];
    assert_eq!(resolved_names("core-v0.1.0", &crates), vec!["core"]);
}

#[test]
fn resolve_tag_to_crate_non_numeric_remainder_does_not_match() {
    let crates = vec![tagged_crate("app", "v{{ .Version }}")];
    // `vendor-branch` shares the `v` prefix but the remainder isn't a version.
    assert!(resolve_tag_to_crates("vendor-branch", &crates).is_empty());
}

#[test]
fn resolve_tag_to_crate_unmatched_prefix_is_none() {
    let crates = vec![tagged_crate("core", "core-v{{ .Version }}")];
    assert!(resolve_tag_to_crates("cli-v1.0.0", &crates).is_empty());
}

/// A `dist/` holding only the files the run itself writes before any stage
/// produces an artifact is what a refused run leaves behind; a retry must
/// pass the gate without `--clean`.
#[test]
fn dist_holding_only_its_own_bookkeeping_passes_the_dist_gate() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    // Spelled out rather than read from `RUN_BOOKKEEPING_FILES`, so a
    // shrunken set fails this pin instead of shrinking the fixture with it.
    for name in ["config.yaml", "release-notes.md", "matrix.json"] {
        std::fs::write(dist.join(name), "x").unwrap();
    }
    let config = Config {
        dist,
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    enforce_dist_state(&config, &base_release_opts(), &log).unwrap();
}

/// A `--split` run creates its shard directory before any stage runs, so a
/// run refused after that point leaves an empty `dist/<shard>/` behind. The
/// retry must not trip over it: population is files, not directories, and a
/// shard holding only its own `context.json` produced nothing either.
#[test]
fn an_empty_split_shard_directory_is_not_dist_population() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(dist.join("linux")).unwrap();
    std::fs::write(dist.join("config.yaml"), "x").unwrap();
    let config = Config {
        dist: dist.clone(),
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    enforce_dist_state(&config, &base_release_opts(), &log).unwrap();

    std::fs::write(dist.join("linux").join("context.json"), "{}").unwrap();
    enforce_dist_state(&config, &base_release_opts(), &log).unwrap();
}

/// The bookkeeping exemptions are anchored to the depth the run writes them
/// at: a file that merely shares a bookkeeping name deeper in the tree is a
/// stage artifact, and the gate must refuse to build over it.
#[test]
fn a_bookkeeping_name_below_the_dist_root_is_still_population() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(dist.join("sub")).unwrap();
    std::fs::write(dist.join("config.yaml"), "x").unwrap();
    std::fs::write(dist.join("sub").join("config.yaml"), "x").unwrap();
    let config = Config {
        dist,
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    let err = enforce_dist_state(&config, &base_release_opts(), &log)
        .unwrap_err()
        .to_string();
    assert!(err.contains("sub/config.yaml"), "{err}");
}

/// `context.json` is a shard's own bookkeeping, so it is excused directly
/// inside `dist/<shard>/` and nowhere else.
#[test]
fn a_context_json_below_a_shard_directory_is_population() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(dist.join("linux").join("deeper")).unwrap();
    std::fs::write(dist.join("linux").join("context.json"), "{}").unwrap();
    let config = Config {
        dist: dist.clone(),
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    enforce_dist_state(&config, &base_release_opts(), &log)
        .expect("a shard's own context.json is not population");

    std::fs::write(dist.join("linux").join("deeper").join("context.json"), "{}").unwrap();
    let err = enforce_dist_state(&config, &base_release_opts(), &log)
        .unwrap_err()
        .to_string();
    assert!(err.contains("linux/deeper/context.json"), "{err}");
}

/// A shard directory holding an artifact IS population, and the gate names it
/// by its `dist`-relative path so the operator can find it.
#[test]
fn a_split_shard_holding_an_artifact_is_refused_by_its_relative_path() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(dist.join("linux")).unwrap();
    std::fs::write(dist.join("linux").join("context.json"), "{}").unwrap();
    std::fs::write(dist.join("linux").join("myapp_linux_amd64.tar.gz"), "x").unwrap();
    let config = Config {
        dist,
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    let err = enforce_dist_state(&config, &base_release_opts(), &log)
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not empty"), "{err}");
    assert!(err.contains("linux/myapp_linux_amd64.tar.gz"), "{err}");
}

/// Bookkeeping only the run that wrote it produces (a `matrix.json` from an
/// earlier `--split`) must not outlive the gate: once the population check
/// passes, every bookkeeping file present is removed.
#[test]
fn passing_the_dist_gate_removes_the_previous_run_bookkeeping() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    for name in ["config.yaml", "matrix.json"] {
        std::fs::write(dist.join(name), "x").unwrap();
    }
    let config = Config {
        dist: dist.clone(),
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    enforce_dist_state(&config, &base_release_opts(), &log).unwrap();
    assert!(
        !dist.join("matrix.json").exists(),
        "stale matrix.json survived"
    );
    assert!(
        !dist.join("config.yaml").exists(),
        "stale config.yaml survived"
    );
}

/// A refused gate deletes nothing — the operator still sees the dist that
/// tripped it.
#[test]
fn a_refused_dist_gate_leaves_the_bookkeeping_in_place() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("matrix.json"), "x").unwrap();
    std::fs::write(dist.join("myapp_linux_amd64.tar.gz"), "x").unwrap();
    let config = Config {
        dist: dist.clone(),
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    enforce_dist_state(&config, &base_release_opts(), &log).unwrap_err();
    assert!(
        dist.join("matrix.json").exists(),
        "a refused gate deleted bookkeeping"
    );
}

/// `--dry-run` announces the removal at status and touches nothing.
#[test]
fn dry_run_reports_stale_bookkeeping_without_removing_it() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("matrix.json"), "x").unwrap();
    let config = Config {
        dist: dist.clone(),
        ..Default::default()
    };
    let (log, capture) = StageLogger::with_capture("test", Verbosity::Quiet);
    let opts = ReleaseOpts {
        dry_run: true,
        ..base_release_opts()
    };
    enforce_dist_state(&config, &opts, &log).unwrap();
    assert!(dist.join("matrix.json").exists(), "dry-run deleted a file");
    let lines = capture.all_messages();
    assert!(
        lines.contains(&(
            anodizer_core::log::LogLevel::Status,
            "(dry-run) would remove stale matrix.json".to_string(),
        )),
        "{lines:?}"
    );
}

/// One artifact next to the bookkeeping is population: the gate refuses and
/// names the entry it tripped over, never the bookkeeping file.
#[test]
fn dist_holding_bookkeeping_and_an_artifact_is_refused() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let dist = tmp.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("config.yaml"), "x").unwrap();
    std::fs::write(dist.join("myapp_linux_amd64.tar.gz"), "x").unwrap();
    let config = Config {
        dist,
        ..Default::default()
    };
    let log = StageLogger::new("test", Verbosity::Quiet);
    let err = enforce_dist_state(&config, &base_release_opts(), &log)
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not empty"), "{err}");
    assert!(err.contains("myapp_linux_amd64.tar.gz"), "{err}");
    assert!(!err.contains("config.yaml"), "{err}");
}

// -----------------------------------------------------------------------
// A repository git refuses to read is not "no release tags at HEAD"
// -----------------------------------------------------------------------

fn refused_repository() -> anyhow::Error {
    anyhow::Error::new(anodizer_core::git::RepositoryUnreadable {
        message: "detected dubious ownership in repository at '/src'".to_string(),
    })
    .context("failed to read tags at HEAD")
}

/// The whole tagless-mode population, asked both questions: a mode that never
/// consults HEAD's tags tolerates a repository git cannot read, and one that
/// does consult them never does. The two answers come from one list, so the
/// walk is what proves a new mode joined it.
#[test]
fn every_tagless_mode_answers_both_questions_the_same_way() {
    type ModeSetter = fn(&mut ReleaseOpts);
    let modes: [(&str, ModeSetter); 8] = [
        ("--snapshot", |o| o.snapshot = true),
        ("--nightly", |o| o.nightly = true),
        ("--dry-run", |o| o.dry_run = true),
        ("--publish-only", |o| o.publish_only = true),
        ("--announce-only", |o| o.announce_only = true),
        ("--split", |o| o.split = true),
        ("--merge", |o| o.merge = true),
        ("--preflight-secrets", |o| o.preflight_secrets = true),
    ];
    for (flag, set) in modes {
        let mut opts = base_release_opts();
        set(&mut opts);
        assert!(
            !super::run::selection_depends_on_head_tags(&opts),
            "{flag} does not read HEAD's tags to pick crates"
        );
        let log = StageLogger::new("release", Verbosity::Quiet);
        super::run::recover_crate_selection(Err(refused_repository()), &opts, &log)
            .unwrap_or_else(|e| panic!("{flag} must survive an unreadable repository: {e:#}"));
    }
    let plain = base_release_opts();
    assert!(
        super::run::selection_depends_on_head_tags(&plain),
        "a plain release picks its crates from HEAD's tags"
    );
    let log = StageLogger::new("release", Verbosity::Quiet);
    super::run::recover_crate_selection(Err(refused_repository()), &plain, &log)
        .expect_err("a plain release fails on an unreadable repository");
}

#[test]
fn release_fails_when_git_refuses_the_repository() {
    let opts = base_release_opts();
    let (log, capture) = StageLogger::with_capture("release", Verbosity::Normal);
    let err = super::run::recover_crate_selection(Err(refused_repository()), &opts, &log)
        .expect_err("a repository git cannot read must fail the release");
    let text = format!("{err:#}");
    assert!(
        text.contains("dubious ownership"),
        "git's own error must reach the operator: {text}"
    );
    assert_eq!(
        capture.warn_count(),
        0,
        "the run fails; it does not warn and carry on"
    );
}

#[test]
fn a_crate_selection_release_does_not_recover_from_an_unreadable_repository() {
    // `--crate foo` names the crate itself, so HEAD's tags do not pick the
    // selection — but the run still tags, builds and publishes out of a
    // repository git must be able to read.
    let mut opts = base_release_opts();
    opts.crate_names = vec!["myapp".to_string()];
    let (log, capture) = StageLogger::with_capture("release", Verbosity::Normal);
    let err = super::run::recover_crate_selection(Err(refused_repository()), &opts, &log)
        .expect_err("a named crate does not make an unreadable repository survivable");
    assert!(
        format!("{err:#}").contains("dubious ownership"),
        "git's own error must reach the operator: {err:#}"
    );
    assert_eq!(
        capture.warn_count(),
        0,
        "the run fails; it does not warn and carry on"
    );
}

#[test]
fn an_all_crates_release_does_not_recover_from_an_unreadable_repository() {
    let mut opts = base_release_opts();
    opts.all = true;
    let (log, capture) = StageLogger::with_capture("release", Verbosity::Normal);
    super::run::recover_crate_selection(Err(refused_repository()), &opts, &log)
        .expect_err("--all does not make an unreadable repository survivable");
    assert_eq!(capture.warn_count(), 0, "the run fails; it does not warn");
}

#[test]
fn snapshot_release_warns_when_git_refuses_the_repository() {
    let mut opts = base_release_opts();
    opts.snapshot = true;
    let (log, capture) = StageLogger::with_capture("release", Verbosity::Normal);
    let selected = super::run::recover_crate_selection(Err(refused_repository()), &opts, &log)
        .expect("a snapshot never consults HEAD's tags, so it continues");
    assert!(selected.is_empty(), "got {selected:?}");
    let warnings: Vec<String> = capture
        .all_messages()
        .into_iter()
        .filter(|(lvl, _)| *lvl == anodizer_core::log::LogLevel::Warn)
        .map(|(_, m)| m)
        .collect();
    assert_eq!(warnings.len(), 1, "exactly one warning; got {warnings:?}");
    assert!(
        warnings[0].contains("dubious ownership"),
        "the warning must carry git's own error: {warnings:?}"
    );
}
