use anodize_core::config::Config;
use anyhow::{Result, bail};
use std::path::PathBuf;

use super::helpers::collect_build_targets;

pub struct TargetsOpts {
    pub json: bool,
    pub crate_names: Vec<String>,
    pub config_override: Option<PathBuf>,
}

/// Single entry emitted in the JSON matrix (include-form).
#[derive(serde::Serialize, Debug, PartialEq)]
struct MatrixEntry {
    /// GitHub Actions runner label (e.g. `ubuntu-latest`).
    os: String,
    /// Rust target triple (e.g. `x86_64-unknown-linux-gnu`).
    target: String,
    /// Split-build artifact name written by the action (e.g. `dist-Linux`).
    /// Matches `actions/upload-artifact@v4` with name `dist-${{ runner.os }}`.
    artifact: String,
}

#[derive(serde::Serialize, Debug)]
struct Matrix {
    include: Vec<MatrixEntry>,
}

/// Map anodize's normalized OS label to a GitHub Actions runner label.
fn runner_label(os: &str) -> &'static str {
    match os {
        "linux" => "ubuntu-latest",
        "darwin" | "ios" => "macos-latest",
        "windows" => "windows-latest",
        // Best-effort default. Users will see targets for these but will need
        // to override `runs-on` themselves — `ubuntu-latest` is the least bad
        // default because emulation / cross-compilation is easiest there.
        _ => "ubuntu-latest",
    }
}

/// Map `runs-on` label back to the `runner.os` capitalisation that
/// `actions/upload-artifact` uses when the action names uploads
/// `dist-${{ runner.os }}`.
fn artifact_suffix(runs_on: &str) -> &'static str {
    match runs_on {
        "ubuntu-latest" => "Linux",
        "macos-latest" => "macOS",
        "windows-latest" => "Windows",
        _ => "Linux",
    }
}

fn build_matrix(targets: &[String]) -> Matrix {
    let mut include = Vec::new();
    for t in targets {
        let (os, _arch) = anodize_core::target::map_target(t);
        let runner = runner_label(&os).to_string();
        let artifact = format!("dist-{}", artifact_suffix(&runner));
        include.push(MatrixEntry {
            os: runner,
            target: t.clone(),
            artifact,
        });
    }
    Matrix { include }
}

pub fn run(opts: TargetsOpts) -> Result<()> {
    let config_path = opts
        .config_override
        .as_deref()
        .filter(|p| p.exists())
        .map(|p| p.to_path_buf())
        .or_else(|| crate::pipeline::find_config(None).ok());

    let config: Config = match config_path {
        Some(ref path) => crate::pipeline::load_config(path)?,
        None => bail!("no anodize config found"),
    };

    let targets = collect_build_targets(&config, &opts.crate_names);
    let matrix = build_matrix(&targets);

    if opts.json {
        let out = serde_json::to_string(&matrix).map_err(|e| anyhow::anyhow!(e))?;
        println!("{}", out);
    } else if matrix.include.is_empty() {
        println!("(no build targets configured)");
    } else {
        println!("{:<20} {:<40} ARTIFACT", "OS (runner)", "TARGET");
        for e in &matrix.include {
            println!("{:<20} {:<40} {}", e.os, e.target, e.artifact);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::{BuildConfig, Config, CrateConfig, Defaults, WorkspaceConfig};

    fn base_config() -> Config {
        Config {
            project_name: "test".to_string(),
            crates: Vec::new(),
            ..Default::default()
        }
    }

    fn crate_with_targets(name: &str, triples: &[&str]) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ Version }}".to_string(),
            builds: Some(vec![BuildConfig {
                targets: Some(triples.iter().map(|s| s.to_string()).collect()),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn runner_label_mapping() {
        assert_eq!(runner_label("linux"), "ubuntu-latest");
        assert_eq!(runner_label("darwin"), "macos-latest");
        assert_eq!(runner_label("windows"), "windows-latest");
        assert_eq!(runner_label("freebsd"), "ubuntu-latest");
    }

    #[test]
    fn artifact_suffix_matches_runner_os() {
        assert_eq!(artifact_suffix("ubuntu-latest"), "Linux");
        assert_eq!(artifact_suffix("macos-latest"), "macOS");
        assert_eq!(artifact_suffix("windows-latest"), "Windows");
    }

    #[test]
    fn build_matrix_flat_crates() {
        let config = Config {
            crates: vec![crate_with_targets(
                "a",
                &["x86_64-unknown-linux-gnu", "aarch64-apple-darwin"],
            )],
            ..base_config()
        };
        let targets = collect_build_targets(&config, &[]);
        let m = build_matrix(&targets);
        assert_eq!(m.include.len(), 2);
        assert_eq!(m.include[0].os, "ubuntu-latest");
        assert_eq!(m.include[0].target, "x86_64-unknown-linux-gnu");
        assert_eq!(m.include[0].artifact, "dist-Linux");
        assert_eq!(m.include[1].os, "macos-latest");
        assert_eq!(m.include[1].artifact, "dist-macOS");
    }

    #[test]
    fn build_matrix_workspaces() {
        let config = Config {
            crates: Vec::new(),
            workspaces: Some(vec![
                WorkspaceConfig {
                    name: "core".to_string(),
                    crates: vec![crate_with_targets("a", &["x86_64-unknown-linux-gnu"])],
                    ..Default::default()
                },
                WorkspaceConfig {
                    name: "cli".to_string(),
                    crates: vec![crate_with_targets("b", &["x86_64-pc-windows-msvc"])],
                    ..Default::default()
                },
            ]),
            ..base_config()
        };
        let targets = collect_build_targets(&config, &[]);
        let m = build_matrix(&targets);
        assert_eq!(m.include.len(), 2);
        let triples: Vec<_> = m.include.iter().map(|e| e.target.as_str()).collect();
        assert!(triples.contains(&"x86_64-unknown-linux-gnu"));
        assert!(triples.contains(&"x86_64-pc-windows-msvc"));
    }

    #[test]
    fn build_matrix_defaults_ignores() {
        let config = Config {
            crates: vec![crate_with_targets(
                "a",
                &["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"],
            )],
            defaults: Some(Defaults {
                ignore: Some(vec![anodize_core::config::BuildIgnore {
                    os: "windows".to_string(),
                    arch: "amd64".to_string(),
                }]),
                ..Default::default()
            }),
            ..base_config()
        };
        let targets = collect_build_targets(&config, &[]);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0], "x86_64-unknown-linux-gnu");
    }

    #[test]
    fn build_matrix_crate_filter() {
        let config = Config {
            crates: vec![
                crate_with_targets("a", &["x86_64-unknown-linux-gnu"]),
                crate_with_targets("b", &["aarch64-apple-darwin"]),
            ],
            ..base_config()
        };
        let targets = collect_build_targets(&config, &["b".to_string()]);
        assert_eq!(targets, vec!["aarch64-apple-darwin".to_string()]);
    }

    #[test]
    fn build_matrix_deduplicates() {
        let config = Config {
            crates: vec![
                crate_with_targets("a", &["x86_64-unknown-linux-gnu"]),
                crate_with_targets("b", &["x86_64-unknown-linux-gnu"]),
            ],
            ..base_config()
        };
        let targets = collect_build_targets(&config, &[]);
        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn empty_matrix_is_serializable() {
        let m = Matrix { include: vec![] };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, "{\"include\":[]}");
    }

    #[test]
    fn matrix_json_shape() {
        let m = build_matrix(&["x86_64-unknown-linux-gnu".to_string()]);
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"include\""));
        assert!(s.contains("\"os\":\"ubuntu-latest\""));
        assert!(s.contains("\"target\":\"x86_64-unknown-linux-gnu\""));
        assert!(s.contains("\"artifact\":\"dist-Linux\""));
    }
}
