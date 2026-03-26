use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{BuildConfig, CrossStrategy};
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anodize_core::target::map_target;

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

                    let (os, _arch) = map_target(target);
                    let bin_name = if os == "windows" {
                        format!("{}.exe", build.binary)
                    } else {
                        build.binary.clone()
                    };

                    // Workspace root target directory (not per-crate)
                    let bin_path = PathBuf::from("target")
                        .join(target)
                        .join(profile)
                        .join(&bin_name);

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
                        let target_env: HashMap<String, String> = build
                            .env
                            .as_ref()
                            .and_then(|m| m.get(target.as_str()))
                            .cloned()
                            .unwrap_or_default();

                        let cmd = build_command(
                            &build.binary,
                            &crate_cfg.path,
                            target,
                            &strategy,
                            flags,
                            &features,
                            no_default_features,
                            &target_env,
                        );

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

                    // Register Binary artifact
                    ctx.artifacts.add(Artifact {
                        kind: ArtifactKind::Binary,
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
}
