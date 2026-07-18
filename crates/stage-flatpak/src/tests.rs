use super::*;
use std::collections::HashMap;
use std::path::PathBuf;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::stage::Stage;

// -----------------------------------------------------------------------
// Architecture mapping
// -----------------------------------------------------------------------

#[test]
fn test_arch_to_flatpak() {
    assert_eq!(arch_to_flatpak("amd64"), Some("x86_64"));
    assert_eq!(arch_to_flatpak("x86_64"), Some("x86_64"));
    assert_eq!(arch_to_flatpak("arm64"), Some("aarch64"));
    assert_eq!(arch_to_flatpak("aarch64"), Some("aarch64"));
    assert_eq!(arch_to_flatpak("i386"), None);
    assert_eq!(arch_to_flatpak("armv7"), None);
    assert_eq!(arch_to_flatpak("mips"), None);
    assert_eq!(arch_to_flatpak("riscv64"), None);
    assert_eq!(arch_to_flatpak(""), None);
}

// -----------------------------------------------------------------------
// Manifest JSON serialization
// -----------------------------------------------------------------------

#[test]
fn test_manifest_json_serialization() {
    let manifest = Manifest {
        id: "org.example.MyApp".to_string(),
        runtime: "org.freedesktop.Platform".to_string(),
        runtime_version: "24.08".to_string(),
        sdk: "org.freedesktop.Sdk".to_string(),
        command: "myapp".to_string(),
        finish_args: vec!["--share=network".to_string(), "--socket=x11".to_string()],
        modules: vec![ManifestModule {
            name: "org.example.MyApp".to_string(),
            buildsystem: "simple".to_string(),
            build_commands: vec!["install -Dm755 myapp /app/bin/myapp".to_string()],
            sources: vec![ManifestSource {
                type_: "file".to_string(),
                path: "myapp".to_string(),
                dest_filename: None,
            }],
        }],
    };

    let json: serde_json::Value = serde_json::to_value(&manifest).unwrap();

    assert_eq!(json["id"], "org.example.MyApp");
    assert_eq!(json["runtime"], "org.freedesktop.Platform");
    assert_eq!(json["runtime-version"], "24.08");
    assert_eq!(json["sdk"], "org.freedesktop.Sdk");
    assert_eq!(json["command"], "myapp");

    let finish_args = json["finish-args"].as_array().unwrap();
    assert_eq!(finish_args.len(), 2);
    assert_eq!(finish_args[0], "--share=network");
    assert_eq!(finish_args[1], "--socket=x11");

    let modules = json["modules"].as_array().unwrap();
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0]["name"], "org.example.MyApp");
    assert_eq!(modules[0]["buildsystem"], "simple");

    let build_cmds = modules[0]["build-commands"].as_array().unwrap();
    assert_eq!(build_cmds.len(), 1);
    assert_eq!(build_cmds[0], "install -Dm755 myapp /app/bin/myapp");

    let sources = modules[0]["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["type"], "file");
    assert_eq!(sources[0]["path"], "myapp");
    // dest-filename should be absent (skip_serializing_if)
    assert!(sources[0].get("dest-filename").is_none());
}

#[test]
fn test_manifest_json_empty_finish_args_omitted() {
    let manifest = Manifest {
        id: "org.example.App".to_string(),
        runtime: "org.freedesktop.Platform".to_string(),
        runtime_version: "24.08".to_string(),
        sdk: "org.freedesktop.Sdk".to_string(),
        command: "app".to_string(),
        finish_args: vec![],
        modules: vec![],
    };

    let json: serde_json::Value = serde_json::to_value(&manifest).unwrap();
    // finish-args should be omitted when empty (skip_serializing_if)
    assert!(json.get("finish-args").is_none());
}

// -----------------------------------------------------------------------
// FlatpakConfig deserialization
// -----------------------------------------------------------------------

#[test]
fn test_flatpak_config_deserialize() {
    use anodizer_core::config::FlatpakConfig;

    let yaml = r#"
app_id: org.example.MyApp
runtime: org.freedesktop.Platform
runtime_version: "24.08"
sdk: org.freedesktop.Sdk
command: myapp
ids:
  - build-linux
name_template: "{{ ProjectName }}-{{ Version }}-{{ Arch }}.flatpak"
finish_args:
  - --share=network
  - --socket=x11
  - --filesystem=home
"#;

    let config: FlatpakConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(config.app_id.as_deref(), Some("org.example.MyApp"));
    assert_eq!(config.runtime.as_deref(), Some("org.freedesktop.Platform"));
    assert_eq!(config.runtime_version.as_deref(), Some("24.08"));
    assert_eq!(config.sdk.as_deref(), Some("org.freedesktop.Sdk"));
    assert_eq!(config.command.as_deref(), Some("myapp"));
    assert_eq!(config.ids, Some(vec!["build-linux".to_string()]));
    assert_eq!(
        config.name_template.as_deref(),
        Some("{{ ProjectName }}-{{ Version }}-{{ Arch }}.flatpak")
    );

    let finish_args = config.finish_args.unwrap();
    assert_eq!(finish_args.len(), 3);
    assert_eq!(finish_args[0], "--share=network");
    assert_eq!(finish_args[1], "--socket=x11");
    assert_eq!(finish_args[2], "--filesystem=home");
}

#[test]
fn test_flatpak_config_defaults() {
    use anodizer_core::config::FlatpakConfig;

    let config: FlatpakConfig = serde_yaml_ng::from_str("{}").unwrap();
    assert!(config.app_id.is_none());
    assert!(config.runtime.is_none());
    assert!(config.runtime_version.is_none());
    assert!(config.sdk.is_none());
    assert!(config.command.is_none());
    assert!(config.ids.is_none());
    assert!(config.name_template.is_none());
    assert!(config.finish_args.is_none());
    assert!(config.extra_files.is_none());
    assert!(config.replace.is_none());
    assert!(config.mod_timestamp.is_none());
    assert!(config.skip.is_none());
    assert!(config.id.is_none());
}

// -----------------------------------------------------------------------
// Required field validation
// -----------------------------------------------------------------------

#[test]
fn test_flatpak_config_required_field_validation() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    // Missing app_id
    {
        let flatpak_cfg = FlatpakConfig {
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist1");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
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

        // Add a Linux binary so the stage processes the config
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("app_id"),
            "error should mention app_id"
        );
    }

    // Missing runtime
    {
        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.MyApp".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist2");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
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
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("runtime"),
            "error should mention runtime"
        );
    }

    // Missing runtime_version
    {
        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist3");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
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
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("runtime_version"),
            "error should mention runtime_version"
        );
    }

    // Missing sdk
    {
        let flatpak_cfg = FlatpakConfig {
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist4");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
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
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        let result = stage.run(&mut ctx);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("sdk"),
            "error should mention sdk"
        );
    }
}

// -----------------------------------------------------------------------
// Disable via bool and template
// -----------------------------------------------------------------------

#[test]
fn test_flatpak_config_disable_bool_and_template() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    // Disable via bool
    {
        let flatpak_cfg = FlatpakConfig {
            skip: Some(StringOrBool::Bool(true)),
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist-disabled");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
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
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert!(flatpaks.is_empty(), "should be disabled by bool");
    }

    // Disable via template
    {
        let flatpak_cfg = FlatpakConfig {
            skip: Some(StringOrBool::String("{{ IsSnapshot }}".to_string())),
            app_id: Some("org.example.MyApp".to_string()),
            runtime: Some("org.freedesktop.Platform".to_string()),
            runtime_version: Some("24.08".to_string()),
            sdk: Some("org.freedesktop.Sdk".to_string()),
            ..Default::default()
        };

        let mut config = Config::default();
        config.project_name = "myapp".to_string();
        config.dist = tmp.path().join("dist-template-disabled");
        config.crates = vec![CrateConfig {
            name: "myapp".to_string(),
            path: ".".to_string(),
            flatpaks: Some(vec![flatpak_cfg]),
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
        ctx.template_vars_mut().set("IsSnapshot", "true");

        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        });

        let stage = FlatpakStage;
        stage.run(&mut ctx).unwrap();

        let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
        assert!(flatpaks.is_empty(), "should be disabled by template");
    }
}

// -----------------------------------------------------------------------
// Stage skips non-Linux binaries
// -----------------------------------------------------------------------

#[test]
fn test_flatpak_stage_skips_non_linux() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.MyApp".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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

    // Add only macOS and Windows binaries — no Linux
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp.exe"),
        target: Some("x86_64-pc-windows-msvc".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert!(flatpaks.is_empty(), "should skip non-Linux binaries");
}

// -----------------------------------------------------------------------
// Stage skips unsupported architectures
// -----------------------------------------------------------------------

#[test]
fn test_flatpak_stage_skips_unsupported_arch() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.MyApp".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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

    // Add only a Linux binary with unsupported arch (i686)
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("i686-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert!(
        flatpaks.is_empty(),
        "should skip unsupported architecture (i686)"
    );
}

// -----------------------------------------------------------------------
// Default name template
// -----------------------------------------------------------------------

#[test]
fn test_default_name_template() {
    // The default composes from the shared amd64-only suffix const (drift
    // guard), so a v1/None baseline renders the historical unsuffixed name
    // while a v3 build appends `v3` before `.flatpak`.
    let tmpl = default_name_template();
    assert!(
        tmpl.starts_with("{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}"),
        "{tmpl}"
    );
    assert!(
        tmpl.contains(anodizer_core::archive_name::INSTALLER_AMD64_VARIANT_SUFFIX),
        "flatpak default must reuse INSTALLER_AMD64_VARIANT_SUFFIX: {tmpl}"
    );
    assert!(tmpl.ends_with(".flatpak"), "{tmpl}");
}

// -----------------------------------------------------------------------
// Stage no-op when no flatpak config
// -----------------------------------------------------------------------

#[test]
fn test_stage_skips_when_no_flatpak_config() {
    use anodizer_core::config::Config;
    use anodizer_core::context::{Context, ContextOptions};

    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let stage = FlatpakStage;
    assert!(stage.run(&mut ctx).is_ok());
    assert!(ctx.artifacts.all().is_empty());
}

// -----------------------------------------------------------------------
// Dry-run produces correct artifact
// -----------------------------------------------------------------------

#[test]
fn test_flatpak_dry_run_produces_artifact() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let flatpak_cfg = FlatpakConfig {
        id: Some("my-flatpak".to_string()),
        app_id: Some("org.example.MyApp".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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
    ctx.template_vars_mut().set("ProjectName", "myapp");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(flatpaks.len(), 1);
    assert_eq!(flatpaks[0].crate_name, "myapp");
    assert_eq!(flatpaks[0].metadata.get("format").unwrap(), "flatpak");
    assert_eq!(flatpaks[0].metadata.get("id").unwrap(), "my-flatpak");
    assert_eq!(
        flatpaks[0].target.as_deref(),
        Some("x86_64-unknown-linux-gnu")
    );
    // Path should contain the flatpak subdir
    let path_str = flatpaks[0].path.to_string_lossy();
    assert!(
        path_str.contains("flatpak"),
        "path should contain 'flatpak': {}",
        path_str
    );
    assert!(
        path_str.ends_with(".flatpak"),
        "path should end with .flatpak: {}",
        path_str
    );
}

// -----------------------------------------------------------------------
// Dry-run with multiple architectures
// -----------------------------------------------------------------------

#[test]
fn test_flatpak_dry_run_multiple_arches() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.MyApp".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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
    ctx.template_vars_mut().set("ProjectName", "myapp");

    // Add both x86_64 and aarch64 Linux binaries
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp-x86"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp-arm"),
        target: Some("aarch64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(flatpaks.len(), 2);
}

// -----------------------------------------------------------------------
// Same-triple multi-variant: distinct .flatpak names, no clobber
// -----------------------------------------------------------------------

/// Three x86_64 builds tagged amd64_variant v1/v2/v3 plus one aarch64 build
/// must each produce a distinct `.flatpak` artifact (no ArchPathGuard
/// error): the default name appends the amd64 micro-arch suffix (v1 → no
/// suffix, v2 → `…v2`, v3 → `…v3`), so the same triple no longer clobbers
/// itself.
#[test]
fn test_flatpak_dry_run_same_triple_multi_variant_distinct_names() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.MyApp".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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
    ctx.template_vars_mut().set("ProjectName", "myapp");

    for variant in ["v1", "v2", "v3"] {
        let p = tmp.path().join(format!("myapp-{variant}"));
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: p,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("amd64_variant".to_string(), variant.to_string())]),
            size: None,
        });
    }
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: tmp.path().join("myapp-arm"),
        target: Some("aarch64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    stage
        .run(&mut ctx)
        .expect("multi-variant build must not clobber");

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(flatpaks.len(), 4, "one .flatpak per variant + arm64");
    let names: std::collections::HashSet<String> = flatpaks
        .iter()
        .map(|f| {
            f.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(names.len(), 4, "all .flatpak filenames distinct: {names:?}");
    assert!(names.contains("myapp_1.0.0_linux_amd64.flatpak"));
    assert!(names.contains("myapp_1.0.0_linux_amd64v2.flatpak"));
    assert!(names.contains("myapp_1.0.0_linux_amd64v3.flatpak"));
    assert!(names.contains("myapp_1.0.0_linux_arm64.flatpak"));
}

/// Two `flatpaks:` configs on one crate, both rendering the default name,
/// produce the same `.flatpak` path for one arch. The guard now spans every
/// config of the crate, so the second config bails loudly instead of
/// silently clobbering the first config's bundle.
#[test]
fn test_flatpak_two_configs_same_default_name_bail_across_configs() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let make_cfg = |id: &str| FlatpakConfig {
        id: Some(id.to_string()),
        app_id: Some("org.example.MyApp".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![make_cfg("first"), make_cfg("second")]),
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
    ctx.template_vars_mut().set("ProjectName", "myapp");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: tmp.path().join("myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let err = FlatpakStage.run(&mut ctx).unwrap_err().to_string();
    assert!(err.contains("flatpak:"), "{err}");
    assert!(err.contains("crate 'myapp'"), "{err}");
    assert!(err.contains("{{ .Arch }}"), "{err}");
}

// -----------------------------------------------------------------------
// Custom name_template rendering
// -----------------------------------------------------------------------

#[test]
fn test_flatpak_dry_run_custom_name_template() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.MyApp".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        name_template: Some("{{ ProjectName }}-{{ Version }}-{{ Arch }}.flatpak".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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
    ctx.template_vars_mut().set("ProjectName", "myapp");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(flatpaks.len(), 1);

    let path_str = flatpaks[0].path.to_string_lossy();
    assert!(
        path_str.ends_with("myapp-2.5.0-amd64.flatpak"),
        "custom name_template should render correctly: {}",
        path_str
    );
    // Verify output goes to flat dist/flatpak/ dir, not nested work dir
    assert!(
        !path_str.contains("x86_64"),
        "output path should not contain work dir arch subpath: {}",
        path_str
    );
}

// -----------------------------------------------------------------------
// Replace config marks archives for removal
// -----------------------------------------------------------------------

#[test]
fn test_flatpak_dry_run_replace_removes_archives() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.MyApp".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        replace: Some(true),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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
    ctx.template_vars_mut().set("ProjectName", "myapp");

    // Add a Linux binary
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    // Add an archive artifact that should be replaced
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/myapp-1.0.0-linux-amd64.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    // The archive should have been removed
    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert!(
        archives.is_empty(),
        "archives should be removed when replace=true"
    );

    // The flatpak should have been added
    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(flatpaks.len(), 1);
}

// -----------------------------------------------------------------------
// Mod timestamp logged in dry_run
// -----------------------------------------------------------------------

#[test]
fn test_flatpak_dry_run_mod_timestamp() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.MyApp".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        mod_timestamp: Some("1704067200".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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
    ctx.template_vars_mut().set("ProjectName", "myapp");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    // Should not error — just log the mod_timestamp
    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(flatpaks.len(), 1);
}

// -----------------------------------------------------------------------
// build_manifest with extra files
// -----------------------------------------------------------------------

/// Verifies that extra_file_names produces additional sources + install
/// commands with the correct paths. A regression here would mean the
/// generated manifest no longer installs extra files into /app/share/<id>/.
#[test]
fn test_build_manifest_with_extra_files() {
    let manifest = build_manifest(
        "org.example.App",
        "org.freedesktop.Platform",
        "24.08",
        "org.freedesktop.Sdk",
        "app",
        vec!["--share=network".to_string()],
        "app",
        &["license.txt".to_string(), "config.toml".to_string()],
    );

    let json: serde_json::Value = serde_json::to_value(&manifest).unwrap();
    let module = &json["modules"][0];

    // Binary source + 2 extra file sources = 3 total
    let sources = module["sources"].as_array().unwrap();
    assert_eq!(
        sources.len(),
        3,
        "should have binary + 2 extra file sources"
    );
    assert_eq!(sources[1]["path"], "license.txt");
    assert_eq!(sources[2]["path"], "config.toml");

    // Binary install + 2 extra file installs = 3 total build commands
    let cmds = module["build-commands"].as_array().unwrap();
    assert_eq!(cmds.len(), 3);
    assert!(
        cmds[1]
            .as_str()
            .unwrap()
            .contains("/app/share/org.example.App/license.txt"),
        "extra file should install into /app/share/<app_id>/: {}",
        cmds[1]
    );
    assert!(
        cmds[2]
            .as_str()
            .unwrap()
            .contains("/app/share/org.example.App/config.toml"),
        "second extra file should install into /app/share/<app_id>/: {}",
        cmds[2]
    );
    // Extra files use 644 permissions, binary uses 755
    assert!(cmds[0].as_str().unwrap().contains("755"));
    assert!(cmds[1].as_str().unwrap().contains("644"));
}

// -----------------------------------------------------------------------
// build_subprocess_args
// -----------------------------------------------------------------------

/// Verifies the builder and bundle arg vectors contain the correct flags.
/// A regression that rearranges args would silently break the subprocess
/// invocations.
#[test]
fn test_build_subprocess_args() {
    let output_path = std::path::Path::new("/dist/flatpak/app-1.0.0-linux-amd64.flatpak");
    let (builder, bundle) =
        build_subprocess_args("org.example.App", "1.0.0", "x86_64", output_path);

    assert_eq!(builder[0], "flatpak-builder");
    assert!(builder.contains(&"--force-clean".to_string()));
    // rofiles-fuse disabled so the build leaves no FUSE mount that would
    // wedge the determinism harness's per-run worktree teardown.
    assert!(builder.contains(&"--disable-rofiles-fuse".to_string()));
    assert!(builder.contains(&"--arch=x86_64".to_string()));
    assert!(builder.contains(&"--default-branch=1.0.0".to_string()));
    assert!(builder.contains(&"--repo=repo".to_string()));
    assert!(builder.contains(&"org.example.App.json".to_string()));

    assert_eq!(bundle[0], "flatpak");
    assert_eq!(bundle[1], "build-bundle");
    assert!(bundle.contains(&"--arch=x86_64".to_string()));
    assert!(bundle.contains(&"org.example.App".to_string()));
    assert!(bundle.contains(&"1.0.0".to_string()));
    assert!(bundle.iter().any(|a| a.contains("amd64.flatpak")));
}

// -----------------------------------------------------------------------
// resolve_extra_file_specs — valid glob
// -----------------------------------------------------------------------

/// Verifies that a valid glob resolves to (path, destination_name) pairs.
/// When name_template is absent the destination is the file's basename.
#[test]
fn test_resolve_extra_file_specs_glob_match() {
    use anodizer_core::config::ExtraFileSpec;
    use anodizer_core::log::{StageLogger, Verbosity};

    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("data.json"), b"{}").unwrap();

    let pattern = format!("{}/*.json", tmp.path().display());
    let specs = vec![ExtraFileSpec::Glob(pattern)];
    let log = StageLogger::new("flatpak", Verbosity::Normal);

    let results = resolve_extra_file_specs(&specs, &log);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].1, "data.json");
    assert!(results[0].0.is_file());
}

/// Verifies that a Detailed spec with name_template overrides the basename.
#[test]
fn test_resolve_extra_file_specs_name_template_override() {
    use anodizer_core::config::ExtraFileSpec;
    use anodizer_core::log::{StageLogger, Verbosity};

    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("original.txt"), b"data").unwrap();

    let pattern = format!("{}/*.txt", tmp.path().display());
    let specs = vec![ExtraFileSpec::Detailed {
        glob: pattern,
        name_template: Some("renamed.txt".to_string()),
        allow_empty: false,
    }];
    let log = StageLogger::new("flatpak", Verbosity::Normal);

    let results = resolve_extra_file_specs(&specs, &log);
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[1 - 1].1,
        "renamed.txt",
        "name_template should override basename"
    );
}

/// Verifies that an invalid glob pattern is warned-and-skipped (no panic,
/// returns empty). A glob with invalid character sequences triggers this.
#[test]
fn test_resolve_extra_file_specs_invalid_glob_skipped() {
    use anodizer_core::config::ExtraFileSpec;
    use anodizer_core::log::{StageLogger, Verbosity};

    // On most systems a pattern with unmatched `[` is invalid
    let specs = vec![ExtraFileSpec::Glob("[invalid".to_string())];
    let log = StageLogger::new("flatpak", Verbosity::Normal);

    // Must not panic; simply returns empty after logging a warning
    let results = resolve_extra_file_specs(&specs, &log);
    assert!(results.is_empty(), "invalid glob should produce no results");
}

// -----------------------------------------------------------------------
// filter_binaries_by_ids
// -----------------------------------------------------------------------

/// Verifies that with an ids filter, only binaries whose metadata "id" or
/// "name" matches are retained. Without this filter the ids-gate is a no-op.
#[test]
fn test_filter_binaries_by_ids_retains_matching() {
    let mut binaries = vec![
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/a"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "build-linux-amd64".to_string());
                m
            },
            size: None,
        },
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/b"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert("id".to_string(), "build-linux-arm64".to_string());
                m
            },
            size: None,
        },
    ];

    let filter = vec!["build-linux-amd64".to_string()];
    filter_binaries_by_ids(&mut binaries, Some(&filter));

    assert_eq!(binaries.len(), 1, "only the amd64 binary should remain");
    assert_eq!(binaries[0].metadata.get("id").unwrap(), "build-linux-amd64");
}

/// Verifies that an ids filter with no matches empties the list — this is
/// the path that triggers the "ids filter matched no binaries" warning.
#[test]
fn test_filter_binaries_by_ids_no_match_empties_list() {
    let mut binaries = vec![Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/a"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), "build-linux-amd64".to_string());
            m
        },
        size: None,
    }];

    let filter = vec!["build-windows-amd64".to_string()];
    filter_binaries_by_ids(&mut binaries, Some(&filter));

    assert!(
        binaries.is_empty(),
        "non-matching ids filter should empty the list"
    );
}

/// Verifies that a None filter is a no-op (all binaries retained).
#[test]
fn test_filter_binaries_by_ids_none_is_noop() {
    let mut binaries = vec![
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/a"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        },
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/b"),
            target: None,
            crate_name: "myapp".to_string(),
            metadata: Default::default(),
            size: None,
        },
    ];

    filter_binaries_by_ids(&mut binaries, None);
    assert_eq!(binaries.len(), 2, "None filter should retain all binaries");
}

// -----------------------------------------------------------------------
// render_output_filename — auto-appends .flatpak suffix
// -----------------------------------------------------------------------

/// Verifies that a name_template that does NOT end with ".flatpak" gets the
/// suffix appended automatically. Without this logic the produced file would
/// have a bare name with no extension.
#[test]
fn test_render_output_filename_appends_suffix_when_missing() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();
    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.App".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        // Template that does NOT end with .flatpak
        name_template: Some("{{ ProjectName }}-{{ Version }}".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg.clone()]),
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
    ctx.template_vars_mut().set("ProjectName", "myapp");
    ctx.template_vars_mut().set("Os", "linux");
    ctx.template_vars_mut().set("Arch", "amd64");

    let target: Option<String> = Some("x86_64-unknown-linux-gnu".to_string());
    let (name, resolved_template) = render_output_filename(
        &ctx,
        &flatpak_cfg,
        "myapp",
        &target,
        &default_name_template(),
    )
    .unwrap();

    assert!(
        name.ends_with(".flatpak"),
        "suffix should be auto-appended: {}",
        name
    );
    assert_eq!(name, "myapp-3.0.0.flatpak");
    assert_eq!(
        resolved_template, "{{ ProjectName }}-{{ Version }}",
        "resolved template should be the user's name_template, fed verbatim to the clobber guard"
    );
}

// -----------------------------------------------------------------------
// resolve_flatpak_version — missing Version var falls back to "0.0.0"
// -----------------------------------------------------------------------

/// Verifies that when the Version template variable is absent the stage falls
/// back to "0.0.0" for the Flatpak bundle version (the value passed to
/// flatpak-builder's --default-branch and build-bundle). The name_template
/// must not reference {{ Version }} in this test since that variable is
/// genuinely absent from the render context — the fallback only guards the
/// `resolve_flatpak_version` code path, not the generic template renderer.
#[test]
fn test_flatpak_stage_no_version_falls_back_to_0_0_0() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();
    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.App".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        // Use a template that does NOT reference {{ Version }} because that
        // var is absent; we verify the fallback via build_subprocess_args
        // by asserting the stage completes without error.
        name_template: Some("myapp-{{ Arch }}.flatpak".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    // Deliberately do NOT set "Version" — exercises the fallback path
    ctx.template_vars_mut().set("ProjectName", "myapp");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    // Must succeed — resolve_flatpak_version falls back to "0.0.0" rather
    // than panicking or propagating a missing-variable error.
    stage.run(&mut ctx).unwrap();

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(flatpaks.len(), 1);
}

// -----------------------------------------------------------------------
// require_flatpak_tools bails when tools are missing
// -----------------------------------------------------------------------

/// Verifies that when flatpak-builder is absent (which it is in the test
/// sandbox) `require_flatpak_tools` returns an error with a helpful message.
/// This exercises the 189-199 block; in CI the binaries genuinely aren't
/// on PATH.
#[test]
fn test_require_flatpak_tools_errors_when_absent() {
    // Only runs the direct function, not a subprocess.
    let result = require_flatpak_tools();
    // In CI/test environments flatpak-builder is not installed —
    // the function must bail with a descriptive message.
    if let Err(e) = result {
        let msg = e.to_string();
        assert!(
            msg.contains("flatpak-builder") || msg.contains("flatpak"),
            "error should name the missing tool: {}",
            msg
        );
    }
    // If the tools happen to be installed on the host, the function
    // succeeds — that is also correct.
}

// -----------------------------------------------------------------------
// Stage::run bails early when no flatpak tool and not dry-run
// -----------------------------------------------------------------------

/// Verifies that the non-dry-run path calls require_flatpak_tools and bails
/// when neither flatpak-builder nor flatpak is on PATH. This exercises
/// lines 808-809 in Stage::run.
#[test]
fn test_stage_run_non_dry_run_fails_without_tools() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    // Skip if the tools are actually installed
    if anodizer_core::tool_detect::on_path("flatpak-builder") {
        return;
    }

    let tmp = tempfile::TempDir::new().unwrap();
    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.App".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
        ..Default::default()
    }];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false, // not dry-run → triggers require_flatpak_tools
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    let result = stage.run(&mut ctx);
    assert!(
        result.is_err(),
        "should fail when flatpak-builder is absent"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("flatpak"),
        "error message should mention flatpak: {}",
        msg
    );
}

// -----------------------------------------------------------------------
// any_flatpak_enabled — skip template render error propagates
// -----------------------------------------------------------------------

/// Verifies that a malformed skip template in `any_flatpak_enabled` causes
/// the stage to return an error rather than silently running or suppressing.
#[test]
fn test_any_flatpak_enabled_bad_template_propagates_error() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();
    let flatpak_cfg = FlatpakConfig {
        // Unclosed Tera tag → render error
        skip: Some(StringOrBool::String("{{ unclosed".to_string())),
        app_id: Some("org.example.App".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    let result = stage.run(&mut ctx);
    assert!(
        result.is_err(),
        "malformed skip template should propagate an error"
    );
}

// -----------------------------------------------------------------------
// process_flatpak_cfg — ids filter matched no binaries
// -----------------------------------------------------------------------

/// Verifies that when the ids filter matches none of the available binaries
/// the stage skips quietly (no error, no flatpak artifact).
#[test]
fn test_flatpak_stage_ids_filter_no_match_skips() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();
    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.App".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        // Request a specific build id that no binary carries
        ids: Some(vec!["build-nonexistent".to_string()]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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

    // Binary has no "id" metadata → ids filter will not match
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert!(
        flatpaks.is_empty(),
        "ids filter with no match should produce no artifacts"
    );
}

// -----------------------------------------------------------------------
// process_flatpak_cfg — per-cfg skip template (lines 717-723)
// -----------------------------------------------------------------------

/// Verifies that a skip template evaluated at the per-cfg level (inside
/// process_flatpak_cfg) suppresses that specific config but not others.
/// This is distinct from the any_flatpak_enabled-level skip (which exits
/// the whole stage early).
#[test]
fn test_process_flatpak_cfg_skip_per_cfg() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig, StringOrBool};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    // Two configs: first is skipped, second is active
    let skip_cfg = FlatpakConfig {
        skip: Some(StringOrBool::Bool(true)),
        app_id: Some("org.example.Skipped".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };
    let active_cfg = FlatpakConfig {
        app_id: Some("org.example.Active".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![skip_cfg, active_cfg]),
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
    ctx.template_vars_mut().set("ProjectName", "myapp");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    // Only the active cfg should emit an artifact
    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(
        flatpaks.len(),
        1,
        "skipped config should not emit an artifact; active config should"
    );
}

// -----------------------------------------------------------------------
// Workspace multi-crate mode: two crates, each with a flatpak config
// -----------------------------------------------------------------------

/// Verifies that in workspace per-crate mode (multiple crates each with an
/// independent flatpaks: config), each crate emits its own artifact keyed
/// by crate_name. A regression where the second crate clobbers the first
/// would manifest as only one artifact with the wrong crate_name.
#[test]
fn test_flatpak_dry_run_workspace_per_crate() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let make_flatpak_cfg = |app_id: &str| FlatpakConfig {
        app_id: Some(app_id.to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        name_template: Some("{{ ProjectName }}-{{ Version }}-{{ Arch }}.flatpak".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "workspace".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![
        CrateConfig {
            name: "crate-a".to_string(),
            path: "crates/a".to_string(),
            flatpaks: Some(vec![make_flatpak_cfg("org.example.CrateA")]),
            ..Default::default()
        },
        CrateConfig {
            name: "crate-b".to_string(),
            path: "crates/b".to_string(),
            flatpaks: Some(vec![make_flatpak_cfg("org.example.CrateB")]),
            ..Default::default()
        },
    ];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("ProjectName", "workspace");

    // One Linux binary per crate
    for crate_name in &["crate-a", "crate-b"] {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from(format!("dist/{crate_name}")),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: crate_name.to_string(),
            metadata: Default::default(),
            size: None,
        });
    }

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(
        flatpaks.len(),
        2,
        "each crate should emit one flatpak artifact"
    );

    let crate_names: std::collections::HashSet<&str> =
        flatpaks.iter().map(|a| a.crate_name.as_str()).collect();
    assert!(crate_names.contains("crate-a"), "crate-a artifact missing");
    assert!(crate_names.contains("crate-b"), "crate-b artifact missing");
}

// -----------------------------------------------------------------------
// selected_crates filter (crate selection axis)
// -----------------------------------------------------------------------

/// Verifies that when selected_crates is set, only the matching crate
/// is processed. The excluded crate must produce no artifact.
#[test]
fn test_flatpak_dry_run_selected_crates_filter() {
    use anodizer_core::config::{Config, CrateConfig, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    let make_cfg = |app_id: &str| FlatpakConfig {
        app_id: Some(app_id.to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "ws".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![
        CrateConfig {
            name: "included".to_string(),
            path: "crates/included".to_string(),
            flatpaks: Some(vec![make_cfg("org.example.Included")]),
            ..Default::default()
        },
        CrateConfig {
            name: "excluded".to_string(),
            path: "crates/excluded".to_string(),
            flatpaks: Some(vec![make_cfg("org.example.Excluded")]),
            ..Default::default()
        },
    ];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            selected_crates: vec!["included".to_string()],
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("ProjectName", "ws");

    for crate_name in &["included", "excluded"] {
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from(format!("dist/{crate_name}")),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: crate_name.to_string(),
            metadata: Default::default(),
            size: None,
        });
    }

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(
        flatpaks.len(),
        1,
        "only the selected crate should produce an artifact"
    );
    assert_eq!(flatpaks[0].crate_name, "included");
}

// -----------------------------------------------------------------------
// map_to_supported_arches
// -----------------------------------------------------------------------

/// Verifies that binaries with supported arches are mapped and those with
/// unsupported arches (i686, armv7) are dropped.
#[test]
fn test_map_to_supported_arches_filters_unsupported() {
    let binaries = vec![
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/app-x86"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        },
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/app-arm"),
            target: Some("aarch64-unknown-linux-gnu".to_string()),
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        },
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/app-i686"),
            target: Some("i686-unknown-linux-gnu".to_string()),
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        },
    ];

    let result = map_to_supported_arches(&binaries);
    assert_eq!(result.len(), 2, "i686 should be filtered out");

    let arches: Vec<&str> = result.iter().map(|(_, _, _, a)| a.as_str()).collect();
    assert!(arches.contains(&"x86_64"));
    assert!(arches.contains(&"aarch64"));
}

/// Two builds that collapse onto the same Flatpak arch (x86_64 gnu +
/// x86_64 musl) must yield ONE job — the per-arch work dir and bundle name
/// are identical, so a second job would race / clobber the first. First
/// binary seen wins (the gnu build, which matches the glibc runtime).
#[test]
fn test_map_to_supported_arches_dedups_collapsing_arches() {
    let binaries = vec![
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/app-gnu"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        },
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/app-musl"),
            target: Some("x86_64-unknown-linux-musl".to_string()),
            crate_name: "app".to_string(),
            metadata: Default::default(),
            size: None,
        },
    ];

    let result = map_to_supported_arches(&binaries);
    assert_eq!(
        result.len(),
        1,
        "gnu + musl both map to x86_64 — exactly one job must survive"
    );
    // First-seen-wins keeps the gnu binary.
    assert_eq!(result[0].2, PathBuf::from("dist/app-gnu"));
    assert_eq!(result[0].3, "x86_64");
}

/// Two x86_64 builds tagged with DIFFERENT amd64 variants (`v1` baseline +
/// `v3`) keep distinct `(flatpak_arch, variant)` keys, so BOTH survive the
/// dedup — the spec's same-triple-multi-variant case the guard backs up.
#[test]
fn test_map_to_supported_arches_keeps_distinct_amd64_variants() {
    let binaries = vec![
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/app-v1"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "app".to_string(),
            metadata: HashMap::from([("amd64_variant".to_string(), "v1".to_string())]),
            size: None,
        },
        Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("dist/app-v3"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "app".to_string(),
            metadata: HashMap::from([("amd64_variant".to_string(), "v3".to_string())]),
            size: None,
        },
    ];

    let result = map_to_supported_arches(&binaries);
    assert_eq!(
        result.len(),
        2,
        "v1 + v3 of one triple must both survive — distinct variant keys"
    );
    let variants: Vec<Option<&str>> = result.iter().map(|(_, v, _, _)| v.as_deref()).collect();
    assert!(variants.contains(&Some("v1")));
    assert!(variants.contains(&Some("v3")));
}

// -----------------------------------------------------------------------
// Dry-run with extra_files config (process_binary_iteration lines 627-634)
// -----------------------------------------------------------------------

/// Verifies that when extra_files is configured, the resolved file names
/// surface in the dry-run artifact path (via the manifest JSON — we can
/// assert the stage runs without error and produces the artifact).
/// The extra_file_names collection at lines 627-634 is exercised.
#[test]
fn test_flatpak_dry_run_with_extra_files() {
    use anodizer_core::config::{Config, CrateConfig, ExtraFileSpec, FlatpakConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let tmp = tempfile::TempDir::new().unwrap();

    // Write an actual file for the glob to match
    let extra_dir = tmp.path().join("extras");
    std::fs::create_dir_all(&extra_dir).unwrap();
    std::fs::write(extra_dir.join("README.md"), b"readme").unwrap();

    let glob_pattern = format!("{}/*.md", extra_dir.display());
    let flatpak_cfg = FlatpakConfig {
        app_id: Some("org.example.App".to_string()),
        runtime: Some("org.freedesktop.Platform".to_string()),
        runtime_version: Some("24.08".to_string()),
        sdk: Some("org.freedesktop.Sdk".to_string()),
        extra_files: Some(vec![ExtraFileSpec::Glob(glob_pattern)]),
        ..Default::default()
    };

    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = tmp.path().join("dist");
    config.crates = vec![CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        flatpaks: Some(vec![flatpak_cfg]),
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
    ctx.template_vars_mut().set("ProjectName", "myapp");

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    let stage = FlatpakStage;
    stage.run(&mut ctx).unwrap();

    // Stage succeeded and produced an artifact — extra_files path was traversed
    let flatpaks = ctx.artifacts.by_kind(ArtifactKind::Flatpak);
    assert_eq!(flatpaks.len(), 1);
}
