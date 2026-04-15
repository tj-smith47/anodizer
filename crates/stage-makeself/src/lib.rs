use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// LSM (Linux Software Map) metadata
// ---------------------------------------------------------------------------

struct Lsm {
    title: String,
    version: String,
    description: String,
    keywords: Vec<String>,
    maintained_by: String,
    primary_site: String,
    platform: String,
    copying_policy: String,
}

impl Lsm {
    fn render(&self) -> String {
        let mut sb = String::from("Begin4\n");
        let mut w = |name: &str, value: &str| {
            if !value.is_empty() {
                sb.push_str(&format!("{}: {}\n", name, value));
            }
        };
        w("Title", &self.title);
        w("Version", &self.version);
        w("Description", &self.description);
        if !self.keywords.is_empty() {
            w("Keywords", &self.keywords.join(", "));
        }
        w("Maintained-by", &self.maintained_by);
        w("Author", &self.maintained_by);
        w("Primary-site", &self.primary_site);
        w("Platforms", &self.platform);
        w("Copying-policy", &self.copying_policy);
        sb.push_str("End");
        sb
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the makeself command arguments.
fn make_args(
    name: &str,
    filename: &str,
    compression: Option<&str>,
    script: &str,
    extra_args: &[String],
) -> Vec<String> {
    let mut args = vec!["--quiet".to_string()];

    match compression {
        Some("gzip") => args.push("--gzip".to_string()),
        Some("bzip2") => args.push("--bzip2".to_string()),
        Some("xz") => args.push("--xz".to_string()),
        Some("lzo") => args.push("--lzo".to_string()),
        Some("compress") => args.push("--compress".to_string()),
        Some("none") => args.push("--nocomp".to_string()),
        _ => {} // let makeself choose default
    }

    args.push("--lsm".to_string());
    args.push("package.lsm".to_string());

    args.extend(extra_args.iter().cloned());

    // positional args: archive_dir output_file label startup_script
    args.push(".".to_string());
    args.push(filename.to_string());
    args.push(name.to_string());
    args.push(script.to_string());

    args
}

/// Group artifacts by platform string (e.g. "linux_amd64").
fn group_by_platform(artifacts: &[Artifact]) -> HashMap<String, Vec<&Artifact>> {
    let mut groups: HashMap<String, Vec<&Artifact>> = HashMap::new();
    for a in artifacts {
        let platform = match &a.target {
            Some(t) => {
                let (os, arch) = anodize_core::target::map_target(t);
                format!("{}_{}", os, arch)
            }
            None => "unknown".to_string(),
        };
        groups.entry(platform).or_default().push(a);
    }
    groups
}

// ---------------------------------------------------------------------------
// MakeselfStage
// ---------------------------------------------------------------------------

pub struct MakeselfStage;

impl Stage for MakeselfStage {
    fn name(&self) -> &str {
        "makeself"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("makeself");
        let configs = ctx.config.makeselfs.clone();

        if configs.is_empty() {
            return Ok(());
        }

        let dist = ctx.config.dist.clone();
        let dry_run = ctx.options.dry_run;

        // Validate IDs are unique
        let mut seen_ids = std::collections::HashSet::new();
        for cfg in &configs {
            let id = cfg.id.as_deref().unwrap_or("default");
            if !seen_ids.insert(id.to_string()) {
                anyhow::bail!("makeself: duplicate id '{}'", id);
            }
        }

        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());
        let project_name = ctx.config.project_name.clone();

        for cfg in &configs {
            // Check disable
            if let Some(ref d) = cfg.disable
                && d.is_disabled(|tmpl| ctx.render_template(tmpl))
            {
                log.verbose("skipping disabled makeself config");
                continue;
            }

            let id = cfg.id.as_deref().unwrap_or("default");
            let name = cfg.name.as_deref().unwrap_or(&project_name);
            // GoReleaser makeself.go:31 default name_template:
            //   {{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}
            //   {{ with .Arm }}v{{ . }}{{ end }}
            //   {{ with .Mips }}_{{ . }}{{ end }}
            //   {{ if not (eq .Amd64 "v1") }}{{ .Amd64 }}{{ end }}.run
            // Rendered here using the Tera-style syntax anodize exposes.
            let default_name_template = concat!(
                "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}",
                "{% if Arm %}v{{ Arm }}{% endif %}",
                "{% if Mips %}_{{ Mips }}{% endif %}",
                "{% if Amd64 and Amd64 != \"v1\" %}{{ Amd64 }}{% endif %}.run",
            );
            let name_template = cfg
                .name_template
                .as_deref()
                .unwrap_or(default_name_template);

            let script = cfg.script.as_deref().unwrap_or("");
            if script.is_empty() {
                anyhow::bail!("makeself: 'script' is required for config id '{}'", id);
            }

            // Default goos: linux and darwin
            let goos_filter: Vec<String> = cfg
                .goos
                .clone()
                .unwrap_or_else(|| vec!["linux".to_string(), "darwin".to_string()]);

            // Collect matching binary artifacts (cloned to release borrow on ctx)
            let all_binaries: Vec<Artifact> = ctx
                .artifacts
                .all()
                .iter()
                .filter(|a| {
                    matches!(
                        a.kind,
                        ArtifactKind::Binary
                            | ArtifactKind::UniversalBinary
                            | ArtifactKind::Header
                            | ArtifactKind::CArchive
                            | ArtifactKind::CShared
                    )
                })
                .filter(|a| {
                    // Filter by IDs if configured
                    if let Some(ref ids) = cfg.ids {
                        let a_id = a.metadata.get("id").map(|s| s.as_str()).unwrap_or("");
                        let a_name = a.metadata.get("name").map(|s| s.as_str()).unwrap_or("");
                        ids.iter().any(|id| id == a_id || id == a_name)
                    } else {
                        true
                    }
                })
                .filter(|a| {
                    // Filter by goos
                    if let Some(ref target) = a.target {
                        let (os, _) = anodize_core::target::map_target(target);
                        goos_filter.iter().any(|g| g == &os)
                    } else {
                        false
                    }
                })
                .filter(|a| {
                    // Filter by goarch if configured
                    if let Some(ref goarch) = cfg.goarch {
                        if let Some(ref target) = a.target {
                            let (_, arch) = anodize_core::target::map_target(target);
                            goarch.iter().any(|g| g == &arch)
                        } else {
                            false
                        }
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();

            if all_binaries.is_empty() {
                anyhow::bail!(
                    "makeself: no binaries found for config '{}' with goos {:?}",
                    id,
                    goos_filter
                );
            }

            let groups = group_by_platform(&all_binaries);

            for (platform, binaries) in &groups {
                let primary = binaries[0];
                let (os, arch) = primary
                    .target
                    .as_deref()
                    .map(anodize_core::target::map_target)
                    .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));

                // Render templates
                ctx.template_vars_mut().set("Os", &os);
                ctx.template_vars_mut().set("Arch", &arch);
                ctx.template_vars_mut()
                    .set("Target", primary.target.as_deref().unwrap_or(""));

                // Per-target variant vars (mirror stage-build/src/lib.rs 1530-1537)
                // so the default name_template can render v7/v8/v1/mips suffixes.
                let first_component = primary
                    .target
                    .as_deref()
                    .and_then(|t| t.split('-').next())
                    .unwrap_or("");
                // Clear previous values so each target starts clean.
                ctx.template_vars_mut().set("Arm", "");
                ctx.template_vars_mut().set("Arm64", "");
                ctx.template_vars_mut().set("Amd64", "");
                ctx.template_vars_mut().set("Mips", "");
                ctx.template_vars_mut().set("I386", "");
                match first_component {
                    "aarch64" => ctx.template_vars_mut().set("Arm64", "v8"),
                    "armv7" | "armv7l" => ctx.template_vars_mut().set("Arm", "7"),
                    "armv6" | "armv6l" | "arm" => ctx.template_vars_mut().set("Arm", "6"),
                    "x86_64" => ctx.template_vars_mut().set("Amd64", "v1"),
                    "i686" | "i386" | "i586" => ctx.template_vars_mut().set("I386", "sse2"),
                    c if c.starts_with("mips") => {
                        // Set Mips variant (mips, mipsel, mips64, mips64el)
                        ctx.template_vars_mut().set("Mips", c);
                    }
                    _ => {}
                }

                let rendered_name = if cfg.name.is_some() {
                    ctx.render_template(name)?
                } else {
                    project_name.clone()
                };

                let filename = if !name_template.is_empty() {
                    let rendered = ctx.render_template(name_template)?;
                    if rendered.ends_with(".run") {
                        rendered
                    } else {
                        format!("{}.run", rendered)
                    }
                } else {
                    format!("{}_{}_{}_{}.run", project_name, version, os, arch)
                };

                let rendered_description = cfg
                    .description
                    .as_deref()
                    .map(|d| ctx.render_template(d))
                    .transpose()?
                    .unwrap_or_default();
                let rendered_maintainer = cfg
                    .maintainer
                    .as_deref()
                    .map(|m| ctx.render_template(m))
                    .transpose()?
                    .unwrap_or_default();
                let rendered_homepage = cfg
                    .homepage
                    .as_deref()
                    .map(|h| ctx.render_template(h))
                    .transpose()?
                    .unwrap_or_default();
                let rendered_license = cfg
                    .license
                    .as_deref()
                    .map(|l| ctx.render_template(l))
                    .transpose()?
                    .unwrap_or_default();
                let rendered_script = ctx.render_template(script)?;
                let rendered_compression = cfg
                    .compression
                    .as_deref()
                    .map(|c| ctx.render_template(c))
                    .transpose()?;

                let keywords: Vec<String> = cfg
                    .keywords
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|k| ctx.render_template(k))
                    .collect::<Result<Vec<_>>>()?;

                let extra_args: Vec<String> = cfg
                    .extra_args
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .map(|a| ctx.render_template(a))
                    .collect::<Result<Vec<_>>>()?;

                // Build LSM metadata
                let lsm = Lsm {
                    title: rendered_name.clone(),
                    version: version.clone(),
                    description: rendered_description,
                    keywords,
                    maintained_by: rendered_maintainer,
                    primary_site: rendered_homepage,
                    platform: platform.clone(),
                    copying_policy: rendered_license,
                };

                // Set up working directory
                let work_dir = dist.join("makeself").join(id).join(platform);

                if dry_run {
                    log.status(&format!(
                        "(dry-run) would create makeself package: {}",
                        filename
                    ));
                    continue;
                }

                fs::create_dir_all(&work_dir)
                    .with_context(|| format!("makeself: create dir {}", work_dir.display()))?;

                // Copy binaries
                for binary in binaries {
                    let dst = work_dir.join(&binary.name);
                    if let Some(parent) = dst.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(&binary.path, &dst).with_context(|| {
                        format!(
                            "makeself: copy binary {} -> {}",
                            binary.path.display(),
                            dst.display()
                        )
                    })?;
                }

                // Copy extra files
                if let Some(ref files) = cfg.files {
                    for f in files {
                        let src = Path::new(&f.source);
                        let dest_name = if let Some(ref dst) = f.destination {
                            dst.as_str()
                        } else if f.strip_parent.unwrap_or(false) {
                            // strip_parent: use only the filename, dropping parent dirs
                            src.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or(&f.source)
                        } else {
                            // default: preserve the relative path as-is
                            &f.source
                        };
                        let dst = work_dir.join(dest_name);
                        if let Some(parent) = dst.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::copy(src, &dst).with_context(|| {
                            format!("makeself: copy file {} -> {}", src.display(), dst.display())
                        })?;
                    }
                }

                // Copy startup script
                let script_path = Path::new(&rendered_script);
                let script_basename = script_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("setup.sh");
                fs::copy(script_path, work_dir.join(script_basename))
                    .with_context(|| format!("makeself: copy script {}", script_path.display()))?;

                // Write LSM file
                fs::write(work_dir.join("package.lsm"), lsm.render()).with_context(|| {
                    format!("makeself: write LSM file in {}", work_dir.display())
                })?;

                // Build makeself command
                let args = make_args(
                    &rendered_name,
                    &filename,
                    rendered_compression.as_deref(),
                    &format!("./{}", script_basename),
                    &extra_args,
                );

                log.status(&format!("creating makeself package: {}", filename));

                let output = Command::new("makeself")
                    .args(&args)
                    .current_dir(&work_dir)
                    .output()
                    .with_context(|| {
                        format!(
                            "makeself: failed to spawn 'makeself {}' in {}",
                            args.join(" "),
                            work_dir.display()
                        )
                    })?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    anyhow::bail!(
                        "makeself command failed for '{}' (id={}): {}{}",
                        filename,
                        id,
                        stdout,
                        stderr
                    );
                }

                // Move the generated archive from the work dir to the dist root
                let built_path = work_dir.join(&filename);
                let output_path = dist.join(&filename);
                fs::rename(&built_path, &output_path)
                    .or_else(|_| {
                        // rename fails across filesystems; fall back to copy+remove
                        fs::copy(&built_path, &output_path)?;
                        fs::remove_file(&built_path)
                    })
                    .with_context(|| {
                        format!(
                            "makeself: move {} -> {}",
                            built_path.display(),
                            output_path.display()
                        )
                    })?;

                // Register artifact
                let mut metadata = HashMap::new();
                metadata.insert("id".to_string(), id.to_string());
                metadata.insert("format".to_string(), "makeself".to_string());

                // GoReleaser parity: copy ExtraReplaces ("replaces") metadata
                // from the source binary artifact to the makeself artifact.
                if let Some(replaces) = primary.metadata.get("replaces") {
                    metadata.insert("replaces".to_string(), replaces.clone());
                }

                ctx.artifacts.add(Artifact {
                    kind: ArtifactKind::Makeself,
                    name: filename.clone(),
                    path: output_path,
                    target: primary.target.clone(),
                    crate_name: primary.crate_name.clone(),
                    metadata,
                    size: None,
                });
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::config::MakeselfConfig;
    use std::path::PathBuf;

    #[test]
    fn test_lsm_render() {
        let lsm = Lsm {
            title: "MyApp".to_string(),
            version: "1.0.0".to_string(),
            description: "A test application".to_string(),
            keywords: vec!["test".to_string(), "app".to_string()],
            maintained_by: "Test User".to_string(),
            primary_site: "https://example.com".to_string(),
            platform: "linux_amd64".to_string(),
            copying_policy: "MIT".to_string(),
        };
        let rendered = lsm.render();
        assert!(rendered.starts_with("Begin4\n"));
        assert!(rendered.ends_with("End"));
        assert!(rendered.contains("Title: MyApp"));
        assert!(rendered.contains("Version: 1.0.0"));
        assert!(rendered.contains("Keywords: test, app"));
        assert!(rendered.contains("Copying-policy: MIT"));
    }

    #[test]
    fn test_make_args_default_compression() {
        let args = make_args("MyApp", "myapp.run", None, "./setup.sh", &[]);
        assert_eq!(args[0], "--quiet");
        assert!(args.contains(&"--lsm".to_string()));
        assert!(args.contains(&"package.lsm".to_string()));
        assert!(args.contains(&".".to_string()));
        assert!(args.contains(&"myapp.run".to_string()));
        assert!(args.contains(&"MyApp".to_string()));
        assert!(args.contains(&"./setup.sh".to_string()));
    }

    #[test]
    fn test_make_args_xz_compression() {
        let args = make_args("MyApp", "myapp.run", Some("xz"), "./setup.sh", &[]);
        assert!(args.contains(&"--xz".to_string()));
    }

    #[test]
    fn test_make_args_no_compression() {
        let args = make_args("MyApp", "myapp.run", Some("none"), "./setup.sh", &[]);
        assert!(args.contains(&"--nocomp".to_string()));
    }

    #[test]
    fn test_make_args_extra_args() {
        let extra = vec!["--noprogress".to_string(), "--nox11".to_string()];
        let args = make_args("MyApp", "myapp.run", None, "./setup.sh", &extra);
        assert!(args.contains(&"--noprogress".to_string()));
        assert!(args.contains(&"--nox11".to_string()));
    }

    #[test]
    fn test_group_by_platform() {
        let a1 = Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: PathBuf::from("/dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        let a2 = Artifact {
            kind: ArtifactKind::Binary,
            name: "myapp".to_string(),
            path: PathBuf::from("/dist/myapp-darwin"),
            target: Some("aarch64-apple-darwin".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        };
        let artifacts = vec![a1, a2];
        let groups = group_by_platform(&artifacts);
        assert_eq!(groups.len(), 2);
        assert!(groups.contains_key("linux_amd64"));
        assert!(groups.contains_key("darwin_arm64"));
    }

    #[test]
    fn test_makeself_stage_skips_empty_configs() {
        let mut ctx = Context::new(
            anodize_core::config::Config::default(),
            anodize_core::context::ContextOptions::default(),
        );
        let stage = MakeselfStage;
        // No makeself configs → no-op
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_makeself_config_parsing() {
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
makeselfs:
  - id: default
    script: install.sh
    compression: xz
    goos:
      - linux
    files:
      - src: README.md
        dst: README.md
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.makeselfs.len(), 1);
        let ms = &config.makeselfs[0];
        assert_eq!(ms.id.as_deref(), Some("default"));
        assert_eq!(ms.script.as_deref(), Some("install.sh"));
        assert_eq!(ms.compression.as_deref(), Some("xz"));
        assert_eq!(ms.goos.as_ref().unwrap(), &["linux"]);
        assert_eq!(ms.files.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_makeself_config_single_object() {
        use anodize_core::config::Config;

        let yaml = r#"
project_name: test
makeself:
  script: install.sh
crates:
  - name: a
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.makeselfs.len(), 1);
        assert_eq!(config.makeselfs[0].script.as_deref(), Some("install.sh"));
    }

    #[test]
    fn test_makeself_requires_script() {
        let mut ctx = Context::new(
            anodize_core::config::Config::default(),
            anodize_core::context::ContextOptions::default(),
        );
        ctx.config.makeselfs = vec![MakeselfConfig {
            id: Some("test".to_string()),
            ..Default::default()
        }];
        let stage = MakeselfStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("script"),
            "should require script field"
        );
    }
}
