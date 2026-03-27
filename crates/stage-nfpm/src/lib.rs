use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::NfpmConfig;
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// generate_nfpm_yaml
// ---------------------------------------------------------------------------

/// Generate an nfpm YAML configuration string from the anodize nfpm config.
pub fn generate_nfpm_yaml(config: &NfpmConfig, version: &str, binary_path: &str) -> String {
    let mut lines: Vec<String> = Vec::new();

    // Required fields
    if let Some(name) = &config.package_name {
        lines.push(format!("name: {name}"));
    }
    lines.push(format!("version: {version}"));

    // Optional metadata
    if let Some(vendor) = &config.vendor {
        lines.push(format!("vendor: {vendor}"));
    }
    if let Some(homepage) = &config.homepage {
        lines.push(format!("homepage: {homepage}"));
    }
    if let Some(maintainer) = &config.maintainer {
        lines.push(format!("maintainer: {maintainer}"));
    }
    if let Some(description) = &config.description {
        lines.push(format!("description: {description}"));
    }
    if let Some(license) = &config.license {
        lines.push(format!("license: {license}"));
    }

    // Scripts section
    if let Some(scripts) = &config.scripts {
        let mut has_script = false;
        let mut script_lines: Vec<String> = Vec::new();
        if let Some(pre) = &scripts.preinstall {
            script_lines.push(format!("  preinstall: {pre}"));
            has_script = true;
        }
        if let Some(post) = &scripts.postinstall {
            script_lines.push(format!("  postinstall: {post}"));
            has_script = true;
        }
        if let Some(pre) = &scripts.preremove {
            script_lines.push(format!("  preremove: {pre}"));
            has_script = true;
        }
        if let Some(post) = &scripts.postremove {
            script_lines.push(format!("  postremove: {post}"));
            has_script = true;
        }
        if has_script {
            lines.push("scripts:".to_string());
            lines.extend(script_lines);
        }
    }

    // Package relationship metadata
    fn push_string_list(lines: &mut Vec<String>, key: &str, items: &Option<Vec<String>>) {
        if let Some(list) = items
            && !list.is_empty()
        {
            lines.push(format!("{key}:"));
            for item in list {
                lines.push(format!("  - {item}"));
            }
        }
    }
    push_string_list(&mut lines, "recommends", &config.recommends);
    push_string_list(&mut lines, "suggests", &config.suggests);
    push_string_list(&mut lines, "conflicts", &config.conflicts);
    push_string_list(&mut lines, "replaces", &config.replaces);
    push_string_list(&mut lines, "provides", &config.provides);

    // Contents section — always include the binary
    lines.push("contents:".to_string());
    let bindir = config.bindir.as_deref().unwrap_or("/usr/local/bin");
    let binary_name = PathBuf::from(binary_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("binary")
        .to_string();
    lines.push(format!("  - src: {binary_path}"));
    lines.push(format!("    dst: {bindir}/{binary_name}"));

    // Extra contents from config
    if let Some(contents) = &config.contents {
        for entry in contents {
            lines.push(format!("  - src: {}", entry.src));
            lines.push(format!("    dst: {}", entry.dst));
            if let Some(ct) = &entry.content_type {
                lines.push(format!("    type: {ct}"));
            }
            if let Some(fi) = &entry.file_info {
                lines.push("    file_info:".to_string());
                if let Some(owner) = &fi.owner {
                    lines.push(format!("      owner: {owner}"));
                }
                if let Some(group) = &fi.group {
                    lines.push(format!("      group: {group}"));
                }
                if let Some(mode) = &fi.mode {
                    lines.push(format!("      mode: \"{mode}\""));
                }
            }
        }
    }

    // Per-format overrides
    if let Some(overrides) = &config.overrides
        && !overrides.is_empty()
    {
        lines.push("overrides:".to_string());
        for (fmt, val) in overrides {
            lines.push(format!("  {fmt}:"));
            if let Some(obj) = val.as_object() {
                for (k, v) in obj {
                    lines.push(format!("    {k}: {v}"));
                }
            }
        }
    }

    // Per-format dependencies
    if let Some(deps) = &config.dependencies
        && !deps.is_empty()
    {
        lines.push("dependencies:".to_string());
        for (fmt, dep_list) in deps {
            lines.push(format!("  {fmt}:"));
            for dep in dep_list {
                lines.push(format!("    - {dep}"));
            }
        }
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// nfpm_command
// ---------------------------------------------------------------------------

/// Construct the nfpm CLI command arguments.
pub fn nfpm_command(config_path: &str, format: &str, output_dir: &str) -> Vec<String> {
    vec![
        "nfpm".to_string(),
        "pkg".to_string(),
        "--config".to_string(),
        config_path.to_string(),
        "--packager".to_string(),
        format.to_string(),
        "--target".to_string(),
        output_dir.to_string(),
    ]
}

// ---------------------------------------------------------------------------
// NfpmStage
// ---------------------------------------------------------------------------

pub struct NfpmStage;

impl Stage for NfpmStage {
    fn name(&self) -> &str {
        "nfpm"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have nfpm config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.nfpm.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // Resolve version from template vars
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());

        let mut new_artifacts: Vec<Artifact> = Vec::new();

        for krate in &crates {
            let nfpm_configs = krate.nfpm.as_ref().unwrap();

            // Collect all Linux binary artifacts for this crate
            let linux_binaries: Vec<_> = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                .into_iter()
                .filter(|b| {
                    b.target
                        .as_deref()
                        .map(|t| t.contains("linux"))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            // If no linux binaries found, use a single synthetic entry with a default path
            let effective_binaries: Vec<(Option<String>, String)> = if linux_binaries.is_empty() {
                vec![(None, format!("dist/{}", krate.name))]
            } else {
                linux_binaries
                    .iter()
                    .map(|b| (b.target.clone(), b.path.to_string_lossy().into_owned()))
                    .collect()
            };

            for nfpm_cfg in nfpm_configs {
                for (target, binary_path) in &effective_binaries {
                    // Derive Os/Arch from the target triple for template rendering
                    let (os, arch) = target
                        .as_deref()
                        .map(anodize_core::target::map_target)
                        .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()));

                    // Generate YAML content for this specific binary
                    let yaml_content = generate_nfpm_yaml(nfpm_cfg, &version, binary_path);

                    for format in &nfpm_cfg.formats {
                        // Ensure output directory exists
                        let output_dir = dist.join("linux");
                        if !dry_run {
                            fs::create_dir_all(&output_dir).with_context(|| {
                                format!("create nfpm output dir: {}", output_dir.display())
                            })?;
                        }

                        // Determine package file name (template or default)
                        let pkg_name = nfpm_cfg.package_name.as_deref().unwrap_or(&krate.name);
                        let ext = format_extension(format);
                        let pkg_filename = if let Some(tmpl) = &nfpm_cfg.file_name_template {
                            // Set Os/Arch in template vars temporarily
                            ctx.template_vars_mut().set("Os", &os);
                            ctx.template_vars_mut().set("Arch", &arch);
                            let rendered = ctx.render_template(tmpl).with_context(|| {
                                format!(
                                    "nfpm: render file_name_template for crate {} target {:?}",
                                    krate.name, target
                                )
                            })?;
                            format!("{rendered}{ext}")
                        } else {
                            format!("{pkg_name}_{version}{ext}")
                        };
                        let pkg_path = output_dir.join(&pkg_filename);

                        if dry_run {
                            eprintln!(
                                "[nfpm] (dry-run) would run: nfpm pkg --packager {format} for crate {} target {:?}",
                                krate.name, target
                            );
                            new_artifacts.push(Artifact {
                                kind: ArtifactKind::LinuxPackage,
                                path: pkg_path,
                                target: target.clone(),
                                crate_name: krate.name.clone(),
                                metadata: {
                                    let mut m = HashMap::new();
                                    m.insert("format".to_string(), format.clone());
                                    m
                                },
                            });
                            continue;
                        }

                        // Write temp nfpm YAML config
                        let tmp_dir =
                            tempfile::tempdir().context("create temp dir for nfpm config")?;
                        let config_path = tmp_dir.path().join("nfpm.yaml");
                        fs::write(&config_path, &yaml_content).with_context(|| {
                            format!("write nfpm config to {}", config_path.display())
                        })?;

                        let cmd_args = nfpm_command(
                            &config_path.to_string_lossy(),
                            format,
                            &output_dir.to_string_lossy(),
                        );

                        eprintln!("[nfpm] running: {}", cmd_args.join(" "));

                        let status = Command::new(&cmd_args[0])
                            .args(&cmd_args[1..])
                            .status()
                            .with_context(|| {
                                format!(
                                    "execute nfpm for format {format} (crate {} target {:?})",
                                    krate.name, target
                                )
                            })?;

                        if !status.success() {
                            anyhow::bail!(
                                "nfpm failed for format {format} (crate {} target {:?}): exit code {:?}",
                                krate.name,
                                target,
                                status.code()
                            );
                        }

                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::LinuxPackage,
                            path: pkg_path,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            metadata: {
                                let mut m = HashMap::new();
                                m.insert("format".to_string(), format.clone());
                                m
                            },
                        });
                    }
                }
            }
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

/// Return the file extension for a given nfpm packager format.
fn format_extension(format: &str) -> &str {
    match format {
        "deb" => ".deb",
        "rpm" => ".rpm",
        "apk" => ".apk",
        "archlinux" => ".pkg.tar.zst",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_generate_nfpm_yaml() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            vendor: Some("Test Vendor".to_string()),
            homepage: Some("https://example.com".to_string()),
            maintainer: Some("test@example.com".to_string()),
            description: Some("A test app".to_string()),
            license: Some("MIT".to_string()),
            bindir: Some("/usr/bin".to_string()),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/path/to/binary");
        assert!(yaml.contains("name: myapp"));
        assert!(yaml.contains("version: 1.0.0"));
        assert!(yaml.contains("vendor: Test Vendor"));
        assert!(yaml.contains("/usr/bin/"));
    }

    #[test]
    fn test_nfpm_command() {
        let cmd = nfpm_command("/tmp/nfpm.yaml", "deb", "/tmp/output");
        assert_eq!(cmd[0], "nfpm");
        assert!(cmd.contains(&"pkg".to_string()));
        assert!(cmd.contains(&"deb".to_string()));
    }

    #[test]
    fn test_stage_skips_when_no_nfpm_config() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

        // NfpmStage should be a no-op when crates have no nfpm block
        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = NfpmStage;
        // Should succeed (no-op)
        assert!(stage.run(&mut ctx).is_ok());
    }

    #[test]
    fn test_generate_nfpm_yaml_with_contents() {
        use anodize_core::config::NfpmContent;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["rpm".to_string()],
            description: Some("desc".to_string()),
            contents: Some(vec![NfpmContent {
                src: "/src/config".to_string(),
                dst: "/etc/myapp/config".to_string(),
                content_type: None,
                file_info: None,
            }]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "2.0.0", "/dist/myapp");
        assert!(yaml.contains("version: 2.0.0"));
        assert!(yaml.contains("/etc/myapp/config"));
        assert!(yaml.contains("/usr/local/bin/myapp"));
    }

    #[test]
    fn test_nfpm_command_structure() {
        let cmd = nfpm_command("/etc/nfpm.yaml", "rpm", "/out");
        assert_eq!(
            cmd,
            vec![
                "nfpm",
                "pkg",
                "--config",
                "/etc/nfpm.yaml",
                "--packager",
                "rpm",
                "--target",
                "/out",
            ]
        );
    }

    #[test]
    fn test_stage_dry_run_registers_artifacts() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string(), "rpm".to_string()],
            ..Default::default()
        };

        let crate_cfg = CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![crate_cfg];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        let stage = NfpmStage;
        stage.run(&mut ctx).unwrap();

        // In dry-run mode, two artifacts (deb + rpm) should be registered
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 2);

        let formats: Vec<&str> = pkgs
            .iter()
            .map(|a| a.metadata.get("format").unwrap().as_str())
            .collect();
        assert!(formats.contains(&"deb"));
        assert!(formats.contains(&"rpm"));
    }

    // Ensure unused import from task spec compiles (tempfile::TempDir is used above)
    #[test]
    fn test_tempdir_compiles() {
        let _tmp = TempDir::new().unwrap();
        let _path = _tmp.path().join("test.yaml");
        fs::write(&_path, "test").unwrap();
        assert!(_path.exists());
    }

    #[test]
    fn test_generate_nfpm_yaml_with_scripts() {
        use anodize_core::config::NfpmScripts;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            scripts: Some(NfpmScripts {
                preinstall: Some("/scripts/preinstall.sh".to_string()),
                postinstall: Some("/scripts/postinstall.sh".to_string()),
                preremove: Some("/scripts/preremove.sh".to_string()),
                postremove: None,
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp");
        assert!(yaml.contains("scripts:"));
        assert!(yaml.contains("  preinstall: /scripts/preinstall.sh"));
        assert!(yaml.contains("  postinstall: /scripts/postinstall.sh"));
        assert!(yaml.contains("  preremove: /scripts/preremove.sh"));
        assert!(!yaml.contains("postremove"));
    }

    #[test]
    fn test_generate_nfpm_yaml_with_package_metadata() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            recommends: Some(vec!["libfoo".to_string(), "libbar".to_string()]),
            suggests: Some(vec!["optional-pkg".to_string()]),
            conflicts: Some(vec!["old-myapp".to_string()]),
            replaces: Some(vec!["old-myapp".to_string()]),
            provides: Some(vec!["myapp-bin".to_string()]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp");
        assert!(yaml.contains("recommends:"));
        assert!(yaml.contains("  - libfoo"));
        assert!(yaml.contains("  - libbar"));
        assert!(yaml.contains("suggests:"));
        assert!(yaml.contains("  - optional-pkg"));
        assert!(yaml.contains("conflicts:"));
        assert!(yaml.contains("  - old-myapp"));
        assert!(yaml.contains("replaces:"));
        assert!(yaml.contains("provides:"));
        assert!(yaml.contains("  - myapp-bin"));
    }

    #[test]
    fn test_generate_nfpm_yaml_with_contents_type_and_file_info() {
        use anodize_core::config::{NfpmContent, NfpmFileInfo};
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            contents: Some(vec![NfpmContent {
                src: "/src/myapp.conf".to_string(),
                dst: "/etc/myapp/myapp.conf".to_string(),
                content_type: Some("config".to_string()),
                file_info: Some(NfpmFileInfo {
                    owner: Some("root".to_string()),
                    group: Some("root".to_string()),
                    mode: Some("0644".to_string()),
                }),
            }]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp");
        assert!(yaml.contains("    type: config"));
        assert!(yaml.contains("    file_info:"));
        assert!(yaml.contains("      owner: root"));
        assert!(yaml.contains("      group: root"));
        assert!(yaml.contains("      mode: \"0644\""));
    }

    #[test]
    fn test_generate_nfpm_yaml_contents_without_file_info() {
        use anodize_core::config::NfpmContent;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            contents: Some(vec![NfpmContent {
                src: "/src/data".to_string(),
                dst: "/var/lib/myapp/data".to_string(),
                content_type: Some("dir".to_string()),
                file_info: None,
            }]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp");
        assert!(yaml.contains("    type: dir"));
        assert!(!yaml.contains("file_info"));
    }

    #[test]
    fn test_config_parse_nfpm_scripts() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        scripts:
          preinstall: /scripts/pre.sh
          postinstall: /scripts/post.sh
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = config.crates[0].nfpm.as_ref().unwrap();
        let scripts = nfpm[0].scripts.as_ref().unwrap();
        assert_eq!(scripts.preinstall.as_deref(), Some("/scripts/pre.sh"));
        assert_eq!(scripts.postinstall.as_deref(), Some("/scripts/post.sh"));
        assert!(scripts.preremove.is_none());
        assert!(scripts.postremove.is_none());
    }

    #[test]
    fn test_config_parse_nfpm_package_relationships() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        recommends:
          - libfoo
        suggests:
          - libbar
        conflicts:
          - old-test
        replaces:
          - old-test
        provides:
          - test-bin
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = config.crates[0].nfpm.as_ref().unwrap();
        assert_eq!(nfpm[0].recommends.as_ref().unwrap(), &["libfoo"]);
        assert_eq!(nfpm[0].suggests.as_ref().unwrap(), &["libbar"]);
        assert_eq!(nfpm[0].conflicts.as_ref().unwrap(), &["old-test"]);
        assert_eq!(nfpm[0].replaces.as_ref().unwrap(), &["old-test"]);
        assert_eq!(nfpm[0].provides.as_ref().unwrap(), &["test-bin"]);
    }

    #[test]
    fn test_config_parse_nfpm_contents_with_type_and_file_info() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    nfpm:
      - package_name: test
        formats: [deb]
        contents:
          - src: /src/conf
            dst: /etc/test/conf
            type: config
            file_info:
              owner: root
              group: wheel
              mode: "0755"
"#;
        let config: anodize_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let nfpm = config.crates[0].nfpm.as_ref().unwrap();
        let contents = nfpm[0].contents.as_ref().unwrap();
        assert_eq!(contents[0].content_type.as_deref(), Some("config"));
        let fi = contents[0].file_info.as_ref().unwrap();
        assert_eq!(fi.owner.as_deref(), Some("root"));
        assert_eq!(fi.group.as_deref(), Some("wheel"));
        assert_eq!(fi.mode.as_deref(), Some("0755"));
    }

    #[test]
    fn test_generate_nfpm_yaml_empty_lists_omitted() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            recommends: Some(vec![]),
            suggests: None,
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp");
        // Empty lists should not produce a section
        assert!(!yaml.contains("recommends:"));
        assert!(!yaml.contains("suggests:"));
    }

    // -----------------------------------------------------------------------
    // Task 4C: Additional behavior tests — config fields actually do things
    // -----------------------------------------------------------------------

    #[test]
    fn test_scripts_block_appears_in_generated_yaml() {
        use anodize_core::config::NfpmScripts;
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            scripts: Some(NfpmScripts {
                preinstall: Some("/scripts/pre.sh".to_string()),
                postinstall: Some("/scripts/post.sh".to_string()),
                preremove: Some("/scripts/prerm.sh".to_string()),
                postremove: Some("/scripts/postrm.sh".to_string()),
            }),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp");
        assert!(yaml.contains("scripts:"));
        assert!(yaml.contains("  preinstall: /scripts/pre.sh"));
        assert!(yaml.contains("  postinstall: /scripts/post.sh"));
        assert!(yaml.contains("  preremove: /scripts/prerm.sh"));
        assert!(yaml.contains("  postremove: /scripts/postrm.sh"));
    }

    #[test]
    fn test_all_package_relationship_fields_in_yaml() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            recommends: Some(vec!["libfoo".to_string(), "libbar".to_string()]),
            suggests: Some(vec!["opt-pkg".to_string()]),
            conflicts: Some(vec!["old-myapp".to_string()]),
            replaces: Some(vec!["old-myapp".to_string()]),
            provides: Some(vec!["myapp-bin".to_string()]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp");

        // Each field should appear with its items
        assert!(yaml.contains("recommends:\n  - libfoo\n  - libbar"));
        assert!(yaml.contains("suggests:\n  - opt-pkg"));
        assert!(yaml.contains("conflicts:\n  - old-myapp"));
        assert!(yaml.contains("replaces:\n  - old-myapp"));
        assert!(yaml.contains("provides:\n  - myapp-bin"));
    }

    #[test]
    fn test_contents_type_and_file_info_serialize_correctly() {
        use anodize_core::config::{NfpmContent, NfpmFileInfo};
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            contents: Some(vec![
                NfpmContent {
                    src: "/src/config.toml".to_string(),
                    dst: "/etc/myapp/config.toml".to_string(),
                    content_type: Some("config".to_string()),
                    file_info: Some(NfpmFileInfo {
                        owner: Some("root".to_string()),
                        group: Some("admin".to_string()),
                        mode: Some("0640".to_string()),
                    }),
                },
                NfpmContent {
                    src: "/src/readme".to_string(),
                    dst: "/usr/share/doc/myapp/README".to_string(),
                    content_type: Some("doc".to_string()),
                    file_info: None,
                },
            ]),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "2.0.0", "/dist/myapp");

        // First content entry with type and file_info
        assert!(yaml.contains("  - src: /src/config.toml"));
        assert!(yaml.contains("    dst: /etc/myapp/config.toml"));
        assert!(yaml.contains("    type: config"));
        assert!(yaml.contains("    file_info:"));
        assert!(yaml.contains("      owner: root"));
        assert!(yaml.contains("      group: admin"));
        assert!(yaml.contains("      mode: \"0640\""));

        // Second content entry with type but no file_info
        assert!(yaml.contains("  - src: /src/readme"));
        assert!(yaml.contains("    type: doc"));
    }

    #[test]
    fn test_multiple_formats_in_one_pass() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string(), "rpm".to_string(), "apk".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        NfpmStage.run(&mut ctx).unwrap();

        // Should register 3 artifacts (one per format)
        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 3);

        let formats: Vec<&str> = pkgs
            .iter()
            .map(|a| a.metadata.get("format").unwrap().as_str())
            .collect();
        assert!(formats.contains(&"deb"));
        assert!(formats.contains(&"rpm"));
        assert!(formats.contains(&"apk"));
    }

    #[test]
    fn test_file_name_template_rendering() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            file_name_template: Some(
                "{{ .ProjectName }}_{{ .Version }}_{{ .Os }}_{{ .Arch }}".to_string(),
            ),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "3.0.0");

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);

        // The file path should use the rendered template + extension
        let path_str = pkgs[0].path.file_name().unwrap().to_str().unwrap();
        assert!(
            path_str.starts_with("myapp_3.0.0_"),
            "expected file_name_template to be rendered, got: {}",
            path_str
        );
        assert!(path_str.ends_with(".deb"));
    }

    #[test]
    fn test_artifact_registration_of_linux_package() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        NfpmStage.run(&mut ctx).unwrap();

        let pkgs = ctx.artifacts.by_kind(ArtifactKind::LinuxPackage);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].kind, ArtifactKind::LinuxPackage);
        assert_eq!(pkgs[0].crate_name, "myapp");
        assert_eq!(
            pkgs[0].metadata.get("format"),
            Some(&"deb".to_string())
        );
    }

    #[test]
    fn test_format_extension_mapping() {
        assert_eq!(format_extension("deb"), ".deb");
        assert_eq!(format_extension("rpm"), ".rpm");
        assert_eq!(format_extension("apk"), ".apk");
        assert_eq!(format_extension("archlinux"), ".pkg.tar.zst");
        assert_eq!(format_extension("unknown"), "");
    }

    #[test]
    fn test_nfpm_yaml_binary_path_included_in_contents() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            bindir: Some("/usr/local/bin".to_string()),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/build/myapp");

        // Binary should appear in the contents section
        assert!(yaml.contains("contents:"));
        assert!(yaml.contains("  - src: /build/myapp"));
        assert!(yaml.contains("    dst: /usr/local/bin/myapp"));
    }

    #[test]
    fn test_nfpm_yaml_custom_bindir() {
        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            bindir: Some("/opt/myapp/bin".to_string()),
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/build/myapp");
        assert!(yaml.contains("    dst: /opt/myapp/bin/myapp"));
    }

    // ---- Error path tests (Task 4D) ----

    #[test]
    fn test_nfpm_missing_binary_produces_error_in_live_mode() {
        // When nfpm binary is missing, the stage should fail with a clear error
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false, // live mode
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        let stage = NfpmStage;
        let result = stage.run(&mut ctx);
        // nfpm binary likely not installed in test environment
        assert!(result.is_err(), "nfpm binary missing should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nfpm") || err.contains("execute"),
            "error should mention nfpm or execution failure, got: {err}"
        );
    }

    #[test]
    fn test_format_extension_unknown_returns_empty() {
        // Unknown format returns empty extension
        assert_eq!(format_extension("invalid-format"), "");
        assert_eq!(format_extension(""), "");
        assert_eq!(format_extension("snap"), "");
    }

    #[test]
    fn test_generate_nfpm_yaml_without_package_name() {
        // When package_name is None, it should not appear in YAML
        let nfpm_cfg = NfpmConfig {
            package_name: None,
            formats: vec!["deb".to_string()],
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "1.0.0", "/dist/myapp");
        assert!(!yaml.contains("name:"), "no name: line should appear when package_name is None");
        assert!(yaml.contains("version: 1.0.0"));
    }

    #[test]
    fn test_generate_nfpm_yaml_minimal_config() {
        // A minimal config with just formats should still produce valid YAML
        let nfpm_cfg = NfpmConfig {
            formats: vec!["deb".to_string()],
            ..Default::default()
        };
        let yaml = generate_nfpm_yaml(&nfpm_cfg, "0.1.0", "/bin/test");
        assert!(yaml.contains("version: 0.1.0"));
        assert!(yaml.contains("contents:"));
        assert!(yaml.contains("  - src: /bin/test"));
        assert!(yaml.contains("    dst: /usr/local/bin/test"));
    }

    #[test]
    fn test_invalid_file_name_template_errors() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            // Invalid Tera template — unclosed tag
            file_name_template: Some("{{ bad_template".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true, // dry-run still renders the template
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        let result = NfpmStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "invalid file_name_template should cause a render error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("template") || err.contains("render"),
            "error should mention template rendering, got: {err}"
        );
    }

    #[test]
    fn test_create_output_dir_failure_errors() {
        use anodize_core::config::{Config, CrateConfig, NfpmConfig};
        use anodize_core::context::{Context, ContextOptions};

        let nfpm_cfg = NfpmConfig {
            package_name: Some("myapp".to_string()),
            formats: vec!["deb".to_string()],
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        // Use an impossible path that create_dir_all will fail on
        config.dist = std::path::PathBuf::from("/dev/null/impossible/dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            nfpm: Some(vec![nfpm_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: false, // live mode triggers create_dir_all
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "1.0.0");

        let result = NfpmStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "creating output dir under /dev/null should fail"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("nfpm") || err.contains("dir") || err.contains("create"),
            "error should mention directory creation context, got: {err}"
        );
    }
}
