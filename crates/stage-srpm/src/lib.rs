use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::SrpmConfig;
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// SrpmStage
// ---------------------------------------------------------------------------

pub struct SrpmStage;

impl Stage for SrpmStage {
    fn name(&self) -> &str {
        "srpm"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("srpm");
        let srpm_cfg = match ctx.config.srpm.clone() {
            Some(cfg) if cfg.enabled.unwrap_or(false) => cfg,
            _ => return Ok(()),
        };

        // Check disable
        if let Some(ref d) = srpm_cfg.disable
            && d.is_disabled(|tmpl| ctx.render_template(tmpl))
        {
            log.verbose("skipping disabled SRPM config");
            return Ok(());
        }

        // GoReleaser parity: when global skip_sign is active, clear signature config
        let skip_sign = ctx.should_skip("sign");

        let dist = ctx.config.dist.clone();
        let dry_run = ctx.options.dry_run;
        let project_name = ctx.config.project_name.clone();
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());

        // Find source archives — clone to release borrow on ctx
        let source_archives: Vec<Artifact> = ctx
            .artifacts
            .all()
            .iter()
            .filter(|a| a.kind == ArtifactKind::SourceArchive)
            .cloned()
            .collect();

        if source_archives.is_empty() {
            if ctx.options.snapshot || dry_run {
                log.verbose("skipping SRPM: no source archives found (snapshot/dry-run mode)");
                return Ok(());
            }
            anyhow::bail!("srpm: no source archives found. Enable the source stage first.");
        }
        if source_archives.len() > 1 {
            anyhow::bail!(
                "srpm: multiple source archives found ({}). Expected exactly one.",
                source_archives.len()
            );
        }

        let source_archive = &source_archives[0];
        let package_name = srpm_cfg.package_name.as_deref().unwrap_or(&project_name);

        // Read and render the spec file template
        let spec_file = srpm_cfg.spec_file.as_deref().unwrap_or({
            // No spec file configured — we'll generate a minimal one
            ""
        });

        let spec_contents = if spec_file.is_empty() {
            // Generate a minimal spec file
            generate_default_spec(package_name, &version, &srpm_cfg, &source_archive.name)
        } else {
            // Read the user-provided spec template and render it
            let template = fs::read_to_string(spec_file)
                .with_context(|| format!("srpm: read spec file '{}'", spec_file))?;

            // Set SRPM-specific template vars
            ctx.template_vars_mut().set("PackageName", package_name);
            ctx.template_vars_mut().set("Source", &source_archive.name);
            if let Some(ref summary) = srpm_cfg.summary {
                ctx.template_vars_mut().set("Summary", summary);
            }
            if let Some(ref group) = srpm_cfg.group {
                ctx.template_vars_mut().set("Group", group);
            }
            if let Some(ref license) = srpm_cfg.license {
                ctx.template_vars_mut().set("License", license);
            }
            if let Some(ref url) = srpm_cfg.url {
                ctx.template_vars_mut().set("URL", url);
            }
            if let Some(ref description) = srpm_cfg.description {
                ctx.template_vars_mut().set("Description", description);
            }
            if let Some(ref maintainer) = srpm_cfg.maintainer {
                ctx.template_vars_mut().set("Maintainer", maintainer);
            }
            if let Some(ref vendor) = srpm_cfg.vendor {
                ctx.template_vars_mut().set("Vendor", vendor);
            }
            if let Some(ref packager) = srpm_cfg.packager {
                ctx.template_vars_mut().set("Packager", packager);
            }

            ctx.render_template(&template)
                .with_context(|| format!("srpm: render spec template '{}'", spec_file))?
        };

        // Determine output filename
        let file_name_template = srpm_cfg
            .file_name_template
            .as_deref()
            .unwrap_or("{{ PackageName }}-{{ Version }}.src.rpm");

        ctx.template_vars_mut().set("PackageName", package_name);

        let package_filename = ctx
            .render_template(file_name_template)
            .with_context(|| "srpm: render file_name_template")?;
        let package_filename = if package_filename.ends_with(".src.rpm") {
            package_filename
        } else {
            format!("{}.src.rpm", package_filename)
        };

        if dry_run {
            log.status(&format!(
                "(dry-run) would create source RPM: {}",
                package_filename
            ));
            return Ok(());
        }

        // Write spec file
        let spec_path = dist.join(format!("{}.srpm.spec", package_name));
        fs::create_dir_all(&dist)
            .with_context(|| format!("srpm: create dist dir {}", dist.display()))?;
        fs::write(&spec_path, &spec_contents)
            .with_context(|| format!("srpm: write spec file {}", spec_path.display()))?;

        log.status(&format!("creating source RPM: {}", package_filename));

        // Build the SRPM using rpmbuild -bs
        let srpm_path = dist.join(&package_filename);

        // Create rpmbuild directory structure
        let rpmbuild_dir = dist.join("rpmbuild");
        let sources_dir = rpmbuild_dir.join("SOURCES");
        let specs_dir = rpmbuild_dir.join("SPECS");
        let srpms_dir = rpmbuild_dir.join("SRPMS");
        for dir in &[&sources_dir, &specs_dir, &srpms_dir] {
            fs::create_dir_all(dir)?;
        }

        // Copy source archive to SOURCES
        fs::copy(&source_archive.path, sources_dir.join(&source_archive.name))
            .with_context(|| "srpm: copy source archive to rpmbuild SOURCES")?;

        // Copy spec file to SPECS
        let spec_dest = specs_dir.join(format!("{}.spec", package_name));
        fs::copy(&spec_path, &spec_dest).with_context(|| "srpm: copy spec to rpmbuild SPECS")?;

        // Resolve signature configuration (GoReleaser parity: skip_sign + SRPM_PASSPHRASE)
        let effective_signature = if skip_sign {
            None
        } else {
            srpm_cfg.signature.as_ref()
        };

        // Run rpmbuild
        let mut rpmbuild_cmd = Command::new("rpmbuild");
        rpmbuild_cmd
            .arg("-bs")
            .arg("--define")
            .arg(format!("_topdir {}", rpmbuild_dir.display()));

        // Wire signing options when signature config is present
        if let Some(sig) = effective_signature
            && let Some(ref key_file) = sig.key_file
        {
            rpmbuild_cmd
                .arg("--define")
                .arg(format!("_gpg_name {}", key_file));
            rpmbuild_cmd.arg("--sign");

            // GoReleaser parity: read SRPM_PASSPHRASE env var when no
            // passphrase is configured inline.
            if let Some(ref passphrase) = sig.passphrase {
                rpmbuild_cmd.env("GPG_PASSPHRASE", passphrase);
            } else if let Ok(passphrase) = std::env::var("SRPM_PASSPHRASE")
                && !passphrase.is_empty()
            {
                rpmbuild_cmd.env("GPG_PASSPHRASE", &passphrase);
            }
        }

        rpmbuild_cmd.arg(&spec_dest);
        let output = rpmbuild_cmd
            .output()
            .with_context(|| "srpm: failed to spawn rpmbuild")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            anyhow::bail!("rpmbuild -bs failed:\n{}{}", stdout, stderr);
        }

        // Find the generated SRPM in SRPMS/
        let generated: Vec<PathBuf> = glob::glob(&format!("{}/**/*.src.rpm", srpms_dir.display()))
            .into_iter()
            .flat_map(|entries| entries.filter_map(|e| e.ok()))
            .collect();

        let generated_path = generated.first().ok_or_else(|| {
            anyhow::anyhow!("srpm: rpmbuild succeeded but no .src.rpm found in SRPMS/")
        })?;

        // Move to dist with the desired filename
        fs::copy(generated_path, &srpm_path).with_context(|| {
            format!(
                "srpm: copy {} -> {}",
                generated_path.display(),
                srpm_path.display()
            )
        })?;

        // Register artifact
        let mut metadata = HashMap::new();
        metadata.insert("format".to_string(), "srpm".to_string());

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::SourceRpm,
            name: package_filename,
            path: srpm_path,
            target: None,
            crate_name: project_name,
            metadata,
            size: None,
        });

        Ok(())
    }
}

/// Generate a minimal RPM spec file when no user template is provided.
fn generate_default_spec(
    package_name: &str,
    version: &str,
    cfg: &SrpmConfig,
    source_name: &str,
) -> String {
    let summary = cfg.summary.as_deref().unwrap_or(package_name);
    let license = cfg.license.as_deref().unwrap_or("MIT");
    let url = cfg.url.as_deref().unwrap_or("");
    let description = cfg.description.as_deref().unwrap_or(package_name);

    let maintainer = cfg.maintainer.as_deref().unwrap_or(package_name);
    format!(
        r#"Name:           {package_name}
Version:        {version}
Release:        1%{{?dist}}
Summary:        {summary}
License:        {license}
URL:            {url}
Source0:        {source_name}

%description
{description}

%prep
%autosetup

%build

%install

%files

%changelog
* {date} {maintainer} - {version}-1
- Release {version}
"#,
        date = chrono::Utc::now().format("%a %b %d %Y"),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_srpm_stage_skips_when_not_enabled() {
        let mut ctx = Context::new(
            anodize_core::config::Config::default(),
            anodize_core::context::ContextOptions::default(),
        );
        let stage = SrpmStage;
        // No srpm config → no-op
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_srpm_stage_skips_when_disabled() {
        let mut ctx = Context::new(
            anodize_core::config::Config::default(),
            anodize_core::context::ContextOptions::default(),
        );
        ctx.config.srpm = Some(SrpmConfig {
            enabled: Some(false),
            ..Default::default()
        });
        let stage = SrpmStage;
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_srpm_requires_source_archive() {
        let mut ctx = Context::new(
            anodize_core::config::Config::default(),
            anodize_core::context::ContextOptions::default(),
        );
        ctx.config.srpm = Some(SrpmConfig {
            enabled: Some(true),
            ..Default::default()
        });
        let stage = SrpmStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no source archives"),
            "should require source archive"
        );
    }

    #[test]
    fn test_generate_default_spec() {
        let cfg = SrpmConfig {
            summary: Some("A test package".to_string()),
            license: Some("Apache-2.0".to_string()),
            url: Some("https://example.com".to_string()),
            description: Some("Test description".to_string()),
            ..Default::default()
        };
        let spec = generate_default_spec("myapp", "1.0.0", &cfg, "myapp-1.0.0.tar.gz");
        assert!(spec.contains("Name:           myapp"));
        assert!(spec.contains("Version:        1.0.0"));
        assert!(spec.contains("Summary:        A test package"));
        assert!(spec.contains("License:        Apache-2.0"));
        assert!(spec.contains("Source0:        myapp-1.0.0.tar.gz"));
    }

    #[test]
    fn test_srpm_config_parsing() {
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
srpm:
  enabled: true
  package_name: myapp
  spec_file: myapp.spec
  summary: "My application"
  license: MIT
  url: "https://example.com"
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let srpm = config.srpm.as_ref().unwrap();
        assert_eq!(srpm.enabled, Some(true));
        assert_eq!(srpm.package_name.as_deref(), Some("myapp"));
        assert_eq!(srpm.spec_file.as_deref(), Some("myapp.spec"));
        assert_eq!(srpm.summary.as_deref(), Some("My application"));
    }
}
