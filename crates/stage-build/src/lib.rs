use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{BuildConfig, CrossStrategy, UniversalBinaryConfig};
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anodize_core::target::map_target;

pub mod binstall;
pub mod version_sync;

// ---------------------------------------------------------------------------
// BuildCommand — a description of the command to run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BuildCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    /// Working directory for the command (crate root)
    pub cwd: PathBuf,
}

// ---------------------------------------------------------------------------
// detect_cross_strategy
// ---------------------------------------------------------------------------

pub fn detect_cross_strategy() -> CrossStrategy {
    if which("cargo-zigbuild") {
        return CrossStrategy::Zigbuild;
    }
    if which("cross") {
        return CrossStrategy::Cross;
    }
    CrossStrategy::Cargo
}

fn which(program: &str) -> bool {
    Command::new("which")
        .arg(program)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// build_command
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn build_command(
    binary: &str,
    crate_path: &str,
    target: &str,
    strategy: &CrossStrategy,
    flags: Option<&str>,
    features: &[String],
    no_default_features: bool,
    env: &HashMap<String, String>,
) -> BuildCommand {
    // Resolve Auto strategy
    let resolved = if *strategy == CrossStrategy::Auto {
        detect_cross_strategy()
    } else {
        strategy.clone()
    };

    let (program, subcommand) = match resolved {
        CrossStrategy::Zigbuild => ("cargo".to_string(), "zigbuild".to_string()),
        CrossStrategy::Cross => ("cross".to_string(), "build".to_string()),
        // Cargo and Auto (already resolved above)
        _ => ("cargo".to_string(), "build".to_string()),
    };

    let mut args: Vec<String> = vec![
        subcommand,
        "--bin".to_string(),
        binary.to_string(),
        "--target".to_string(),
        target.to_string(),
    ];

    // Append flags (split on whitespace)
    if let Some(f) = flags {
        for part in f.split_whitespace() {
            args.push(part.to_string());
        }
    }

    // Features
    if !features.is_empty() {
        args.push("--features".to_string());
        args.push(features.join(","));
    }

    if no_default_features {
        args.push("--no-default-features".to_string());
    }

    BuildCommand {
        program,
        args,
        env: env.clone(),
        cwd: PathBuf::from(crate_path),
    }
}

// ---------------------------------------------------------------------------
// build_lib_command
// ---------------------------------------------------------------------------

/// Build command for library targets (cdylib, staticlib, etc.).
/// Uses `--lib` instead of `--bin`.
#[allow(clippy::too_many_arguments)]
pub fn build_lib_command(
    crate_path: &str,
    target: &str,
    strategy: &CrossStrategy,
    flags: Option<&str>,
    features: &[String],
    no_default_features: bool,
    env: &HashMap<String, String>,
) -> BuildCommand {
    // Resolve Auto strategy
    let resolved = if *strategy == CrossStrategy::Auto {
        detect_cross_strategy()
    } else {
        strategy.clone()
    };

    let (program, subcommand) = match resolved {
        CrossStrategy::Zigbuild => ("cargo".to_string(), "zigbuild".to_string()),
        CrossStrategy::Cross => ("cross".to_string(), "build".to_string()),
        _ => ("cargo".to_string(), "build".to_string()),
    };

    let mut args: Vec<String> = vec![
        subcommand,
        "--lib".to_string(),
        "--target".to_string(),
        target.to_string(),
    ];

    // Append flags (split on whitespace)
    if let Some(f) = flags {
        for part in f.split_whitespace() {
            args.push(part.to_string());
        }
    }

    // Features
    if !features.is_empty() {
        args.push("--features".to_string());
        args.push(features.join(","));
    }

    if no_default_features {
        args.push("--no-default-features".to_string());
    }

    BuildCommand {
        program,
        args,
        env: env.clone(),
        cwd: PathBuf::from(crate_path),
    }
}

// ---------------------------------------------------------------------------
// detect_crate_type
// ---------------------------------------------------------------------------

/// Read a crate's Cargo.toml and return the first `crate-type` from [lib],
/// if present (e.g. "cdylib", "staticlib", "rlib").
pub fn detect_crate_type(crate_path: &str) -> Option<String> {
    let cargo_toml_path = Path::new(crate_path).join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml_path).ok()?;
    let doc = content.parse::<toml_edit::DocumentMut>().ok()?;
    let lib = doc.get("lib")?;
    let crate_types = lib.get("crate-type").or_else(|| lib.get("crate_type"))?;
    let arr = crate_types.as_array()?;
    arr.get(0).and_then(|v| v.as_str()).map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// build_universal_binary — run `lipo` to combine arm64 + x86_64 macOS binaries
// ---------------------------------------------------------------------------

fn build_universal_binary(
    crate_name: &str,
    ub: &UniversalBinaryConfig,
    ctx: &mut Context,
    dry_run: bool,
) -> anyhow::Result<()> {
    // Collect arm64 and x86_64 macOS binary artifacts for this crate.
    // When `ids` is set, only consider artifacts whose metadata "id" is in the list.
    let binaries = ctx
        .artifacts
        .by_kind_and_crate(ArtifactKind::Binary, crate_name);

    let filtered: Vec<_> = if let Some(ref ids) = ub.ids {
        binaries
            .into_iter()
            .filter(|a| {
                a.metadata
                    .get("id")
                    .map(|id| ids.contains(id))
                    .unwrap_or(false)
            })
            .collect()
    } else {
        binaries
    };

    let arm64 = filtered.iter().find(|a| {
        a.target.as_deref() == Some("aarch64-apple-darwin")
    });
    let x86_64 = filtered.iter().find(|a| {
        a.target.as_deref() == Some("x86_64-apple-darwin")
    });

    let (arm64_path, x86_64_path) = match (arm64, x86_64) {
        (Some(a), Some(x)) => (a.path.clone(), x.path.clone()),
        _ => {
            eprintln!(
                "[build] universal_binaries: skipping {crate_name} — \
                 both aarch64-apple-darwin and x86_64-apple-darwin binaries required"
            );
            return Ok(());
        }
    };

    // Determine output path / name
    let binary_name = arm64_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate_name.to_string());

    let out_name = if let Some(ref tmpl) = ub.name_template {
        ctx.render_template(tmpl).unwrap_or_else(|_| tmpl.clone())
    } else {
        binary_name.clone()
    };

    // Place the universal binary in the same directory as the arm64 binary
    let out_path = arm64_path
        .parent()
        .map(|p| p.join(&out_name))
        .unwrap_or_else(|| PathBuf::from(&out_name));

    if dry_run {
        eprintln!(
            "[build] (dry-run) lipo -create -output {} {} {}",
            out_path.display(),
            arm64_path.display(),
            x86_64_path.display()
        );
    } else {
        // Check lipo is available
        if !which("lipo") {
            eprintln!(
                "[build] warning: lipo not found, skipping universal binary for {crate_name}"
            );
            return Ok(());
        }

        eprintln!(
            "[build] lipo -create -output {} {} {}",
            out_path.display(),
            arm64_path.display(),
            x86_64_path.display()
        );

        let status = Command::new("lipo")
            .args([
                "-create",
                "-output",
                &out_path.to_string_lossy(),
                &arm64_path.to_string_lossy(),
                &x86_64_path.to_string_lossy(),
            ])
            .status()
            .with_context(|| format!("failed to spawn lipo for {crate_name}"))?;

        if !status.success() {
            eprintln!(
                "[build] warning: lipo failed for {crate_name}, skipping universal binary"
            );
            return Ok(());
        }
    }

    // Register the universal binary artifact
    let mut metadata = HashMap::new();
    metadata.insert("binary".to_string(), binary_name);
    metadata.insert("universal".to_string(), "true".to_string());

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        path: out_path,
        target: Some("darwin-universal".to_string()),
        crate_name: crate_name.to_string(),
        metadata,
    });

    // When `replace` is true, remove the source arm64/x86_64 artifacts from
    // the registry so downstream stages do not publish them alongside the
    // universal binary.
    if ub.replace == Some(true) {
        ctx.artifacts
            .remove_by_paths(&[arm64_path, x86_64_path]);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// BuildStage
// ---------------------------------------------------------------------------

pub struct BuildStage;

impl Stage for BuildStage {
    fn name(&self) -> &str {
        "build"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;

        // Collect global defaults
        let default_targets: Vec<String> = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.targets.clone())
            .unwrap_or_default();
        let default_strategy = ctx
            .config
            .defaults
            .as_ref()
            .and_then(|d| d.cross.clone())
            .unwrap_or(CrossStrategy::Auto);
        let default_flags: Option<String> =
            ctx.config.defaults.as_ref().and_then(|d| d.flags.clone());

        // Collect crates to process (cloned to avoid borrow conflict with ctx.artifacts)
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .cloned()
            .collect();

        // --- Version sync: update Cargo.toml versions before building ---
        let version = ctx
            .template_vars()
            .get("RawVersion")
            .or_else(|| ctx.template_vars().get("Version"))
            .cloned()
            .unwrap_or_default();
        for crate_cfg in &crates {
            if let Some(ref vs) = crate_cfg.version_sync
                && vs.enabled.unwrap_or(false)
                && !version.is_empty()
            {
                version_sync::sync_version(&crate_cfg.path, &version, dry_run)?;
            }
        }

        // --- Binstall: generate cargo-binstall metadata before building ---
        for crate_cfg in &crates {
            if let Some(ref bs) = crate_cfg.binstall
                && bs.enabled.unwrap_or(false)
            {
                binstall::generate_binstall_metadata(&crate_cfg.path, bs, ctx, dry_run)?;
            }
        }

        for crate_cfg in &crates {
            // Determine builds for this crate
            let builds: Vec<BuildConfig> = match &crate_cfg.builds {
                Some(b) if !b.is_empty() => b.clone(),
                _ => {
                    // Default build: binary name == crate name
                    vec![BuildConfig {
                        binary: crate_cfg.name.clone(),
                        ..Default::default()
                    }]
                }
            };

            // Detect crate type for cdylib/wasm awareness (once per crate)
            let crate_type = detect_crate_type(&crate_cfg.path);
            let is_wasm_crate = crate_type.as_deref() == Some("cdylib");
            let is_library = crate_type.as_deref() == Some("cdylib")
                || crate_type.as_deref() == Some("staticlib")
                || crate_type.as_deref() == Some("dylib");

            for build in &builds {
                // Targets: per-build override, else global defaults, else host only
                let targets: Vec<String> = build
                    .targets
                    .clone()
                    .filter(|t| !t.is_empty())
                    .or_else(|| {
                        if default_targets.is_empty() {
                            None
                        } else {
                            Some(default_targets.clone())
                        }
                    })
                    .unwrap_or_default();

                // If no targets configured, skip (caller should ensure defaults)
                if targets.is_empty() {
                    eprintln!(
                        "[build] no targets configured for {}/{}, skipping",
                        crate_cfg.name, build.binary
                    );
                    continue;
                }

                // Strategy: per-crate override, else global default
                let strategy = crate_cfg
                    .cross
                    .clone()
                    .unwrap_or_else(|| default_strategy.clone());

                // Flags: per-build or global default
                let flags: Option<&str> = build.flags.as_deref().or(default_flags.as_deref());

                // Features and no_default_features
                let features: Vec<String> = build.features.clone().unwrap_or_default();
                let no_default_features: bool = build.no_default_features.unwrap_or(false);

                // Per-target env (target-keyed map in BuildConfig.env)
                for target in &targets {
                    // Determine the binary path
                    // Flags may contain --release; check for it
                    let profile = if flags.map(|f| f.contains("--release")).unwrap_or(false) {
                        "release"
                    } else {
                        "debug"
                    };

                    let is_wasm_target = target.contains("wasm32");
                    let (os, _arch) = map_target(target);

                    // Determine the output file name based on target and crate type
                    let (output_name, artifact_kind) = if is_wasm_target && is_wasm_crate {
                        // wasm32 target with cdylib — output is .wasm
                        (
                            format!("{}.wasm", build.binary),
                            ArtifactKind::Wasm,
                        )
                    } else if is_library && !is_wasm_target {
                        // Library target — output is .so/.dylib/.dll
                        let ext = match os.as_str() {
                            "windows" => "dll",
                            "darwin" => "dylib",
                            _ => "so",
                        };
                        let prefix = if os == "windows" { "" } else { "lib" };
                        (
                            format!("{}{}.{}", prefix, build.binary, ext),
                            ArtifactKind::Library,
                        )
                    } else {
                        // Standard binary
                        let name = if os == "windows" {
                            format!("{}.exe", build.binary)
                        } else {
                            build.binary.clone()
                        };
                        (name, ArtifactKind::Binary)
                    };

                    // Workspace root target directory (not per-crate)
                    let bin_path = PathBuf::from("target")
                        .join(target)
                        .join(profile)
                        .join(&output_name);

                    // Handle copy_from: skip compilation, just copy from source binary
                    let final_path = if let Some(src_binary) = &build.copy_from {
                        let src_name = if os == "windows" {
                            format!("{}.exe", src_binary)
                        } else {
                            src_binary.clone()
                        };

                        // Find the source path: check registered artifacts first,
                        // then fall back to the expected workspace target path
                        let src_path = ctx
                            .artifacts
                            .by_kind(ArtifactKind::Binary)
                            .into_iter()
                            .find(|a| {
                                a.target.as_deref() == Some(target.as_str())
                                    && a.metadata.get("binary").map(|b| b.as_str())
                                        == Some(src_binary.as_str())
                            })
                            .map(|a| a.path.clone())
                            .unwrap_or_else(|| {
                                PathBuf::from("target")
                                    .join(target)
                                    .join(profile)
                                    .join(&src_name)
                            });

                        if !dry_run {
                            std::fs::copy(&src_path, &bin_path).with_context(|| {
                                format!(
                                    "copy_from: failed to copy {} -> {}",
                                    src_path.display(),
                                    bin_path.display()
                                )
                            })?;
                        } else {
                            eprintln!(
                                "[build] (dry-run) copy {} -> {}",
                                src_path.display(),
                                bin_path.display()
                            );
                        }
                        bin_path.clone()
                    } else {
                        // No copy_from: run the build command
                        let mut target_env: HashMap<String, String> = build
                            .env
                            .as_ref()
                            .and_then(|m| m.get(target.as_str()))
                            .cloned()
                            .unwrap_or_default();

                        // Reproducible builds: inject SOURCE_DATE_EPOCH and RUSTFLAGS
                        if build.reproducible.unwrap_or(false) {
                            let epoch = ctx
                                .template_vars()
                                .get("CommitTimestamp")
                                .cloned()
                                .unwrap_or_else(|| "0".to_string());
                            target_env
                                .entry("SOURCE_DATE_EPOCH".to_string())
                                .or_insert(epoch);

                            let cwd = std::env::current_dir()
                                .unwrap_or_else(|_| PathBuf::from("."))
                                .to_string_lossy()
                                .into_owned();
                            let remap_flag =
                                format!("--remap-path-prefix={cwd}=/build");
                            let existing_rustflags =
                                target_env.get("RUSTFLAGS").cloned().unwrap_or_default();
                            let new_rustflags = if existing_rustflags.is_empty() {
                                remap_flag
                            } else {
                                format!("{existing_rustflags} {remap_flag}")
                            };
                            target_env
                                .insert("RUSTFLAGS".to_string(), new_rustflags);
                        }

                        // For library/wasm targets, use --lib; otherwise --bin
                        let cmd = if is_library || is_wasm_target {
                            build_lib_command(
                                &crate_cfg.path,
                                target,
                                &strategy,
                                flags,
                                &features,
                                no_default_features,
                                &target_env,
                            )
                        } else {
                            build_command(
                                &build.binary,
                                &crate_cfg.path,
                                target,
                                &strategy,
                                flags,
                                &features,
                                no_default_features,
                                &target_env,
                            )
                        };

                        if dry_run {
                            eprintln!("[build] (dry-run) {} {}", cmd.program, cmd.args.join(" "));
                        } else {
                            eprintln!("[build] running: {} {}", cmd.program, cmd.args.join(" "));
                            let status = Command::new(&cmd.program)
                                .args(&cmd.args)
                                .envs(&cmd.env)
                                .current_dir(&cmd.cwd)
                                .status()
                                .with_context(|| {
                                    format!(
                                        "failed to spawn `{} {}`",
                                        cmd.program,
                                        cmd.args.join(" ")
                                    )
                                })?;
                            if !status.success() {
                                anyhow::bail!(
                                    "build failed for {}/{} on target {} (exit: {})",
                                    crate_cfg.name,
                                    build.binary,
                                    target,
                                    status
                                );
                            }
                        }

                        bin_path
                    };

                    // Set stage-scoped Binary template var
                    ctx.template_vars_mut().set("Binary", &build.binary);

                    // Register artifact with appropriate kind
                    ctx.artifacts.add(Artifact {
                        kind: artifact_kind,
                        path: final_path,
                        target: Some(target.clone()),
                        crate_name: crate_cfg.name.clone(),
                        metadata: {
                            let mut m = HashMap::new();
                            m.insert("binary".to_string(), build.binary.clone());
                            m
                        },
                    });
                }
            }
        }

        // --- Universal binaries (macOS lipo) ---
        for crate_cfg in &crates {
            if let Some(ref ub_configs) = crate_cfg.universal_binaries {
                for ub in ub_configs {
                    build_universal_binary(crate_cfg.name.as_str(), ub, ctx, dry_run)?;
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn test_build_command_native_cargo() {
        let cmd = build_command(
            "cfgd",
            "crates/cfgd",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            false,
            &Default::default(),
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
        let cmd = build_command(
            "cfgd",
            "crates/cfgd",
            "aarch64-unknown-linux-gnu",
            &CrossStrategy::Zigbuild,
            Some("--release"),
            &[],
            false,
            &Default::default(),
        );
        assert_eq!(cmd.program, "cargo");
        assert!(cmd.args.contains(&"zigbuild".to_string()));
        assert!(cmd.args.contains(&"--target".to_string()));
    }

    #[test]
    fn test_build_command_cross() {
        let cmd = build_command(
            "cfgd",
            "crates/cfgd",
            "aarch64-unknown-linux-gnu",
            &CrossStrategy::Cross,
            Some("--release"),
            &[],
            false,
            &Default::default(),
        );
        assert_eq!(cmd.program, "cross");
        assert!(cmd.args.contains(&"build".to_string()));
    }

    #[test]
    fn test_build_command_with_features() {
        let cmd = build_command(
            "cfgd",
            "crates/cfgd",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            Some("--release"),
            &["tls".to_string(), "json".to_string()],
            false,
            &Default::default(),
        );
        assert!(cmd.args.contains(&"--features".to_string()));
        assert!(cmd.args.contains(&"tls,json".to_string()));
    }

    #[test]
    fn test_build_command_no_default_features() {
        let cmd = build_command(
            "cfgd",
            "crates/cfgd",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            true,
            &Default::default(),
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

    // ---- Error path tests (Task 3B) ----

    #[test]
    fn test_build_command_with_invalid_target_triple() {
        // build_command itself does not validate target triples -- it just
        // constructs the command.  Verify the invalid triple is passed through
        // so that cargo (or cross) reports the error at execution time.
        let cmd = build_command(
            "mybin",
            "crates/mybin",
            "this-is-not-a-valid-triple",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            false,
            &Default::default(),
        );
        assert!(cmd.args.contains(&"this-is-not-a-valid-triple".to_string()));
        assert_eq!(cmd.program, "cargo");
    }

    #[test]
    fn test_build_command_empty_binary_name() {
        // An empty binary name should still be passed through to --bin
        let cmd = build_command(
            "",
            ".",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            None,
            &[],
            false,
            &Default::default(),
        );
        assert!(cmd.args.contains(&"--bin".to_string()));
        // Empty string is present in args
        assert!(cmd.args.contains(&"".to_string()));
    }

    #[test]
    fn test_build_stage_no_targets_skips_gracefully() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
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
            .by_kind(anodize_core::artifact::ArtifactKind::Binary);
        assert!(binaries.is_empty());
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_copy_from_nonexistent_binary_errors_with_paths() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp_dir = std::env::temp_dir().join("anodize_build_test_copy_from");
        let _ = std::fs::create_dir_all(&tmp_dir);

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: tmp_dir.to_string_lossy().into_owned(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
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
        assert!(result.is_err(), "copy_from with nonexistent source should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("copy_from") || err.contains("copy"),
            "error should mention copy_from, got: {err}"
        );
    }

    #[test]
    fn test_build_failure_nonzero_exit_produces_clear_error() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp_dir = std::env::temp_dir().join("anodize_build_test_nonzero");
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
                binary: "this-binary-does-not-exist".to_string(),
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
            err.contains("build failed") || err.contains("this-binary-does-not-exist"),
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

        let cmd = build_command(
            "mybin",
            ".",
            "x86_64-unknown-linux-musl",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            false,
            &env,
        );
        assert_eq!(cmd.env.get("CC").unwrap(), "gcc-12");
        assert_eq!(
            cmd.env.get("RUSTFLAGS").unwrap(),
            "-C target-feature=+crt-static"
        );
    }

    // ---- Task 5A: cdylib detection tests ----

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

    // ---- Task 5A: build_lib_command tests ----

    #[test]
    fn test_build_lib_command_uses_lib_flag() {
        let cmd = build_lib_command(
            "crates/my-lib",
            "x86_64-unknown-linux-gnu",
            &CrossStrategy::Cargo,
            Some("--release"),
            &[],
            false,
            &Default::default(),
        );
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
        let cmd = build_lib_command(
            "crates/my-lib",
            "wasm32-unknown-unknown",
            &CrossStrategy::Cargo,
            None,
            &["wasm-bindgen".to_string()],
            true,
            &Default::default(),
        );
        assert!(cmd.args.contains(&"--lib".to_string()));
        assert!(cmd.args.contains(&"--features".to_string()));
        assert!(cmd.args.contains(&"wasm-bindgen".to_string()));
        assert!(cmd.args.contains(&"--no-default-features".to_string()));
    }

    #[test]
    fn test_build_lib_command_zigbuild() {
        let cmd = build_lib_command(
            ".",
            "aarch64-unknown-linux-gnu",
            &CrossStrategy::Zigbuild,
            Some("--release"),
            &[],
            false,
            &Default::default(),
        );
        assert_eq!(cmd.program, "cargo");
        assert!(cmd.args.contains(&"zigbuild".to_string()));
        assert!(cmd.args.contains(&"--lib".to_string()));
    }

    // ---- Task 5E: reproducible build env var injection ----

    #[test]
    fn test_reproducible_build_sets_source_date_epoch_and_rustflags() {
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
                targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                reproducible: Some(true),
                flags: Some("--release".to_string()),
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
        use std::collections::HashMap;

        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

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
                binary: "myapp".to_string(),
                targets: Some(vec!["x86_64-unknown-linux-musl".to_string()]),
                reproducible: Some(true),
                flags: Some("--release".to_string()),
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
        use anodize_core::config::{BuildConfig, Config, CrateConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                binary: "myapp".to_string(),
                targets: Some(vec!["x86_64-unknown-linux-gnu".to_string()]),
                reproducible: Some(false),
                flags: Some("--release".to_string()),
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

    // ---- Task 5F: universal binary tests ----

    /// Helper: register a fake Binary artifact directly in the context.
    fn register_binary(
        ctx: &mut anodize_core::context::Context,
        crate_name: &str,
        target: &str,
        path: std::path::PathBuf,
    ) {
        use anodize_core::artifact::{Artifact, ArtifactKind};
        let mut meta = HashMap::new();
        meta.insert(
            "binary".to_string(),
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            path,
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: meta,
        });
    }

    #[test]
    fn test_universal_binary_dry_run_registers_artifact() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::{Config, CrateConfig, UniversalBinaryConfig};
        use anodize_core::context::{Context, ContextOptions};

        let mut config = Config::default();
        config.project_name = "test".to_string();
        config.crates.push(CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            universal_binaries: Some(vec![UniversalBinaryConfig {
                name_template: None,
                replace: None,
                ids: None,
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
                name_template: None,
                replace: None,
                ids: None,
            },
            &mut ctx,
            true, // dry_run
        );
        assert!(result.is_ok(), "dry-run universal binary should succeed");

        // A universal artifact should have been registered
        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::Binary)
            .into_iter()
            .filter(|a| a.target.as_deref() == Some("darwin-universal"))
            .collect();
        assert_eq!(universals.len(), 1, "one universal artifact should be registered");
        assert_eq!(
            universals[0].metadata.get("universal").map(|s| s.as_str()),
            Some("true")
        );
    }

    #[test]
    fn test_universal_binary_dry_run_uses_name_template() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::UniversalBinaryConfig;
        use anodize_core::context::{Context, ContextOptions};

        let config = anodize_core::config::Config::default();
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
            name_template: Some("{{ .ProjectName }}-universal".to_string()),
            replace: None,
            ids: None,
        };

        let result = build_universal_binary("myapp", &ub, &mut ctx, true);
        assert!(result.is_ok());

        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::Binary)
            .into_iter()
            .filter(|a| a.target.as_deref() == Some("darwin-universal"))
            .collect();
        assert_eq!(universals.len(), 1);
        assert!(
            universals[0].path.to_string_lossy().contains("myapp-universal"),
            "output path should use rendered name template, got: {}",
            universals[0].path.display()
        );
    }

    #[test]
    fn test_universal_binary_skips_when_missing_arch() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::UniversalBinaryConfig;
        use anodize_core::context::{Context, ContextOptions};

        let config = anodize_core::config::Config::default();
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
            name_template: None,
            replace: None,
            ids: None,
        };

        let result = build_universal_binary("myapp", &ub, &mut ctx, true);
        assert!(result.is_ok(), "missing arch should not error, just skip");

        // No universal artifact should have been registered
        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::Binary)
            .into_iter()
            .filter(|a| a.target.as_deref() == Some("darwin-universal"))
            .collect();
        assert!(universals.is_empty(), "no universal artifact when arch is missing");
    }

    #[test]
    fn test_universal_binary_skips_for_different_crate() {
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::UniversalBinaryConfig;
        use anodize_core::context::{Context, ContextOptions};

        let config = anodize_core::config::Config::default();
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
            name_template: None,
            replace: None,
            ids: None,
        };

        // Ask for "myapp" universal — should be skipped since myapp has no arch binaries
        let result = build_universal_binary("myapp", &ub, &mut ctx, true);
        assert!(result.is_ok());

        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::Binary)
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
        use anodize_core::artifact::ArtifactKind;
        use anodize_core::config::UniversalBinaryConfig;
        use anodize_core::context::{Context, ContextOptions};

        let config = anodize_core::config::Config::default();
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
            name_template: None,
            replace: None,
            ids: None,
        };

        build_universal_binary("myapp", &ub, &mut ctx, true).unwrap();

        let universals: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::Binary)
            .into_iter()
            .filter(|a| a.target.as_deref() == Some("darwin-universal"))
            .collect();
        assert_eq!(universals.len(), 1);
        let art = universals[0];
        assert_eq!(art.crate_name, "myapp");
        assert_eq!(art.metadata.get("universal").map(|s| s.as_str()), Some("true"));
        assert_eq!(art.metadata.get("binary").map(|s| s.as_str()), Some("myapp"));
    }
}
