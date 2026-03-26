use anodize_core::config::Config;
use anodize_core::context::Context;
use anodize_core::stage::Stage;
use anyhow::{Context as _, Result, bail};
use colored::Colorize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Find config file. If `config_override` is provided, use that path directly;
/// otherwise search the current directory for well-known config file names.
pub fn find_config(config_override: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = config_override {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        bail!("config file not found: {}", path.display());
    }
    let candidates = [
        ".anodize.yaml",
        ".anodize.yml",
        ".anodize.toml",
        "anodize.yaml",
        "anodize.yml",
        "anodize.toml",
    ];
    for name in &candidates {
        let path = PathBuf::from(name);
        if path.exists() {
            return Ok(path);
        }
    }
    bail!(
        "no anodize config file found (tried: {}). Run `anodize init` to generate one.",
        candidates.join(", ")
    )
}

/// Load config from a file, auto-detecting format by extension
pub fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "yaml" | "yml" => Ok(serde_yaml::from_str(&content)?),
        "toml" => Ok(toml::from_str(&content)?),
        _ => bail!("unsupported config format: {}", ext),
    }
}

/// Execute a list of shell hook commands.
/// In dry-run mode, log but do not execute.
pub fn run_hooks(hooks: &[String], label: &str, dry_run: bool) -> Result<()> {
    for hook in hooks {
        if dry_run {
            eprintln!("  [dry-run] {} hook: {}", label, hook);
        } else {
            eprintln!("  running {} hook: {}", label, hook);
            let status = Command::new("sh")
                .arg("-c")
                .arg(hook)
                .status()
                .with_context(|| format!("failed to spawn {} hook: {}", label, hook))?;
            if !status.success() {
                bail!(
                    "{} hook failed (exit {}): {}",
                    label,
                    status.code().unwrap_or(-1),
                    hook
                );
            }
        }
    }
    Ok(())
}

pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    pub fn new() -> Self {
        Self { stages: vec![] }
    }

    pub fn add(&mut self, stage: Box<dyn Stage>) {
        self.stages.push(stage);
    }

    pub fn run(&self, ctx: &mut Context) -> Result<()> {
        for stage in &self.stages {
            let name = stage.name().bold();
            if ctx.should_skip(stage.name()) {
                eprintln!("  {} {}", name, "skipped".yellow());
                continue;
            }
            eprintln!("  \u{2022} {}...", name);
            match stage.run(ctx) {
                Ok(()) => eprintln!("  {} {}", "\u{2713}".green().bold(), name),
                Err(e) => {
                    eprintln!("  {} {} — {}", "\u{2717}".red().bold(), name, e);
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

/// Build the full release pipeline with all stages in order
pub fn build_release_pipeline() -> Pipeline {
    use anodize_stage_announce::AnnounceStage;
    use anodize_stage_archive::ArchiveStage;
    use anodize_stage_build::BuildStage;
    use anodize_stage_changelog::ChangelogStage;
    use anodize_stage_checksum::ChecksumStage;
    use anodize_stage_docker::DockerStage;
    use anodize_stage_nfpm::NfpmStage;
    use anodize_stage_publish::PublishStage;
    use anodize_stage_release::ReleaseStage;
    use anodize_stage_sign::SignStage;

    let mut p = Pipeline::new();
    p.add(Box::new(BuildStage));
    p.add(Box::new(ArchiveStage));
    p.add(Box::new(NfpmStage));
    p.add(Box::new(ChecksumStage));
    p.add(Box::new(ChangelogStage));
    p.add(Box::new(ReleaseStage));
    p.add(Box::new(PublishStage));
    p.add(Box::new(DockerStage));
    p.add(Box::new(SignStage));
    p.add(Box::new(AnnounceStage));
    p
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_find_config_with_override_existing() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("custom-config.yaml");
        fs::write(&cfg_path, "project_name: test\ncrates: []\n").unwrap();

        let result = find_config(Some(cfg_path.as_path()));
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert_eq!(result.unwrap(), cfg_path);
    }

    #[test]
    fn test_find_config_with_override_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let cfg_path = tmp.path().join("does-not-exist.yaml");

        let result = find_config(Some(cfg_path.as_path()));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("config file not found"),
            "unexpected error message: {}",
            msg
        );
    }

    #[test]
    fn test_find_config_override_with_subdirectory_path() {
        let tmp = TempDir::new().unwrap();
        let subdir = tmp.path().join("nested").join("dir");
        fs::create_dir_all(&subdir).unwrap();
        let cfg_path = subdir.join("my-release.toml");
        fs::write(&cfg_path, "project_name = \"test\"\ncrates = []\n").unwrap();

        let result = find_config(Some(cfg_path.as_path()));
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert_eq!(result.unwrap(), cfg_path);
    }
}
