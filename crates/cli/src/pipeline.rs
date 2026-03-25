use std::path::{Path, PathBuf};
use anyhow::{Result, bail};
use anodize_core::config::Config;
use anodize_core::context::Context;
use anodize_core::stage::Stage;

/// Find config file in current directory
pub fn find_config() -> Result<PathBuf> {
    let candidates = [
        "anodize.yaml", "anodize.yml", "anodize.toml",
        ".anodize.yaml", ".anodize.yml", ".anodize.toml",
    ];
    for name in &candidates {
        let path = PathBuf::from(name);
        if path.exists() {
            return Ok(path);
        }
    }
    bail!("no anodize config file found (tried: {}). Run `anodize init` to generate one.", candidates.join(", "))
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

pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    pub fn new() -> Self { Self { stages: vec![] } }

    pub fn add(&mut self, stage: Box<dyn Stage>) {
        self.stages.push(stage);
    }

    pub fn run(&self, ctx: &mut Context) -> Result<()> {
        for stage in &self.stages {
            if ctx.should_skip(stage.name()) {
                eprintln!("  \u{2022} skipping {}", stage.name());
                continue;
            }
            eprintln!("  \u{2022} running {}...", stage.name());
            stage.run(ctx)?;
            eprintln!("  \u{2713} {}", stage.name());
        }
        Ok(())
    }
}

/// Build the full release pipeline with all stages in order
pub fn build_release_pipeline() -> Pipeline {
    use anodize_stage_build::BuildStage;
    use anodize_stage_archive::ArchiveStage;
    use anodize_stage_nfpm::NfpmStage;
    use anodize_stage_checksum::ChecksumStage;
    use anodize_stage_changelog::ChangelogStage;
    use anodize_stage_release::ReleaseStage;
    use anodize_stage_publish::PublishStage;
    use anodize_stage_docker::DockerStage;
    use anodize_stage_sign::SignStage;
    use anodize_stage_announce::AnnounceStage;

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
