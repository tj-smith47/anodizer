#![allow(clippy::field_reassign_with_default)]

// External crates
use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::{BuildIgnore, BuildOverride, CrossStrategy};
use anodizer_core::log::{StageLogger, Verbosity};
use anodizer_core::stage::Stage;
use std::collections::HashMap;
use std::path::PathBuf;

// Crate-internal items
use super::BuildStage;
use super::command::{
    BuildContext, build_command, build_lib_command, crate_has_binary_target, detect_crate_type,
    detect_cross_strategy, detect_cross_strategy_for_target_impl, is_linux_gnu,
    resolve_build_program, same_apple_family, same_windows_family, zigbuild_available,
};
use anodizer_core::build_plan::crate_declares_bin;

/// Test helper — assembles a [`BuildContext`] from the varying parts most
/// tests want to vary, defaulting `cross_tool` and `command_override` to
/// `None`. Keeps each test as a single struct literal (field-named) rather
/// than 9 unlabelled positional arguments.
fn ctx_for_test<'a>(
    crate_path: &'a str,
    target: &'a str,
    strategy: &'a CrossStrategy,
    flags: &'a [String],
    features: &'a [String],
    no_default_features: bool,
    env: &'a HashMap<String, String>,
) -> BuildContext<'a> {
    BuildContext {
        crate_path,
        target,
        strategy,
        flags,
        features,
        no_default_features,
        env,
        cross_tool: None,
        command_override: None,
    }
}
use super::profile::{detect_amd64_variant, parse_amd64_variant_from_rustflags};
use super::targets::KNOWN_TARGETS;
use super::targets::{find_matching_override, is_target_ignored, resolve_target_env};
use super::universal::build_universal_binary;
use super::validation::{is_dynamically_linked, strip_glibc_suffix, target_for_validation};
use super::workspace::check_workspace_package;
use anodizer_core::target::DEFAULT_TARGETS;

fn test_logger() -> StageLogger {
    StageLogger::new("build", Verbosity::Normal)
}

#[test]
fn test_build_command_native_cargo() {
    let env = HashMap::new();
    let flags = vec!["--release".to_string()];
    let cmd = build_command(
        "cfgd",
        &ctx_for_test(
            "crates/cfgd",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            &flags,
            &[],
            false,
            &env,
        ),
    );
    assert_eq!(cmd.program, "cargo");
    assert!(cmd.args.contains(&"build".to_string()));
    assert!(cmd.args.contains(&"--target".to_string()));
    assert!(cmd.args.contains(&"x86_64-unknown-linux-gnu".to_string()));
    assert!(cmd.args.contains(&"--release".to_string()));
    assert!(cmd.args.contains(&"--bin".to_string()));
    assert!(cmd.args.contains(&"cfgd".to_string()));
}

#[test]
fn test_build_command_zigbuild() {
    let env = HashMap::new();
    let flags = vec!["--release".to_string()];
    let cmd = build_command(
        "cfgd",
        &ctx_for_test(
            "crates/cfgd",
            "aarch64-unknown-linux-gnu",
            &CrossStrategy::Zigbuild,
            &flags,
            &[],
            false,
            &env,
        ),
    );
    assert_eq!(cmd.program, "cargo");
    assert!(cmd.args.contains(&"zigbuild".to_string()));
    assert!(cmd.args.contains(&"--target".to_string()));
}

#[test]
fn test_build_command_cross() {
    let env = HashMap::new();
    let flags = vec!["--release".to_string()];
    let cmd = build_command(
        "cfgd",
        &ctx_for_test(
            "crates/cfgd",
            "aarch64-unknown-linux-gnu",
            &CrossStrategy::Cross,
            &flags,
            &[],
            false,
            &env,
        ),
    );
    assert_eq!(cmd.program, "cross");
    assert!(cmd.args.contains(&"build".to_string()));
}

#[test]
fn test_build_command_with_features() {
    let env = HashMap::new();
    let flags = vec!["--release".to_string()];
    let features = vec!["tls".to_string(), "json".to_string()];
    let cmd = build_command(
        "cfgd",
        &ctx_for_test(
            "crates/cfgd",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            &flags,
            &features,
            false,
            &env,
        ),
    );
    assert!(cmd.args.contains(&"--features".to_string()));
    assert!(cmd.args.contains(&"tls,json".to_string()));
}

#[test]
fn test_build_command_no_default_features() {
    let env = HashMap::new();
    let flags = vec!["--release".to_string()];
    let cmd = build_command(
        "cfgd",
        &ctx_for_test(
            "crates/cfgd",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            &flags,
            &[],
            true,
            &env,
        ),
    );
    assert!(cmd.args.contains(&"--no-default-features".to_string()));
}

#[test]
fn test_detect_cross_strategy_auto() {
    let strategy = detect_cross_strategy();
    // At minimum, cargo is always available
    assert!(matches!(
        strategy,
        CrossStrategy::Cargo | CrossStrategy::Zigbuild | CrossStrategy::Cross
    ));
}

// ---- Error path tests: invalid triple / build failures ----

#[test]
fn test_build_command_with_invalid_target_triple() {
    // build_command itself does not validate target triples -- it just
    // constructs the command.  Verify the invalid triple is passed through
    // so that cargo (or cross) reports the error at execution time.
    let env = HashMap::new();
    let flags = vec!["--release".to_string()];
    let cmd = build_command(
        "mybin",
        &ctx_for_test(
            "crates/mybin",
            "this-is-not-a-valid-triple",
            &CrossStrategy::Cargo,
            &flags,
            &[],
            false,
            &env,
        ),
    );
    assert!(cmd.args.contains(&"this-is-not-a-valid-triple".to_string()));
    assert_eq!(cmd.program, "cargo");
}

#[test]
fn test_build_command_empty_binary_name() {
    // An empty binary name should still be passed through to --bin
    let env = HashMap::new();
    let cmd = build_command(
        "",
        &ctx_for_test(
            ".",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            &[],
            &[],
            false,
            &env,
        ),
    );
    assert!(cmd.args.contains(&"--bin".to_string()));
    // Empty string is present in args
    assert!(cmd.args.contains(&"".to_string()));
}

#[test]
fn build_stage_appends_runtime_allowlist_to_determinism_state() {
    // BuildStage must mirror `ctx.options.runtime_nondeterministic_allowlist`
    // into `ctx.determinism.runtime_allowlist` so downstream consumers
    // (run summary, release body, harness allow-list) read from a single
    // source of truth instead of poking at `ctx.options`.
    use anodizer_core::config::{BuildConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec![]),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        runtime_nondeterministic_allowlist: vec![
            ("foo.rpm".to_string(), "tool-bug-1234".to_string()),
            ("bar.msi".to_string(), "signing-cert-rotation".to_string()),
        ],
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    // Seed CommitTimestamp so the BuildStage's SDE resolution path
    // populates ctx.determinism (otherwise the runtime-append branch
    // sees None and silently no-ops, which is the intentional fallback
    // but not what this test wants to exercise).
    ctx.template_vars_mut().set("CommitTimestamp", "1700000000");

    let stage = BuildStage;
    stage.run(&mut ctx).expect("BuildStage::run");

    let state = ctx
        .determinism
        .as_ref()
        .expect("DeterminismState must be seeded after BuildStage::run");
    assert_eq!(
        state.runtime_allowlist.len(),
        2,
        "both runtime entries must be appended to DeterminismState",
    );
    assert!(
        state
            .runtime_allowlist
            .iter()
            .any(|(n, r)| n == "foo.rpm" && r == "tool-bug-1234"),
        "first runtime entry must round-trip: {:?}",
        state.runtime_allowlist,
    );
    assert!(
        state
            .runtime_allowlist
            .iter()
            .any(|(n, r)| n == "bar.msi" && r == "signing-cert-rotation"),
        "second runtime entry must round-trip: {:?}",
        state.runtime_allowlist,
    );
    // Compile-time list still populated from the spec contract.
    assert!(
        !state.compile_time_allowlist.is_empty(),
        "compile-time list must remain populated alongside the appended runtime list",
    );
}

#[test]
fn test_build_stage_no_targets_skips_gracefully() {
    use anodizer_core::config::{BuildConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec![]), // explicitly empty targets
            ..Default::default()
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    let stage = BuildStage;
    // Should succeed without error -- empty targets list is skipped
    assert!(stage.run(&mut ctx).is_ok());
    // No artifacts should be registered
    let binaries = ctx
        .artifacts
        .by_kind(anodizer_core::artifact::ArtifactKind::Binary);
    assert!(binaries.is_empty());
}

// ---- Error path tests: missing binaries / copy failures ----

#[test]
fn test_copy_from_nonexistent_binary_errors_with_paths() {
    use anodizer_core::config::{BuildConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp_dir = std::env::temp_dir().join("anodizer_build_test_copy_from");
    let _ = std::fs::create_dir_all(&tmp_dir);

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "myapp".to_string(),
        path: tmp_dir.to_string_lossy().into_owned(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
            copy_from: Some("nonexistent-binary".to_string()),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: false,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    let stage = BuildStage;
    let result = stage.run(&mut ctx);
    assert!(
        result.is_err(),
        "copy_from with nonexistent source should fail"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("copy_from") || err.contains("copy"),
        "error should mention copy_from, got: {err}"
    );
}

#[test]
fn test_build_failure_nonzero_exit_produces_clear_error() {
    use anodizer_core::config::{BuildConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp_dir = std::env::temp_dir().join("anodizer_build_test_nonzero");
    let _ = std::fs::create_dir_all(&tmp_dir);
    // Create a minimal project so cargo can find Cargo.toml but fail on build
    std::fs::write(
        tmp_dir.join("Cargo.toml"),
        "[package]\nname = \"no-such-bin\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp_dir.join("src")).unwrap();
    std::fs::write(tmp_dir.join("src/lib.rs"), "").unwrap();

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "no-such-bin".to_string(),
        path: tmp_dir.to_string_lossy().into_owned(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![BuildConfig {
            binary: Some("this-binary-does-not-exist".to_string()),
            targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: false,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    let stage = BuildStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err(), "build with nonexistent binary should fail");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("failed with exit code")
            || err.contains("build failed")
            || err.contains("this-binary-does-not-exist"),
        "error should mention the build failure or binary name, got: {err}"
    );
}

#[test]
fn test_build_command_with_env_vars() {
    let mut env = HashMap::new();
    env.insert("CC".to_string(), "gcc-12".to_string());
    env.insert(
        "RUSTFLAGS".to_string(),
        "-C target-feature=+crt-static".to_string(),
    );

    let flags = vec!["--release".to_string()];
    let cmd = build_command(
        "mybin",
        &ctx_for_test(
            ".",
            "x86_64-unknown-linux-musl",
            &CrossStrategy::Cargo,
            &flags,
            &[],
            false,
            &env,
        ),
    );
    assert_eq!(cmd.env.get("CC").unwrap(), "gcc-12");
    assert_eq!(
        cmd.env.get("RUSTFLAGS").unwrap(),
        "-C target-feature=+crt-static"
    );
}

// ---- cdylib detection tests ----

#[test]
fn test_detect_crate_type_cdylib() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_toml = tmp.path().join("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        r#"[package]
name = "my-lib"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]
"#,
    )
    .unwrap();

    let result = detect_crate_type(tmp.path().to_str().unwrap());
    assert_eq!(result, Some("cdylib".to_string()));
}

#[test]
fn test_detect_crate_type_staticlib() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_toml = tmp.path().join("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        r#"[package]
name = "my-lib"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["staticlib", "rlib"]
"#,
    )
    .unwrap();

    let result = detect_crate_type(tmp.path().to_str().unwrap());
    assert_eq!(result, Some("staticlib".to_string()));
}

#[test]
fn test_detect_crate_type_no_lib_section() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_toml = tmp.path().join("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        r#"[package]
name = "my-bin"
version = "0.1.0"
edition = "2024"
"#,
    )
    .unwrap();

    let result = detect_crate_type(tmp.path().to_str().unwrap());
    assert_eq!(result, None);
}

#[test]
fn test_detect_crate_type_missing_cargo_toml() {
    let tmp = tempfile::tempdir().unwrap();
    let result = detect_crate_type(tmp.path().to_str().unwrap());
    assert_eq!(result, None);
}

#[test]
fn test_detect_crate_type_underscore_variant() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_toml = tmp.path().join("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        r#"[package]
name = "my-lib"
version = "0.1.0"
edition = "2024"

[lib]
crate_type = ["dylib"]
"#,
    )
    .unwrap();

    let result = detect_crate_type(tmp.path().to_str().unwrap());
    assert_eq!(result, Some("dylib".to_string()));
}

// ---- build_lib_command tests ----

#[test]
fn test_build_lib_command_uses_lib_flag() {
    let env = HashMap::new();
    let flags = vec!["--release".to_string()];
    let cmd = build_lib_command(&ctx_for_test(
        "crates/my-lib",
        "x86_64-unknown-linux-gnu",
        &CrossStrategy::Cargo,
        &flags,
        &[],
        false,
        &env,
    ));
    assert_eq!(cmd.program, "cargo");
    assert!(cmd.args.contains(&"build".to_string()));
    assert!(cmd.args.contains(&"--lib".to_string()));
    assert!(cmd.args.contains(&"--target".to_string()));
    assert!(cmd.args.contains(&"x86_64-unknown-linux-gnu".to_string()));
    assert!(cmd.args.contains(&"--release".to_string()));
    // Should NOT contain --bin
    assert!(!cmd.args.contains(&"--bin".to_string()));
}

#[test]
fn test_build_lib_command_with_features() {
    let env = HashMap::new();
    let features = vec!["wasm-bindgen".to_string()];
    let cmd = build_lib_command(&ctx_for_test(
        "crates/my-lib",
        "wasm32-unknown-unknown",
        &CrossStrategy::Cargo,
        &[],
        &features,
        true,
        &env,
    ));
    assert!(cmd.args.contains(&"--lib".to_string()));
    assert!(cmd.args.contains(&"--features".to_string()));
    assert!(cmd.args.contains(&"wasm-bindgen".to_string()));
    assert!(cmd.args.contains(&"--no-default-features".to_string()));
}

#[test]
fn test_build_lib_command_zigbuild() {
    let env = HashMap::new();
    let flags = vec!["--release".to_string()];
    let cmd = build_lib_command(&ctx_for_test(
        ".",
        "aarch64-unknown-linux-gnu",
        &CrossStrategy::Zigbuild,
        &flags,
        &[],
        false,
        &env,
    ));
    assert_eq!(cmd.program, "cargo");
    assert!(cmd.args.contains(&"zigbuild".to_string()));
    assert!(cmd.args.contains(&"--lib".to_string()));
}

// ---- Reproducible build env var injection ----

#[test]
fn test_reproducible_build_sets_source_date_epoch_and_rustflags() {
    use anodizer_core::config::{BuildConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
            reproducible: Some(true),
            flags: Some(vec!["--release".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    // Inject CommitTimestamp so the build stage can read it
    ctx.template_vars_mut().set("CommitTimestamp", "1700000000");

    let stage = BuildStage;
    // dry_run means command is not executed, just eprintln'd — should succeed
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_reproducible_build_appends_to_existing_rustflags() {
    // Verify that when RUSTFLAGS is pre-set in the per-target env, the
    // remap-path-prefix flag is appended rather than replacing it.

    use anodizer_core::config::{BuildConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let mut target_env: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut inner: HashMap<String, String> = HashMap::new();
    inner.insert(
        "RUSTFLAGS".to_string(),
        "-C target-feature=+crt-static".to_string(),
    );
    target_env.insert("x86_64-unknown-linux-musl".to_string(), inner);

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec!["x86_64-unknown-linux-musl".to_string()]),
            reproducible: Some(true),
            flags: Some(vec!["--release".to_string()]),
            env: Some(target_env),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("CommitTimestamp", "1700000000");

    let stage = BuildStage;
    // dry_run — should succeed without actually running cargo
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_reproducible_false_does_not_inject_env_vars() {
    use anodizer_core::config::{BuildConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
            reproducible: Some(false),
            flags: Some(vec!["--release".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    let stage = BuildStage;
    assert!(stage.run(&mut ctx).is_ok());
}

// ---- Universal binary tests ----

/// Helper: register a fake Binary artifact directly in the context.
/// Mirrors production `artifact_meta` — both `binary` and `id` are set
/// (id defaults to the binary name when no explicit build.id is given).
fn register_binary(
    ctx: &mut anodizer_core::context::Context,
    crate_name: &str,
    target: &str,
    path: std::path::PathBuf,
) {
    use anodizer_core::artifact::Artifact;
    let binary_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut meta = HashMap::new();
    meta.insert("binary".to_string(), binary_name.clone());
    meta.insert("id".to_string(), binary_name);
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path,
        target: Some(target.to_string()),
        crate_name: crate_name.to_string(),
        metadata: meta,
        size: None,
    });
}

#[test]
fn test_universal_binary_dry_run_registers_artifact() {
    use anodizer_core::config::{Config, CrateConfig, UniversalBinaryConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        universal_binaries: Some(vec![UniversalBinaryConfig {
            id: None,
            name_template: None,
            replace: None,
            ids: None,
            hooks: None,
            mod_timestamp: None,
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    // Pre-register both macOS arch binaries as already-built artifacts
    register_binary(
        &mut ctx,
        "myapp",
        "aarch64-apple-darwin",
        std::path::PathBuf::from("target/aarch64-apple-darwin/release/myapp"),
    );
    register_binary(
        &mut ctx,
        "myapp",
        "x86_64-apple-darwin",
        std::path::PathBuf::from("target/x86_64-apple-darwin/release/myapp"),
    );

    let result = build_universal_binary(
        "myapp",
        &UniversalBinaryConfig {
            id: None,
            name_template: None,
            replace: None,
            ids: None,
            hooks: None,
            mod_timestamp: None,
        },
        &mut ctx,
        true, // dry_run
    );
    assert!(result.is_ok(), "dry-run universal binary should succeed");

    // A universal artifact should have been registered
    let universals: Vec<_> = ctx
        .artifacts
        .by_kind(ArtifactKind::UniversalBinary)
        .into_iter()
        .filter(|a| a.target.as_deref() == Some("darwin-universal"))
        .collect();
    assert_eq!(
        universals.len(),
        1,
        "one universal artifact should be registered"
    );
    assert_eq!(
        universals[0].metadata.get("universal").map(|s| s.as_str()),
        Some("true")
    );
}

#[test]
fn test_universal_binary_dry_run_uses_name_template() {
    use anodizer_core::config::UniversalBinaryConfig;
    use anodizer_core::context::{Context, ContextOptions};

    let config = anodizer_core::config::Config::default();
    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    ctx.template_vars_mut().set("ProjectName", "myapp");

    register_binary(
        &mut ctx,
        "myapp",
        "aarch64-apple-darwin",
        std::path::PathBuf::from("target/aarch64-apple-darwin/release/myapp"),
    );
    register_binary(
        &mut ctx,
        "myapp",
        "x86_64-apple-darwin",
        std::path::PathBuf::from("target/x86_64-apple-darwin/release/myapp"),
    );

    let ub = UniversalBinaryConfig {
        id: None,
        name_template: Some("{{ .ProjectName }}-universal".to_string()),
        replace: None,
        ids: None,
        hooks: None,
        mod_timestamp: None,
    };

    let result = build_universal_binary("myapp", &ub, &mut ctx, true);
    assert!(result.is_ok());

    let universals: Vec<_> = ctx
        .artifacts
        .by_kind(ArtifactKind::UniversalBinary)
        .into_iter()
        .filter(|a| a.target.as_deref() == Some("darwin-universal"))
        .collect();
    assert_eq!(universals.len(), 1);
    assert!(
        universals[0]
            .path
            .to_string_lossy()
            .contains("myapp-universal"),
        "output path should use rendered name template, got: {}",
        universals[0].path.display()
    );
}

#[test]
fn test_universal_binary_skips_when_missing_arch() {
    use anodizer_core::config::UniversalBinaryConfig;
    use anodizer_core::context::{Context, ContextOptions};

    let config = anodizer_core::config::Config::default();
    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    // Only arm64 — no x86_64
    register_binary(
        &mut ctx,
        "myapp",
        "aarch64-apple-darwin",
        std::path::PathBuf::from("target/aarch64-apple-darwin/release/myapp"),
    );

    let ub = UniversalBinaryConfig {
        id: None,
        name_template: None,
        replace: None,
        ids: None,
        hooks: None,
        mod_timestamp: None,
    };

    let result = build_universal_binary("myapp", &ub, &mut ctx, true);
    assert!(result.is_ok(), "missing arch should not error, just skip");

    // No universal artifact should have been registered
    let universals: Vec<_> = ctx
        .artifacts
        .by_kind(ArtifactKind::UniversalBinary)
        .into_iter()
        .filter(|a| a.target.as_deref() == Some("darwin-universal"))
        .collect();
    assert!(
        universals.is_empty(),
        "no universal artifact when arch is missing"
    );
}

#[test]
fn test_universal_binary_skips_for_different_crate() {
    use anodizer_core::config::UniversalBinaryConfig;
    use anodizer_core::context::{Context, ContextOptions};

    let config = anodizer_core::config::Config::default();
    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    // Register binaries for "other-crate", not "myapp"
    register_binary(
        &mut ctx,
        "other-crate",
        "aarch64-apple-darwin",
        std::path::PathBuf::from("target/aarch64-apple-darwin/release/other"),
    );
    register_binary(
        &mut ctx,
        "other-crate",
        "x86_64-apple-darwin",
        std::path::PathBuf::from("target/x86_64-apple-darwin/release/other"),
    );

    let ub = UniversalBinaryConfig {
        id: None,
        name_template: None,
        replace: None,
        ids: None,
        hooks: None,
        mod_timestamp: None,
    };

    // Ask for "myapp" universal — should be skipped since myapp has no arch binaries
    let result = build_universal_binary("myapp", &ub, &mut ctx, true);
    assert!(result.is_ok());

    let universals: Vec<_> = ctx
        .artifacts
        .by_kind(ArtifactKind::UniversalBinary)
        .into_iter()
        .filter(|a| a.target.as_deref() == Some("darwin-universal"))
        .collect();
    assert!(
        universals.is_empty(),
        "should not create universal for wrong crate"
    );
}

#[test]
fn test_universal_binary_artifact_has_correct_metadata() {
    use anodizer_core::config::UniversalBinaryConfig;
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = anodizer_core::config::Config::default();
    config.project_name = "myapp".to_string();
    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    register_binary(
        &mut ctx,
        "myapp",
        "aarch64-apple-darwin",
        std::path::PathBuf::from("target/aarch64-apple-darwin/release/myapp"),
    );
    register_binary(
        &mut ctx,
        "myapp",
        "x86_64-apple-darwin",
        std::path::PathBuf::from("target/x86_64-apple-darwin/release/myapp"),
    );

    let ub = UniversalBinaryConfig {
        id: None,
        name_template: None,
        replace: None,
        ids: None,
        hooks: None,
        mod_timestamp: None,
    };

    build_universal_binary("myapp", &ub, &mut ctx, true).unwrap();

    let universals: Vec<_> = ctx
        .artifacts
        .by_kind(ArtifactKind::UniversalBinary)
        .into_iter()
        .filter(|a| a.target.as_deref() == Some("darwin-universal"))
        .collect();
    assert_eq!(universals.len(), 1);
    let art = universals[0];
    assert_eq!(art.crate_name, "myapp");
    assert_eq!(
        art.metadata.get("universal").map(|s| s.as_str()),
        Some("true")
    );
    assert_eq!(
        art.metadata.get("binary").map(|s| s.as_str()),
        Some("myapp")
    );
}

/// Pins C-new-1: universal-binary metadata copy is the FULL `Extra` map of
/// the first source binary (the extra-map copy
/// `maps.Copy(extra, binaries[0].Extra)`), not a hardcoded whitelist. A
/// regression that re-introduces the old 4-key whitelist would silently
/// drop arbitrary metadata keys downstream stages emit.
#[test]
fn test_universal_binary_copies_full_extras_from_first_source() {
    use anodizer_core::artifact::Artifact;
    use anodizer_core::config::UniversalBinaryConfig;
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = anodizer_core::config::Config::default();
    config.project_name = "myapp".to_string();
    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    // arm64 (first source — its Extra map is the one copied) carries a
    // miscellaneous metadata key that the old whitelist would have dropped.
    let mut arm_meta = HashMap::new();
    arm_meta.insert("binary".to_string(), "myapp".to_string());
    arm_meta.insert("id".to_string(), "myapp".to_string());
    arm_meta.insert("DynamicallyLinked".to_string(), "true".to_string());
    arm_meta.insert("custom_future_key".to_string(), "preserved".to_string());
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: std::path::PathBuf::from("target/aarch64-apple-darwin/release/myapp"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: arm_meta,
        size: None,
    });
    // x86_64 source — also valid, but we only assert against arm64's keys.
    register_binary(
        &mut ctx,
        "myapp",
        "x86_64-apple-darwin",
        std::path::PathBuf::from("target/x86_64-apple-darwin/release/myapp"),
    );

    build_universal_binary("myapp", &UniversalBinaryConfig::default(), &mut ctx, true).unwrap();

    let universals: Vec<_> = ctx
        .artifacts
        .by_kind(ArtifactKind::UniversalBinary)
        .into_iter()
        .filter(|a| a.target.as_deref() == Some("darwin-universal"))
        .collect();
    assert_eq!(universals.len(), 1);
    let art = universals[0];
    // Inherited from arm64 (first source).
    assert_eq!(
        art.metadata.get("DynamicallyLinked").map(String::as_str),
        Some("true"),
        "DynamicallyLinked must be copied from first source binary"
    );
    assert_eq!(
        art.metadata.get("custom_future_key").map(String::as_str),
        Some("preserved"),
        "non-whitelisted metadata key must be preserved (no whitelist)"
    );
    // Universal-specific overrides — copied id is overridden only when ub.id is set.
    assert_eq!(
        art.metadata.get("universal").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        art.metadata.get("replaces").map(String::as_str),
        Some("false")
    );
}

/// Pins C-new-3: universal-binary id-only filter (no `binary`-key fallback).
/// A binary whose `id` does not match the universal binary's id list must
/// NOT be matched by its `binary`-key value.
#[test]
fn test_universal_binary_filter_id_only_no_binary_fallback() {
    use anodizer_core::artifact::Artifact;
    use anodizer_core::config::UniversalBinaryConfig;
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = anodizer_core::config::Config::default();
    config.project_name = "myapp".to_string();
    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    // Register binaries whose `binary` key matches the requested id "wanted"
    // but whose `id` key does NOT — the new id-only filter must reject these.
    for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
        let mut meta = HashMap::new();
        meta.insert("binary".to_string(), "wanted".to_string());
        meta.insert("id".to_string(), "different".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from(format!("target/{target}/release/wanted")),
            target: Some(target.to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta,
            size: None,
        });
    }

    let ub = UniversalBinaryConfig {
        id: Some("wanted".to_string()),
        ids: Some(vec!["wanted".to_string()]),
        ..Default::default()
    };

    build_universal_binary("myapp", &ub, &mut ctx, true).unwrap();

    // Filter rejects binary-key matches → no universal binary produced.
    let universals = ctx.artifacts.by_kind(ArtifactKind::UniversalBinary);
    assert!(
        universals.is_empty(),
        "id-only filter must not match via `binary` key: got {} universals",
        universals.len()
    );
}

/// Regression test for the universal-binary default name template —
/// default `name_template` is `{{ .ProjectName }}`, NOT the source binary
/// filename. Source binaries named `myapp-bin` with project_name `myapp`
/// must produce `myapp_darwin_all/myapp`, not `myapp_darwin_all/myapp-bin`.
#[test]
fn test_universal_binary_default_name_uses_project_name() {
    use anodizer_core::artifact::Artifact;
    use anodizer_core::config::UniversalBinaryConfig;
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = anodizer_core::config::Config::default();
    config.project_name = "myapp".to_string();
    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    // Register source binaries with a distinct on-disk filename
    // (`myapp-bin`) but the crate-matching `id` so the universal-binary
    // filter selects them. The old bug — defaulting to source filename —
    // would leak through as `myapp_darwin_all/myapp-bin`.
    for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"] {
        let path = std::path::PathBuf::from(format!("target/{target}/release/myapp-bin"));
        let mut meta = HashMap::new();
        meta.insert("binary".to_string(), "myapp-bin".to_string());
        meta.insert("id".to_string(), "myapp".to_string());
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path,
            target: Some(target.to_string()),
            crate_name: "myapp".to_string(),
            metadata: meta,
            size: None,
        });
    }

    let ub = UniversalBinaryConfig {
        id: None,
        name_template: None,
        replace: None,
        ids: None,
        hooks: None,
        mod_timestamp: None,
    };

    build_universal_binary("myapp", &ub, &mut ctx, true).unwrap();

    let universals: Vec<_> = ctx
        .artifacts
        .by_kind(ArtifactKind::UniversalBinary)
        .into_iter()
        .filter(|a| a.target.as_deref() == Some("darwin-universal"))
        .collect();
    assert_eq!(universals.len(), 1);
    let art = universals[0];
    let fname = art.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    assert_eq!(
        fname,
        "myapp",
        "default universal binary filename must render `{{{{ .ProjectName }}}}` (got `{}` — path {})",
        fname,
        art.path.display()
    );
    // `binary` metadata reflects the source filename, not the universal
    // output name.
    assert_eq!(
        art.metadata.get("binary").map(|s| s.as_str()),
        Some("myapp-bin")
    );
}

// ---- Build ignore tests ----

#[test]
fn test_is_target_ignored_matches() {
    let ignores = vec![BuildIgnore {
        os: "windows".to_string(),
        arch: "arm64".to_string(),
    }];
    // aarch64-pc-windows-msvc maps to os=windows, arch=arm64
    assert!(is_target_ignored("aarch64-pc-windows-msvc", &ignores));
}

#[test]
fn test_is_target_ignored_no_match() {
    let ignores = vec![BuildIgnore {
        os: "windows".to_string(),
        arch: "arm64".to_string(),
    }];
    // x86_64-unknown-linux-gnu maps to os=linux, arch=amd64
    assert!(!is_target_ignored("x86_64-unknown-linux-gnu", &ignores));
}

#[test]
fn test_is_target_ignored_empty_list() {
    assert!(!is_target_ignored("x86_64-unknown-linux-gnu", &[]));
}

#[test]
fn test_is_target_ignored_multiple_rules() {
    let ignores = vec![
        BuildIgnore {
            os: "windows".to_string(),
            arch: "arm64".to_string(),
        },
        BuildIgnore {
            os: "linux".to_string(),
            arch: "arm64".to_string(),
        },
    ];
    assert!(is_target_ignored("aarch64-unknown-linux-gnu", &ignores));
    assert!(!is_target_ignored("x86_64-unknown-linux-gnu", &ignores));
}

// ---- Build override tests ----

// ---- C-new-2: resolve_target_env glob-match tests ----

#[test]
fn test_resolve_target_env_exact_match() {
    // Exact target strings remain trivial-glob matches: backward compat.
    let log = test_logger();
    let env: HashMap<String, HashMap<String, String>> = HashMap::from([(
        "x86_64-unknown-linux-gnu".to_string(),
        HashMap::from([("CC".to_string(), "gcc".to_string())]),
    )]);
    let merged = resolve_target_env(Some(&env), "x86_64-unknown-linux-gnu", &log, false)
        .unwrap()
        .unwrap();
    assert_eq!(merged.get("CC").map(String::as_str), Some("gcc"));
}

#[test]
fn test_resolve_target_env_glob_match() {
    // Pins C-new-2: a glob-keyed env entry now matches its targets instead
    // of being silently ignored.
    let log = test_logger();
    let env: HashMap<String, HashMap<String, String>> = HashMap::from([(
        "*-linux-gnu".to_string(),
        HashMap::from([("CC".to_string(), "musl-gcc".to_string())]),
    )]);
    let merged = resolve_target_env(Some(&env), "x86_64-unknown-linux-gnu", &log, false)
        .unwrap()
        .unwrap();
    assert_eq!(merged.get("CC").map(String::as_str), Some("musl-gcc"));
}

#[test]
fn test_resolve_target_env_no_match_returns_none() {
    let log = test_logger();
    let env: HashMap<String, HashMap<String, String>> = HashMap::from([(
        "*-linux-gnu".to_string(),
        HashMap::from([("CC".to_string(), "musl-gcc".to_string())]),
    )]);
    let merged = resolve_target_env(Some(&env), "aarch64-apple-darwin", &log, false).unwrap();
    assert!(merged.is_none());
}

#[test]
fn test_resolve_target_env_merges_multiple_matches() {
    // When two patterns both match the target, both are merged.
    // Lexicographic key order: later (alphabetically-greater) wins.
    // "x86_64-unknown-linux-gnu" > "*-linux-gnu" so the exact-match value
    // overrides the glob value on the conflicting key.
    let log = test_logger();
    let env: HashMap<String, HashMap<String, String>> = HashMap::from([
        (
            "*-linux-gnu".to_string(),
            HashMap::from([
                ("CC".to_string(), "musl-gcc".to_string()),
                ("CFLAGS".to_string(), "-O2".to_string()),
            ]),
        ),
        (
            "x86_64-unknown-linux-gnu".to_string(),
            HashMap::from([("CC".to_string(), "gcc-12".to_string())]),
        ),
    ]);
    let merged = resolve_target_env(Some(&env), "x86_64-unknown-linux-gnu", &log, false)
        .unwrap()
        .unwrap();
    // From the exact-match entry (later in sort order), wins on CC.
    assert_eq!(merged.get("CC").map(String::as_str), Some("gcc-12"));
    // From the glob entry (no override), preserved.
    assert_eq!(merged.get("CFLAGS").map(String::as_str), Some("-O2"));
}

#[test]
fn test_per_target_build_env_reaches_only_its_targets_hook() {
    // End-to-end no-leak invariant: the per-target env that the planner feeds
    // into `BuildJob.build_env` is resolved by `resolve_target_env` for the
    // SPECIFIC (crate, target) being built, then layered into that job's build
    // hooks by `run_hooks`. Target A's hook must see A's env, target B's hook
    // must see B's env — never the other's. This covers the multi-target /
    // workspace-per-crate axis where each build resolves independently.
    use anodizer_core::config::{HookEntry, StructuredHook};
    use anodizer_core::hooks::{HookRunContext, run_hooks};
    use anodizer_core::template::TemplateVars;

    let log = test_logger();
    let env: HashMap<String, HashMap<String, String>> = HashMap::from([
        (
            "x86_64-unknown-linux-gnu".to_string(),
            HashMap::from([("TARGET_TAG".to_string(), "linux-amd64".to_string())]),
        ),
        (
            "aarch64-apple-darwin".to_string(),
            HashMap::from([("TARGET_TAG".to_string(), "darwin-arm64".to_string())]),
        ),
    ]);

    let dir = std::env::temp_dir().join(format!("anodizer-mt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let probe = |target: &str, out: &std::path::Path| {
        let resolved = resolve_target_env(Some(&env), target, &log, false)
            .unwrap()
            .unwrap_or_default();
        // sh -c mangles backslashes; feed it a forward-slash path so the redirect target resolves on Windows
        let out_str = out.display().to_string().replace('\\', "/");
        let hooks = vec![HookEntry::Structured(StructuredHook {
            cmd: format!("echo TARGET_TAG=$TARGET_TAG > {out_str}"),
            ..Default::default()
        })];
        let vars = TemplateVars::new();
        run_hooks(
            &hooks,
            "post-build",
            HookRunContext {
                dry_run: false,
                log: &log,
                template_vars: Some(&vars),
                build_env: Some(&resolved),
                extra_env: None,
            },
        )
        .unwrap();
        std::fs::read_to_string(out).unwrap()
    };

    let a_out = dir.join("a.txt");
    let b_out = dir.join("b.txt");
    let a = probe("x86_64-unknown-linux-gnu", &a_out);
    let b = probe("aarch64-apple-darwin", &b_out);

    assert!(
        a.contains("TARGET_TAG=linux-amd64") && !a.contains("darwin-arm64"),
        "linux target's hook must see only linux env; got: {a:?}"
    );
    assert!(
        b.contains("TARGET_TAG=darwin-arm64") && !b.contains("linux-amd64"),
        "darwin target's hook must see only darwin env; got: {b:?}"
    );
    let _ = std::fs::remove_file(&a_out);
    let _ = std::fs::remove_file(&b_out);
}

#[test]
fn test_resolve_target_env_invalid_glob_strict_errors() {
    let log = test_logger();
    let env: HashMap<String, HashMap<String, String>> = HashMap::from([(
        "[invalid".to_string(),
        HashMap::from([("X".to_string(), "y".to_string())]),
    )]);
    let err = resolve_target_env(Some(&env), "x86_64-unknown-linux-gnu", &log, true)
        .expect_err("strict mode must reject invalid glob");
    assert!(
        err.to_string().contains("invalid glob pattern"),
        "unexpected error: {err}"
    );
}

#[test]
fn test_resolve_target_env_none_input_returns_none() {
    let log = test_logger();
    assert!(
        resolve_target_env(None, "x86_64-unknown-linux-gnu", &log, false)
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_resolve_target_env_empty_map_returns_none() {
    // Coverage gap: env: {} (Some empty map) must behave like env: None.
    let log = test_logger();
    let env: HashMap<String, HashMap<String, String>> = HashMap::new();
    assert!(
        resolve_target_env(Some(&env), "x86_64-unknown-linux-gnu", &log, false)
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_resolve_target_env_alphabetic_not_specific() {
    // Pins the documented merge contract (W1): when two glob keys both
    // legitimately apply, ASCII order — not pattern specificity — decides.
    // Here `*-linux-*` and `*-linux-gnu` both match. ASCII: `*` is 0x2A,
    // `g` is 0x67; so `*-linux-*` sorts before `*-linux-gnu`. The later
    // (alphabetically-greater) key `*-linux-gnu` wins on conflict.
    let log = test_logger();
    let env: HashMap<String, HashMap<String, String>> = HashMap::from([
        (
            "*-linux-*".to_string(),
            HashMap::from([("CC".to_string(), "less-specific".to_string())]),
        ),
        (
            "*-linux-gnu".to_string(),
            HashMap::from([("CC".to_string(), "more-specific".to_string())]),
        ),
    ]);
    let merged = resolve_target_env(Some(&env), "x86_64-unknown-linux-gnu", &log, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        merged.get("CC").map(String::as_str),
        Some("more-specific"),
        "ASCII-order winner: `*-linux-gnu` sorts after `*-linux-*` and overrides on conflict"
    );
}

#[test]
fn test_find_matching_override_glob_match() {
    let overrides = vec![BuildOverride {
        targets: vec!["x86_64-*".to_string()],
        features: Some(vec!["simd".to_string()]),
        ..Default::default()
    }];
    let log = test_logger();
    let result =
        find_matching_override("x86_64-unknown-linux-gnu", &overrides, &log, false).unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap().features, Some(vec!["simd".to_string()]));
}

#[test]
fn test_find_matching_override_no_match() {
    let log = test_logger();
    let overrides = vec![BuildOverride {
        targets: vec!["x86_64-*".to_string()],
        features: Some(vec!["simd".to_string()]),
        ..Default::default()
    }];
    let result = find_matching_override("aarch64-apple-darwin", &overrides, &log, false).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_find_matching_override_wildcard_in_middle() {
    let log = test_logger();
    let overrides = vec![BuildOverride {
        targets: vec!["*-apple-darwin".to_string()],
        features: Some(vec!["metal".to_string()]),
        ..Default::default()
    }];
    let result = find_matching_override("aarch64-apple-darwin", &overrides, &log, false).unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap().features, Some(vec!["metal".to_string()]));
}

#[test]
fn test_find_matching_override_empty_list() {
    let log = test_logger();
    let result = find_matching_override("x86_64-unknown-linux-gnu", &[], &log, false).unwrap();
    assert!(result.is_none());
}

#[test]
fn test_find_matching_override_returns_first_match() {
    let log = test_logger();
    let overrides = vec![
        BuildOverride {
            targets: vec!["x86_64-*".to_string()],
            flags: Some(vec!["--release".to_string()]),
            ..Default::default()
        },
        BuildOverride {
            targets: vec!["*-linux-*".to_string()],
            flags: Some(vec!["--opt-level=3".to_string()]),
            ..Default::default()
        },
    ];
    let result =
        find_matching_override("x86_64-unknown-linux-gnu", &overrides, &log, false).unwrap();
    assert!(result.is_some());
    assert_eq!(result.unwrap().flags, Some(vec!["--release".to_string()]));
}

#[test]
fn test_override_env_actually_overrides_existing() {
    // Simulate the merge logic used in BuildStage::run:
    // target_env starts with an existing key, override should replace it.
    let mut target_env: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    target_env.insert("CC".into(), "gcc".into());
    target_env.insert("EXISTING".into(), "keep".into());

    let override_env: std::collections::HashMap<String, String> = [
        ("CC".into(), "clang".into()),
        ("NEW_VAR".into(), "added".into()),
    ]
    .into_iter()
    .collect();

    // This mirrors the fixed merge logic (insert, not or_insert_with)
    for (k, v) in &override_env {
        target_env.insert(k.clone(), v.clone());
    }

    assert_eq!(
        target_env.get("CC").unwrap(),
        "clang",
        "override should replace existing CC value"
    );
    assert_eq!(
        target_env.get("EXISTING").unwrap(),
        "keep",
        "non-overridden key should be preserved"
    );
    assert_eq!(
        target_env.get("NEW_VAR").unwrap(),
        "added",
        "new override key should be inserted"
    );
}

#[test]
fn test_find_matching_override_invalid_glob_warns() {
    // An invalid glob pattern like "[unclosed" should not panic,
    // and the function should skip it gracefully.
    let log = test_logger();
    let overrides = vec![BuildOverride {
        targets: vec!["[unclosed".to_string()],
        flags: Some(vec!["--bad".to_string()]),
        ..Default::default()
    }];
    let result =
        find_matching_override("x86_64-unknown-linux-gnu", &overrides, &log, false).unwrap();
    assert!(result.is_none(), "invalid glob should not match anything");
}

// ---- Fix 5: cross_tool override test ----

#[test]
fn test_build_command_with_cross_tool() {
    let env = HashMap::new();
    let flags = vec!["--release".to_string()];
    let cmd = build_command(
        "test-crate",
        &BuildContext {
            crate_path: ".",
            target: "x86_64-unknown-linux-gnu",
            strategy: &CrossStrategy::Auto,
            flags: &flags,
            features: &[],
            no_default_features: false,
            env: &env,
            cross_tool: Some("/usr/bin/my-cross"),
            command_override: None,
        },
    );
    assert_eq!(cmd.program, "/usr/bin/my-cross");
    assert!(cmd.args.contains(&"build".to_string()));
}

// ---- Fix 5: DEFAULT_TARGETS const test ----

#[test]
fn test_default_targets_has_six_entries() {
    assert_eq!(DEFAULT_TARGETS.len(), 6);
    assert!(DEFAULT_TARGETS.contains(&"x86_64-unknown-linux-gnu"));
    assert!(DEFAULT_TARGETS.contains(&"x86_64-apple-darwin"));
    assert!(DEFAULT_TARGETS.contains(&"aarch64-apple-darwin"));
    assert!(DEFAULT_TARGETS.contains(&"x86_64-pc-windows-msvc"));
    assert!(DEFAULT_TARGETS.contains(&"aarch64-pc-windows-msvc"));
    assert!(DEFAULT_TARGETS.contains(&"aarch64-unknown-linux-gnu"));
}

// ---- cargo_target_dir tests ----
//
// Drives the env-fallback branches via injected `MapEnvSource` — no
// `unsafe set_var`, no serial-test contention with sibling crates that
// read `CARGO_TARGET_DIR` from the live process env.

use super::workspace::cargo_target_dir_with_env;
use anodizer_core::MapEnvSource;

#[test]
fn test_cargo_target_dir_default() {
    let env = MapEnvSource::new();
    assert_eq!(
        cargo_target_dir_with_env(None, &env),
        PathBuf::from("target")
    );
}

#[test]
fn test_cargo_target_dir_from_env() {
    let env = MapEnvSource::new().with("CARGO_TARGET_DIR", "/tmp/my-target");
    assert_eq!(
        cargo_target_dir_with_env(None, &env),
        PathBuf::from("/tmp/my-target")
    );
}

#[test]
fn test_cargo_target_dir_empty_falls_through() {
    let env = MapEnvSource::new().with("CARGO_TARGET_DIR", "");
    assert_eq!(
        cargo_target_dir_with_env(None, &env),
        PathBuf::from("target")
    );
}

#[test]
fn test_cargo_build_target_dir_fallback() {
    let env = MapEnvSource::new().with("CARGO_BUILD_TARGET_DIR", "/tmp/build-target");
    assert_eq!(
        cargo_target_dir_with_env(None, &env),
        PathBuf::from("/tmp/build-target")
    );
}

#[test]
fn test_cargo_target_dir_takes_precedence() {
    let env = MapEnvSource::new()
        .with("CARGO_TARGET_DIR", "/tmp/primary")
        .with("CARGO_BUILD_TARGET_DIR", "/tmp/secondary");
    assert_eq!(
        cargo_target_dir_with_env(None, &env),
        PathBuf::from("/tmp/primary")
    );
}

#[test]
fn test_cargo_target_dir_from_build_env() {
    let env_src = MapEnvSource::new();
    let mut env = HashMap::new();
    env.insert(
        "CARGO_TARGET_DIR".to_string(),
        "/tmp/build-env-target".to_string(),
    );
    assert_eq!(
        cargo_target_dir_with_env(Some(&env), &env_src),
        PathBuf::from("/tmp/build-env-target")
    );
}

#[test]
fn test_cargo_target_dir_build_env_overrides_process_env() {
    let env_src = MapEnvSource::new().with("CARGO_TARGET_DIR", "/tmp/process-env");
    let mut env = HashMap::new();
    env.insert("CARGO_TARGET_DIR".to_string(), "/tmp/build-env".to_string());
    assert_eq!(
        cargo_target_dir_with_env(Some(&env), &env_src),
        PathBuf::from("/tmp/build-env")
    );
}

// ---- Fix 5: resolve_build_program tests ----

#[test]
fn test_resolve_build_program_auto() {
    let (prog, sub) = resolve_build_program(&CrossStrategy::Auto, None, None, None);
    // Auto resolves at runtime — at minimum it falls back to cargo
    assert!(
        prog == "cargo" || prog == "cross",
        "Auto should resolve to cargo or cross, got: {prog}"
    );
    assert!(sub == "build" || sub == "zigbuild");
}

#[test]
fn test_resolve_build_program_zigbuild() {
    let (prog, sub) = resolve_build_program(&CrossStrategy::Zigbuild, None, None, None);
    assert_eq!(prog, "cargo");
    assert_eq!(sub, "zigbuild");
}

#[test]
fn test_resolve_build_program_cross() {
    let (prog, sub) = resolve_build_program(&CrossStrategy::Cross, None, None, None);
    assert_eq!(prog, "cross");
    assert_eq!(sub, "build");
}

#[test]
fn test_resolve_build_program_cross_tool_overrides() {
    let (prog, sub) = resolve_build_program(
        &CrossStrategy::Zigbuild,
        Some("/usr/bin/custom"),
        None,
        None,
    );
    assert_eq!(prog, "/usr/bin/custom");
    assert_eq!(sub, "build");
}

#[test]
fn test_resolve_build_program_auto_native() {
    // Target == host resolves to plain cargo, except on a linux-gnu host
    // with cargo-zigbuild installed, where the hermetic-glibc routing
    // sends even the host triple through zigbuild.
    let host = anodizer_core::partial::detect_host_target().unwrap_or_default();
    if host.is_empty() {
        return;
    }
    let (prog, sub) = resolve_build_program(&CrossStrategy::Auto, None, None, Some(&host));
    assert_eq!(prog, "cargo", "native target should use the cargo binary");
    if is_linux_gnu(&host) && zigbuild_available() {
        assert_eq!(sub, "zigbuild", "linux-gnu host with zigbuild on PATH");
    } else {
        assert_eq!(sub, "build", "native target should use plain build");
    }
}

#[test]
fn test_same_apple_family() {
    assert!(same_apple_family(
        "aarch64-apple-darwin",
        "x86_64-apple-darwin"
    ));
    assert!(same_apple_family(
        "x86_64-apple-darwin",
        "aarch64-apple-darwin"
    ));
    assert!(same_apple_family(
        "aarch64-apple-darwin",
        "aarch64-apple-ios"
    ));
    assert!(!same_apple_family(
        "x86_64-unknown-linux-gnu",
        "x86_64-apple-darwin"
    ));
    assert!(!same_apple_family(
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc"
    ));
}

#[test]
fn test_same_windows_family() {
    assert!(same_windows_family(
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc"
    ));
    assert!(same_windows_family(
        "x86_64-pc-windows-gnu",
        "x86_64-pc-windows-msvc"
    ));
    assert!(!same_windows_family(
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc"
    ));
    assert!(!same_windows_family(
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc"
    ));
}

#[test]
fn test_detect_cross_strategy_for_target_apple_cross_arch() {
    // On any apple host, building a different apple arch should still
    // use cargo (clang handles apple targets universally) — even with
    // zigbuild installed (zig mis-links large apple framework lines).
    let strategy = detect_cross_strategy_for_target_impl(
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        true,
        true,
    );
    assert_eq!(strategy, CrossStrategy::Cargo);

    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        true,
        true,
    );
    assert_eq!(strategy, CrossStrategy::Cargo);
}

#[test]
fn test_detect_cross_strategy_for_target_linux_cross_arch_uses_auto() {
    // On a Linux host, building a different Linux arch does NOT get the
    // same-family exemption — it requires cross tooling (multilib gcc
    // or cross/zigbuild).
    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        true,
        false,
    );
    assert_eq!(strategy, CrossStrategy::Zigbuild);

    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        false,
        true,
    );
    assert_eq!(strategy, CrossStrategy::Cross);
}

#[test]
fn test_detect_cross_strategy_host_linux_gnu_prefers_zigbuild() {
    // Regression: the v0.7.0 x86_64-linux release binary required GLIBC_2.39
    // because the host-triple build short-circuited to native cargo and
    // linked the ubuntu-24.04 runner's ambient glibc. With zigbuild
    // available, even the exact-host linux-gnu target must go through
    // zigbuild so the glibc floor stays hermetic.
    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
        true,
        false,
    );
    assert_eq!(strategy, CrossStrategy::Zigbuild);

    // Same for an aarch64 host building its own triple.
    let strategy = detect_cross_strategy_for_target_impl(
        "aarch64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        true,
        false,
    );
    assert_eq!(strategy, CrossStrategy::Zigbuild);

    // glibc-pinned target spelling (`<triple>.<ver>`) also routes through
    // zigbuild — it is the only tool that understands the suffix.
    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu.2.17",
        true,
        false,
    );
    assert_eq!(strategy, CrossStrategy::Zigbuild);
}

#[test]
fn cross_gnu_cargo_fallback_warning_fires_for_cross_arch_plain_cargo() {
    // The nightly-runner failure shape: aarch64 gnu on an x86_64 host with
    // neither zigbuild nor cross installed resolves to plain cargo, which
    // needs aarch64-linux-gnu-gcc the runner doesn't have. The warning must
    // name the missing cross cc and the hermetic alternative.
    let msg = crate::command::cross_gnu_cargo_fallback_warning(
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        &CrossStrategy::Cargo,
    )
    .expect("cross-arch gnu target on plain cargo must warn");
    assert!(msg.contains("aarch64-linux-gnu-gcc"), "msg: {msg}");
    assert!(msg.contains("cargo-zigbuild"), "msg: {msg}");
}

#[test]
fn cross_gnu_cargo_fallback_warning_silent_when_not_plain_cargo() {
    for resolved in [CrossStrategy::Zigbuild, CrossStrategy::Cross] {
        assert_eq!(
            crate::command::cross_gnu_cargo_fallback_warning(
                "x86_64-unknown-linux-gnu",
                "aarch64-unknown-linux-gnu",
                &resolved,
            ),
            None,
        );
    }
}

#[test]
fn cross_gnu_cargo_fallback_warning_silent_for_native_and_non_gnu() {
    // Host triple itself — native build, no cross cc needed.
    assert_eq!(
        crate::command::cross_gnu_cargo_fallback_warning(
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
        ),
        None,
    );
    // Glibc-pinned spelling of the host triple is still a native build.
    assert_eq!(
        crate::command::cross_gnu_cargo_fallback_warning(
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu.2.17",
            &CrossStrategy::Cargo,
        ),
        None,
    );
    // musl links libc statically; the gnu cross-cc concern doesn't apply.
    assert_eq!(
        crate::command::cross_gnu_cargo_fallback_warning(
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-musl",
            &CrossStrategy::Cargo,
        ),
        None,
    );
    // Unknown host (detection failed) — can't judge, stay quiet.
    assert_eq!(
        crate::command::cross_gnu_cargo_fallback_warning(
            "",
            "aarch64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
        ),
        None,
    );
}

#[test]
fn resolved_strategy_for_target_passes_explicit_strategy_through() {
    // Only Auto consults the host/tool probes; explicit strategies are
    // taken verbatim regardless of target.
    for explicit in [
        CrossStrategy::Cargo,
        CrossStrategy::Cross,
        CrossStrategy::Zigbuild,
    ] {
        assert_eq!(
            crate::command::resolved_strategy_for_target(&explicit, "aarch64-unknown-linux-gnu"),
            explicit,
        );
    }
}

#[test]
fn test_detect_cross_strategy_host_linux_gnu_without_zigbuild_uses_cargo() {
    // Local-dev fallback: no cargo-zigbuild on PATH → native cargo for the
    // host triple, even when `cross` is installed (the host needs no
    // container build, and cross would change the produced glibc anyway).
    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
        false,
        false,
    );
    assert_eq!(strategy, CrossStrategy::Cargo);

    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
        false,
        true,
    );
    assert_eq!(strategy, CrossStrategy::Cargo);
}

#[test]
fn test_detect_cross_strategy_musl_routes_through_zigbuild() {
    // musl Linux triples route through zigbuild whenever it is available,
    // mirroring the linux-gnu rule. The apk package ships a musl binary, and
    // on the glibc CI release host a musl build is always a cross-libc compile
    // — plain cargo dies in cc-rs (no musl cross C toolchain). cargo-zigbuild
    // bundles musl headers for every arch, so it cross-compiles musl cleanly.
    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-musl",
        true,
        false,
    );
    assert_eq!(strategy, CrossStrategy::Zigbuild);

    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-musl",
        true,
        false,
    );
    assert_eq!(strategy, CrossStrategy::Zigbuild);

    // Even when the target is the exact host triple, musl routes through
    // zigbuild (uniform with the linux-gnu rule); no native-cargo shortcut.
    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-unknown-linux-musl",
        "x86_64-unknown-linux-musl",
        true,
        true,
    );
    assert_eq!(strategy, CrossStrategy::Zigbuild);
}

#[test]
fn test_detect_cross_strategy_linux_gnu_rule_beats_apple_host_shortcut() {
    // An apple host crossing to linux-gnu must hit the hermetic-glibc rule,
    // not any same-host shortcut: zigbuild when available.
    let strategy = detect_cross_strategy_for_target_impl(
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        true,
        false,
    );
    assert_eq!(strategy, CrossStrategy::Zigbuild);
}

#[test]
fn test_detect_cross_strategy_host_linux_gnu_zigbuild_beats_cross() {
    // With BOTH tools installed, the host linux-gnu triple picks zigbuild
    // (hermetic glibc), never cross.
    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-linux-gnu",
        true,
        true,
    );
    assert_eq!(strategy, CrossStrategy::Zigbuild);
}

#[test]
fn test_detect_cross_strategy_host_windows_keeps_cargo() {
    let strategy = detect_cross_strategy_for_target_impl(
        "x86_64-pc-windows-msvc",
        "x86_64-pc-windows-msvc",
        true,
        true,
    );
    assert_eq!(strategy, CrossStrategy::Cargo);
}

// ---- Fix 5: resolve_reproducible_epoch tests ----

use super::workspace::resolve_reproducible_epoch_with_env;

#[test]
fn test_resolve_reproducible_epoch_from_timestamp() {
    // No SOURCE_DATE_EPOCH override — exercise the commit_timestamp
    // fallback path through the injected env source.
    let env = MapEnvSource::new();
    let epoch = resolve_reproducible_epoch_with_env("1700000000", &env);
    assert_eq!(epoch, Some(1700000000));
}

#[test]
fn test_resolve_reproducible_epoch_invalid_timestamp() {
    let env = MapEnvSource::new();
    let epoch = resolve_reproducible_epoch_with_env("not-a-number", &env);
    assert_eq!(epoch, None);
}

#[test]
fn test_resolve_reproducible_epoch_uses_source_date_epoch_when_set() {
    let env = MapEnvSource::new().with("SOURCE_DATE_EPOCH", "1234567890");
    let epoch = resolve_reproducible_epoch_with_env("1700000000", &env);
    assert_eq!(epoch, Some(1234567890));
}

// ---- Fix 5: config parsing with hooks test ----

#[test]
fn test_build_config_with_hooks() {
    use anodizer_core::config::Config;

    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        hooks:
          pre:
            - "echo pre-build"
          post:
            - "echo post-build"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    let hooks = build.hooks.as_ref().unwrap();
    assert_eq!(hooks.pre.as_ref().unwrap().len(), 1);
    assert_eq!(hooks.post.as_ref().unwrap().len(), 1);
}

// ---- Parity gap tests ----

#[test]
fn test_strip_glibc_suffix_with_version() {
    let (stripped, has_suffix) = strip_glibc_suffix("aarch64-unknown-linux-gnu.2.17");
    assert_eq!(stripped, "aarch64-unknown-linux-gnu");
    assert!(has_suffix);
}

#[test]
fn test_strip_glibc_suffix_no_suffix() {
    let (stripped, has_suffix) = strip_glibc_suffix("aarch64-unknown-linux-gnu");
    assert_eq!(stripped, "aarch64-unknown-linux-gnu");
    assert!(!has_suffix);
}

#[test]
fn test_strip_glibc_suffix_musl_version() {
    let (stripped, has_suffix) = strip_glibc_suffix("x86_64-unknown-linux-musl.1.1");
    assert_eq!(stripped, "x86_64-unknown-linux-musl");
    assert!(has_suffix);
}

#[test]
fn test_strip_glibc_suffix_windows_no_change() {
    let (stripped, has_suffix) = strip_glibc_suffix("x86_64-pc-windows-msvc");
    assert_eq!(stripped, "x86_64-pc-windows-msvc");
    assert!(!has_suffix);
}

#[test]
fn test_target_for_validation_strips_suffix() {
    let t = target_for_validation("aarch64-unknown-linux-gnu.2.17");
    assert_eq!(t, "aarch64-unknown-linux-gnu");
}

#[test]
fn test_target_for_validation_no_suffix() {
    let t = target_for_validation("x86_64-unknown-linux-gnu");
    assert_eq!(t, "x86_64-unknown-linux-gnu");
}

#[test]
fn test_is_dynamically_linked_nonexistent() {
    // A genuinely-absent path is `Ok(false)` (not our concern), never an error.
    let tmp = tempfile::tempdir().unwrap();
    let absent = tmp.path().join("does_not_exist");
    assert!(!is_dynamically_linked(&absent).unwrap());
}

#[test]
fn test_is_dynamically_linked_non_elf() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("not_elf");
    std::fs::write(&path, b"not an elf file").unwrap();
    assert!(!is_dynamically_linked(&path).unwrap());
}

#[test]
fn test_is_dynamically_linked_unreadable_is_error() {
    // A path that exists but cannot be read as a file (a directory: open
    // succeeds, read yields EISDIR) must surface an error, not a silent
    // `false` that would mask a build artifact anodizer cannot inspect.
    let tmp = tempfile::tempdir().unwrap();
    assert!(is_dynamically_linked(tmp.path()).is_err());
}

#[test]
fn test_check_workspace_package_no_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_toml = tmp.path().join("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let result = check_workspace_package(tmp.path().to_str().unwrap(), &[]);
    assert!(result.is_ok());
}

#[test]
fn test_check_workspace_package_workspace_without_package_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_toml = tmp.path().join("Cargo.toml");
    std::fs::write(
        &cargo_toml,
        "[workspace]\nmembers = [\"crates/a\", \"crates/b\"]\n",
    )
    .unwrap();
    let result = check_workspace_package(tmp.path().to_str().unwrap(), &["--release".to_string()]);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("--package=<name>"));
}

#[test]
fn test_check_workspace_package_workspace_with_package_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_toml = tmp.path().join("Cargo.toml");
    std::fs::write(&cargo_toml, "[workspace]\nmembers = [\"crates/a\"]\n").unwrap();
    let result = check_workspace_package(
        tmp.path().to_str().unwrap(),
        &["--release".to_string(), "--package=myapp".to_string()],
    );
    assert!(result.is_ok());
}

#[test]
fn test_check_workspace_package_workspace_with_p_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let cargo_toml = tmp.path().join("Cargo.toml");
    std::fs::write(&cargo_toml, "[workspace]\nmembers = [\"crates/a\"]\n").unwrap();
    let result = check_workspace_package(
        tmp.path().to_str().unwrap(),
        &[
            "--release".to_string(),
            "-p".to_string(),
            "myapp".to_string(),
        ],
    );
    assert!(result.is_ok());
}

#[test]
fn test_duplicate_build_id_validation() {
    use anodizer_core::config::{BuildConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![
            BuildConfig {
                id: Some("dup".to_string()),
                binary: Some("myapp".to_string()),
                targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                ..Default::default()
            },
            BuildConfig {
                id: Some("dup".to_string()),
                binary: Some("myapp2".to_string()),
                targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                ..Default::default()
            },
        ]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    let stage = BuildStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("found 2 builds with the ID 'dup'"),
        "expected duplicate ID error, got: {err}"
    );
}

#[test]
fn test_invalid_target_errors() {
    use anodizer_core::config::{BuildConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![BuildConfig {
            binary: Some("myapp".to_string()),
            targets: Some(vec!["this-is-not-a-valid-triple".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    let stage = BuildStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err(), "invalid target should error");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("not in the known targets list"),
        "expected known targets error, got: {err}"
    );
}

#[test]
fn test_skip_build_with_string_or_bool() {
    use anodizer_core::config::{BuildConfig, Config, CrateConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![BuildConfig {
            binary: Some("myapp".to_string()),
            skip: Some(StringOrBool::Bool(true)),
            targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    let stage = BuildStage;
    assert!(stage.run(&mut ctx).is_ok());
    // No artifacts should be registered since the build was skipped
    let binaries = ctx.artifacts.by_kind(ArtifactKind::Binary);
    assert!(
        binaries.is_empty(),
        "skipped build should produce no artifacts"
    );
}

/// Regression: a library-only crate (no `src/main.rs`, no `[[bin]]`)
/// that inherited a `defaults.builds:` template must NOT trigger a
/// `cargo build --bin <library-name>` invocation. Before the
/// `crate_has_binary_target` guard inside the per-build loop, the
/// inherited template (with `binary:` left None) would fall back to
/// the crate name and `cargo build --bin <library-name>` would fail
/// with `no bin target named '<library-name>'`.
#[test]
fn test_library_crate_with_inherited_builds_skipped() {
    use anodizer_core::config::{BuildConfig, Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use std::fs;

    // Build a fake library-only crate on disk: a Cargo.toml with a
    // [lib] section, no [[bin]], and no src/main.rs.
    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("mylib");
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::write(
        crate_dir.join("Cargo.toml"),
        r#"
[package]
name = "mylib"
version = "0.1.0"
edition = "2021"

[lib]
"#,
    )
    .unwrap();
    fs::write(crate_dir.join("src/lib.rs"), "// library crate\n").unwrap();

    // Sanity: confirm the helper agrees there's no binary target.
    assert!(!crate_has_binary_target(crate_dir.to_str().unwrap()));

    // Simulate the post-`apply_defaults` shape: the crate's `builds`
    // field carries an inherited template with NO `binary:` set.
    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "mylib".to_string(),
        path: crate_dir.to_string_lossy().into_owned(),
        tag_template: "v{{ .Version }}".to_string(),
        builds: Some(vec![BuildConfig {
            // Inherited from defaults.builds — binary intentionally None.
            binary: None,
            targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
            ..Default::default()
        }]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    let stage = BuildStage;
    let result = stage.run(&mut ctx);
    assert!(
        result.is_ok(),
        "library crate with inherited defaults.builds should not error, got: {result:?}"
    );

    // No binary artifacts should be registered: the per-build loop
    // must skip when `build.binary` is None and the crate has no
    // binary target on disk.
    let binaries = ctx.artifacts.by_kind(ArtifactKind::Binary);
    assert!(
        binaries.is_empty(),
        "library crate must not produce any binary artifacts; got {} entries",
        binaries.len()
    );
}

/// Regression (cfgd-core shape): a library crate that ALSO carries helper
/// binaries whose names do NOT match the crate (e.g. `src/bin/gen.rs` renamed
/// via `[[bin]]` to `mylib-gen`). `crate_has_binary_target` is true (it has
/// bins), so the old guard let the synthesized default build fall through to
/// `cargo build --bin mylib` — which fails with `no bin target named 'mylib'`
/// and sank every determinism leg. The build planner must instead recognize
/// there is no binary named after the crate and skip the default build.
#[test]
fn test_library_crate_with_renamed_helper_bins_skipped() {
    use anodizer_core::config::{Config, CrateConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("mylib");
    fs::create_dir_all(crate_dir.join("src/bin")).unwrap();
    fs::write(
        crate_dir.join("Cargo.toml"),
        r#"
[package]
name = "mylib"
version = "0.1.0"
edition = "2021"

[lib]

[[bin]]
name = "mylib-gen"
path = "src/bin/gen.rs"
"#,
    )
    .unwrap();
    fs::write(crate_dir.join("src/lib.rs"), "// library crate\n").unwrap();
    fs::write(crate_dir.join("src/bin/gen.rs"), "fn main() {}\n").unwrap();

    let crate_path = crate_dir.to_str().unwrap();
    // The crate HAS a binary target ...
    assert!(crate_has_binary_target(crate_path));
    // ... but none named after the crate, so no default build may be synthesized ...
    assert!(!crate_declares_bin(crate_path, "mylib"));
    // ... while the renamed helper IS recognized as a real target.
    assert!(crate_declares_bin(crate_path, "mylib-gen"));

    // End-to-end: no `builds:` configured → synthesis path → must skip, not
    // produce a phantom `--bin mylib` build that cargo would reject.
    let mut config = Config::default();
    config.project_name = "test".to_string();
    config.crates.push(CrateConfig {
        name: "mylib".to_string(),
        path: crate_path.to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);
    let result = BuildStage.run(&mut ctx);
    assert!(
        result.is_ok(),
        "library crate with renamed helper bins must not error, got: {result:?}"
    );
    assert!(
        ctx.artifacts.by_kind(ArtifactKind::Binary).is_empty(),
        "no default `--bin <crate>` build should be synthesized for a crate with no bin named after it"
    );
}

/// Positive control: a normal binary crate (`src/main.rs`, package name ==
/// crate name) still resolves a bin named after the crate, and an explicit
/// `[[bin]] name = "<crate>"` does too — these must keep building.
#[test]
fn test_crate_declares_bin_positive_cases() {
    use std::fs;

    // src/main.rs → bin named after the package.
    let tmp = tempfile::tempdir().unwrap();
    let main_dir = tmp.path().join("app");
    fs::create_dir_all(main_dir.join("src")).unwrap();
    fs::write(
        main_dir.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(main_dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    assert!(crate_declares_bin(main_dir.to_str().unwrap(), "app"));
    assert!(!crate_declares_bin(main_dir.to_str().unwrap(), "other"));

    // Explicit [[bin]] name == crate.
    let bin_dir = tmp.path().join("app2");
    fs::create_dir_all(bin_dir.join("src")).unwrap();
    fs::write(
        bin_dir.join("Cargo.toml"),
        "[package]\nname = \"app2\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"app2\"\npath = \"src/entry.rs\"\n",
    )
    .unwrap();
    fs::write(bin_dir.join("src/entry.rs"), "fn main() {}\n").unwrap();
    assert!(crate_declares_bin(bin_dir.to_str().unwrap(), "app2"));

    // Auto-discovered src/bin/<crate>.rs (no [[bin]] re-paths it) → cargo
    // synthesizes a bin named after the file stem, which equals the crate.
    let auto_dir = tmp.path().join("app3");
    fs::create_dir_all(auto_dir.join("src/bin")).unwrap();
    fs::write(
        auto_dir.join("Cargo.toml"),
        "[package]\nname = \"app3\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\n",
    )
    .unwrap();
    fs::write(auto_dir.join("src/lib.rs"), "// library\n").unwrap();
    fs::write(auto_dir.join("src/bin/app3.rs"), "fn main() {}\n").unwrap();
    assert!(crate_declares_bin(auto_dir.to_str().unwrap(), "app3"));
}

/// Hardening (branch 3): an auto-discoverable `src/bin/<crate>.rs` file is
/// NOT a bin named after the crate when an explicit `[[bin]]` re-paths that
/// same file to a *different* name — cargo then builds `renamed`, never
/// `<crate>`. Synthesizing `--bin <crate>` here would fail at build time, so
/// `crate_declares_bin` must return false for the crate name and true for the
/// reclaimed name.
#[test]
fn test_crate_declares_bin_reclaimed_src_bin_stem() {
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("reclaimed");
    fs::create_dir_all(crate_dir.join("src/bin")).unwrap();
    fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"reclaimed\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\n\n[[bin]]\nname = \"renamed\"\npath = \"src/bin/reclaimed.rs\"\n",
    )
    .unwrap();
    fs::write(crate_dir.join("src/lib.rs"), "// library\n").unwrap();
    fs::write(crate_dir.join("src/bin/reclaimed.rs"), "fn main() {}\n").unwrap();

    assert!(
        !crate_declares_bin(crate_dir.to_str().unwrap(), "reclaimed"),
        "src/bin/reclaimed.rs re-pathed to a differently-named [[bin]] must not resolve --bin reclaimed"
    );
    assert!(
        crate_declares_bin(crate_dir.to_str().unwrap(), "renamed"),
        "the explicit [[bin]] name must still resolve"
    );
}

/// Divergence: a `src/main.rs` crate whose `[package].name` differs from the
/// directory / crate-config name resolves a bin named after the *package*,
/// not the directory. `crate_declares_bin` keys off the package name so a
/// crate-config name that doesn't match the package must return false.
#[test]
fn test_crate_declares_bin_package_name_divergence() {
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("dir-name");
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"pkg-name\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(crate_dir.join("src/main.rs"), "fn main() {}\n").unwrap();

    assert!(crate_declares_bin(crate_dir.to_str().unwrap(), "pkg-name"));
    assert!(
        !crate_declares_bin(crate_dir.to_str().unwrap(), "dir-name"),
        "main.rs resolves a bin named after [package].name, not the directory"
    );
}

/// Regression: a crate that has no `src/main.rs` and no `[[bin]]`
/// section but DOES have `src/bin/<name>.rs` files is still a
/// binary crate (cargo auto-discovers each file as a bin target).
/// Before the third probe, this layout was misclassified as
/// library-only and the build was skipped.
#[test]
fn test_crate_has_binary_target_detects_src_bin_layout() {
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("multi-bin");
    fs::create_dir_all(crate_dir.join("src/bin")).unwrap();
    fs::write(
        crate_dir.join("Cargo.toml"),
        r#"
[package]
name = "multi-bin"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    // No src/main.rs, no [[bin]], just src/bin/foo.rs.
    fs::write(crate_dir.join("src/bin/foo.rs"), "fn main() {}\n").unwrap();

    assert!(
        crate_has_binary_target(crate_dir.to_str().unwrap()),
        "crate with src/bin/<name>.rs should be detected as a binary crate"
    );
}

/// Negative control for the src/bin probe: an empty `src/bin/`
/// directory (created but with no .rs files) should NOT count as
/// having a binary target.
#[test]
fn test_crate_has_binary_target_rejects_empty_src_bin_dir() {
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("no-bin");
    fs::create_dir_all(crate_dir.join("src/bin")).unwrap();
    fs::write(
        crate_dir.join("Cargo.toml"),
        r#"
[package]
name = "no-bin"
version = "0.1.0"
edition = "2021"

[lib]
"#,
    )
    .unwrap();
    fs::write(crate_dir.join("src/lib.rs"), "// library only\n").unwrap();
    // src/bin exists but is empty.

    assert!(
        !crate_has_binary_target(crate_dir.to_str().unwrap()),
        "empty src/bin directory should not count as a binary target"
    );
}

#[test]
fn test_command_override() {
    let env = HashMap::new();
    let flags = vec!["--release".to_string()];
    let cmd = build_command(
        "mybin",
        &BuildContext {
            crate_path: ".",
            target: "x86_64-unknown-linux-gnu",
            strategy: &CrossStrategy::Cargo,
            flags: &flags,
            features: &[],
            no_default_features: false,
            env: &env,
            cross_tool: None,
            command_override: Some("auditable build"),
        },
    );
    assert_eq!(cmd.program, "cargo");
    // "auditable build" should be split into two args
    assert!(cmd.args.contains(&"auditable".to_string()));
    assert!(cmd.args.contains(&"build".to_string()));
    assert!(cmd.args.contains(&"--bin".to_string()));
}

#[test]
fn test_resolve_build_program_with_command_override() {
    let (prog, sub) =
        resolve_build_program(&CrossStrategy::Cargo, None, Some("auditable build"), None);
    assert_eq!(prog, "cargo");
    assert_eq!(sub, "auditable build");
}

#[test]
fn test_known_targets_contains_mips() {
    assert!(KNOWN_TARGETS.contains(&"mips-unknown-linux-gnu"));
    assert!(KNOWN_TARGETS.contains(&"mipsel-unknown-linux-gnu"));
    assert!(KNOWN_TARGETS.contains(&"mips64-unknown-linux-gnuabi64"));
}

#[test]
fn test_known_targets_contains_riscv() {
    assert!(KNOWN_TARGETS.contains(&"riscv64gc-unknown-linux-gnu"));
    assert!(KNOWN_TARGETS.contains(&"riscv64gc-unknown-linux-musl"));
    assert!(KNOWN_TARGETS.contains(&"riscv32imac-unknown-none-elf"));
}

#[test]
fn test_known_targets_contains_powerpc() {
    assert!(KNOWN_TARGETS.contains(&"powerpc-unknown-linux-gnu"));
    assert!(KNOWN_TARGETS.contains(&"powerpc64-unknown-linux-gnu"));
    assert!(KNOWN_TARGETS.contains(&"powerpc64le-unknown-linux-gnu"));
}

#[test]
fn test_known_targets_contains_sparc() {
    assert!(KNOWN_TARGETS.contains(&"sparc64-unknown-linux-gnu"));
}

#[test]
fn test_known_targets_contains_thumb() {
    assert!(KNOWN_TARGETS.contains(&"thumbv6m-none-eabi"));
    assert!(KNOWN_TARGETS.contains(&"thumbv7em-none-eabi"));
}

#[test]
fn test_known_targets_contains_wasm() {
    assert!(KNOWN_TARGETS.contains(&"wasm32-unknown-unknown"));
    assert!(KNOWN_TARGETS.contains(&"wasm32-wasi"));
    assert!(KNOWN_TARGETS.contains(&"wasm32-wasip1"));
    assert!(KNOWN_TARGETS.contains(&"wasm32-wasip2"));
}

#[test]
fn test_known_targets_contains_i686() {
    assert!(KNOWN_TARGETS.contains(&"i686-unknown-linux-gnu"));
    assert!(KNOWN_TARGETS.contains(&"i686-unknown-freebsd"));
    assert!(KNOWN_TARGETS.contains(&"i586-unknown-linux-gnu"));
}

#[test]
fn test_known_targets_contains_s390x() {
    assert!(KNOWN_TARGETS.contains(&"s390x-unknown-linux-gnu"));
    assert!(KNOWN_TARGETS.contains(&"s390x-unknown-linux-musl"));
}

#[test]
fn test_build_config_command_field_parses() {
    use anodizer_core::config::Config;

    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        command: "auditable build"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.command.as_deref(), Some("auditable build"));
}

#[test]
fn test_build_config_skip_string_or_bool_parses() {
    use anodizer_core::config::{Config, StringOrBool};

    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        skip: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.skip, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_build_config_skip_template_parses() {
    use anodizer_core::config::{Config, StringOrBool};

    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        skip: "{{ if .IsSnapshot }}true{{ endif }}"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    match &build.skip {
        Some(StringOrBool::String(s)) => {
            assert!(s.contains("IsSnapshot"));
        }
        other => panic!("expected StringOrBool::String, got {:?}", other),
    }
}

#[test]
fn test_build_config_no_unique_dist_dir_string_or_bool() {
    use anodizer_core::config::{Config, StringOrBool};

    let yaml = r#"
project_name: test
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
    builds:
      - binary: myapp
        no_unique_dist_dir: true
"#;
    let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
    let build = &config.crates[0].builds.as_ref().unwrap()[0];
    assert_eq!(build.no_unique_dist_dir, Some(StringOrBool::Bool(true)));
}

#[test]
fn test_parse_amd64_variant_compact_flag() {
    assert_eq!(
        parse_amd64_variant_from_rustflags("-Ctarget-cpu=x86-64-v3"),
        Some("v3".to_string())
    );
}

#[test]
fn test_parse_amd64_variant_spaced_flag() {
    assert_eq!(
        parse_amd64_variant_from_rustflags("-C target-cpu=x86-64-v2"),
        Some("v2".to_string())
    );
}

#[test]
fn test_parse_amd64_variant_mixed_flags() {
    assert_eq!(
        parse_amd64_variant_from_rustflags(
            "--remap-path-prefix=/build -C target-cpu=x86-64-v4 -C opt-level=3"
        ),
        Some("v4".to_string())
    );
}

#[test]
fn test_parse_amd64_variant_non_x86_cpu() {
    assert_eq!(
        parse_amd64_variant_from_rustflags("-Ctarget-cpu=native"),
        None
    );
}

#[test]
fn test_parse_amd64_variant_no_flags() {
    assert_eq!(parse_amd64_variant_from_rustflags(""), None);
}

#[test]
fn test_detect_amd64_variant_x86_64_with_rustflags() {
    let mut env = HashMap::new();
    env.insert(
        "RUSTFLAGS".to_string(),
        "-C target-cpu=x86-64-v3".to_string(),
    );
    assert_eq!(
        detect_amd64_variant("x86_64-unknown-linux-gnu", &env),
        Some("v3".to_string())
    );
}

#[test]
fn test_detect_amd64_variant_non_x86_target() {
    let mut env = HashMap::new();
    env.insert(
        "RUSTFLAGS".to_string(),
        "-C target-cpu=x86-64-v3".to_string(),
    );
    assert_eq!(
        detect_amd64_variant("aarch64-unknown-linux-gnu", &env),
        None
    );
}

#[test]
fn test_detect_amd64_variant_no_rustflags() {
    let env = HashMap::new();
    assert_eq!(detect_amd64_variant("x86_64-unknown-linux-gnu", &env), None);
}

// ---------------------------------------------------------------------------
// 2026-05-08 second-opinion parity audit regressions (Q-univ1, Q-rust1)
// ---------------------------------------------------------------------------

/// Q-univ1 — `universal_binaries[].id` default falls back to ProjectName-derived
/// ids so a config that says `ids: [<project>]` matches
/// in single-crate workspaces. Exercises the "binary id == project_name" path.
#[test]
fn test_universal_binary_default_id_matches_project_name() {
    use anodizer_core::artifact::Artifact;
    use anodizer_core::config::{Config, CrateConfig, UniversalBinaryConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    // Single-crate workspace where the binary id matches project_name (the
    // typical case). Crate name differs from project_name on purpose so
    // the legacy `crate_name` fallback would fail to match.
    config.project_name = "myproject".to_string();
    config.crates.push(CrateConfig {
        name: "different-crate".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        universal_binaries: Some(vec![UniversalBinaryConfig::default()]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    // Binary id metadata == project_name (e.g. user set `build.id: myproject`).
    let mk = |target: &str| {
        let mut m = HashMap::new();
        m.insert("binary".to_string(), "different-crate".to_string());
        m.insert("id".to_string(), "myproject".to_string());
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from(format!("target/{target}/release/different-crate")),
            target: Some(target.to_string()),
            crate_name: "different-crate".to_string(),
            metadata: m,
            size: None,
        }
    };
    ctx.artifacts.add(mk("aarch64-apple-darwin"));
    ctx.artifacts.add(mk("x86_64-apple-darwin"));

    build_universal_binary(
        "different-crate",
        &UniversalBinaryConfig::default(),
        &mut ctx,
        true,
    )
    .unwrap();

    let universals: Vec<_> = ctx
        .artifacts
        .by_kind(ArtifactKind::UniversalBinary)
        .into_iter()
        .filter(|a| a.target.as_deref() == Some("darwin-universal"))
        .collect();
    assert_eq!(
        universals.len(),
        1,
        "default id must resolve to project_name when the candidate binaries \
         carry that id (GR `ids: [<project>]` migration path)"
    );
}

/// Q-univ1 — multi-crate fallback to crate_name when binaries do NOT carry
/// project_name as their id (the anodizer per-crate workspace default).
#[test]
fn test_universal_binary_default_id_falls_back_to_crate_name() {
    use anodizer_core::artifact::Artifact;
    use anodizer_core::config::{Config, CrateConfig, UniversalBinaryConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let mut config = Config::default();
    config.project_name = "workspace".to_string();
    config.crates.push(CrateConfig {
        name: "crate-a".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        universal_binaries: Some(vec![UniversalBinaryConfig::default()]),
        ..Default::default()
    });

    let opts = ContextOptions {
        dry_run: true,
        ..Default::default()
    };
    let mut ctx = Context::new(config, opts);

    // Binary id metadata == crate_name (default) — does NOT match project_name.
    let mk = |target: &str| {
        let mut m = HashMap::new();
        m.insert("binary".to_string(), "crate-a".to_string());
        m.insert("id".to_string(), "crate-a".to_string());
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: std::path::PathBuf::from(format!("target/{target}/release/crate-a")),
            target: Some(target.to_string()),
            crate_name: "crate-a".to_string(),
            metadata: m,
            size: None,
        }
    };
    ctx.artifacts.add(mk("aarch64-apple-darwin"));
    ctx.artifacts.add(mk("x86_64-apple-darwin"));

    build_universal_binary("crate-a", &UniversalBinaryConfig::default(), &mut ctx, true).unwrap();

    let universals: Vec<_> = ctx
        .artifacts
        .by_kind(ArtifactKind::UniversalBinary)
        .into_iter()
        .filter(|a| a.target.as_deref() == Some("darwin-universal"))
        .collect();
    assert_eq!(
        universals.len(),
        1,
        "default id must fall back to crate_name when no candidate binary \
         carries project_name as its id"
    );
}

/// `rustup target add` failure is a hard error. Previously a warn that allowed
/// the subsequent `cargo build --target=...` to fail with a less-clear
/// "no such target" error.
#[test]
fn test_rustup_target_add_failure_is_hard_error() {
    use crate::workspace::ensure_targets_installed;
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let opts = ContextOptions::default();
    let ctx = Context::new(config, opts);
    let log = ctx.logger("build");

    // A target that rustup cannot possibly recognize. If rustup is not
    // present on the test runner the helper will return Ok via the
    // strict_guard "rustup not found" branch — that's fine, the
    // hard-error contract still holds for the rustup-present case.
    let bogus_target = "definitely-not-a-real-target-zzz".to_string();
    let result = ensure_targets_installed(&ctx, std::slice::from_ref(&bogus_target), &log, false);

    // Detect rustup presence by attempting the same probe the helper uses.
    // Pin cwd: a peer test that deletes the process-global cwd would otherwise
    // make this forked probe fail spuriously and silently skip the assertion.
    let rustup_present = std::process::Command::new("rustup")
        .arg("--version")
        .current_dir(anodizer_core::path_util::probe_dir())
        .output()
        .is_ok();
    if rustup_present {
        assert!(
            result.is_err(),
            "rustup target add for an invalid target must hard-error \
             (GR rust/build.go:60-62)"
        );
        let err = result.err().unwrap().to_string();
        assert!(
            err.contains("rustup target add") || err.contains("could not add target"),
            "error message should reference the failed rustup invocation: {err}"
        );
    }
}

/// A glibc-pinned target (`<triple>.<ver>`) must have its cargo-zigbuild suffix
/// stripped before the host comparison and the `rustup target add` arg — rustup
/// only knows the bare triple. Here the pin is on the *host* triple, so the
/// helper must strip the suffix, recognize it as the host, and skip without
/// ever shelling out to rustup (otherwise `rustup target add <host>.2.28` would
/// hard-error on a rustup-present runner).
#[test]
fn test_glibc_suffixed_host_target_is_stripped_and_skipped() {
    use crate::workspace::ensure_targets_installed;
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let host = anodizer_core::partial::detect_host_target().unwrap_or_default();
    // The strip only applies to `*-linux-gnu` / `*-linux-musl` hosts; skip on
    // non-glibc/musl runners (e.g. macOS darwin) where the pin is meaningless.
    if !(host.ends_with("-linux-gnu") || host.ends_with("-linux-musl")) {
        return;
    }

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let log = ctx.logger("build");

    let pinned_host = format!("{host}.2.28");
    let result = ensure_targets_installed(&ctx, std::slice::from_ref(&pinned_host), &log, false);
    assert!(
        result.is_ok(),
        "a glibc-pinned host target must be stripped, matched as host, and \
         skipped without invoking rustup: {result:?}"
    );
}

/// Two glibc pins on the same *non-host* triple must de-dup to a single
/// `rustup target add`. The pins differ only by glibc version (`.2.28` vs
/// `.2.17`) — a cargo-zigbuild link-time concept rustup is blind to — so both
/// strip to one bare triple and the `seen` set collapses them. Driven in
/// `dry_run` mode so the single emitted "would run" line is the observable
/// proof of the de-dup, with no rustup/network dependency.
#[test]
fn test_duplicate_glibc_pins_collapse_to_single_rustup_call() {
    use crate::workspace::ensure_targets_installed;
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

    let host = anodizer_core::partial::detect_host_target().unwrap_or_default();
    // Pick a Linux triple guaranteed to differ from the host so the entries
    // reach the `seen` de-dup branch rather than the host-skip short-circuit.
    let other = if host.starts_with("x86_64") {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    };

    let ctx = Context::new(Config::default(), ContextOptions::default());
    let (log, capture) = StageLogger::with_capture("build", Verbosity::Normal);

    let targets = vec![format!("{other}.2.28"), format!("{other}.2.17")];
    let result = ensure_targets_installed(&ctx, &targets, &log, true);
    assert!(result.is_ok(), "dry-run must not error: {result:?}");

    let would_run: Vec<String> = capture
        .all_messages()
        .into_iter()
        .map(|(_, m)| m)
        .filter(|m| m.contains(&format!("rustup target add {other}")))
        .collect();
    assert_eq!(
        would_run.len(),
        1,
        "two glibc pins on {other} must collapse to one rustup invocation, got: {would_run:?}"
    );
}

/// The per-crate "library crate has no binary target" skip is a no-op the
/// build planner emits once per non-binary workspace member. In a workspace
/// of N library crates it would print N near-identical lines at default
/// verbosity, so it routes through `log.skip_line(ctx.options.show_skipped, …)`
/// — Debug by default (invisible at Normal and Verbose), Status only under
/// `--show-skipped`. These two tests pin that visibility contract on the
/// exact message the planner emits at `run.rs`.
mod no_binary_skip_visibility {
    use anodizer_core::context::ContextOptions;
    use anodizer_core::log::{LogCapture, LogLevel, StageLogger, Verbosity};

    /// Drive the build skip line at the given `show_skipped` and verbosity,
    /// returning the recorded `(level, message)` lines.
    fn capture_build_skip(show_skipped: bool, verbosity: Verbosity) -> Vec<(LogLevel, String)> {
        let opts = ContextOptions {
            show_skipped,
            ..Default::default()
        };
        let log = StageLogger::new("build", verbosity);
        let cap = LogCapture::new();
        let log = log.with_capture_handle(cap.clone());
        log.skip_line(
            opts.show_skipped,
            "skipped build for crate 'lib-only' — no explicit binary, no binary target found",
        );
        cap.all_messages()
    }

    #[test]
    fn no_binary_skip_is_debug_level_by_default() {
        // Default (show_skipped=false, Normal): the per-crate no-binary line
        // records at Debug, NOT Status, so a library-heavy workspace does not
        // emit one such line per member at default verbosity.
        let lines = capture_build_skip(false, Verbosity::Normal);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].0, LogLevel::Debug, "{lines:?}");
        assert!(lines[0].1.contains("no binary target found"), "{lines:?}");
        assert!(
            lines.iter().all(|(l, _)| *l != LogLevel::Status),
            "no-binary skip must not record at Status by default: {lines:?}"
        );
    }

    #[test]
    fn no_binary_skip_stays_debug_at_verbose() {
        // `-v` alone must NOT surface the skip — it is per-crate no-op noise,
        // not subprocess detail; only `--show-skipped` promotes it.
        let lines = capture_build_skip(false, Verbosity::Verbose);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].0, LogLevel::Debug, "{lines:?}");
    }

    #[test]
    fn no_binary_skip_surfaces_with_show_skipped() {
        // --show-skipped forces the line back to Status for diagnosis.
        let lines = capture_build_skip(true, Verbosity::Normal);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].0, LogLevel::Status, "{lines:?}");
    }
}
