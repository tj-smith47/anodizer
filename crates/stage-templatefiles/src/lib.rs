use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::parse_octal_mode;
use anodize_core::context::Context;
use anodize_core::stage::Stage;

/// Default file permission mode (octal 0o655 = decimal 429).
const DEFAULT_MODE: u32 = 0o655;

pub struct TemplateFilesStage;

impl Stage for TemplateFilesStage {
    fn name(&self) -> &str {
        "templatefiles"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let entries = match ctx.config.template_files {
            Some(ref entries) if !entries.is_empty() => entries.clone(),
            _ => return Ok(()),
        };

        let log = ctx.logger("templatefiles");
        let dist = ctx.config.dist.clone();
        std::fs::create_dir_all(&dist).with_context(|| {
            format!(
                "templatefiles: failed to create dist dir: {}",
                dist.display()
            )
        })?;

        for entry in &entries {
            let id = entry.id.as_deref().unwrap_or("default");

            // Render the src path through the template engine
            let rendered_src = ctx.render_template(&entry.src).with_context(|| {
                format!("templatefiles: failed to render src path for id '{}'", id)
            })?;

            // Read the source file
            let src_path = PathBuf::from(&rendered_src);
            let src_contents = std::fs::read_to_string(&src_path).with_context(|| {
                format!(
                    "templatefiles: source file '{}' not found (id: '{}')",
                    src_path.display(),
                    id
                )
            })?;

            // Render the file contents through the template engine
            let rendered_contents = ctx.render_template(&src_contents).with_context(|| {
                format!(
                    "templatefiles: failed to render contents of '{}' (id: '{}')",
                    src_path.display(),
                    id
                )
            })?;

            // Render the dst path through the template engine
            let rendered_dst = ctx.render_template(&entry.dst).with_context(|| {
                format!("templatefiles: failed to render dst path for id '{}'", id)
            })?;

            // Reject path traversal attempts
            if rendered_dst.contains("..") || Path::new(&rendered_dst).is_absolute() {
                bail!(
                    "templatefiles: dst '{}' must be a relative path within dist (no '..' or absolute paths)",
                    rendered_dst
                );
            }

            // Write to dist/dst
            let output_path = dist.join(&rendered_dst);
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "templatefiles: failed to create parent dir for '{}'",
                        output_path.display()
                    )
                })?;
            }
            std::fs::write(&output_path, &rendered_contents).with_context(|| {
                format!("templatefiles: failed to write '{}'", output_path.display())
            })?;

            // Set file permissions
            let _mode = match &entry.mode {
                Some(mode_str) => match parse_octal_mode(mode_str) {
                    Some(m) => m,
                    None => anyhow::bail!(
                        "templatefiles: invalid mode '{}' for id '{}' (expected octal like \"0755\")",
                        mode_str,
                        id
                    ),
                },
                None => DEFAULT_MODE,
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&output_path, std::fs::Permissions::from_mode(_mode))
                    .with_context(|| {
                        format!(
                            "templatefiles: failed to set permissions on '{}'",
                            output_path.display()
                        )
                    })?;
            }

            log.status(&format!(
                "rendered '{}' -> '{}'",
                rendered_src,
                output_path.display()
            ));

            // Register as an artifact
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::UploadableFile,
                path: output_path,
                name: rendered_dst,
                target: None,
                crate_name: ctx.config.project_name.clone(),
                metadata: HashMap::from([("id".to_string(), id.to_string())]),
                size: None,
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anodize_core::test_helpers::TestContextBuilder;
    use std::fs;
    use tempfile::TempDir;

    /// Helper: build a context with dist pointing to a temp directory.
    fn build_ctx(tmp: &TempDir) -> Context {
        let dist = tmp.path().join("dist");
        fs::create_dir_all(&dist).unwrap();
        TestContextBuilder::new()
            .project_name("myapp")
            .tag("v1.0.0")
            .dist(dist)
            .build()
    }

    #[test]
    fn test_renders_template_file_contents() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        // Create a template source file with template variables
        let src_path = tmp.path().join("greeting.txt.tpl");
        fs::write(
            &src_path,
            "Hello from {{ .ProjectName }} version {{ .Version }}!",
        )
        .unwrap();

        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: Some("greeting".to_string()),
            src: src_path.to_string_lossy().to_string(),
            dst: "greeting.txt".to_string(),
            mode: None,
        }]);

        let stage = TemplateFilesStage;
        stage.run(&mut ctx).unwrap();

        let output = ctx.config.dist.join("greeting.txt");
        assert!(output.exists(), "output file should exist");
        let contents = fs::read_to_string(&output).unwrap();
        assert_eq!(contents, "Hello from myapp version 1.0.0!");
    }

    #[test]
    fn test_writes_to_dist_dst_path() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        let src_path = tmp.path().join("input.tpl");
        fs::write(&src_path, "static content").unwrap();

        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: "subdir/output.txt".to_string(),
            mode: None,
        }]);

        let stage = TemplateFilesStage;
        stage.run(&mut ctx).unwrap();

        let output = ctx.config.dist.join("subdir/output.txt");
        assert!(output.exists(), "output file in subdirectory should exist");
        assert_eq!(fs::read_to_string(&output).unwrap(), "static content");
    }

    #[test]
    fn test_default_mode_is_0o655() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        let src_path = tmp.path().join("script.tpl");
        fs::write(&src_path, "#!/bin/sh\necho hi").unwrap();

        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: "script.sh".to_string(),
            mode: None,
        }]);

        let stage = TemplateFilesStage;
        stage.run(&mut ctx).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let output = ctx.config.dist.join("script.sh");
            let perms = fs::metadata(&output).unwrap().permissions();
            assert_eq!(perms.mode() & 0o7777, 0o655, "default mode should be 0o655");
        }
    }

    #[test]
    fn test_custom_mode_is_applied() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        let src_path = tmp.path().join("exec.tpl");
        fs::write(&src_path, "#!/bin/bash").unwrap();

        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: "exec.sh".to_string(),
            mode: Some("0755".to_string()),
        }]);

        let stage = TemplateFilesStage;
        stage.run(&mut ctx).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let output = ctx.config.dist.join("exec.sh");
            let perms = fs::metadata(&output).unwrap().permissions();
            assert_eq!(
                perms.mode() & 0o7777,
                0o755,
                "custom mode 0o755 should be applied"
            );
        }
    }

    #[test]
    fn test_src_and_dst_paths_are_template_rendered() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        // Create source file using a name that will be constructed via template
        let src_path = tmp.path().join("myapp-install.tpl");
        fs::write(&src_path, "install {{ .Tag }}").unwrap();

        // Use template expressions in both src and dst
        let src_template = format!(
            "{}/{{{{ .ProjectName }}}}-install.tpl",
            tmp.path().display()
        );
        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: None,
            src: src_template,
            dst: "{{ .ProjectName }}-{{ .Version }}-install.sh".to_string(),
            mode: None,
        }]);

        let stage = TemplateFilesStage;
        stage.run(&mut ctx).unwrap();

        let output = ctx.config.dist.join("myapp-1.0.0-install.sh");
        assert!(output.exists(), "template-rendered dst path should exist");
        assert_eq!(fs::read_to_string(&output).unwrap(), "install v1.0.0");
    }

    #[test]
    fn test_noop_when_template_files_is_none() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);
        ctx.config.template_files = None;

        let stage = TemplateFilesStage;
        stage.run(&mut ctx).unwrap();

        // No artifacts should be registered
        assert!(ctx.artifacts.all().is_empty());
    }

    #[test]
    fn test_noop_when_template_files_is_empty() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);
        ctx.config.template_files = Some(vec![]);

        let stage = TemplateFilesStage;
        stage.run(&mut ctx).unwrap();

        assert!(ctx.artifacts.all().is_empty());
    }

    #[test]
    fn test_error_when_src_file_does_not_exist() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: Some("missing".to_string()),
            src: "/nonexistent/path/template.tpl".to_string(),
            dst: "output.txt".to_string(),
            mode: None,
        }]);

        let stage = TemplateFilesStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not found") || err.contains("No such file"),
            "error should mention file not found: {}",
            err
        );
    }

    #[test]
    fn test_registers_artifact_with_correct_kind() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        let src_path = tmp.path().join("data.tpl");
        fs::write(&src_path, "some data").unwrap();

        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: Some("my-file".to_string()),
            src: src_path.to_string_lossy().to_string(),
            dst: "data.txt".to_string(),
            mode: None,
        }]);

        let stage = TemplateFilesStage;
        stage.run(&mut ctx).unwrap();

        let artifacts = ctx.artifacts.by_kind(ArtifactKind::UploadableFile);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].name, "data.txt");
        assert_eq!(artifacts[0].crate_name, "myapp");
        assert_eq!(artifacts[0].metadata.get("id").unwrap(), "my-file");
    }

    #[test]
    fn test_default_id_is_default() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        let src_path = tmp.path().join("file.tpl");
        fs::write(&src_path, "content").unwrap();

        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: "file.txt".to_string(),
            mode: None,
        }]);

        let stage = TemplateFilesStage;
        stage.run(&mut ctx).unwrap();

        let artifacts = ctx.artifacts.by_kind(ArtifactKind::UploadableFile);
        assert_eq!(artifacts[0].metadata.get("id").unwrap(), "default");
    }

    #[test]
    fn test_multiple_template_file_entries() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        let src_a = tmp.path().join("a.tpl");
        let src_b = tmp.path().join("b.tpl");
        let src_c = tmp.path().join("c.tpl");
        fs::write(&src_a, "content A for {{ .ProjectName }}").unwrap();
        fs::write(&src_b, "content B version {{ .Version }}").unwrap();
        fs::write(&src_c, "content C static").unwrap();

        ctx.config.template_files = Some(vec![
            anodize_core::config::TemplateFileConfig {
                id: Some("file-a".to_string()),
                src: src_a.to_string_lossy().to_string(),
                dst: "a.txt".to_string(),
                mode: None,
            },
            anodize_core::config::TemplateFileConfig {
                id: Some("file-b".to_string()),
                src: src_b.to_string_lossy().to_string(),
                dst: "subdir/b.txt".to_string(),
                mode: Some("0755".to_string()),
            },
            anodize_core::config::TemplateFileConfig {
                id: Some("file-c".to_string()),
                src: src_c.to_string_lossy().to_string(),
                dst: "c.txt".to_string(),
                mode: None,
            },
        ]);

        let stage = TemplateFilesStage;
        stage.run(&mut ctx).unwrap();

        // Verify all files were written
        assert_eq!(
            fs::read_to_string(ctx.config.dist.join("a.txt")).unwrap(),
            "content A for myapp"
        );
        assert_eq!(
            fs::read_to_string(ctx.config.dist.join("subdir/b.txt")).unwrap(),
            "content B version 1.0.0"
        );
        assert_eq!(
            fs::read_to_string(ctx.config.dist.join("c.txt")).unwrap(),
            "content C static"
        );

        // Verify all artifacts were registered
        let artifacts = ctx.artifacts.by_kind(ArtifactKind::UploadableFile);
        assert_eq!(artifacts.len(), 3);
        let ids: Vec<&str> = artifacts
            .iter()
            .map(|a| a.metadata.get("id").unwrap().as_str())
            .collect();
        assert!(ids.contains(&"file-a"));
        assert!(ids.contains(&"file-b"));
        assert!(ids.contains(&"file-c"));
    }

    #[test]
    fn test_malformed_template_returns_error() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        // Create a source file with a malformed template expression
        let src_path = tmp.path().join("bad.tpl");
        fs::write(&src_path, "Hello {{ invalid").unwrap();

        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: Some("bad-template".to_string()),
            src: src_path.to_string_lossy().to_string(),
            dst: "bad.txt".to_string(),
            mode: None,
        }]);

        let stage = TemplateFilesStage;
        let result = stage.run(&mut ctx);
        assert!(
            result.is_err(),
            "malformed template should produce an error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to render") || err.contains("template"),
            "error should mention rendering failure: {}",
            err
        );
    }

    #[test]
    fn test_path_traversal_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        let src_path = tmp.path().join("escape.tpl");
        fs::write(&src_path, "trying to escape").unwrap();

        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: "../escaped.txt".to_string(),
            mode: None,
        }]);

        let stage = TemplateFilesStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "path traversal should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("must be a relative path within dist"),
            "error should mention path restriction: {}",
            err
        );
    }

    #[test]
    fn test_absolute_dst_path_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        let src_path = tmp.path().join("abs.tpl");
        fs::write(&src_path, "content").unwrap();

        ctx.config.template_files = Some(vec![anodize_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: if cfg!(windows) {
                "C:\\etc\\evil.txt".to_string()
            } else {
                "/etc/evil.txt".to_string()
            },
            mode: None,
        }]);

        let stage = TemplateFilesStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "absolute dst path should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("must be a relative path within dist"),
            "error should mention path restriction: {}",
            err
        );
    }
}
