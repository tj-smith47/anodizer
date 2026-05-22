#![cfg(test)]
#![allow(clippy::field_reassign_with_default)]

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{
    Config, CrateConfig, SnapcraftApp, SnapcraftConfig, SnapcraftExtraFileSpec, SnapcraftLayout,
    StringOrBool,
};
use anodizer_core::context::{Context, ContextOptions};
use anodizer_core::stage::Stage;
use tempfile::TempDir;

use crate::command::{
    is_retriable_snap_push, resolve_effective_channels, snapcraft_command, snapcraft_upload_command,
};
use crate::generate::generate_snap_yaml;
use crate::{SnapcraftPublishStage, SnapcraftStage};

// -----------------------------------------------------------------------
// generate_snap_yaml tests
// -----------------------------------------------------------------------

#[test]
fn test_generate_snap_yaml_basic() {
    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("A test snap".to_string()),
        description: Some("A longer description of the snap".to_string()),
        base: Some("core22".to_string()),
        grade: Some("stable".to_string()),
        confinement: Some("strict".to_string()),
        license: Some("MIT".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.2.3", &["myapp"], None, None).unwrap();
    assert!(yaml.contains("name: mysnap"), "missing name");
    assert!(yaml.contains("version: 1.2.3"), "missing version");
    assert!(yaml.contains("summary: A test snap"), "missing summary");
    assert!(
        yaml.contains("description: A longer description of the snap"),
        "missing description"
    );
    assert!(yaml.contains("base: core22"), "missing base");
    assert!(yaml.contains("grade: stable"), "missing grade");
    assert!(yaml.contains("confinement: strict"), "missing confinement");
    assert!(yaml.contains("license: MIT"), "missing license");
    // Prime-dir approach: no parts section in snap.yaml
    assert!(!yaml.contains("parts:"), "snap.yaml should not have parts");
    assert!(
        !yaml.contains("plugin:"),
        "snap.yaml should not have plugin"
    );
}

#[test]
fn test_generate_snap_yaml_with_apps() {
    let mut apps = BTreeMap::new();
    apps.insert(
        "myapp".to_string(),
        SnapcraftApp {
            command: Some("myapp".to_string()),
            daemon: Some("simple".to_string()),
            stop_mode: Some("sigterm".to_string()),
            restart_condition: Some("on-failure".to_string()),
            plugs: Some(vec!["network".to_string(), "home".to_string()]),
            environment: Some(BTreeMap::from([(
                "LANG".to_string(),
                serde_json::json!("C.UTF-8"),
            )])),
            args: Some("--verbose".to_string()),
            ..Default::default()
        },
    );

    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        apps: Some(apps),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
    assert!(yaml.contains("apps:"), "missing apps section");
    assert!(yaml.contains("myapp:"), "missing app name");
    // S4: args should be appended to command, not a separate field
    assert!(
        yaml.contains("command: myapp --verbose"),
        "args should be appended to command, got:\n{yaml}"
    );
    assert!(
        !yaml.contains("args:"),
        "args should not be a separate field in snapcraft.yaml"
    );
    assert!(yaml.contains("daemon: simple"), "missing daemon");
    assert!(yaml.contains("stop-mode: sigterm"), "missing stop-mode");
    assert!(
        yaml.contains("restart-condition: on-failure"),
        "missing restart-condition"
    );
    assert!(yaml.contains("- network"), "missing network plug");
    assert!(yaml.contains("- home"), "missing home plug");
    assert!(yaml.contains("LANG: C.UTF-8"), "missing environment");
}

#[test]
fn test_generate_snapcraft_yaml_with_plugs_and_app_slots() {
    // Snapcraft has no top-level `slots:` concept; app-scoped slots remain
    // via `apps.<name>.slots` and that path is exercised here.
    let mut plugs = BTreeMap::new();
    plugs.insert("network".to_string(), serde_json::Value::Null);
    plugs.insert("home".to_string(), serde_json::Value::Null);
    plugs.insert(
        "personal-files".to_string(),
        serde_json::json!({ "interface": "personal-files", "read": ["/etc/myapp"] }),
    );

    let mut apps_map = BTreeMap::new();
    apps_map.insert(
        "mysnap".to_string(),
        anodizer_core::config::SnapcraftApp {
            slots: Some(vec!["dbus-slot".to_string()]),
            ..Default::default()
        },
    );

    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        apps: Some(apps_map),
        plugs: Some(plugs),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
    assert!(yaml.contains("plugs:"), "missing plugs section");
    assert!(yaml.contains("network:"), "missing network plug");
    assert!(yaml.contains("home:"), "missing home plug");
    assert!(
        yaml.contains("personal-files:"),
        "missing personal-files plug"
    );
    assert!(
        yaml.contains("interface: personal-files"),
        "missing interface attribute in personal-files plug"
    );
    assert!(yaml.contains("- dbus-slot"), "missing app-scoped dbus-slot");
}

#[test]
fn test_generate_snapcraft_yaml_with_layouts() {
    let mut layouts = BTreeMap::new();
    layouts.insert(
        "/usr/share/myapp".to_string(),
        SnapcraftLayout {
            bind: Some("$SNAP/usr/share/myapp".to_string()),
            symlink: None,
            bind_file: None,
            type_: None,
        },
    );
    layouts.insert(
        "/etc/myapp".to_string(),
        SnapcraftLayout {
            bind: None,
            bind_file: None,
            symlink: Some("$SNAP_DATA/etc/myapp".to_string()),
            type_: None,
        },
    );

    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        layouts: Some(layouts),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
    assert!(yaml.contains("layout:"), "missing layout section");
    assert!(
        yaml.contains("/usr/share/myapp"),
        "missing layout path /usr/share/myapp"
    );
    assert!(
        yaml.contains("bind: $SNAP/usr/share/myapp"),
        "missing bind value"
    );
    assert!(
        yaml.contains("/etc/myapp"),
        "missing layout path /etc/myapp"
    );
    assert!(
        yaml.contains("symlink: $SNAP_DATA/etc/myapp"),
        "missing symlink value"
    );
}

#[test]
fn test_generate_snapcraft_yaml_confinement_modes() {
    for mode in &["strict", "devmode", "classic"] {
        let cfg = SnapcraftConfig {
            name: Some("mysnap".to_string()),
            confinement: Some(mode.to_string()),
            summary: Some("Test snap".to_string()),
            description: Some("A test snap package".to_string()),
            ..Default::default()
        };
        let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
        assert!(
            yaml.contains(&format!("confinement: {mode}")),
            "missing confinement: {mode}"
        );
    }
}

#[test]
fn test_generate_snapcraft_yaml_defaults() {
    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
    // GoReleaser parity: do NOT default `base:`. Classic-confinement snaps
    // need no base at all, and modern snaps may want `core24`. Forcing
    // `base: core22` breaks both. The line must be absent unless the
    // user-supplied config provides one.
    assert!(
        !yaml.contains("base:"),
        "no `base:` line should be emitted by default (GR snapcraft.go::Default parity)"
    );
    // Default confinement should be strict
    assert!(
        yaml.contains("confinement: strict"),
        "default confinement not strict"
    );
}

#[test]
fn test_generate_snapcraft_yaml_emits_user_supplied_base() {
    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        base: Some("core24".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
    assert!(
        yaml.contains("base: core24"),
        "user-supplied base must be emitted as-is\n{yaml}"
    );
}

#[test]
fn test_generate_snapcraft_yaml_minimal() {
    // summary and description are required
    let cfg = SnapcraftConfig::default();
    let err = generate_snap_yaml(&cfg, "0.1.0", &["mytool"], None, None);
    assert!(err.is_err(), "should error when summary is missing");
    assert!(
        err.unwrap_err().to_string().contains("summary is required"),
        "error should mention missing summary"
    );
}

#[test]
fn test_generate_snapcraft_yaml_minimal_with_required_fields() {
    let cfg = SnapcraftConfig {
        summary: Some("A test snap".to_string()),
        description: Some("A test description".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "0.1.0", &["mytool"], None, None).unwrap();
    assert!(yaml.contains("name: mytool"), "missing fallback name");
    assert!(yaml.contains("version: 0.1.0"), "missing version");
    // GoReleaser parity: `base:` is only emitted when the user supplied one.
    assert!(
        !yaml.contains("base:"),
        "no `base:` line should appear when user did not configure one\n{yaml}"
    );
    assert!(
        yaml.contains("confinement: strict"),
        "missing default confinement"
    );
    assert!(!yaml.contains("parts:"), "snap.yaml should not have parts");
    assert!(yaml.contains("summary:"), "missing summary");
    assert!(yaml.contains("description:"), "missing description");
}

// -----------------------------------------------------------------------
// Snap Store schema parity — `icon:` must NOT leak into snap.yaml unless
// the user explicitly configured it. The Store's `snap.json` validator
// rejects the field with "Additional properties are not allowed
// ('icon' was unexpected)", so a stray top-level `icon:` line blocks
// snap upload even though `snapcraft pack` accepts it locally. These
// tests pin the omit-when-None behaviour so a future serde refactor
// (e.g. dropping `skip_serializing_if`) can't silently regress us back
// into the v0.3.0 upload failure mode.
// -----------------------------------------------------------------------

#[test]
fn test_generate_snap_yaml_omits_icon_when_none() {
    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("A test snap".to_string()),
        description: Some("A test description".to_string()),
        // icon: None is the default — spelled out for clarity.
        icon: None,
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "0.1.0", &["mytool"], None, None).unwrap();

    // The strict assertion: no `icon:` line anywhere. A simple
    // `contains("icon")` would false-trigger on hypothetical apps named
    // `icon-utils` or a description that mentions icons, so anchor on
    // the YAML key shape: line-start `icon:`. snap.yaml is line-oriented
    // top-level keys, so this is sufficient.
    let has_icon_key = yaml.lines().any(|l| {
        let trimmed = l.trim_start();
        trimmed == "icon:" || trimmed.starts_with("icon: ") || trimmed.starts_with("icon:\t")
    });
    assert!(
        !has_icon_key,
        "snap.yaml must NOT emit `icon:` when SnapcraftConfig.icon is None \
         (Snap Store snap.json schema rejects the field). Got:\n{yaml}"
    );
}

#[test]
fn test_generate_snap_yaml_emits_icon_when_set() {
    // Sanity check: when the user *does* set an icon, the field round-trips.
    // This pins the existing leak path so reviewers can see what happens
    // (the build stage emits a warning; the user has to move the icon to
    // snap/gui/<name>.png to actually publish). Until we add a proper
    // snap/gui/ writer, this test documents the failure-mode shape.
    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("A test snap".to_string()),
        description: Some("A test description".to_string()),
        icon: Some("assets/logo.png".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "0.1.0", &["mytool"], None, None).unwrap();
    assert!(
        yaml.contains("icon: assets/logo.png"),
        "icon should round-trip when configured (build stage warns about \
         Snap Store rejection). Got:\n{yaml}"
    );
}

// -----------------------------------------------------------------------
// snapcraft_command tests
// -----------------------------------------------------------------------

#[test]
fn test_snapcraft_command_basic() {
    let cmd = snapcraft_command("/tmp/prime", "/tmp/output/mysnap_1.0.0_amd64.snap");
    assert_eq!(cmd[0], "snapcraft");
    assert_eq!(cmd[1], "pack");
    assert_eq!(cmd[2], "/tmp/prime");
    assert_eq!(cmd[3], "--output");
    assert_eq!(cmd[4], "/tmp/output/mysnap_1.0.0_amd64.snap");
    assert_eq!(cmd.len(), 5);
}

#[test]
fn test_snapcraft_command_no_destructive_mode() {
    // Prime-dir approach: no --destructive-mode needed
    let cmd = snapcraft_command("/tmp/prime", "/tmp/out.snap");
    assert!(
        !cmd.contains(&"--destructive-mode".to_string()),
        "pack command should not have --destructive-mode with prime-dir approach"
    );
    assert!(
        !cmd.contains(&"--publish".to_string()),
        "pack command should not have --publish"
    );
}

#[test]
fn test_snapcraft_upload_command_no_channels() {
    let cmd = snapcraft_upload_command("/tmp/out.snap", None);
    assert_eq!(cmd[0], "snapcraft");
    assert_eq!(cmd[1], "upload");
    assert_eq!(cmd[2], "/tmp/out.snap");
    assert_eq!(cmd.len(), 3);
}

#[test]
fn test_snapcraft_upload_command_with_channels() {
    let channels = vec!["edge".to_string(), "beta".to_string()];
    let cmd = snapcraft_upload_command("/tmp/out.snap", Some(&channels));
    assert_eq!(cmd[0], "snapcraft");
    assert_eq!(cmd[1], "upload");
    assert_eq!(cmd[2], "/tmp/out.snap");
    assert_eq!(cmd[3], "--release=edge,beta");
    assert_eq!(cmd.len(), 4);
}

// -----------------------------------------------------------------------
// Channel auto-population tests
// -----------------------------------------------------------------------

#[test]
fn test_resolve_effective_channels_devel_grade() {
    let channels = resolve_effective_channels(None, Some("devel"));
    assert_eq!(channels.unwrap(), vec!["edge", "beta"],);
}

#[test]
fn test_resolve_effective_channels_stable_grade() {
    let channels = resolve_effective_channels(None, Some("stable"));
    assert_eq!(
        channels.unwrap(),
        vec!["edge", "beta", "candidate", "stable"],
    );
}

#[test]
fn test_resolve_effective_channels_default_grade_is_stable() {
    // When grade is None, default is "stable"
    let channels = resolve_effective_channels(None, None);
    assert_eq!(
        channels.unwrap(),
        vec!["edge", "beta", "candidate", "stable"],
    );
}

#[test]
fn test_resolve_effective_channels_explicit_overrides_grade() {
    let explicit = vec!["edge".to_string()];
    let channels = resolve_effective_channels(Some(&explicit), Some("stable"));
    assert_eq!(
        channels.unwrap(),
        vec!["edge"],
        "explicit channel_templates should override grade-based defaults"
    );
}

#[test]
fn test_resolve_effective_channels_empty_explicit_falls_through() {
    let empty: Vec<String> = Vec::new();
    let channels = resolve_effective_channels(Some(&empty), Some("devel"));
    assert_eq!(
        channels.unwrap(),
        vec!["edge", "beta"],
        "empty channel_templates should fall through to grade-based defaults"
    );
}

// -----------------------------------------------------------------------
// Stage integration tests
// -----------------------------------------------------------------------

#[test]
fn test_stage_skips_when_no_snapcraft_config() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let stage = SnapcraftStage;
    // Should succeed (no-op)
    assert!(stage.run(&mut ctx).is_ok());
    // No artifacts registered
    assert!(ctx.artifacts.by_kind(ArtifactKind::Snap).is_empty());
}

#[test]
fn test_stage_skips_when_disabled() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        skip: Some(StringOrBool::Bool(true)),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    // No artifacts — config was disabled
    assert!(ctx.artifacts.by_kind(ArtifactKind::Snap).is_empty());
}

#[test]
fn test_stage_dry_run_registers_artifacts() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // Register a linux binary
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].crate_name, "myapp");

    // Default filename (B2 fix): {name}_{version}_{os}_{arch}.snap —
    // matches GoReleaser snapcraft.go:103,120-122.
    let path_str = snaps[0].path.to_string_lossy();
    assert!(
        path_str.ends_with("mysnap_1.0.0_linux_amd64.snap"),
        "unexpected path: {path_str}"
    );
}

#[test]
fn test_stage_dry_run_with_name_template() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        name_template: Some("{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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
    ctx.template_vars_mut().set("Version", "2.0.0");

    // Register a linux binary
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1);

    let path_str = snaps[0].path.to_string_lossy();
    assert!(
        path_str.ends_with("mysnap_2.0.0_linux_amd64.snap"),
        "unexpected path: {path_str}"
    );
}

#[test]
fn test_ids_filtering() {
    let tmp = TempDir::new().unwrap();

    // Snap config that filters by id "build-linux-arm"
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        ids: Some(vec!["build-linux-arm".to_string()]),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // Register two linux binaries: one matching the id filter, one not
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-amd64"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "build-linux-amd64".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-arm"),
        target: Some("aarch64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "build-linux-arm".to_string())]),
        size: None,
    });

    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1, "should only produce one snap (filtered)");
    // The matching binary is the arm one
    assert_eq!(
        snaps[0].target.as_deref(),
        Some("aarch64-unknown-linux-gnu")
    );
}

#[test]
fn test_ids_filtering_by_name() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        ids: Some(vec!["myapp".to_string()]),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // Register a binary with name metadata but no id metadata
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("name".to_string(), "myapp".to_string())]),
        size: None,
    });
    // Register a binary that doesn't match
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/other"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("name".to_string(), "other".to_string())]),
        size: None,
    });

    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(
        snaps.len(),
        1,
        "should only produce one snap (filtered by name)"
    );
}

#[test]
fn test_ids_filtering_empty_ids_includes_all() {
    let tmp = TempDir::new().unwrap();

    // Empty ids list should not filter anything
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        ids: Some(vec![]),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-amd64"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "build1".to_string())]),
        size: None,
    });
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-arm"),
        target: Some("aarch64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("id".to_string(), "build2".to_string())]),
        size: None,
    });

    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 2, "empty ids should include all binaries");
}

#[test]
fn test_generate_snap_yaml_no_extra_files_in_metadata() {
    // Prime-dir approach: extra files are staged physically, not in snap.yaml
    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
    // snap.yaml should not have parts/organize/stage/prime sections
    assert!(!yaml.contains("parts:"), "snap.yaml should not have parts");
    assert!(
        !yaml.contains("organize:"),
        "snap.yaml should not have organize"
    );
    // Default app command is just the binary name (at prime root)
    assert!(
        yaml.contains("command: myapp"),
        "default app command should be binary name"
    );
}

#[test]
fn test_artifact_metadata_includes_id() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        id: Some("main-snap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // Register a linux binary
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1);
    assert_eq!(
        snaps[0].metadata.get("id").map(|s| s.as_str()),
        Some("main-snap"),
        "artifact metadata should contain the config id"
    );
}

#[test]
fn test_config_parse_snapcraft() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - name: mysnap
        summary: A test snap
        confinement: strict
"#;
    let config: Config = serde_yaml_ng::from_str(yaml)
        .unwrap_or_else(|e| panic!("failed to parse snapcraft config: {e}"));
    assert_eq!(config.crates.len(), 1);
    let snaps = config.crates[0].snapcrafts.as_ref().unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].name.as_deref(), Some("mysnap"));
    assert_eq!(snaps[0].summary.as_deref(), Some("A test snap"));
    assert_eq!(snaps[0].confinement.as_deref(), Some("strict"));
}

#[test]
fn test_config_parse_snapcraft_full() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - id: main
        ids:
          - build1
        name: mysnap
        title: My Snap Application
        summary: A test snap
        description: A longer description
        icon: icon.png
        base: core24
        grade: devel
        license: Apache-2.0
        publish: true
        channel_templates:
          - edge
          - beta
        confinement: devmode
        plugs:
          network: null
          home: null
        assumes:
          - snapd2.39
        apps:
          myapp:
            command: bin/myapp
            daemon: simple
            stop_mode: sigterm
            restart_condition: on-failure
            plugs:
              - network
            environment:
              LANG: C.UTF-8
            args: --verbose
        layouts:
          /usr/share/myapp:
            bind: $SNAP/usr/share/myapp
        extra_files:
          - README.md
        name_template: "mysnap_{{ Version }}_{{ Arch }}"
        skip: false
        replace: true
        mod_timestamp: "1704067200"
"#;
    let config: Config = serde_yaml_ng::from_str(yaml)
        .unwrap_or_else(|e| panic!("failed to parse full snapcraft config: {e}"));
    let snaps = config.crates[0].snapcrafts.as_ref().unwrap();
    let snap = &snaps[0];
    assert_eq!(snap.id.as_deref(), Some("main"));
    assert_eq!(snap.ids.as_ref().unwrap(), &["build1"]);
    assert_eq!(snap.name.as_deref(), Some("mysnap"));
    assert_eq!(snap.title.as_deref(), Some("My Snap Application"));
    assert_eq!(snap.summary.as_deref(), Some("A test snap"));
    assert_eq!(snap.description.as_deref(), Some("A longer description"));
    assert_eq!(snap.icon.as_deref(), Some("icon.png"));
    assert_eq!(snap.base.as_deref(), Some("core24"));
    assert_eq!(snap.grade.as_deref(), Some("devel"));
    assert_eq!(snap.license.as_deref(), Some("Apache-2.0"));
    assert_eq!(snap.publish, Some(true));
    assert_eq!(snap.channel_templates.as_ref().unwrap(), &["edge", "beta"]);
    assert_eq!(snap.confinement.as_deref(), Some("devmode"));
    let plugs = snap.plugs.as_ref().unwrap();
    assert!(plugs.contains_key("network"), "missing network plug");
    assert!(plugs.contains_key("home"), "missing home plug");
    assert_eq!(snap.assumes.as_ref().unwrap(), &["snapd2.39"]);

    let apps = snap.apps.as_ref().unwrap();
    let app = apps.get("myapp").unwrap();
    assert_eq!(app.command.as_deref(), Some("bin/myapp"));
    assert_eq!(app.daemon.as_deref(), Some("simple"));
    assert_eq!(app.stop_mode.as_deref(), Some("sigterm"));
    assert_eq!(app.restart_condition.as_deref(), Some("on-failure"));
    assert_eq!(app.plugs.as_ref().unwrap(), &["network"]);
    assert_eq!(
        app.environment.as_ref().unwrap().get("LANG").unwrap(),
        &serde_json::json!("C.UTF-8")
    );
    assert_eq!(app.args.as_deref(), Some("--verbose"));

    let layouts = snap.layouts.as_ref().unwrap();
    let layout = layouts.get("/usr/share/myapp").unwrap();
    assert_eq!(layout.bind.as_deref(), Some("$SNAP/usr/share/myapp"));

    assert_eq!(
        snap.extra_files.as_ref().unwrap(),
        &[SnapcraftExtraFileSpec::Source("README.md".to_string())]
    );
    assert_eq!(
        snap.name_template.as_deref(),
        Some("mysnap_{{ Version }}_{{ Arch }}")
    );
    assert_eq!(snap.skip, Some(StringOrBool::Bool(false)));
    assert_eq!(snap.replace, Some(true));
    assert_eq!(snap.mod_timestamp.as_deref(), Some("1704067200"));
}

#[test]
fn test_invalid_name_template_errors() {
    let tmp = TempDir::new().unwrap();

    // Use an invalid Tera template — unclosed tag
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        name_template: Some("{{ invalid unclosed".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // Register a linux binary so we don't skip before reaching template rendering
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err(), "expected error for invalid template");
    let err_msg = format!("{:#}", result.unwrap_err());
    assert!(
        err_msg.contains("name_template") || err_msg.contains("template"),
        "error should mention template: {err_msg}"
    );
}

#[test]
fn test_stage_dry_run_multiple_configs() {
    let tmp = TempDir::new().unwrap();

    // Two snapcraft configs with different confinements
    let snap_cfg_strict = SnapcraftConfig {
        name: Some("mysnap-strict".to_string()),
        confinement: Some("strict".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let snap_cfg_classic = SnapcraftConfig {
        name: Some("mysnap-classic".to_string()),
        confinement: Some("classic".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg_strict, snap_cfg_classic]),
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

    // Register a linux binary so each config produces an artifact
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    // Verify both produce artifacts
    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(
        snaps.len(),
        2,
        "each snapcraft config should produce one artifact"
    );

    let paths: Vec<String> = snaps
        .iter()
        .map(|s| s.path.to_string_lossy().into_owned())
        .collect();
    assert!(
        paths.iter().any(|p| p.contains("mysnap-strict")),
        "missing artifact for strict config, got: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.contains("mysnap-classic")),
        "missing artifact for classic config, got: {paths:?}"
    );
}

#[test]
fn test_stage_only_selects_linux_binaries() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // Add a linux binary
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-linux"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    // Add a darwin binary — should be excluded
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp-darwin"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    // Verify only linux binary produces snap artifact
    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1, "only linux binary should produce a snap");
    assert_eq!(
        snaps[0].target.as_deref(),
        Some("x86_64-unknown-linux-gnu"),
        "snap should be for the linux target"
    );
}

#[test]
fn test_stage_dry_run_replace_removes_archives() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        replace: Some(true),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // Register a linux binary
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("dist/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: Default::default(),
        size: None,
    });

    // Register an archive artifact for the same crate+target
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/myapp_1.0.0_linux_amd64.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("format".to_string(), "tar.gz".to_string())]),
        size: None,
    });

    // Also register a darwin archive that should NOT be removed
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/myapp_1.0.0_darwin_arm64.tar.gz"),
        target: Some("aarch64-apple-darwin".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::from([("format".to_string(), "tar.gz".to_string())]),
        size: None,
    });

    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    // Snap artifact should be registered
    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1);

    // The linux archive should have been removed (replace: true)
    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(archives.len(), 1, "only the darwin archive should remain");
    assert!(
        archives[0].target.as_deref().unwrap().contains("darwin"),
        "remaining archive should be the darwin one"
    );
}

#[test]
fn test_confinement_validation() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        confinement: Some("invalid-confinement".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // Register a linux binary so we reach the validation
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    let result = stage.run(&mut ctx);
    assert!(result.is_err(), "expected error for invalid confinement");
    let err_msg = format!("{:#}", result.unwrap_err());
    assert!(
        err_msg.contains("invalid confinement"),
        "error should mention invalid confinement: {err_msg}"
    );
    assert!(
        err_msg.contains("invalid-confinement"),
        "error should include the bad value: {err_msg}"
    );
}

#[test]
fn test_no_linux_binaries_skips_snapcraft() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // No binaries registered at all
    let stage = SnapcraftStage;
    stage.run(&mut ctx).unwrap();

    // Should produce no snap artifacts (warn+skip)
    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert!(snaps.is_empty(), "should skip when no linux binaries exist");
}

// -----------------------------------------------------------------------
// SnapcraftPublishStage tests
// -----------------------------------------------------------------------

#[test]
fn test_publish_stage_skips_when_no_snapcraft_config() {
    let config = Config::default();
    let mut ctx = Context::new(config, ContextOptions::default());
    let stage = SnapcraftPublishStage;
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_publish_stage_skips_when_publish_false() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        publish: Some(false),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // Register a snap artifact
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Snap,
        name: String::new(),
        path: PathBuf::from("/tmp/dist/mysnap_1.0.0_amd64.snap"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftPublishStage;
    // Should complete without attempting upload (publish: false)
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_publish_stage_dry_run_logs_upload() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        publish: Some(true),
        channel_templates: Some(vec!["edge".to_string()]),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    // Register a snap artifact
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Snap,
        name: String::new(),
        path: PathBuf::from("/tmp/dist/mysnap_1.0.0_amd64.snap"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftPublishStage;
    // Dry-run should log but not actually run snapcraft upload
    assert!(stage.run(&mut ctx).is_ok());
}

#[test]
fn test_publish_stage_skips_disabled_config() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        publish: Some(true),
        skip: Some(StringOrBool::Bool(true)),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };

    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
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

    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Snap,
        name: String::new(),
        path: PathBuf::from("/tmp/dist/mysnap_1.0.0_amd64.snap"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftPublishStage;
    // Should complete without attempting upload (disabled)
    assert!(stage.run(&mut ctx).is_ok());
}

// -----------------------------------------------------------------------
// New fields: all 24 missing SnapcraftApp fields + hooks + extra_files
// -----------------------------------------------------------------------

#[test]
fn test_generate_yaml_all_new_app_fields() {
    let mut apps = BTreeMap::new();
    apps.insert(
        "mydaemon".to_string(),
        SnapcraftApp {
            command: Some("bin/mydaemon".to_string()),
            daemon: Some("dbus".to_string()),
            adapter: Some("none".to_string()),
            after: Some(vec!["network-manager".to_string()]),
            aliases: Some(vec!["md".to_string(), "myd".to_string()]),
            autostart: Some("mydaemon.desktop".to_string()),
            before: Some(vec!["other-svc".to_string()]),
            bus_name: Some("com.example.mydaemon".to_string()),
            command_chain: Some(vec!["bin/wrapper".to_string(), "bin/setup".to_string()]),
            common_id: Some("com.example.mydaemon".to_string()),
            completer: Some("completions/mydaemon.bash".to_string()),
            desktop: Some("gui/mydaemon.desktop".to_string()),
            extensions: Some(vec!["gnome".to_string()]),
            install_mode: Some("disable".to_string()),
            passthrough: Some(BTreeMap::from([(
                "custom-key".to_string(),
                serde_json::json!("custom-value"),
            )])),
            post_stop_command: Some("bin/cleanup".to_string()),
            refresh_mode: Some("endure".to_string()),
            reload_command: Some("bin/reload".to_string()),
            restart_condition: Some("on-failure".to_string()),
            restart_delay: Some("10s".to_string()),
            slots: Some(vec!["dbus-slot".to_string()]),
            sockets: Some(BTreeMap::from([(
                "mysock".to_string(),
                serde_json::json!({"listen-stream": "$SNAP_DATA/mysock.sock"}),
            )])),
            start_timeout: Some("30s".to_string()),
            stop_command: Some("bin/stop".to_string()),
            stop_mode: Some("sigterm-all".to_string()),
            stop_timeout: Some("15s".to_string()),
            timer: Some("mon,10:00-12:00,,fri,13:00-14:00".to_string()),
            watchdog_timeout: Some("60s".to_string()),
            ..Default::default()
        },
    );

    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        apps: Some(apps),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "2.0.0", &["mydaemon"], None, None).unwrap();

    // Verify all kebab-case fields are present
    assert!(yaml.contains("adapter: none"), "missing adapter\n{yaml}");
    assert!(
        yaml.contains("- network-manager"),
        "missing after entry\n{yaml}"
    );
    assert!(yaml.contains("aliases:"), "missing aliases section\n{yaml}");
    assert!(yaml.contains("- md"), "missing alias md\n{yaml}");
    assert!(yaml.contains("- myd"), "missing alias myd\n{yaml}");
    assert!(
        yaml.contains("autostart: mydaemon.desktop"),
        "missing autostart\n{yaml}"
    );
    assert!(yaml.contains("- other-svc"), "missing before entry\n{yaml}");
    assert!(
        yaml.contains("bus-name: com.example.mydaemon"),
        "missing bus-name\n{yaml}"
    );
    assert!(
        yaml.contains("command-chain:"),
        "missing command-chain\n{yaml}"
    );
    assert!(
        yaml.contains("- bin/wrapper"),
        "missing command-chain entry\n{yaml}"
    );
    assert!(
        yaml.contains("common-id: com.example.mydaemon"),
        "missing common-id\n{yaml}"
    );
    assert!(
        yaml.contains("completer: completions/mydaemon.bash"),
        "missing completer\n{yaml}"
    );
    assert!(
        yaml.contains("desktop: gui/mydaemon.desktop"),
        "missing desktop\n{yaml}"
    );
    assert!(yaml.contains("- gnome"), "missing extensions entry\n{yaml}");
    assert!(
        yaml.contains("install-mode: disable"),
        "missing install-mode\n{yaml}"
    );
    assert!(
        yaml.contains("custom-key: custom-value"),
        "missing passthrough key\n{yaml}"
    );
    assert!(
        yaml.contains("post-stop-command: bin/cleanup"),
        "missing post-stop-command\n{yaml}"
    );
    assert!(
        yaml.contains("refresh-mode: endure"),
        "missing refresh-mode\n{yaml}"
    );
    assert!(
        yaml.contains("reload-command: bin/reload"),
        "missing reload-command\n{yaml}"
    );
    assert!(
        yaml.contains("restart-delay: 10s"),
        "missing restart-delay\n{yaml}"
    );
    assert!(yaml.contains("- dbus-slot"), "missing slots entry\n{yaml}");
    assert!(yaml.contains("mysock:"), "missing sockets entry\n{yaml}");
    assert!(
        yaml.contains("start-timeout: 30s"),
        "missing start-timeout\n{yaml}"
    );
    assert!(
        yaml.contains("stop-command: bin/stop"),
        "missing stop-command\n{yaml}"
    );
    assert!(
        yaml.contains("stop-mode: sigterm-all"),
        "missing stop-mode\n{yaml}"
    );
    assert!(
        yaml.contains("stop-timeout: 15s"),
        "missing stop-timeout\n{yaml}"
    );
    assert!(
        yaml.contains("timer: mon,10:00-12:00,,fri,13:00-14:00"),
        "missing timer\n{yaml}"
    );
    assert!(
        yaml.contains("watchdog-timeout: 60s"),
        "missing watchdog-timeout\n{yaml}"
    );
}

#[test]
fn test_generate_yaml_with_hooks() {
    let mut hooks = BTreeMap::new();
    hooks.insert(
        "configure".to_string(),
        serde_json::json!({"plugs": ["network"]}),
    );
    hooks.insert(
        "install".to_string(),
        serde_json::json!({"plugs": ["home", "network"]}),
    );

    // hooks are emitted only when
    // `apps` is non-empty (the loop runs per-app). Supply a minimal app.
    let mut apps_map = BTreeMap::new();
    apps_map.insert(
        "mysnap".to_string(),
        anodizer_core::config::SnapcraftApp::default(),
    );

    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        apps: Some(apps_map),
        hooks: Some(hooks),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
    assert!(yaml.contains("hooks:"), "missing hooks section\n{yaml}");
    assert!(
        yaml.contains("configure:"),
        "missing configure hook\n{yaml}"
    );
    assert!(yaml.contains("install:"), "missing install hook\n{yaml}");
}

#[test]
fn test_generate_snap_yaml_layout_key_is_singular() {
    // snap.yaml uses "layout:" (singular), not "layouts:"
    let mut layouts = BTreeMap::new();
    layouts.insert(
        "/usr/share/myapp".to_string(),
        SnapcraftLayout {
            bind: Some("$SNAP/usr/share/myapp".to_string()),
            symlink: None,
            bind_file: None,
            type_: None,
        },
    );
    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        layouts: Some(layouts),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
    assert!(yaml.contains("layout:"), "should use singular 'layout:'");
    assert!(
        !yaml.contains("layouts:"),
        "should not use plural 'layouts:'"
    );
}

#[test]
fn test_config_parse_new_app_fields() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: .
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - name: mysnap
        hooks:
          configure:
            plugs:
              - network
        apps:
          myapp:
            command: bin/myapp
            adapter: none
            after:
              - network-manager
            aliases:
              - ma
            autostart: myapp.desktop
            before:
              - other-svc
            bus_name: com.example.myapp
            command_chain:
              - bin/wrapper
            common_id: com.example.myapp
            completer: completions/myapp.bash
            desktop: gui/myapp.desktop
            extensions:
              - gnome
            install_mode: disable
            passthrough:
              custom: value
            post_stop_command: bin/cleanup
            refresh_mode: endure
            reload_command: bin/reload
            restart_delay: 10s
            slots:
              - dbus-slot
            sockets:
              mysock:
                listen-stream: "$SNAP_DATA/mysock.sock"
            start_timeout: 30s
            stop_command: bin/stop
            stop_timeout: 15s
            timer: "mon,10:00-12:00"
            watchdog_timeout: 60s
        extra_files:
          - README.md
          - source: config/app.conf
            destination: etc/app.conf
            mode: 420
"#;
    let config: Config = serde_yaml_ng::from_str(yaml)
        .unwrap_or_else(|e| panic!("failed to parse config with new fields: {e}"));
    let snaps = config.crates[0].snapcrafts.as_ref().unwrap();
    let snap = &snaps[0];

    // Verify hooks
    let hooks = snap.hooks.as_ref().unwrap();
    assert!(hooks.contains_key("configure"), "missing configure hook");

    // Verify app fields
    let apps = snap.apps.as_ref().unwrap();
    let app = apps.get("myapp").unwrap();
    assert_eq!(app.adapter.as_deref(), Some("none"));
    assert_eq!(app.after.as_ref().unwrap(), &["network-manager"]);
    assert_eq!(app.aliases.as_ref().unwrap(), &["ma"]);
    assert_eq!(app.autostart.as_deref(), Some("myapp.desktop"));
    assert_eq!(app.before.as_ref().unwrap(), &["other-svc"]);
    assert_eq!(app.bus_name.as_deref(), Some("com.example.myapp"));
    assert_eq!(app.command_chain.as_ref().unwrap(), &["bin/wrapper"]);
    assert_eq!(app.common_id.as_deref(), Some("com.example.myapp"));
    assert_eq!(app.completer.as_deref(), Some("completions/myapp.bash"));
    assert_eq!(app.desktop.as_deref(), Some("gui/myapp.desktop"));
    assert_eq!(app.extensions.as_ref().unwrap(), &["gnome"]);
    assert_eq!(app.install_mode.as_deref(), Some("disable"));
    assert!(app.passthrough.as_ref().unwrap().contains_key("custom"));
    assert_eq!(app.post_stop_command.as_deref(), Some("bin/cleanup"));
    assert_eq!(app.refresh_mode.as_deref(), Some("endure"));
    assert_eq!(app.reload_command.as_deref(), Some("bin/reload"));
    assert_eq!(app.restart_delay.as_deref(), Some("10s"));
    assert_eq!(app.slots.as_ref().unwrap(), &["dbus-slot"]);
    assert!(app.sockets.as_ref().unwrap().contains_key("mysock"));
    assert_eq!(app.start_timeout.as_deref(), Some("30s"));
    assert_eq!(app.stop_command.as_deref(), Some("bin/stop"));
    assert_eq!(app.stop_timeout.as_deref(), Some("15s"));
    assert_eq!(app.timer.as_deref(), Some("mon,10:00-12:00"));
    assert_eq!(app.watchdog_timeout.as_deref(), Some("60s"));

    // Verify extra_files mixed form
    let extra = snap.extra_files.as_ref().unwrap();
    assert_eq!(extra.len(), 2);
    assert_eq!(
        extra[0],
        SnapcraftExtraFileSpec::Source("README.md".to_string())
    );
    match &extra[1] {
        SnapcraftExtraFileSpec::Detailed {
            source,
            destination,
            mode,
        } => {
            assert_eq!(source, "config/app.conf");
            assert_eq!(destination.as_deref(), Some("etc/app.conf"));
            assert_eq!(*mode, Some(420)); // 0o644 = 420 decimal
        }
        other => panic!("expected Detailed, got {:?}", other),
    }
}

#[test]
fn test_generate_yaml_environment_non_string_values() {
    let mut env = BTreeMap::new();
    env.insert("MY_PORT".to_string(), serde_json::json!(8080));
    env.insert("DEBUG".to_string(), serde_json::json!(true));
    env.insert("NAME".to_string(), serde_json::json!("myapp"));

    let mut apps = BTreeMap::new();
    apps.insert(
        "myapp".to_string(),
        SnapcraftApp {
            command: Some("bin/myapp".to_string()),
            environment: Some(env),
            ..Default::default()
        },
    );

    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        apps: Some(apps),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
    assert!(
        yaml.contains("MY_PORT: 8080"),
        "missing integer env\n{yaml}"
    );
    assert!(yaml.contains("DEBUG: true"), "missing boolean env\n{yaml}");
    assert!(yaml.contains("NAME: myapp"), "missing string env\n{yaml}");
}

#[test]
fn test_generate_snap_yaml_multiple_binaries() {
    // Multiple binaries should all get default app entries
    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["server", "client"], None, None).unwrap();
    // Default app uses first binary name
    assert!(
        yaml.contains("command: server"),
        "default app should use first binary\n{yaml}"
    );
}

#[test]
fn test_config_parse_kebab_case_aliases() {
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: .
    tag_template: "v{{ .Version }}"
    snapcrafts:
      - name: mysnap
        apps:
          myapp:
            command: bin/myapp
            stop-mode: sigterm
            restart-condition: on-failure
            bus-name: com.example.myapp
            command-chain:
              - bin/wrapper
            common-id: com.example.myapp
            install-mode: disable
            post-stop-command: bin/cleanup
            refresh-mode: endure
            reload-command: bin/reload
            restart-delay: 10s
            start-timeout: 30s
            stop-command: bin/stop
            stop-timeout: 15s
            watchdog-timeout: 60s
"#;
    let config: Config = serde_yaml_ng::from_str(yaml)
        .unwrap_or_else(|e| panic!("failed to parse kebab-case config: {e}"));
    let snaps = config.crates[0].snapcrafts.as_ref().unwrap();
    let app = snaps[0].apps.as_ref().unwrap().get("myapp").unwrap();
    assert_eq!(app.stop_mode.as_deref(), Some("sigterm"));
    assert_eq!(app.restart_condition.as_deref(), Some("on-failure"));
    assert_eq!(app.bus_name.as_deref(), Some("com.example.myapp"));
    assert_eq!(app.command_chain.as_ref().unwrap(), &["bin/wrapper"]);
    assert_eq!(app.common_id.as_deref(), Some("com.example.myapp"));
    assert_eq!(app.install_mode.as_deref(), Some("disable"));
    assert_eq!(app.post_stop_command.as_deref(), Some("bin/cleanup"));
    assert_eq!(app.refresh_mode.as_deref(), Some("endure"));
    assert_eq!(app.reload_command.as_deref(), Some("bin/reload"));
    assert_eq!(app.restart_delay.as_deref(), Some("10s"));
    assert_eq!(app.start_timeout.as_deref(), Some("30s"));
    assert_eq!(app.stop_command.as_deref(), Some("bin/stop"));
    assert_eq!(app.stop_timeout.as_deref(), Some("15s"));
    assert_eq!(app.watchdog_timeout.as_deref(), Some("60s"));
}

#[test]
fn test_new_app_fields_omitted_when_empty() {
    // When new fields are not set, they should NOT appear in generated YAML
    let mut apps = BTreeMap::new();
    apps.insert(
        "myapp".to_string(),
        SnapcraftApp {
            command: Some("bin/myapp".to_string()),
            ..Default::default()
        },
    );

    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        apps: Some(apps),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();

    // None of the new fields should appear
    assert!(
        !yaml.contains("adapter:"),
        "adapter should be omitted\n{yaml}"
    );
    assert!(!yaml.contains("after:"), "after should be omitted\n{yaml}");
    assert!(
        !yaml.contains("aliases:"),
        "aliases should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("autostart:"),
        "autostart should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("before:"),
        "before should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("bus-name:"),
        "bus-name should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("command-chain:"),
        "command-chain should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("common-id:"),
        "common-id should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("completer:"),
        "completer should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("desktop:"),
        "desktop should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("extensions:"),
        "extensions should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("install-mode:"),
        "install-mode should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("passthrough:"),
        "passthrough should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("post-stop-command:"),
        "post-stop-command should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("refresh-mode:"),
        "refresh-mode should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("reload-command:"),
        "reload-command should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("restart-delay:"),
        "restart-delay should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("sockets:"),
        "sockets should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("start-timeout:"),
        "start-timeout should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("stop-command:"),
        "stop-command should be omitted\n{yaml}"
    );
    assert!(
        !yaml.contains("stop-timeout:"),
        "stop-timeout should be omitted\n{yaml}"
    );
    assert!(!yaml.contains("timer:"), "timer should be omitted\n{yaml}");
    assert!(
        !yaml.contains("watchdog-timeout:"),
        "watchdog-timeout should be omitted\n{yaml}"
    );
    assert!(!yaml.contains("hooks:"), "hooks should be omitted\n{yaml}");
}

// -----------------------------------------------------------------------
// Q8.1 — snapcraft 5xx retry classifier
// -----------------------------------------------------------------------

#[test]
fn snapcraft_5xx_classifies_retriable() {
    // Bracketed-status form (canonical upstream `isRetriableSnapPush`
    // shape, mirroring GR commit eb944f9).
    for marker in ["[500]", "[502]", "[503]", "[504]"] {
        let combined = format!("snapcraft: server returned {marker} while pushing snap\n");
        assert!(
            is_retriable_snap_push(&combined),
            "expected retriable for marker {marker}: {combined}"
        );
    }

    // Reason-text form (defense-in-depth: the snapcraft CLI may format
    // server errors without the `[NNN]` brackets in some versions).
    for marker in [
        "500 Internal Server Error",
        "502 Bad Gateway",
        "503 Service Unavailable",
        "504 Gateway Timeout",
    ] {
        let combined = format!("snapcraft: store returned '{marker}'\n");
        assert!(
            is_retriable_snap_push(&combined),
            "expected retriable for marker {marker}: {combined}"
        );
    }
}

#[test]
fn snapcraft_non_5xx_classifies_unrecoverable() {
    // 4xx and auth failures must NOT be retried — they would just burn
    // retry budget against a permanently-broken request.
    for combined in [
        "snapcraft: 401 Unauthorized — please run `snapcraft login`",
        "snapcraft: 403 Forbidden",
        "snapcraft: 404 Not Found",
        "[400] Bad Request: malformed snap",
        "could not parse snap.yaml",
        "snap failed validation: missing summary",
    ] {
        assert!(
            !is_retriable_snap_push(combined),
            "expected NOT retriable: {combined}"
        );
    }
}
