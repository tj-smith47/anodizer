//! Windows MSI installer stage.
//!
//! Split into focused submodules:
//!
//! - [`wix`] — WiX version detection + CLI command construction + arch mapping.
//! - [`template`] — `.wxs` rendering and the stage's template-variable plumbing.
//! - [`build`] — per-crate build orchestration, dry-run, artifacts, and hooks.
//!
//! The [`MsiStage`] entry point and its [`Stage`] impl live here; the
//! submodules' public surface is re-exported below so external callers and
//! the test module keep reaching items via `anodizer_stage_msi::<item>`.

use std::path::PathBuf;

use anyhow::Result;

use anodizer_core::artifact::Artifact;
use anodizer_core::context::Context;
use anodizer_core::stage::Stage;

mod build;
mod template;
mod wix;

pub use template::render_wxs_template;
pub use wix::{MsiCommands, WixVersion, map_arch_to_msi, msi_command};

use build::process_msi_crate;
use template::clear_msi_template_vars;

// Exercised only by the `#[cfg(test)] tests` module below; importing them into
// the parent scope under the same gate keeps `use super::*` resolving without
// flagging the path as unused in a non-test build.
#[cfg(test)]
use build::run_msi_post_hook;
#[cfg(test)]
use template::build_post_hook_template_vars;

pub struct MsiStage;

impl Stage for MsiStage {
    fn name(&self) -> &str {
        "msi"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("msi");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have MSI config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.msis.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // In workspace per-crate mode the same pipeline run produces an MSI for
        // each crate. Rebinding `ProjectName` to the current crate's name
        // (mirroring the archive stage) keeps default name templates like
        // `{{ ProjectName }}_{{ MsiArch }}` distinct per crate so two crates'
        // installers don't render the same filename and clobber each other.
        // Restored after the loop.
        let multi_crate = crates.len() > 1;
        let original_project_name = ctx
            .template_vars()
            .get("ProjectName")
            .cloned()
            .unwrap_or_else(|| ctx.config.project_name.clone());

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archives_to_remove: Vec<PathBuf> = Vec::new();

        // Capture the loop result rather than `?`-ing inside it: a per-crate
        // failure must still restore the rebound `ProjectName` below before
        // propagating, so the workspace value never leaks past this stage.
        let loop_result: Result<()> = (|| {
            for krate in &crates {
                if multi_crate {
                    ctx.template_vars_mut().set("ProjectName", &krate.name);
                }
                process_msi_crate(
                    ctx,
                    &log,
                    krate,
                    &dist,
                    dry_run,
                    &mut new_artifacts,
                    &mut archives_to_remove,
                )?;
            }
            Ok(())
        })();

        if multi_crate {
            ctx.template_vars_mut()
                .set("ProjectName", &original_project_name);
        }
        loop_result?;

        clear_msi_template_vars(ctx);

        if !archives_to_remove.is_empty() {
            ctx.artifacts.remove_by_paths(&archives_to_remove);
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

/// Environment requirements for the msi stage: when any active `msis:`
/// entry exists and the configured build targets include Windows, the WiX
/// toolchain resolved by the same policy the build uses (explicit
/// `version:` > `.wxs` namespace sniff > installed-tool probe) — `wix` for
/// v4, `candle` + `light` for v3, `wixl` for the Linux-native msitools path.
pub fn env_requirements(
    ctx: &anodizer_core::context::Context,
) -> Vec<anodizer_core::EnvRequirement> {
    if !anodizer_core::env_preflight::configured_build_targets(ctx)
        .iter()
        .any(|t| anodizer_core::target::is_windows(t))
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    for krate in anodizer_core::env_preflight::crate_universe(&ctx.config) {
        for cfg in krate.msis.iter().flatten() {
            if anodizer_core::env_preflight::entry_inactive(
                ctx,
                cfg.skip.as_ref(),
                None,
                cfg.if_condition.as_deref(),
            ) {
                continue;
            }
            // Render the wxs path like the build does; an unrenderable
            // template falls through to the installed-tool probe.
            let wxs = cfg
                .wxs
                .as_deref()
                .map(|raw| ctx.render_template(raw).unwrap_or_else(|_| raw.to_string()))
                .unwrap_or_default();
            match wix::resolve_wix_version_quiet(cfg, &wxs) {
                WixVersion::V4 => out.push(anodizer_core::EnvRequirement::Tool {
                    name: "wix".to_string(),
                }),
                WixVersion::V3 => {
                    out.push(anodizer_core::EnvRequirement::Tool {
                        name: "candle".to_string(),
                    });
                    out.push(anodizer_core::EnvRequirement::Tool {
                        name: "light".to_string(),
                    });
                }
                WixVersion::Wixl => out.push(anodizer_core::EnvRequirement::Tool {
                    name: "wixl".to_string(),
                }),
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodizer_core::artifact::ArtifactKind;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // WiX version detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_wix_v3_from_wxs() {
        let wxs = r#"<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://schemas.microsoft.com/wix/2006/wi">
  <Product Id="*" Name="MyApp" Version="1.0.0" />
</Wix>"#;
        assert_eq!(WixVersion::detect_from_wxs(wxs), WixVersion::V3);
    }

    #[test]
    fn test_detect_wix_v4_from_wxs() {
        let wxs = r#"<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">
  <Package Name="MyApp" Version="1.0.0" />
</Wix>"#;
        assert_eq!(WixVersion::detect_from_wxs(wxs), WixVersion::V4);
    }

    #[test]
    fn test_detect_wix_default_v4() {
        // No recognized namespace — defaults to V4
        let wxs = r#"<?xml version="1.0" encoding="UTF-8"?>
<Wix>
  <Package Name="MyApp" Version="1.0.0" />
</Wix>"#;
        assert_eq!(WixVersion::detect_from_wxs(wxs), WixVersion::V4);

        // Completely unrelated content also defaults to V4
        assert_eq!(
            WixVersion::detect_from_wxs("some random content"),
            WixVersion::V4
        );

        // Empty content defaults to V4
        assert_eq!(WixVersion::detect_from_wxs(""), WixVersion::V4);
    }

    // -----------------------------------------------------------------------
    // MSI command construction tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_msi_command_v4() {
        let cmds = msi_command(WixVersion::V4, "/tmp/app.wxs", "/out/app.msi", &[]);
        assert_eq!(
            cmds.primary,
            vec!["wix", "build", "/tmp/app.wxs", "-o", "/out/app.msi"]
        );
        assert!(cmds.link.is_none());
    }

    #[test]
    fn test_msi_command_v3() {
        let cmds = msi_command(WixVersion::V3, "/tmp/app.wxs", "/out/app.msi", &[]);
        assert_eq!(
            cmds.primary,
            vec!["candle", "-nologo", "/tmp/app.wxs", "-o", "/out/app.wixobj"]
        );
        let link = cmds.link.unwrap();
        assert_eq!(
            link,
            vec!["light", "-nologo", "/out/app.wixobj", "-o", "/out/app.msi"]
        );
    }

    #[test]
    fn test_msi_command_wixl() {
        let cmds = msi_command(WixVersion::Wixl, "/tmp/app.wxs", "/out/app.msi", &[]);
        assert_eq!(
            cmds.primary,
            vec!["wixl", "-o", "/out/app.msi", "/tmp/app.wxs"]
        );
        assert!(cmds.link.is_none());
    }

    #[test]
    fn test_msi_command_wixl_ignores_extensions() {
        let exts = vec!["WixUIExtension".to_string()];
        let cmds = msi_command(WixVersion::Wixl, "/tmp/app.wxs", "/out/app.msi", &exts);
        // wixl does not understand WiX `-ext`; extensions must not appear.
        assert_eq!(
            cmds.primary,
            vec!["wixl", "-o", "/out/app.msi", "/tmp/app.wxs"]
        );
        assert!(!cmds.primary.iter().any(|a| a == "-ext"));
        assert!(!cmds.primary.iter().any(|a| a == "WixUIExtension"));
        assert!(cmds.link.is_none());
    }

    // -----------------------------------------------------------------------
    // Architecture mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_arch_to_msi() {
        // amd64 variants -> x64
        assert_eq!(map_arch_to_msi("amd64"), "x64");
        assert_eq!(map_arch_to_msi("x86_64"), "x64");

        // 32-bit variants -> x86
        assert_eq!(map_arch_to_msi("386"), "x86");
        assert_eq!(map_arch_to_msi("i686"), "x86");
        assert_eq!(map_arch_to_msi("i386"), "x86");
        assert_eq!(map_arch_to_msi("x86"), "x86");

        // arm64 variants -> arm64
        assert_eq!(map_arch_to_msi("arm64"), "arm64");
        assert_eq!(map_arch_to_msi("aarch64"), "arm64");

        // Unknown -> passthrough
        assert_eq!(map_arch_to_msi("riscv64"), "riscv64");
    }

    // -----------------------------------------------------------------------
    // Stage behavior tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_skips_when_no_msi_config() {
        use anodizer_core::config::Config;
        use anodizer_core::context::{Context, ContextOptions};

        let config = Config::default();
        let mut ctx = Context::new(config, ContextOptions::default());
        let stage = MsiStage;
        assert!(stage.run(&mut ctx).is_ok());
        // No artifacts should be registered
        assert!(ctx.artifacts.by_kind(ArtifactKind::Installer).is_empty());
    }

    #[test]
    fn test_stage_skips_when_disabled() {
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Write a dummy .wxs file
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            skip: Some(anodizer_core::config::StringOrBool::Bool(true)),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // No artifacts because config is disabled
        assert!(ctx.artifacts.by_kind(ArtifactKind::Installer).is_empty());
    }

    #[test]
    fn test_stage_dry_run_registers_artifacts() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Write a dummy .wxs file
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // Register a Windows binary artifact so the stage picks it up
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
        assert_eq!(installers[0].kind, ArtifactKind::Installer);
        assert_eq!(installers[0].crate_name, "myapp");
        assert_eq!(
            installers[0].metadata.get("format"),
            Some(&"msi".to_string())
        );
        // Default name uses the `{{ ProjectName }}_{{ MsiArch }}` shape.
        assert!(
            installers[0]
                .path
                .to_string_lossy()
                .contains("myapp_x64.msi")
        );
        assert_eq!(
            installers[0].target,
            Some("x86_64-pc-windows-msvc".to_string())
        );
    }

    #[test]
    fn test_stage_dry_run_with_name_template() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            name: Some("{{ .ProjectName }}-{{ .Version }}-{{ .MsiArch }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "2.5.0");

        // Register a Windows binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("aarch64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);

        let path_str = installers[0].path.file_name().unwrap().to_str().unwrap();
        assert_eq!(path_str, "myapp-2.5.0-arm64.msi");
    }

    #[test]
    fn test_stage_errors_without_wxs() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Config with no wxs field
        let msi_cfg = MsiConfig {
            wxs: None,
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // Register a Windows binary so the stage doesn't skip before wxs check
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("wxs") && err.contains("required"),
            "error should mention wxs is required, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Config parsing roundtrip tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_msi() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(msis.len(), 1);
        assert_eq!(msis[0].wxs.as_deref(), Some("app.wxs"));
        assert!(msis[0].name.is_none());
        assert!(msis[0].version.is_none());
        assert!(msis[0].replace.is_none());
        assert!(msis[0].skip.is_none());
    }

    #[test]
    fn test_config_parse_msi_full() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - id: my-msi
        ids:
          - build-win-amd64
        wxs: installer/app.wxs
        name: "myapp-{{ .Version }}-{{ .MsiArch }}"
        version: v4
        replace: true
        mod_timestamp: "2024-01-01T00:00:00Z"
        skip: false
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(msis.len(), 1);

        let msi = &msis[0];
        assert_eq!(msi.id.as_deref(), Some("my-msi"));
        assert_eq!(msi.ids.as_ref().unwrap(), &["build-win-amd64".to_string()]);
        assert_eq!(msi.wxs.as_deref(), Some("installer/app.wxs"));
        assert_eq!(
            msi.name.as_deref(),
            Some("myapp-{{ .Version }}-{{ .MsiArch }}")
        );
        assert_eq!(msi.version.as_deref(), Some("v4"));
        assert_eq!(msi.replace, Some(true));
        assert_eq!(msi.mod_timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(
            msi.skip,
            Some(anodizer_core::config::StringOrBool::Bool(false))
        );
    }

    // -----------------------------------------------------------------------
    // WXS template rendering test
    // -----------------------------------------------------------------------

    #[test]
    fn test_wxs_template_rendering() {
        use anodizer_core::test_helpers::TestContextBuilder;

        let tmp = TempDir::new().unwrap();
        let wxs_content = r#"<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">
  <Package Name="{{ .ProjectName }}" Version="{{ .Version }}" Manufacturer="Test">
    <File Source="{{ .ProjectName }}.exe" />
  </Package>
</Wix>"#;

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, wxs_content).unwrap();

        let ctx = TestContextBuilder::new()
            .project_name("myapp")
            .tag("v3.0.0")
            .build();

        let rendered = render_wxs_template(&ctx, &wxs_path.to_string_lossy()).unwrap();
        assert!(rendered.contains("Name=\"myapp\""));
        assert!(rendered.contains("Version=\"3.0.0\""));
        assert!(rendered.contains("Source=\"myapp.exe\""));
        // Original template vars should be expanded
        assert!(!rendered.contains("{{ .ProjectName }}"));
        assert!(!rendered.contains("{{ .Version }}"));
    }

    // -----------------------------------------------------------------------
    // Invalid template error test
    // -----------------------------------------------------------------------

    #[test]
    fn test_invalid_name_template_errors() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            // Invalid Tera template — unclosed tag
            name: Some("{{ bad_template".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // Register a Windows binary
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let result = MsiStage.run(&mut ctx);
        assert!(
            result.is_err(),
            "invalid name template should cause a render error"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("template") || err.contains("render"),
            "error should mention template rendering, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // WiX version config string parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_wix_version_from_config_str() {
        assert_eq!(WixVersion::from_config_str("v3"), Some(WixVersion::V3));
        assert_eq!(WixVersion::from_config_str("v4"), Some(WixVersion::V4));
        assert_eq!(WixVersion::from_config_str("V3"), Some(WixVersion::V3));
        assert_eq!(WixVersion::from_config_str("V4"), Some(WixVersion::V4));
        assert_eq!(WixVersion::from_config_str("3"), Some(WixVersion::V3));
        assert_eq!(WixVersion::from_config_str("4"), Some(WixVersion::V4));
        assert_eq!(WixVersion::from_config_str("v5"), None);
        assert_eq!(WixVersion::from_config_str("invalid"), None);
        // Linux-native msitools path
        assert_eq!(WixVersion::from_config_str("wixl"), Some(WixVersion::Wixl));
        assert_eq!(WixVersion::from_config_str("WIXL"), Some(WixVersion::Wixl));
        assert_eq!(WixVersion::from_config_str("linux"), Some(WixVersion::Wixl));
        assert_eq!(WixVersion::from_config_str("Linux"), Some(WixVersion::Wixl));
    }

    #[test]
    fn test_resolve_explicit_wixl_forces_linux_path() {
        use anodizer_core::config::MsiConfig;
        // An explicit `version: wixl` selects the Linux path with no tool probe,
        // so the result is deterministic regardless of the host's toolchain.
        let cfg = MsiConfig {
            version: Some("wixl".to_string()),
            ..Default::default()
        };
        assert_eq!(
            wix::resolve_wix_version_quiet(&cfg, "/nonexistent.wxs"),
            WixVersion::Wixl
        );
    }

    #[test]
    fn test_resolve_explicit_v4_never_downgrades_to_wixl() {
        use anodizer_core::config::MsiConfig;
        // A v4-authored wxs is incompatible with wixl's v3 dialect; the
        // substitution must never reroute V4 to Wixl even on a wixl-only host.
        let cfg = MsiConfig {
            version: Some("v4".to_string()),
            ..Default::default()
        };
        assert_eq!(
            wix::resolve_wix_version_quiet(&cfg, "/nonexistent.wxs"),
            WixVersion::V4
        );
    }

    // -----------------------------------------------------------------------
    // Replace option removes archives
    // -----------------------------------------------------------------------

    #[test]
    fn test_replace_removes_archive_artifacts() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            replace: Some(true),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // Register a Windows binary and a corresponding archive
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: String::new(),
            path: PathBuf::from("dist/myapp_1.0.0_windows_amd64.zip"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("format".to_string(), "zip".to_string())]),
            size: None,
        });

        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Archive).len(), 1);

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // Archive should have been removed
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Archive).len(), 0);
        // MSI artifact should exist
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Installer).len(), 1);
    }

    // -----------------------------------------------------------------------
    // No binaries — warns and skips
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_skips_with_warning_when_no_binaries() {
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // No binary artifacts registered — should skip with warning, not create synthetic
        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // No installer artifacts should be produced
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert!(
            installers.is_empty(),
            "expected no installers when no Windows binaries exist, got {}",
            installers.len()
        );
    }

    // -----------------------------------------------------------------------
    // ids filtering
    // -----------------------------------------------------------------------

    #[test]
    fn test_ids_filtering_retains_matching_binaries() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ids: Some(vec!["build-win-amd64".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // Register two Windows binaries with different ids
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp-amd64.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-win-amd64".to_string())]),
            size: None,
        });
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp-arm64.exe"),
            target: Some("aarch64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-win-arm64".to_string())]),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(
            installers.len(),
            1,
            "ids filter should keep only one binary"
        );
        assert_eq!(
            installers[0].target,
            Some("x86_64-pc-windows-msvc".to_string())
        );
    }

    #[test]
    fn test_ids_filtering_skips_when_no_match() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ids: Some(vec!["nonexistent-id".to_string()]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "build-win-amd64".to_string())]),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert!(
            installers.is_empty(),
            "non-matching ids should produce no installers"
        );
    }

    // -----------------------------------------------------------------------
    // id stored in artifact metadata
    // -----------------------------------------------------------------------

    #[test]
    fn test_artifact_stores_config_id_in_metadata() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            id: Some("my-msi-id".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
        assert_eq!(
            installers[0].metadata.get("id"),
            Some(&"my-msi-id".to_string()),
            "artifact metadata should contain the config id"
        );
    }

    // -----------------------------------------------------------------------
    // Multiple MSI configs per crate
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_dry_run_multiple_configs() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Two MSI configs with different wxs files
        let wxs_path_a = tmp.path().join("a.wxs");
        let wxs_path_b = tmp.path().join("b.wxs");
        fs::write(&wxs_path_a, "<Wix/>").unwrap();
        fs::write(&wxs_path_b, "<Wix/>").unwrap();

        let msi_cfg_a = MsiConfig {
            wxs: Some(wxs_path_a.to_string_lossy().into_owned()),
            id: Some("msi-a".to_string()),
            name: Some("myapp-a-{{ .MsiArch }}".to_string()),
            ..Default::default()
        };
        let msi_cfg_b = MsiConfig {
            wxs: Some(wxs_path_b.to_string_lossy().into_owned()),
            id: Some("msi-b".to_string()),
            name: Some("myapp-b-{{ .MsiArch }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg_a, msi_cfg_b]),
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

        // Add a Windows binary artifact
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // Verify both produce MSI artifacts
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(
            installers.len(),
            2,
            "two MSI configs should produce two installer artifacts"
        );

        let ids: Vec<_> = installers
            .iter()
            .filter_map(|a| a.metadata.get("id").cloned())
            .collect();
        assert!(ids.contains(&"msi-a".to_string()));
        assert!(ids.contains(&"msi-b".to_string()));

        let filenames: Vec<_> = installers
            .iter()
            .map(|a| a.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert!(filenames.contains(&"myapp-a-x64.msi".to_string()));
        assert!(filenames.contains(&"myapp-b-x64.msi".to_string()));
    }

    // -----------------------------------------------------------------------
    // mod_timestamp adds -d BindTimestamp for V4
    // -----------------------------------------------------------------------

    #[test]
    fn test_mod_timestamp_adds_bind_timestamp_v4() {
        // Build commands for V4 with mod_timestamp should include -d BindTimestamp=...
        let mut commands = msi_command(WixVersion::V4, "/tmp/app.wxs", "/out/app.msi", &[]);
        let ts = "1704067200";
        // Simulate what the stage does for V4 with mod_timestamp
        commands.primary.push("-d".to_string());
        commands.primary.push(format!("BindTimestamp={ts}"));

        assert!(
            commands.primary.contains(&"-d".to_string()),
            "V4 command should have -d flag"
        );
        assert!(
            commands
                .primary
                .contains(&"BindTimestamp=1704067200".to_string()),
            "V4 command should have BindTimestamp value"
        );

        // Verify the full command looks correct
        assert_eq!(
            commands.primary,
            vec![
                "wix",
                "build",
                "/tmp/app.wxs",
                "-o",
                "/out/app.msi",
                "-d",
                "BindTimestamp=1704067200"
            ]
        );

        // V3 should NOT get -d BindTimestamp
        let v3_commands = msi_command(WixVersion::V3, "/tmp/app.wxs", "/out/app.msi", &[]);
        assert!(
            !v3_commands.primary.contains(&"-d".to_string()),
            "V3 command should not have -d flag"
        );
    }

    // -----------------------------------------------------------------------
    // Per-target template vars are cleared on stage exit
    // -----------------------------------------------------------------------

    #[test]
    fn test_per_target_template_vars_cleared_after_stage() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        MsiStage.run(&mut ctx).unwrap();

        // Per-target vars must not leak into downstream stages.
        for key in ["Os", "Arch", "Target", "MsiArch", "BinaryPath"] {
            assert_eq!(
                ctx.template_vars().get(key).map(String::as_str),
                Some(""),
                "per-target var {key} should be cleared on stage exit"
            );
        }
    }

    // -----------------------------------------------------------------------
    // extra_files config parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_extra_files() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
        extra_files:
          - README.md
          - LICENSE
          - doc/guide.pdf
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        let extras = msis[0].extra_files.as_ref().unwrap();
        assert_eq!(extras.len(), 3);
        assert_eq!(extras[0], "README.md");
        assert_eq!(extras[1], "LICENSE");
        assert_eq!(extras[2], "doc/guide.pdf");
    }

    // -----------------------------------------------------------------------
    // extensions config parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_extensions() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
        extensions:
          - WixUIExtension
          - WixUtilExtension
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        let exts = msis[0].extensions.as_ref().unwrap();
        assert_eq!(exts.len(), 2);
        assert_eq!(exts[0], "WixUIExtension");
        assert_eq!(exts[1], "WixUtilExtension");
    }

    // -----------------------------------------------------------------------
    // disable as StringOrBool
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_disable_bool_true() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
        skip: true
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(
            msis[0].skip,
            Some(anodizer_core::config::StringOrBool::Bool(true))
        );
    }

    #[test]
    fn test_config_parse_disable_string_true() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
        skip: "true"
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(
            msis[0].skip,
            Some(anodizer_core::config::StringOrBool::String(
                "true".to_string()
            ))
        );
    }

    #[test]
    fn test_config_parse_disable_template_string() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: app.wxs
        skip: "{{ .Env.SKIP_MSI }}"
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        assert_eq!(
            msis[0].skip,
            Some(anodizer_core::config::StringOrBool::String(
                "{{ .Env.SKIP_MSI }}".to_string()
            ))
        );
    }

    #[test]
    fn test_stage_disable_with_string_true() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            skip: Some(StringOrBool::String("true".to_string())),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        // skip: "true" should skip
        assert!(ctx.artifacts.by_kind(ArtifactKind::Installer).is_empty());
    }

    // -----------------------------------------------------------------------
    // extensions passed to WiX commands
    // -----------------------------------------------------------------------

    #[test]
    fn test_msi_command_v4_with_extensions() {
        let exts = vec!["WixUIExtension".to_string(), "WixUtilExtension".to_string()];
        let cmds = msi_command(WixVersion::V4, "/tmp/app.wxs", "/out/app.msi", &exts);
        assert_eq!(
            cmds.primary,
            vec![
                "wix",
                "build",
                "/tmp/app.wxs",
                "-o",
                "/out/app.msi",
                "-ext",
                "WixUIExtension",
                "-ext",
                "WixUtilExtension",
            ]
        );
        assert!(cmds.link.is_none());
    }

    #[test]
    fn test_msi_command_v3_with_extensions() {
        let exts = vec!["WixUIExtension".to_string()];
        let cmds = msi_command(WixVersion::V3, "/tmp/app.wxs", "/out/app.msi", &exts);

        // candle gets -ext too
        assert_eq!(
            cmds.primary,
            vec![
                "candle",
                "-nologo",
                "/tmp/app.wxs",
                "-o",
                "/out/app.wixobj",
                "-ext",
                "WixUIExtension",
            ]
        );

        // light also gets -ext
        let link = cmds.link.unwrap();
        assert_eq!(
            link,
            vec![
                "light",
                "-nologo",
                "/out/app.wixobj",
                "-o",
                "/out/app.msi",
                "-ext",
                "WixUIExtension",
            ]
        );
    }

    // -----------------------------------------------------------------------
    // extra_files copied to build context (live mode)
    // -----------------------------------------------------------------------

    #[test]
    fn test_extra_files_copied_to_build_context() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        // Create extra files
        let readme_path = tmp.path().join("README.md");
        let license_path = tmp.path().join("LICENSE");
        fs::write(&readme_path, "# My App").unwrap();
        fs::write(&license_path, "MIT License").unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            extra_files: Some(vec![
                readme_path.to_string_lossy().into_owned(),
                license_path.to_string_lossy().into_owned(),
            ]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // In dry-run mode, extra_files are only logged, not copied.
        // We verify the config is accepted and the stage runs successfully.
        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(
            installers.len(),
            1,
            "should produce MSI artifact even with extra_files"
        );
    }

    // -----------------------------------------------------------------------
    // extensions dry-run with template rendering
    // -----------------------------------------------------------------------

    #[test]
    fn test_extensions_in_dry_run() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();

        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            extensions: Some(vec![
                "WixUIExtension".to_string(),
                "WixUtilExtension".to_string(),
            ]),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Should succeed — extensions are logged in dry-run mode
        let stage = MsiStage;
        stage.run(&mut ctx).unwrap();

        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Full config roundtrip with new fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_parse_msi_full_with_new_fields() {
        let yaml = r#"
project_name: test
crates:
  - name: test
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - id: my-msi
        ids:
          - build-win-amd64
        wxs: installer/app.wxs
        name: "myapp-{{ .Version }}-{{ .MsiArch }}"
        version: v4
        replace: true
        mod_timestamp: "2024-01-01T00:00:00Z"
        skip: "{{ .Env.SKIP_MSI }}"
        extra_files:
          - README.md
          - LICENSE
        extensions:
          - WixUIExtension
          - WixUtilExtension
"#;
        let config: anodizer_core::config::Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msi = &config.crates[0].msis.as_ref().unwrap()[0];

        assert_eq!(msi.id.as_deref(), Some("my-msi"));
        assert_eq!(msi.ids.as_ref().unwrap(), &["build-win-amd64".to_string()]);
        assert_eq!(msi.wxs.as_deref(), Some("installer/app.wxs"));
        assert_eq!(
            msi.name.as_deref(),
            Some("myapp-{{ .Version }}-{{ .MsiArch }}")
        );
        assert_eq!(msi.version.as_deref(), Some("v4"));
        assert_eq!(msi.replace, Some(true));
        assert_eq!(msi.mod_timestamp.as_deref(), Some("2024-01-01T00:00:00Z"));
        assert_eq!(
            msi.skip,
            Some(anodizer_core::config::StringOrBool::String(
                "{{ .Env.SKIP_MSI }}".to_string()
            ))
        );
        assert_eq!(
            msi.extra_files.as_ref().unwrap(),
            &["README.md".to_string(), "LICENSE".to_string()]
        );
        assert_eq!(
            msi.extensions.as_ref().unwrap(),
            &["WixUIExtension".to_string(), "WixUtilExtension".to_string()]
        );
    }

    // --- `msi.if` + `msi.hooks` ---

    fn msi_test_ctx_with_if(if_expr: Option<&str>) -> anodizer_core::context::Context {
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        std::fs::create_dir_all(&config.dist).unwrap();
        let msi_cfg = MsiConfig {
            wxs: Some("dummy.wxs".to_string()),
            if_condition: if_expr.map(str::to_string),
            ..Default::default()
        };
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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
        ctx.template_vars_mut().set("Os", "windows");
        ctx
    }

    #[test]
    fn test_msi_if_false_skips_config() {
        let mut ctx = msi_test_ctx_with_if(Some("false"));
        MsiStage.run(&mut ctx).unwrap();
        assert_eq!(
            ctx.artifacts.by_kind(ArtifactKind::Installer).len(),
            0,
            "msi if=false should skip"
        );
    }

    #[test]
    fn test_msi_if_render_failure_is_hard_error() {
        let mut ctx = msi_test_ctx_with_if(Some("{{ undefined_function 42 }}"));
        let err = MsiStage
            .run(&mut ctx)
            .expect_err("unrenderable `if` should hard-error");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("`if` template render failed"),
            "error should name `if` render failure, got: {msg}"
        );
    }

    #[test]
    fn test_config_parse_msi_hooks_pre_post() {
        // BuildHooksConfig uses `pre:` / `post:`.
        use anodizer_core::config::Config;
        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    msis:
      - wxs: installer.wxs
        hooks:
          pre:
            - echo pre-msi-build
          post:
            - echo post-msi-build
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let msis = config.crates[0].msis.as_ref().unwrap();
        let hooks = msis[0].hooks.as_ref().unwrap();
        let pre = hooks
            .pre
            .as_ref()
            .expect("`pre:` yaml should populate `pre`");
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0], "echo pre-msi-build");
        let post = hooks
            .post
            .as_ref()
            .expect("`post:` yaml should populate `post`");
        assert_eq!(post.len(), 1);
        assert_eq!(post[0], "echo post-msi-build");
    }

    // -------------------------------------------------------------------
    // `msi.amd64_variant` filter
    // -------------------------------------------------------------------

    /// Build a context with three windows/amd64 binaries (variants v1/v2/v3)
    /// plus one windows/arm64 binary. The `amd64_variant` field on the config
    /// drives which subset of amd64 binaries reaches `Installer` artifact creation.
    fn msi_amd64_variant_test_ctx(amd64_variant: Option<&str>) -> anodizer_core::context::Context {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            amd64_variant: amd64_variant.map(str::to_string),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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

        // 3 amd64 variants — same target triple, different `amd64_variant`
        // metadata. Path differs so artifact-add doesn't dedup.
        for variant in ["v1", "v2", "v3"] {
            ctx.artifacts.add(Artifact {
                kind: ArtifactKind::Binary,
                name: String::new(),
                path: PathBuf::from(format!("dist/myapp_{variant}.exe")),
                target: Some("x86_64-pc-windows-msvc".to_string()),
                crate_name: "myapp".to_string(),
                metadata: HashMap::from([("amd64_variant".to_string(), variant.to_string())]),
                size: None,
            });
        }
        // arm64 binary — outside the amd64 filter's scope.
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp_arm.exe"),
            target: Some("aarch64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });
        ctx
    }

    #[test]
    fn test_msi_unfiltered_same_arch_variants_get_distinct_names() {
        // amd64_variant unset passes all three x86_64 variants through the
        // filter. The default name appends the amd64 micro-arch suffix, so the
        // three same-triple builds render `myapp_x64.msi` / `myapp_x64v2.msi` /
        // `myapp_x64v3.msi` (v1 baseline renders no suffix) and arm64 renders
        // `myapp_arm64.msi` — 4 distinct installers, no clobber.
        let mut ctx = msi_amd64_variant_test_ctx(None);
        MsiStage.run(&mut ctx).unwrap();
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 4, "{installers:?}");
        let names: Vec<&str> = installers
            .iter()
            .map(|a| a.path.file_name().unwrap().to_str().unwrap())
            .collect();
        let distinct: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(
            distinct.len(),
            names.len(),
            "all rendered installer names must be distinct: {names:?}"
        );
    }

    #[test]
    fn test_msi_amd64_variant_v3_only_keeps_matching_variant() {
        let mut ctx = msi_amd64_variant_test_ctx(Some("v3"));
        MsiStage.run(&mut ctx).unwrap();
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        // Only v3 amd64 + arm64 (always passes) -> 2 MSIs.
        assert_eq!(installers.len(), 2);
        let targets: Vec<&str> = installers
            .iter()
            .map(|a| a.target.as_deref().unwrap())
            .collect();
        assert!(targets.contains(&"x86_64-pc-windows-msvc"));
        assert!(targets.contains(&"aarch64-pc-windows-msvc"));
    }

    #[test]
    fn test_msi_amd64_variant_filter_does_not_drop_arm64() {
        // Pin: filter only constrains amd64. arm64 must still pass even
        // when no amd64 variant matches.
        let mut ctx = msi_amd64_variant_test_ctx(Some("v9000"));
        MsiStage.run(&mut ctx).unwrap();
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
        assert_eq!(
            installers[0].target.as_deref(),
            Some("aarch64-pc-windows-msvc")
        );
    }

    // -----------------------------------------------------------------------
    // post-hook artifact var injection
    // -----------------------------------------------------------------------

    #[test]
    fn test_post_hook_artifact_vars_are_set() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{BuildHooksConfig, Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        // A post-hook that writes ArtifactPath into a temp file so we can
        // assert it was rendered.
        let marker = tmp.path().join("artifact_path.txt");
        let hook_cmd = format!("echo '{{{{ ArtifactPath }}}}' > {}", marker.display());

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            hooks: Some(BuildHooksConfig {
                post: Some(vec![anodizer_core::config::HookEntry::Simple(hook_cmd)]),
                pre: None,
            }),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // In dry-run mode hooks are skipped; we verify the stage completes
        // and an installer artifact is registered (hook path is exercised).
        MsiStage.run(&mut ctx).unwrap();
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Installer).len(), 1);
    }

    #[test]
    fn test_build_post_hook_template_vars_injects_artifact_keys() {
        use anodizer_core::context::{Context, ContextOptions};

        // A real on-disk file so canonicalize() returns an absolute path
        // rather than falling back to the cwd-join branch.
        let tmp = TempDir::new().unwrap();
        let msi_path = tmp.path().join("myapp_x64.msi");
        fs::write(&msi_path, b"fake-msi").unwrap();

        let mut config = anodizer_core::config::Config::default();
        config.project_name = "myapp".to_string();
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("Version", "1.2.3");

        let vars = build_post_hook_template_vars(&ctx, &msi_path);

        let artifact_path = vars.get("ArtifactPath").expect("ArtifactPath set");
        assert!(
            std::path::Path::new(artifact_path).is_absolute(),
            "ArtifactPath must be absolute, got: {artifact_path}"
        );
        assert!(
            artifact_path.ends_with("myapp_x64.msi"),
            "ArtifactPath should resolve to the .msi, got: {artifact_path}"
        );
        assert_eq!(
            vars.get("ArtifactName").map(String::as_str),
            Some("myapp_x64.msi")
        );
        assert_eq!(vars.get("ArtifactExt").map(String::as_str), Some(".msi"));
        // Pre-existing vars must still be present in the cloned snapshot.
        assert_eq!(vars.get("Version").map(String::as_str), Some("1.2.3"));
    }

    #[test]
    fn test_build_post_hook_template_vars_resolves_relative_path() {
        use anodizer_core::context::{Context, ContextOptions};

        // Path that does not exist — canonicalize() fails; the relative
        // fallback should still produce an absolute path via cwd.
        let msi_path = PathBuf::from("dist/windows/nonexistent_x64.msi");

        let mut config = anodizer_core::config::Config::default();
        config.project_name = "myapp".to_string();
        let ctx = Context::new(config, ContextOptions::default());

        let vars = build_post_hook_template_vars(&ctx, &msi_path);
        let artifact_path = vars.get("ArtifactPath").expect("ArtifactPath set");
        assert!(
            std::path::Path::new(artifact_path).is_absolute(),
            "relative msi_path should be resolved to an absolute path, got: {artifact_path}"
        );
        assert!(
            artifact_path.ends_with("nonexistent_x64.msi"),
            "ArtifactPath should still end with the filename, got: {artifact_path}"
        );
    }

    #[test]
    fn test_run_msi_post_hook_none_hook_is_noop() {
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let msi_path = tmp.path().join("myapp_x64.msi");

        let config = anodizer_core::config::Config::default();
        let ctx = Context::new(config, ContextOptions::default());

        run_msi_post_hook(
            &ctx,
            None,
            &msi_path,
            "default",
            "myapp",
            true,
            &ctx.logger("msi"),
        )
        .unwrap();
    }

    // -----------------------------------------------------------------------
    // mod_timestamp template rendering
    // -----------------------------------------------------------------------

    #[test]
    fn test_mod_timestamp_template_renders() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        // The raw template references a custom env var. If the stage forwarded
        // the raw string to `parse_mod_timestamp` (the pre-fix bug), the
        // unrendered `{{ ... }}` would be unparseable and the run would fail.
        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            mod_timestamp: Some("{{ Env.SOURCE_DATE_EPOCH }}".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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
        // SOURCE_DATE_EPOCH expands to a valid unix epoch via the Env. namespace.
        ctx.template_vars_mut()
            .set_env("SOURCE_DATE_EPOCH", "1704067200");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Stage must succeed — proves the raw `{{ Env.SOURCE_DATE_EPOCH }}`
        // was rendered to `"1704067200"` before reaching parse_mod_timestamp.
        MsiStage.run(&mut ctx).unwrap();
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Installer).len(), 1);

        // Belt-and-braces: confirm the template engine resolves the var to
        // a parseable timestamp on its own, pinning the contract.
        let rendered = ctx.render_template("{{ Env.SOURCE_DATE_EPOCH }}").unwrap();
        assert_eq!(rendered, "1704067200");
        anodizer_core::util::parse_mod_timestamp(&rendered).unwrap();
    }

    #[test]
    fn test_mod_timestamp_invalid_template_errors() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            // Invalid Tera — unclosed tag
            mod_timestamp: Some("{{ bad_ts".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let err = MsiStage.run(&mut ctx).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("mod_timestamp") || msg.contains("render"),
            "error should mention mod_timestamp rendering, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // wxs path template rendering
    // -----------------------------------------------------------------------

    #[test]
    fn test_wxs_path_template_renders() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        // Path contains a template var — rendered it should resolve to the
        // actual file; unrendered it would produce "no such file".
        let wxs_template = format!("{}/{{{{ ProjectName }}}}.wxs", tmp.path().display());

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_template),
            ..Default::default()
        };

        let mut config = Config::default();
        // ProjectName = "app" so the rendered path is tmp/app.wxs (which exists).
        config.project_name = "app".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "app".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
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
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/app.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "app".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        // Must succeed — the rendered path resolves to the existing .wxs file.
        MsiStage.run(&mut ctx).unwrap();
        assert_eq!(ctx.artifacts.by_kind(ArtifactKind::Installer).len(), 1);
    }

    // -----------------------------------------------------------------------
    // default name shape
    // -----------------------------------------------------------------------

    #[test]
    fn test_default_name_template_contains_amd64_variant_suffix() {
        assert!(
            crate::template::default_msi_name_template()
                .contains(anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX),
            "default name template must reuse the shared amd64 variant suffix"
        );
    }

    #[test]
    fn test_default_name_matches_goreleaser_shape() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let msi_cfg = MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            // No name: field — default applies.
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![msi_cfg]),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        ctx.template_vars_mut().set("Version", "2.0.0");
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        MsiStage.run(&mut ctx).unwrap();
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(installers.len(), 1);
        let filename = installers[0].path.file_name().unwrap().to_str().unwrap();
        // Default: ProjectName_MsiArch (no version injection).
        assert_eq!(filename, "myapp_x64.msi");
        assert!(
            !filename.contains("2.0.0"),
            "default name must not inject version"
        );
    }

    /// Resolve a runnable WiX build path on a Windows runner, returning the
    /// commands that turn `wxs_path` into `out_path`. WiX v3 ships preinstalled
    /// on the hosted image but is not on PATH, so `candle`/`light` are reached
    /// by absolute path under the toolset bin dir when a bare lookup fails. v4's
    /// `wix` (a dotnet global tool) is honored when present. `None` means no WiX
    /// toolchain is reachable and the caller skips hermetically.
    #[cfg(target_os = "windows")]
    fn resolve_windows_wix_build(wxs_path: &str, out_path: &str) -> Option<MsiCommands> {
        if anodizer_core::util::find_binary("wix") {
            return Some(msi_command(WixVersion::V4, wxs_path, out_path, &[]));
        }
        let mut cmds = msi_command(WixVersion::V3, wxs_path, out_path, &[]);
        let resolve_v3 = |bare: &str| -> Option<String> {
            if anodizer_core::util::find_binary(bare) {
                return Some(bare.to_string());
            }
            // The hosted windows image installs WiX v3.14 but leaves its bin dir
            // off PATH (actions/runner-images#9551); reach the tools directly.
            let abs = format!("C:\\Program Files (x86)\\WiX Toolset v3.14\\bin\\{bare}.exe");
            std::path::Path::new(&abs).exists().then_some(abs)
        };
        cmds.primary[0] = resolve_v3("candle")?;
        if let Some(link) = cmds.link.as_mut() {
            link[0] = resolve_v3("light")?;
        }
        Some(cmds)
    }

    /// Two `wix build` runs over an identical `.wxs` at a pinned timestamp are
    /// NOT byte-identical, and this records WHY so the verdict is observable
    /// before a release rather than hidden behind a determinism allowlist.
    ///
    /// WiX has no reproducible-build mode (wixtoolset/issues#8978, open): every
    /// build regenerates a random `SummaryInformation.PackageCode` GUID and
    /// stamps wall-clock `Created`/`LastModified` into the MSI summary stream —
    /// independent of any timestamp flag. anodizer invokes WiX directly with no
    /// post-build summary-stream normalization, so the native `.msi` inherits
    /// that non-determinism. The assertion below proves the drift is real (not a
    /// flake) and pins the root cause; flipping it to byte-equality is the
    /// regression signal if WiX ever ships deterministic output or anodizer adds
    /// summary-stream normalization. Runs on the Windows CI test shard only.
    #[test]
    #[cfg(target_os = "windows")]
    fn msi_is_byte_reproducible_across_time() {
        let tmp = TempDir::new().unwrap();
        // WiX v4-dialect package (also accepted by v3 candle/light via the v3
        // namespace fallback path). A minimal single-file component is enough to
        // exercise the summary-information stream that carries the drift.
        let wxs = r#"<?xml version="1.0" encoding="UTF-8"?>
<Wix xmlns="http://wixtoolset.org/schemas/v4/wxs">
  <Package Name="ReproProbe" Manufacturer="anodizer" Version="1.2.3"
           UpgradeCode="11111111-1111-1111-1111-111111111111">
    <MediaTemplate EmbedCab="yes" />
    <StandardDirectory Id="ProgramFiles6432Folder">
      <Directory Id="INSTALLFOLDER" Name="ReproProbe">
        <Component Id="MainExe" Guid="22222222-2222-2222-2222-222222222222">
          <File Id="MainExe" Source="payload.txt" />
        </Component>
      </Directory>
    </StandardDirectory>
    <Feature Id="Main">
      <ComponentRef Id="MainExe" />
    </Feature>
  </Package>
</Wix>"#;
        let wxs_path = tmp.path().join("repro.wxs");
        fs::write(&wxs_path, wxs).unwrap();
        fs::write(tmp.path().join("payload.txt"), b"deterministic payload\n").unwrap();

        let build = |out_name: &str| -> Option<Vec<u8>> {
            let out_path = tmp.path().join(out_name);
            let cmds = resolve_windows_wix_build(
                &wxs_path.to_string_lossy(),
                &out_path.to_string_lossy(),
            )?;
            let run = |argv: &[String]| {
                std::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .current_dir(tmp.path())
                    // Pin the bind timestamp; WiX ignores it for the summary
                    // stream but it removes one extra source of variance.
                    .env("SOURCE_DATE_EPOCH", "1700000000")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
            };
            run(&cmds.primary)?;
            if let Some(link) = &cmds.link {
                run(link)?;
            }
            fs::read(&out_path).ok()
        };

        let (Some(a), Some(b)) = (build("a.msi"), build("b.msi")) else {
            eprintln!("WiX toolchain unavailable or build failed; test skipped hermetically");
            return;
        };
        assert_ne!(
            a, b,
            "WiX emits a random PackageCode GUID + wall-clock Created/LastModified \
             into the MSI summary stream every build (wixtoolset/issues#8978); the \
             native .msi is NOT byte-reproducible and anodizer does not normalize \
             it. If this now matches, WiX gained deterministic output or anodizer \
             added summary-stream normalization — make the .msi byte-stable and \
             flip this assertion."
        );
    }

    #[test]
    fn test_two_configs_same_crate_same_arch_default_name_bails() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        // Two msi configs for ONE crate, both the DEFAULT name template. With a
        // single Windows target present, both render `myapp_x64.msi` — the same
        // path. A per-config guard would reset between them and let the second
        // silently clobber the first; a per-crate guard must bail.
        let make_cfg = || MsiConfig {
            wxs: Some(wxs_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![make_cfg(), make_cfg()]),
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

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        let err = MsiStage
            .run(&mut ctx)
            .expect_err("two configs rendering the same path must bail");
        let msg = err.to_string();
        assert!(msg.contains("msis:"), "{msg}");
        assert!(msg.contains("crate 'myapp'"), "{msg}");
        assert!(msg.contains("{{ .Arch }}"), "{msg}");
    }

    #[test]
    fn test_two_configs_same_crate_distinct_names_pass() {
        use anodizer_core::artifact::Artifact;
        use anodizer_core::config::{Config, CrateConfig, MsiConfig};
        use anodizer_core::context::{Context, ContextOptions};

        let tmp = TempDir::new().unwrap();
        let wxs_path = tmp.path().join("app.wxs");
        fs::write(&wxs_path, "<Wix/>").unwrap();

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            msis: Some(vec![
                MsiConfig {
                    wxs: Some(wxs_path.to_string_lossy().into_owned()),
                    name: Some("{{ ProjectName }}-one_{{ MsiArch }}.msi".to_string()),
                    ..Default::default()
                },
                MsiConfig {
                    wxs: Some(wxs_path.to_string_lossy().into_owned()),
                    name: Some("{{ ProjectName }}-two_{{ MsiArch }}.msi".to_string()),
                    ..Default::default()
                },
            ]),
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

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp.exe"),
            target: Some("x86_64-pc-windows-msvc".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        });

        MsiStage
            .run(&mut ctx)
            .expect("distinct names across configs must not collide");
        let installers = ctx.artifacts.by_kind(ArtifactKind::Installer);
        assert_eq!(
            installers.len(),
            2,
            "expected one installer per distinct-named config"
        );
    }
}
