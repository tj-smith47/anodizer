use std::collections::HashMap;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::parse_octal_mode;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;
use anodizer_core::template_file_render::render_templated_file_entry;

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

        // Engine-derived case tables for a `curl | sh` installer: the
        // `os-arch -> asset` arms plus the uname detection arms, so both the
        // download URLs and the case KEYS track the engine's own os/arch
        // vocabulary instead of a hand-rolled shell copy that silently drifts.
        let installer_cases = anodizer_core::installer::render_installer_cases(ctx)
            .context("templatefiles: derive installer case tables")?;
        installer_cases.bind(ctx.template_vars_mut());

        for entry in &entries {
            let id = entry.id.as_deref().unwrap_or("default");
            let label = format!("templatefiles: id '{}'", id);

            let render = match render_templated_file_entry(ctx, entry, &label)? {
                Some(r) => r,
                None => {
                    // Skipped (skip=true or empty dst). Log only the
                    // skip-true path here — empty-dst is a quiet
                    // "ignored if empty".
                    if entry.skip.is_some() {
                        let log = ctx.logger("templatefiles");
                        log.status(&format!("skipped id '{}'", id));
                    }
                    continue;
                }
            };

            // Write to dist/dst
            let output_path = dist.join(&render.rendered_dst);
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!(
                        "templatefiles: failed to create parent dir for '{}'",
                        output_path.display()
                    )
                })?;
            }
            std::fs::write(&output_path, &render.rendered_contents).with_context(|| {
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
                "rendered '{}' → '{}'",
                entry.src,
                output_path.display()
            ));

            // Register as an artifact
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::UploadableFile,
                path: output_path,
                name: render.rendered_dst,
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
    use anodizer_core::test_helpers::TestContextBuilder;
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

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: Some("greeting".to_string()),
            src: src_path.to_string_lossy().to_string(),
            dst: "greeting.txt".to_string(),
            mode: None,
            skip: None,
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

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: "subdir/output.txt".to_string(),
            mode: None,
            skip: None,
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

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: "script.sh".to_string(),
            mode: None,
            skip: None,
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

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: "exec.sh".to_string(),
            mode: Some("0755".to_string()),
            skip: None,
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
        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: None,
            src: src_template,
            dst: "{{ .ProjectName }}-{{ .Version }}-install.sh".to_string(),
            mode: None,
            skip: None,
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

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: Some("missing".to_string()),
            src: "/nonexistent/path/template.tpl".to_string(),
            dst: "output.txt".to_string(),
            mode: None,
            skip: None,
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

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: Some("my-file".to_string()),
            src: src_path.to_string_lossy().to_string(),
            dst: "data.txt".to_string(),
            mode: None,
            skip: None,
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

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: "file.txt".to_string(),
            mode: None,
            skip: None,
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
            anodizer_core::config::TemplateFileConfig {
                id: Some("file-a".to_string()),
                src: src_a.to_string_lossy().to_string(),
                dst: "a.txt".to_string(),
                mode: None,
                skip: None,
            },
            anodizer_core::config::TemplateFileConfig {
                id: Some("file-b".to_string()),
                src: src_b.to_string_lossy().to_string(),
                dst: "subdir/b.txt".to_string(),
                mode: Some("0755".to_string()),
                skip: None,
            },
            anodizer_core::config::TemplateFileConfig {
                id: Some("file-c".to_string()),
                src: src_c.to_string_lossy().to_string(),
                dst: "c.txt".to_string(),
                mode: None,
                skip: None,
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

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: Some("bad-template".to_string()),
            src: src_path.to_string_lossy().to_string(),
            dst: "bad.txt".to_string(),
            mode: None,
            skip: None,
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

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: "../escaped.txt".to_string(),
            mode: None,
            skip: None,
        }]);

        let stage = TemplateFilesStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "path traversal should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("must be a relative path"),
            "error should mention path restriction: {}",
            err
        );
    }

    #[test]
    fn test_skip_true_skips_entry() {
        // `skip: true` short-circuits before any file IO — the source
        // path can be invalid and the test still passes.
        use anodizer_core::config::StringOrBool;
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: Some("skipped".to_string()),
            src: "/nonexistent/path.tpl".to_string(),
            dst: "skipped.txt".to_string(),
            mode: None,
            skip: Some(StringOrBool::Bool(true)),
        }]);

        TemplateFilesStage.run(&mut ctx).unwrap();
        assert!(ctx.artifacts.all().is_empty());
    }

    #[test]
    fn test_binary_src_returns_clear_error() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        let src_path = tmp.path().join("binary.tpl");
        fs::write(&src_path, [0xFF, 0xFE, 0xFD]).unwrap();

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: Some("bin".to_string()),
            src: src_path.to_string_lossy().to_string(),
            dst: "out.txt".to_string(),
            mode: None,
            skip: None,
        }]);

        let err = TemplateFilesStage.run(&mut ctx).unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("not valid UTF-8"),
            "binary input should produce a clear UTF-8 error, got: {msg}"
        );
    }

    #[test]
    fn test_empty_rendered_dst_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);
        let src_path = tmp.path().join("source.tpl");
        fs::write(&src_path, "content").unwrap();

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: Some("empty-dst".to_string()),
            src: src_path.to_string_lossy().to_string(),
            dst: String::new(),
            mode: None,
            skip: None,
        }]);

        TemplateFilesStage.run(&mut ctx).unwrap();
        assert!(ctx.artifacts.all().is_empty());
    }

    /// End-to-end proof: rendering anodize's OWN `scripts/install.sh.tpl`
    /// against an anodize-shaped (lockstep) config materializes a `case` table
    /// whose `ARCHIVE` values are the byte-exact asset names the archive stage
    /// uploads — every `curl | sh` URL resolves. Pins installer↔asset-name
    /// agreement so a future `name_template` / `format_overrides` change can't
    /// silently 404 the installer.
    #[test]
    fn test_real_install_sh_tpl_renders_engine_asset_names() {
        use anodizer_core::config::{
            ArchiveConfig, ArchivesConfig, BuildConfig, CrateConfig, Defaults, FormatOverride,
        };

        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);
        // Re-key to anodize's own identity (build_ctx seeds "myapp"/"1.0.0").
        ctx.config.project_name = "anodizer".to_string();
        ctx.template_vars_mut().set("ProjectName", "anodizer");
        ctx.template_vars_mut().set("Version", "0.13.0");

        let targets: Vec<String> = [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        ctx.config.defaults = Some(Defaults {
            targets: Some(targets),
            ..Default::default()
        });
        ctx.config.crates = vec![CrateConfig {
            name: "anodizer".to_string(),
            path: "crates/cli".to_string(),
            builds: Some(vec![BuildConfig {
                id: Some("anodizer".to_string()),
                binary: Some("anodizer".to_string()),
                ..Default::default()
            }]),
            archives: ArchivesConfig::Configs(vec![ArchiveConfig {
                name_template: Some(
                    "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}".to_string(),
                ),
                formats: Some(vec!["tar.gz".to_string()]),
                format_overrides: Some(vec![FormatOverride {
                    os: "windows".to_string(),
                    formats: Some(vec!["zip".to_string()]),
                }]),
                ids: Some(vec!["anodizer".to_string()]),
                ..Default::default()
            }]),
            ..Default::default()
        }];

        // The repo's real installer template, two levels up from this crate.
        let tpl = format!(
            "{}/../../scripts/install.sh.tpl",
            env!("CARGO_MANIFEST_DIR")
        );
        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: Some("install".to_string()),
            src: tpl,
            dst: "install.sh".to_string(),
            mode: Some("0755".to_string()),
            skip: None,
        }]);

        TemplateFilesStage.run(&mut ctx).unwrap();

        let rendered =
            fs::read_to_string(ctx.config.dist.join("install.sh")).expect("install.sh written");

        // Every released target's arm carries the engine's real asset name.
        for (key, asset) in [
            ("linux-amd64", "anodizer-0.13.0-linux-amd64.tar.gz"),
            ("linux-arm64", "anodizer-0.13.0-linux-arm64.tar.gz"),
            ("darwin-amd64", "anodizer-0.13.0-darwin-amd64.tar.gz"),
            ("darwin-arm64", "anodizer-0.13.0-darwin-arm64.tar.gz"),
            ("windows-amd64", "anodizer-0.13.0-windows-amd64.zip"),
            ("windows-arm64", "anodizer-0.13.0-windows-arm64.zip"),
        ] {
            assert!(
                rendered.contains(&format!("{key})")),
                "missing case arm for {key}"
            );
            assert!(
                rendered.contains(&format!("ARCHIVE=\"{asset}\"")),
                "missing ARCHIVE for {key}: expected {asset}"
            );
        }
        // No HTML-escaping of the embedded quotes, and the fallback arm survives.
        assert!(!rendered.contains("&quot;"), "quotes must not be escaped");
        assert!(
            rendered.contains("no prebuilt ${PROJECT} binary"),
            "fallback error arm must be present"
        );
        // Both error paths list the platforms that DO have prebuilt binaries.
        assert_eq!(
            rendered
                .matches(
                    "Prebuilt binaries: darwin-amd64 darwin-arm64 linux-amd64 \
                     linux-arm64 windows-amd64 windows-arm64"
                )
                .count(),
            2,
            "both error paths must list the supported platforms"
        );
        assert_eq!(
            rendered
                .matches("All assets: https://github.com/${REPO}/releases/tag/v${VERSION}")
                .count(),
            2,
            "both error paths must link the release's asset page"
        );

        // The uname detection arms are generated from the same released
        // targets, so every asset arm above is reachable at runtime.
        for arm in [
            "Linux*) echo \"linux\" ;;",
            "Darwin*) echo \"darwin\" ;;",
            "MINGW*|MSYS*|CYGWIN*) echo \"windows\" ;;",
            "x86_64|amd64) echo \"amd64\" ;;",
            "aarch64|arm64) echo \"arm64\" ;;",
        ] {
            assert!(rendered.contains(arm), "missing detection arm: {arm}");
        }
        // Fully rendered — no template syntax may survive into the script.
        assert!(
            !rendered.contains("{{") && !rendered.contains("{%"),
            "unrendered template syntax left in install.sh"
        );

        // The rendered installer must be valid POSIX shell.
        #[cfg(unix)]
        {
            let out = std::process::Command::new("sh")
                .arg("-n")
                .arg(ctx.config.dist.join("install.sh"))
                .output()
                .expect("run sh -n");
            assert!(
                out.status.success(),
                "sh -n rejected the rendered installer: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn test_absolute_dst_path_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut ctx = build_ctx(&tmp);

        let src_path = tmp.path().join("abs.tpl");
        fs::write(&src_path, "content").unwrap();

        ctx.config.template_files = Some(vec![anodizer_core::config::TemplateFileConfig {
            id: None,
            src: src_path.to_string_lossy().to_string(),
            dst: if cfg!(windows) {
                "C:\\etc\\evil.txt".to_string()
            } else {
                "/etc/evil.txt".to_string()
            },
            mode: None,
            skip: None,
        }]);

        let stage = TemplateFilesStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err(), "absolute dst path should be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("must be a relative path"),
            "error should mention path restriction: {}",
            err
        );
    }
}
