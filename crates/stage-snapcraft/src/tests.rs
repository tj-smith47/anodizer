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

use crate::build_stage::copy_snap_icon;
use crate::command::{
    is_retriable_snap_push, resolve_effective_channels, snap_revision_exists_in_output,
    snapcraft_command, snapcraft_list_revisions_command, snapcraft_upload_command,
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
    // args must be appended to command (same field), not a separate `args:` key
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
fn test_generate_snap_yaml_app_missing_command_defaults_to_app_name() {
    // A missing
    // app command to the app key name (`command := name`), NOT erroring.
    let mut apps = BTreeMap::new();
    apps.insert(
        "myapp".to_string(),
        SnapcraftApp {
            command: None,
            daemon: Some("simple".to_string()),
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
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["bin/myapp"], None, None).unwrap();
    assert!(
        yaml.contains("command: myapp"),
        "missing command must default to the app name (GR parity):\n{yaml}"
    );
}

#[test]
fn test_generate_snap_yaml_app_missing_command_with_args_defaults_to_app_name() {
    // The app-name default must still pick up `args:` (appended to command).
    let mut apps = BTreeMap::new();
    apps.insert(
        "server".to_string(),
        SnapcraftApp {
            command: None,
            args: Some("--port 8080".to_string()),
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
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["bin/server"], None, None).unwrap();
    assert!(
        yaml.contains("command: server --port 8080"),
        "defaulted command must still append args:\n{yaml}"
    );
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
        layout: Some(layouts),
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
    // Do NOT default `base:`. Classic-confinement snaps
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
    // `base:` is only emitted when the user supplied one.
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
// (e.g. dropping `skip_serializing_if`) can't silently regress this
// behaviour back into emitting a Store-rejected `icon:` key.
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
fn test_generate_snap_yaml_never_emits_icon_even_when_set() {
    // When `icon` is configured, the build stage copies the file to
    // `meta/gui/<name>.<ext>` inside the prime dir before `snapcraft pack`.
    // `generate_snap_yaml` must NEVER emit `icon:` because the Snap Store
    // rejects `snap.json` with "Additional properties are not allowed
    // ('icon' was unexpected)".
    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("A test snap".to_string()),
        description: Some("A test description".to_string()),
        icon: Some("assets/logo.png".to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "0.1.0", &["mytool"], None, None).unwrap();
    let has_icon_key = yaml.lines().any(|l| {
        let trimmed = l.trim_start();
        trimmed == "icon:" || trimmed.starts_with("icon: ") || trimmed.starts_with("icon:\t")
    });
    assert!(
        !has_icon_key,
        "generate_snap_yaml must NEVER emit `icon:` — the Snap Store \
         rejects snap.json with that field. The icon is delivered via \
         meta/gui/ instead. Got:\n{yaml}"
    );
}

// -----------------------------------------------------------------------
// copy_snap_icon tests
// -----------------------------------------------------------------------

/// icon set + valid source file: copied to meta/gui/<name>.<ext> with correct bytes.
#[test]
fn test_copy_snap_icon_png_copies_bytes_to_meta_gui() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("icon.png");
    let icon_bytes: &[u8] = b"\x89PNG\r\n\x1a\n"; // PNG magic bytes
    std::fs::write(&src, icon_bytes).unwrap();

    let meta_dir = tmp.path().join("meta");
    std::fs::create_dir_all(&meta_dir).unwrap();

    let dest_rel = copy_snap_icon(&src, &meta_dir, "mysnap").unwrap();

    assert_eq!(dest_rel, "meta/gui/mysnap.png");
    let dest = meta_dir.join("gui").join("mysnap.png");
    assert!(dest.exists(), "icon must be copied to meta/gui/mysnap.png");
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        icon_bytes,
        "bytes must match"
    );

    // The generated snap.yaml must NOT contain `icon:` — confirmed via
    // generate_snap_yaml which always returns icon: None.
    let cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test".to_string()),
        description: Some("Test snap".to_string()),
        icon: Some(src.to_string_lossy().to_string()),
        ..Default::default()
    };
    let yaml = generate_snap_yaml(&cfg, "1.0.0", &["mysnap"], None, None).unwrap();
    let has_icon_key = yaml
        .lines()
        .any(|l| l.trim_start().starts_with("icon:") || l.trim_start() == "icon:");
    assert!(
        !has_icon_key,
        "snap.yaml must not contain `icon:`. Got:\n{yaml}"
    );
}

/// icon set with .svg source: extension is preserved in the destination filename.
#[test]
fn test_copy_snap_icon_preserves_svg_extension() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("logo.svg");
    std::fs::write(&src, b"<svg/>").unwrap();

    let meta_dir = tmp.path().join("meta");
    std::fs::create_dir_all(&meta_dir).unwrap();

    let dest_rel = copy_snap_icon(&src, &meta_dir, "myapp").unwrap();
    assert_eq!(dest_rel, "meta/gui/myapp.svg");
    assert!(meta_dir.join("gui").join("myapp.svg").exists());
}

/// `copy_snap_icon` must overwrite an existing destination file. Pins the
/// `fs::copy` truncate-on-conflict contract so a re-run never silently
/// preserves a stale icon from a prior pack attempt.
#[test]
fn test_copy_snap_icon_overwrites_existing_destination() {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("icon.png");
    let source_bytes: &[u8] = b"new-icon-bytes";
    std::fs::write(&src, source_bytes).unwrap();

    let meta_dir = tmp.path().join("meta");
    let gui_dir = meta_dir.join("gui");
    std::fs::create_dir_all(&gui_dir).unwrap();

    // Pre-seed the destination with sentinel bytes that must be overwritten.
    let dest = gui_dir.join("mysnap.png");
    std::fs::write(&dest, b"stale-sentinel-bytes").unwrap();

    copy_snap_icon(&src, &meta_dir, "mysnap").unwrap();

    let on_disk = std::fs::read(&dest).unwrap();
    assert_eq!(
        on_disk, source_bytes,
        "copy_snap_icon must overwrite an existing destination, not preserve stale bytes"
    );
}

/// icon set + unsupported extension (`.jpg`): build stage must error BEFORE
/// pack runs. snapcraft only accepts png/svg for snap icons.
#[test]
fn test_stage_icon_unsupported_extension_errors_before_pack() {
    let tmp = TempDir::new().unwrap();
    // The file must EXIST so we know the extension check fires, not the
    // existence check.
    let icon = tmp.path().join("logo.jpg");
    std::fs::write(&icon, b"fake-jpg").unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        icon: Some(icon.to_string_lossy().to_string()),
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
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    let err = stage.run(&mut ctx).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unsupported extension"),
        "error should mention unsupported extension, got: {msg}"
    );
    assert!(
        msg.contains(".png") && msg.contains(".svg"),
        "error should name the allowed extensions (.png, .svg), got: {msg}"
    );
    // No snap artifacts registered — stage bailed before pack staging.
    assert!(
        ctx.artifacts.by_kind(ArtifactKind::Snap).is_empty(),
        "snapcraft pack must not have been invoked on extension error"
    );
}

/// icon set + extensionless source: extension validator must reject,
/// not silently fall back to `.png`.
#[test]
fn test_stage_icon_extensionless_source_errors_before_pack() {
    let tmp = TempDir::new().unwrap();
    let icon = tmp.path().join("logo");
    std::fs::write(&icon, b"raw bytes").unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        icon: Some(icon.to_string_lossy().to_string()),
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
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    let err = stage.run(&mut ctx).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unsupported extension"),
        "extensionless source must trigger the unsupported-extension error, got: {msg}"
    );
}

/// icon set + missing source file: build stage must error BEFORE pack runs.
#[test]
fn test_stage_icon_missing_source_errors_before_pack() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        icon: Some("/nonexistent/path/icon.png".to_string()),
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
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    let err = stage.run(&mut ctx).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("does not exist") || msg.contains("icon"),
        "error should mention missing icon path, got: {msg}"
    );
    assert!(
        msg.contains("nonexistent") || msg.contains("/nonexistent/path/icon.png"),
        "error should name the missing path, got: {msg}"
    );
}

/// icon not set: no copy, no error, build proceeds normally.
#[test]
fn test_stage_icon_not_set_is_noop() {
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        icon: None,
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
        path: PathBuf::from("/tmp/myapp"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    let stage = SnapcraftStage;
    // Must succeed with no errors when icon is None.
    stage.run(&mut ctx).unwrap();
    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1, "should register one snap artifact");
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
fn snap_is_byte_reproducible_across_time() {
    use std::process::Command;
    // `snapcraft pack <prime_dir>` packs a pre-assembled prime directory via
    // mksquashfs — no LXD/multipass VM is needed (there is no build step), so
    // this runs on any linux host with the snapcraft binary. Hermetic:
    // skip-with-pass when snapcraft is absent. mksquashfs honors
    // SOURCE_DATE_EPOCH for the squashfs superblock mod_time and per-inode
    // mtimes — the value the determinism harness exports into every stage's
    // subprocess env — so two builds with a wall-clock gap are byte-identical.
    if !anodizer_core::util::find_binary("snapcraft") {
        eprintln!("snapcraft absent; .snap reproducibility test skipped hermetically");
        return;
    }
    let dir = TempDir::new().unwrap();
    let prime = dir.path().join("prime");
    std::fs::create_dir_all(prime.join("meta")).unwrap();
    std::fs::create_dir_all(prime.join("bin")).unwrap();
    std::fs::write(
        prime.join("meta/snap.yaml"),
        "name: probe-snap\nversion: '1.2.3'\nsummary: probe\n\
         description: probe snap\narchitectures:\n  - amd64\n\
         confinement: strict\ngrade: stable\n\
         apps:\n  probe:\n    command: bin/probe\n",
    )
    .unwrap();
    let probe_bin = prime.join("bin/probe");
    std::fs::write(&probe_bin, b"#!/bin/sh\necho hi\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        // snapcraft rejects a prime tree whose apps are not world-readable +
        // executable; the real build stage stages binaries 0755.
        std::fs::set_permissions(&probe_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // snapcraft persists per-user state under $XDG_CACHE_HOME/snapcraft
    // (default ~/.cache); xdg's save_cache_path is not race-safe, so two
    // snapcraft processes from parallel tests creating that shared dir collide
    // (Errno 17 File exists). Pin every XDG base to this test's tempdir so the
    // invocation is hermetic and cannot race a concurrent test.
    let xdg_cache = dir.path().join("xdg-cache");
    let xdg_data = dir.path().join("xdg-data");
    let xdg_config = dir.path().join("xdg-config");
    // snapcraft's import-time `save_cache_path("snapcraft", "download")` is a
    // non-atomic check-then-makedirs; snapcraft's internal worker subprocesses
    // re-import and race it (Errno 17) under load. Pre-creating the tree makes
    // xdg's isdir check short-circuit so no makedirs — and therefore no race —
    // ever runs, regardless of worker concurrency.
    std::fs::create_dir_all(xdg_cache.join("snapcraft").join("download")).unwrap();
    let build = |out: &std::path::Path| -> Vec<u8> {
        let args = snapcraft_command(prime.to_str().unwrap(), out.to_str().unwrap());
        let status = Command::new(&args[0])
            .args(&args[1..])
            .env("SOURCE_DATE_EPOCH", "1704067200")
            .env("XDG_CACHE_HOME", &xdg_cache)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .status()
            .expect("spawn snapcraft pack");
        assert!(status.success(), "snapcraft pack must succeed");
        std::fs::read(out).unwrap()
    };
    let a = build(&dir.path().join("a.snap"));
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let b = build(&dir.path().join("b.snap"));
    assert_eq!(
        a, b,
        ".snap must be byte-identical across two builds at a fixed SOURCE_DATE_EPOCH"
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

#[test]
fn test_snapcraft_list_revisions_command() {
    let cmd = snapcraft_list_revisions_command("mytool");
    assert_eq!(cmd, vec!["snapcraft", "list-revisions", "mytool"]);
}

#[test]
fn snap_revision_exists_matches_published_version_only() {
    // Real-ish `snapcraft list-revisions` table.
    let output = "\
Rev    Uploaded              Arches  Version  Channels
3      2024-06-01T10:00:00Z  amd64   1.2.0    stable*
2      2024-05-01T10:00:00Z  amd64   1.1.0    -
1      2024-04-01T10:00:00Z  amd64   1.0.0    -
";
    // A version with an existing revision → present.
    assert!(snap_revision_exists_in_output(output, "1.1.0"));
    assert!(snap_revision_exists_in_output(output, "1.2.0"));
    // A never-published version → absent (genuine first publish).
    assert!(!snap_revision_exists_in_output(output, "1.3.0"));
    // Substring guard: "1.0.0" must not match "1.0.0-rc1" or vice-versa.
    assert!(!snap_revision_exists_in_output(output, "1.0"));
    assert!(!snap_revision_exists_in_output(output, "1.0.0-rc1"));
}

#[test]
fn snap_revision_exists_empty_output_is_absent() {
    // No body / header-only → never falsely report present.
    assert!(!snap_revision_exists_in_output("", "1.0.0"));
    assert!(!snap_revision_exists_in_output(
        "Rev  Uploaded  Arches  Version  Channels\n",
        "1.0.0"
    ));
    // A snap literally named like a version must not false-positive on a
    // header/label row — only data rows are scanned.
    assert!(!snap_revision_exists_in_output(
        "No revisions available for this snap.\n",
        "1.0.0"
    ));
}

/// Version-column isolation: tokens in the Rev, Arches, and Channels columns
/// must never trigger a false-positive when they happen to equal the version
/// string being searched for (e.g. version "3" matching revision "3", or
/// version "amd64" matching the Arches column).
#[test]
fn snap_revision_exists_checks_version_column_only() {
    // Revision numbers are small integers; a bare-integer version collides.
    let output = "\
Rev    Uploaded              Arches  Version  Channels
3      2024-06-01T10:00:00Z  amd64   1.2.0    stable*
2      2024-05-01T10:00:00Z  amd64   1.1.0    -
1      2024-04-01T10:00:00Z  amd64   1.0.0    -
";
    // Rev "3" must not match version "3" — "3" is not in the Version column.
    assert!(!snap_revision_exists_in_output(output, "3"));
    // Arch string must not match — "amd64" appears in Arches, not Version.
    assert!(!snap_revision_exists_in_output(output, "amd64"));
    // A real version still resolves correctly through the column filter.
    assert!(snap_revision_exists_in_output(output, "1.2.0"));
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
    // matches the default snap-name shape.
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

    let layouts = snap.layout.as_ref().unwrap();
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
fn test_config_parse_snapcraft_layout_canonical() {
    // GoReleaser-style canonical singular `layout:` map parses into the field.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    snapcrafts:
      - name: mysnap
        layout:
          /usr/share/myapp:
            bind: $SNAP/usr/share/myapp
"#;
    let config: Config = serde_yaml_ng::from_str(yaml)
        .unwrap_or_else(|e| panic!("failed to parse snapcraft `layout:` config: {e}"));
    let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
    let layout = snap
        .layout
        .as_ref()
        .expect("layout map should be populated");
    assert_eq!(
        layout.get("/usr/share/myapp").unwrap().bind.as_deref(),
        Some("$SNAP/usr/share/myapp")
    );
}

#[test]
fn test_config_parse_snapcraft_layouts_alias_backcompat() {
    // Legacy plural `layouts:` spelling still parses via serde alias.
    let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    snapcrafts:
      - name: mysnap
        layouts:
          /etc/myapp:
            symlink: $SNAP_DATA/etc/myapp
"#;
    let config: Config = serde_yaml_ng::from_str(yaml)
        .unwrap_or_else(|e| panic!("failed to parse legacy `layouts:` alias config: {e}"));
    let snap = &config.crates[0].snapcrafts.as_ref().unwrap()[0];
    let layout = snap
        .layout
        .as_ref()
        .expect("layouts alias should populate the layout field");
    assert_eq!(
        layout.get("/etc/myapp").unwrap().symlink.as_deref(),
        Some("$SNAP_DATA/etc/myapp")
    );
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
        layout: Some(layouts),
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
    // shape).
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

// -----------------------------------------------------------------------
// Branches not exercised above: grade validation, riscv64 skip,
// ARM Arch/Arm template split, multi-binary grouping per target,
// ids-matches-none warn+skip, meta_description fallback,
// project_root relative-icon resolution, and the staging-side error
// paths (mode > 0o7777, extra_files copy, completer copy).
// -----------------------------------------------------------------------

/// Helper: build a one-crate Context with the given SnapcraftConfig and
/// register the supplied binary artifacts. Common scaffolding extracted
/// so each branch test only carries its own variation.
fn stage_ctx_with_binaries(
    dist: PathBuf,
    snap_cfg: SnapcraftConfig,
    binaries: Vec<Artifact>,
    dry_run: bool,
) -> Context {
    let crate_cfg = CrateConfig {
        name: "myapp".to_string(),
        path: ".".to_string(),
        tag_template: "v{{ .Version }}".to_string(),
        snapcrafts: Some(vec![snap_cfg]),
        ..Default::default()
    };
    let mut config = Config::default();
    config.project_name = "myapp".to_string();
    config.dist = dist;
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    for art in binaries {
        ctx.artifacts.add(art);
    }
    ctx
}

fn linux_bin(name: &str, target: &str) -> Artifact {
    Artifact {
        kind: ArtifactKind::Binary,
        name: String::new(),
        path: PathBuf::from(format!("/tmp/{name}")),
        target: Some(target.to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    }
}

#[test]
fn test_grade_validation_rejects_unknown_value() {
    // The grade-validation match arm (build_stage.rs:197-207) bails on any
    // value outside {stable, devel}. Mirrors the confinement validation
    // already tested above for the sibling branch.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        grade: Some("alpha".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![linux_bin("myapp", "x86_64-unknown-linux-gnu")],
        true,
    );

    let err = SnapcraftStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invalid grade") && msg.contains("alpha"),
        "expected grade-validation error naming the bad value, got: {msg}"
    );
}

#[test]
fn test_riscv64_target_is_skipped() {
    // riscv64 → triple_to_snap_arch returns "riscv64", which is_valid_snap_arch
    // rejects. The stage logs a warn and continues without producing a snap
    // for that target (build_stage.rs:303-312).
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![
            linux_bin("myapp-amd64", "x86_64-unknown-linux-gnu"),
            linux_bin("myapp-riscv", "riscv64gc-unknown-linux-gnu"),
        ],
        true,
    );
    SnapcraftStage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(
        snaps.len(),
        1,
        "riscv64 target must be filtered out, only amd64 snap should register"
    );
    assert_eq!(snaps[0].target.as_deref(), Some("x86_64-unknown-linux-gnu"));
}

#[test]
fn test_armv7_target_splits_arch_and_arm_for_default_template() {
    // The default name template renders `{{ .Arch }}{{ with .Arm }}v{{ . }}{{ end }}`.
    // For armv7-* targets the stage must split Arch="arm" and Arm="7" so
    // the rendered filename is `linux_armv7`, not `linux_armv7v7`
    // (build_stage.rs:344-350).
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![linux_bin("myapp", "armv7-unknown-linux-gnueabihf")],
        true,
    );
    SnapcraftStage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1);
    let path_str = snaps[0].path.to_string_lossy().into_owned();
    assert!(
        path_str.ends_with("mysnap_1.0.0_linux_armv7.snap"),
        "armv7 must produce single 'armv7' suffix, not doubled. Got: {path_str}"
    );
}

#[test]
fn test_multiple_binaries_same_target_produce_single_snap() {
    // Two binaries for the same target triple should group into ONE snap
    // (one entry in the by_target BTreeMap → one job). The snap.yaml is
    // unobservable from a dry-run, but the artifact count proves grouping.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![
            linux_bin("server", "x86_64-unknown-linux-gnu"),
            linux_bin("client", "x86_64-unknown-linux-gnu"),
        ],
        true,
    );
    SnapcraftStage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(
        snaps.len(),
        1,
        "two binaries on the same target must collapse into one snap"
    );
}

#[test]
fn test_ids_filter_matches_none_skips_with_warning() {
    // When linux_binaries is non-empty but the ids filter matches zero of
    // them, the stage logs a warn-and-skip distinct from the no-linux-
    // binaries path (build_stage.rs:272-278).
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        ids: Some(vec!["nonexistent-build-id".to_string()]),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/tmp/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::from([("id".to_string(), "actual-build-id".to_string())]),
            size: None,
        }],
        true,
    );
    SnapcraftStage.run(&mut ctx).unwrap();

    assert!(
        ctx.artifacts.by_kind(ArtifactKind::Snap).is_empty(),
        "ids filter matching nothing must skip pack without erroring"
    );
}

#[test]
fn test_project_root_resolves_relative_icon_path() {
    // resolve_icon_path joins relative paths against project_root. When the
    // file exists at <project_root>/<icon>, validation must pass; when only
    // a CWD-relative path exists, validation must fail (the icon isn't
    // where project_root says it should be).
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("myproject");
    std::fs::create_dir_all(project_root.join("assets")).unwrap();
    let icon_at_project = project_root.join("assets").join("icon.png");
    std::fs::write(&icon_at_project, b"\x89PNG\r\n\x1a\n").unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        icon: Some("assets/icon.png".to_string()),
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
            project_root: Some(project_root.clone()),
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts
        .add(linux_bin("myapp", "x86_64-unknown-linux-gnu"));

    // Must succeed — icon resolves to <project_root>/assets/icon.png.
    SnapcraftStage
        .run(&mut ctx)
        .expect("icon resolution via project_root should find the file");
}

#[test]
fn test_extra_files_invalid_mode_errors_during_staging() {
    // mode > 0o7777 is rejected during staging, after the copy succeeds
    // (build_stage.rs:533-540). The error must mention the bad mode and
    // happen BEFORE snapcraft pack spawns — so it's reproducible without
    // a real snapcraft binary.
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("myapp");
    std::fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
    let extra_src = tmp.path().join("README.md");
    std::fs::write(&extra_src, b"hi").unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        extra_files: Some(vec![SnapcraftExtraFileSpec::Detailed {
            source: extra_src.to_string_lossy().to_string(),
            destination: Some("README.md".to_string()),
            mode: Some(0o10000), // one bit over the 0o7777 limit
        }]),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        }],
        false, // must be non-dry-run so the extra_files branch runs
    );

    let err = SnapcraftStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("invalid file mode"),
        "expected mode-range error during staging, got: {msg}"
    );
}

#[test]
fn test_extra_files_copies_source_to_prime_dest() {
    // Detailed-form extra_files (source + custom destination) are staged
    // into the prime dir during the non-dry-run path. We can't peek the
    // tmp prime dir post-run (TempDir drops at end of staging), but a
    // missing source file fails the copy with a specific error that
    // proves the copy site executed (build_stage.rs:525-527).
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("myapp");
    std::fs::write(&bin_path, b"#!/bin/sh\n").unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        extra_files: Some(vec![SnapcraftExtraFileSpec::Detailed {
            source: "/nonexistent/extra/file.txt".to_string(),
            destination: Some("etc/file.txt".to_string()),
            mode: Some(0o644),
        }]),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        }],
        false,
    );
    let err = SnapcraftStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("copy extra file"),
        "expected extra-files copy-site error, got: {msg}"
    );
    assert!(
        msg.contains("/nonexistent/extra/file.txt"),
        "error must name the missing source path, got: {msg}"
    );
}

#[test]
fn test_apps_completer_existing_file_is_copied_to_prime() {
    // The completer-copy branch fires when the apps map contains an entry
    // with `completer:` set. `completer` is a relative path resolved
    // against `ctx.options.project_root` (source) and joined onto the
    // prime dir (destination). With a real source present, the branch
    // runs to completion and the stage then proceeds to the snapcraft
    // spawn, which fails with "program not found" — that's the proof
    // the staging step ran successfully.
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("myapp");
    std::fs::write(&bin_path, b"#!/bin/sh\n").unwrap();
    // Lay the completer source down at <project_root>/completions/myapp.bash
    // so the relative-path resolver in build_stage.rs finds it.
    let completer_rel = "completions/myapp.bash";
    let completer_abs = tmp.path().join(completer_rel);
    std::fs::create_dir_all(completer_abs.parent().unwrap()).unwrap();
    std::fs::write(&completer_abs, b"# completion script").unwrap();

    let mut apps = BTreeMap::new();
    apps.insert(
        "myapp".to_string(),
        SnapcraftApp {
            command: Some("myapp".to_string()),
            completer: Some(completer_rel.to_string()),
            ..Default::default()
        },
    );
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        apps: Some(apps),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        }],
        false,
    );
    ctx.options.project_root = Some(tmp.path().to_path_buf());
    // The contract pinned here: with a valid (relative) completer
    // source at `<project_root>/<completer>`, the staging branch
    // completes without surfacing a `copy completer` error. The
    // subsequent snapcraft spawn may succeed (snapcraft installed)
    // or fail (snapcraft missing); either outcome proves the
    // completer branch ran clean.
    let result = SnapcraftStage.run(&mut ctx);
    if let Err(err) = result {
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("copy completer"),
            "completer staging branch failed unexpectedly: {msg}"
        );
    }
}

#[test]
fn test_apps_completer_absolute_path_is_rejected_with_actionable_error() {
    // An absolute completer path collapses source and destination
    // because `Path::join(absolute)` discards the prefix on every
    // platform. The stage rejects absolute paths up front; this pins
    // the bail message + the app-naming contract.
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("myapp");
    std::fs::write(&bin_path, b"#!/bin/sh\n").unwrap();

    let mut apps = BTreeMap::new();
    apps.insert(
        "myapp".to_string(),
        SnapcraftApp {
            command: Some("myapp".to_string()),
            completer: Some(
                tmp.path()
                    .join("completions/myapp.bash")
                    .to_string_lossy()
                    .to_string(),
            ),
            ..Default::default()
        },
    );
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        apps: Some(apps),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        }],
        false,
    );
    let err = SnapcraftStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("must be relative") && msg.contains("myapp"),
        "absolute-completer bail must name the app + the contract, got: {msg}"
    );
}

#[test]
fn test_summary_template_render_failure_errors() {
    // summary goes through render_template (build_stage.rs:447-451). A
    // malformed template surfaces as a render error before the spawn.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Broken {{ unterminated".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![linux_bin("myapp", "x86_64-unknown-linux-gnu")],
        false,
    );
    let err = SnapcraftStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("summary"),
        "expected summary render-site error context, got: {msg}"
    );
}

#[test]
fn test_skip_template_string_evaluating_true_skips() {
    // skip: StringOrBool::String("true") must be evaluated via the template
    // engine and treated as truthy (build_stage.rs:168-181). Distinct from
    // the bool branch already tested above.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        skip: Some(StringOrBool::String("true".to_string())),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![linux_bin("myapp", "x86_64-unknown-linux-gnu")],
        true,
    );
    SnapcraftStage.run(&mut ctx).unwrap();
    assert!(
        ctx.artifacts.by_kind(ArtifactKind::Snap).is_empty(),
        "skip='true' (string form) must skip the config like skip=true (bool form)"
    );
}

#[test]
fn test_selected_crates_filter_excludes_unmatched_crate() {
    // selected_crates filtering (build_stage.rs:127): when set, only
    // crates whose name is in the list contribute snaps.
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
            selected_crates: vec!["other-crate".to_string()],
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts
        .add(linux_bin("myapp", "x86_64-unknown-linux-gnu"));

    SnapcraftStage.run(&mut ctx).unwrap();
    assert!(
        ctx.artifacts.by_kind(ArtifactKind::Snap).is_empty(),
        "crate not in selected_crates must be skipped"
    );
}

#[test]
fn test_name_template_with_explicit_snap_suffix_is_not_doubled() {
    // The stage appends `.snap` only when the rendered template doesn't
    // already end in `.snap` (build_stage.rs:369-373). Pin both branches.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        name_template: Some("custom_{{ Version }}.snap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![linux_bin("myapp", "x86_64-unknown-linux-gnu")],
        true,
    );
    SnapcraftStage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1);
    let path_str = snaps[0].path.to_string_lossy().into_owned();
    assert!(
        path_str.ends_with("/custom_1.0.0.snap"),
        "rendered .snap suffix must not be doubled. Got: {path_str}"
    );
    assert!(
        !path_str.ends_with(".snap.snap"),
        "unexpected doubled suffix: {path_str}"
    );
}

// -----------------------------------------------------------------------
// Additional uncovered branches: description/grade render errors,
// absolute icon paths, icon copy in non-dry-run, mod_timestamp,
// templated_extra_files, real replace removal, name_template
// ending in .SNAP (case-insensitive suffix check).
// -----------------------------------------------------------------------

#[test]
fn test_description_template_render_failure_errors() {
    // description goes through render_template (build_stage.rs:452-457).
    // A malformed Tera template must surface as a render error before
    // snapcraft pack spawns. Mirrors the summary-render test above.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("Broken {{ unterminated".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![linux_bin("myapp", "x86_64-unknown-linux-gnu")],
        false,
    );
    let err = SnapcraftStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("description"),
        "expected description render-site error context, got: {msg}"
    );
}

#[test]
fn test_absolute_icon_path_resolves_directly() {
    // resolve_icon_path's absolute branch (build_stage.rs:44-45): an
    // absolute path is returned unchanged and must NOT be re-rooted under
    // project_root. Observable by passing an absolute icon path that
    // exists on disk while project_root points elsewhere.
    let tmp = TempDir::new().unwrap();
    let icon_path = tmp.path().join("icon.png");
    std::fs::write(&icon_path, b"\x89PNG\r\n\x1a\n").unwrap();

    // project_root is a sibling that does NOT contain the icon.
    let project_root = tmp.path().join("other-project");
    std::fs::create_dir_all(&project_root).unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        icon: Some(icon_path.to_string_lossy().into_owned()),
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
            project_root: Some(project_root),
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts
        .add(linux_bin("myapp", "x86_64-unknown-linux-gnu"));

    // Absolute icon path must validate-OK regardless of project_root.
    SnapcraftStage
        .run(&mut ctx)
        .expect("absolute icon path should bypass project_root rerooting");
}

#[test]
fn test_icon_resolution_falls_back_to_cwd_when_project_root_unset() {
    // resolve_icon_path's `unwrap_or(Path::new("."))` branch
    // (build_stage.rs:49): when project_root is None AND the icon path
    // is relative, validation looks under CWD. We can't easily mutate
    // CWD safely from a test, so we assert the negative behaviour: a
    // nonexistent relative icon with project_root unset fails validation
    // with the "does not exist" error, proving the resolution branch ran.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        icon: Some("definitely-not-a-real-icon-xyz123.png".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![linux_bin("myapp", "x86_64-unknown-linux-gnu")],
        true,
    );
    let err = SnapcraftStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("does not exist") && msg.contains("definitely-not-a-real-icon-xyz123.png"),
        "expected icon-missing error after CWD resolution, got: {msg}"
    );
}

#[test]
fn test_mod_timestamp_invalid_value_errors_during_staging() {
    // build_stage.rs:591-593: mod_timestamp is applied during the
    // non-dry-run staging path. An unparseable value must surface as a
    // timestamp-parse error before the snapcraft spawn.
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("myapp");
    std::fs::write(&bin_path, b"#!/bin/sh\n").unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        mod_timestamp: Some("not-a-timestamp".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        }],
        false,
    );
    let err = SnapcraftStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mod_timestamp") && msg.contains("not-a-timestamp"),
        "expected mod_timestamp parse error, got: {msg}"
    );
}

#[test]
fn test_templated_extra_files_missing_source_errors() {
    // build_stage.rs:579-588: templated_extra_files invokes
    // anodizer_core::templated_files::process_templated_extra_files. A
    // nonexistent source file surfaces as a "read templated file" error
    // tagged "snapcraft" — proving the branch fires.
    let tmp = TempDir::new().unwrap();
    let bin_path = tmp.path().join("myapp");
    std::fs::write(&bin_path, b"#!/bin/sh\n").unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        templated_extra_files: Some(vec![anodizer_core::config::TemplatedExtraFile {
            src: "/nonexistent/template.tpl".to_string(),
            dst: Some("notes.txt".to_string()),
            mode: None,
        }]),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: bin_path,
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        }],
        false,
    );
    let err = SnapcraftStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("snapcraft") && msg.contains("templated"),
        "expected snapcraft-tagged templated-file error, got: {msg}"
    );
    assert!(
        msg.contains("/nonexistent/template.tpl"),
        "error must name the missing source, got: {msg}"
    );
}

#[test]
fn test_replace_false_does_not_remove_archives() {
    // build_stage.rs:602-609: collect_if_replace is a no-op when
    // replace=false. Pairs with test_stage_dry_run_replace_removes_archives
    // (which pins the replace=true branch) to fence both arms of the
    // conditional.
    let tmp = TempDir::new().unwrap();

    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        replace: Some(false),
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
    ctx.artifacts
        .add(linux_bin("myapp", "x86_64-unknown-linux-gnu"));
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        name: String::new(),
        path: PathBuf::from("dist/myapp_1.0.0_linux_amd64.tar.gz"),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "myapp".to_string(),
        metadata: HashMap::new(),
        size: None,
    });

    SnapcraftStage.run(&mut ctx).unwrap();

    // replace=false must leave the archive in place.
    let archives = ctx.artifacts.by_kind(ArtifactKind::Archive);
    assert_eq!(
        archives.len(),
        1,
        "replace=false must not remove archives, found: {}",
        archives.len()
    );
}

#[test]
fn test_name_template_with_uppercase_snap_suffix_is_not_doubled() {
    // build_stage.rs:369 uses `rendered.to_lowercase().ends_with(".snap")`
    // — uppercase `.SNAP` must also be detected so the suffix isn't
    // doubled. Pins the case-insensitive branch separately from the
    // lowercase test above.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        name_template: Some("custom_{{ Version }}.SNAP".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![linux_bin("myapp", "x86_64-unknown-linux-gnu")],
        true,
    );
    SnapcraftStage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1);
    let path_str = snaps[0].path.to_string_lossy().into_owned();
    assert!(
        path_str.ends_with("/custom_1.0.0.SNAP"),
        "uppercase .SNAP suffix must be preserved and not doubled. Got: {path_str}"
    );
    assert!(
        !path_str.to_lowercase().ends_with(".snap.snap"),
        "unexpected doubled suffix: {path_str}"
    );
}

#[test]
fn test_arm64_target_uses_arch_arm64_not_arm() {
    // build_stage.rs:344-350: the `armv` prefix split applies ONLY to
    // armv* triples. aarch64-* maps to arm64, which must NOT trigger
    // the Arm/Arch split (no `v` suffix). Confirms the strip_prefix
    // branch isn't over-eager.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![linux_bin("myapp", "aarch64-unknown-linux-gnu")],
        true,
    );
    SnapcraftStage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 1);
    let path_str = snaps[0].path.to_string_lossy().into_owned();
    assert!(
        path_str.ends_with("mysnap_1.0.0_linux_arm64.snap"),
        "aarch64 must render as 'arm64' with no `v` suffix. Got: {path_str}"
    );
}

#[test]
fn test_multiple_targets_produce_one_snap_per_target() {
    // build_stage.rs:288-292: by_target BTreeMap groups binaries per
    // target triple. With distinct linux targets, the stage must
    // register exactly one snap per target — and the BTreeMap-ordered
    // iteration means the artifact order is deterministic.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![
            linux_bin("myapp-amd64", "x86_64-unknown-linux-gnu"),
            linux_bin("myapp-arm64", "aarch64-unknown-linux-gnu"),
        ],
        true,
    );
    SnapcraftStage.run(&mut ctx).unwrap();

    let snaps = ctx.artifacts.by_kind(ArtifactKind::Snap);
    assert_eq!(snaps.len(), 2, "one snap per target triple");
    let targets: Vec<&str> = snaps.iter().filter_map(|s| s.target.as_deref()).collect();
    assert!(targets.contains(&"x86_64-unknown-linux-gnu"));
    assert!(targets.contains(&"aarch64-unknown-linux-gnu"));
}

#[test]
fn test_binary_copy_missing_source_errors_with_path_context() {
    // build_stage.rs:500-502: when a registered binary path doesn't
    // exist on disk, the non-dry-run staging must fail at the binary
    // copy site with the source path in the error context.
    let tmp = TempDir::new().unwrap();
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![Artifact {
            kind: ArtifactKind::Binary,
            name: String::new(),
            path: PathBuf::from("/nonexistent/path/myapp"),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "myapp".to_string(),
            metadata: HashMap::new(),
            size: None,
        }],
        false,
    );
    let err = SnapcraftStage.run(&mut ctx).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("copy binary") && msg.contains("/nonexistent/path/myapp"),
        "expected binary-copy error with source path context, got: {msg}"
    );
}

// -----------------------------------------------------------------------
// Templated string fields render before emission (no literal `{{ }}` leak)
// -----------------------------------------------------------------------

/// Render one crate's snap.yaml through the offline renderer with `Tag` set,
/// returning the single emitted document. Asserts exactly one was produced so
/// the field-leak tests below read the resolved manifest directly.
fn render_single_snap_yaml(snap_cfg: SnapcraftConfig) -> String {
    let tmp = TempDir::new().unwrap();
    let mut ctx = stage_ctx_with_binaries(
        tmp.path().join("dist"),
        snap_cfg,
        vec![linux_bin("myapp", "x86_64-unknown-linux-gnu")],
        true,
    );
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    let yamls = crate::snapcraft_snap_yamls_for_crate(&ctx, "myapp").unwrap();
    assert_eq!(
        yamls.len(),
        1,
        "expected exactly one snap.yaml, got: {yamls:?}"
    );
    yamls.into_iter().next().unwrap()
}

#[test]
fn test_name_field_is_template_rendered() {
    let snap_cfg = SnapcraftConfig {
        name: Some("app-{{ .Tag }}".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        ..Default::default()
    };
    let yaml = render_single_snap_yaml(snap_cfg);
    assert!(
        yaml.contains("name: app-v1.2.3"),
        "name not rendered, got: {yaml}"
    );
    assert!(
        !yaml.contains("{{"),
        "literal template delimiter leaked: {yaml}"
    );
}

#[test]
fn test_base_field_is_template_rendered() {
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        base: Some("core{{ .Tag }}".to_string()),
        ..Default::default()
    };
    let yaml = render_single_snap_yaml(snap_cfg);
    assert!(
        yaml.contains("base: corev1.2.3"),
        "base not rendered, got: {yaml}"
    );
    assert!(
        !yaml.contains("{{"),
        "literal template delimiter leaked: {yaml}"
    );
}

#[test]
fn test_confinement_field_is_template_rendered() {
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        confinement: Some("{{ .Tag }}".to_string()),
        ..Default::default()
    };
    let yaml = render_single_snap_yaml(snap_cfg);
    assert!(
        yaml.contains("confinement: v1.2.3"),
        "confinement not rendered, got: {yaml}"
    );
    assert!(
        !yaml.contains("{{"),
        "literal template delimiter leaked: {yaml}"
    );
}

#[test]
fn test_license_field_is_template_rendered() {
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        license: Some("MIT-{{ .Tag }}".to_string()),
        ..Default::default()
    };
    let yaml = render_single_snap_yaml(snap_cfg);
    assert!(
        yaml.contains("license: MIT-v1.2.3"),
        "license not rendered, got: {yaml}"
    );
    assert!(
        !yaml.contains("{{"),
        "literal template delimiter leaked: {yaml}"
    );
}

#[test]
fn test_title_field_is_template_rendered() {
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        title: Some("{{ .Tag }} build".to_string()),
        ..Default::default()
    };
    let yaml = render_single_snap_yaml(snap_cfg);
    assert!(
        yaml.contains("title: v1.2.3 build"),
        "title not rendered, got: {yaml}"
    );
    assert!(
        !yaml.contains("{{"),
        "literal template delimiter leaked: {yaml}"
    );
}

/// Stage-level assertion that the residual-delimiter guard is wired into the
/// snap.yaml chokepoint. Every config STRING field is rendered before
/// emission, but app `passthrough:` is arbitrary YAML copied verbatim into
/// snap.yaml (the user's documented escape hatch — never template-rendered).
/// A literal `{{ … }}` smuggled through it therefore reaches the finished
/// manifest, and the chokepoint guard must turn that into a hard error under
/// strict mode — proving the net is wired regardless of which field leaks.
#[test]
fn test_guard_fires_in_strict_mode_on_unrendered_field() {
    let mut apps = BTreeMap::new();
    let mut passthrough = BTreeMap::new();
    passthrough.insert(
        "x-custom".to_string(),
        serde_json::Value::String("{{ .Tag }}".to_string()),
    );
    apps.insert(
        "myapp".to_string(),
        SnapcraftApp {
            command: Some("myapp".to_string()),
            passthrough: Some(passthrough),
            ..Default::default()
        },
    );
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        apps: Some(apps),
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
    let tmp = TempDir::new().unwrap();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            strict: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts
        .add(linux_bin("myapp", "x86_64-unknown-linux-gnu"));

    let err = crate::snapcraft_snap_yamls_for_crate(&ctx, "myapp").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("snapcraft.yaml") && msg.contains("unrendered template delimiter"),
        "guard error should name the manifest + residual, got: {msg}"
    );
}

/// App `command` / `args` are user-templatable (GoReleaser renders them); a
/// `{{ .Version }}` in either must resolve in the emitted snap.yaml, never
/// ship the literal delimiters.
#[test]
fn test_app_command_and_args_are_template_rendered() {
    let mut apps = BTreeMap::new();
    apps.insert(
        "myapp".to_string(),
        SnapcraftApp {
            command: Some("myapp-{{ .Version }}".to_string()),
            args: Some("--tag {{ .Version }}".to_string()),
            ..Default::default()
        },
    );
    let snap_cfg = SnapcraftConfig {
        name: Some("mysnap".to_string()),
        summary: Some("Test snap".to_string()),
        description: Some("A test snap package".to_string()),
        apps: Some(apps),
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
    let tmp = TempDir::new().unwrap();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(config, ContextOptions::default());
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.artifacts
        .add(linux_bin("myapp", "x86_64-unknown-linux-gnu"));

    let yamls = crate::snapcraft_snap_yamls_for_crate(&ctx, "myapp").unwrap();
    let yaml = &yamls[0];
    assert!(
        yaml.contains("myapp-1.0.0") && yaml.contains("--tag 1.0.0"),
        "command/args should be rendered, got: {yaml}"
    );
    assert!(!yaml.contains("{{"), "no residual delimiters, got: {yaml}");
}

/// A clean manifest (every templated field resolves) passes the chokepoint
/// guard even under strict mode — the guard must not false-positive on the
/// emitted YAML or `$SNAP` shell vars.
#[test]
fn test_guard_passes_clean_manifest_in_strict_mode() {
    let snap_cfg = SnapcraftConfig {
        name: Some("app-{{ .Tag }}".to_string()),
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
    let tmp = TempDir::new().unwrap();
    config.dist = tmp.path().join("dist");
    config.crates = vec![crate_cfg];

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            strict: true,
            ..Default::default()
        },
    );
    ctx.template_vars_mut().set("Version", "1.0.0");
    ctx.template_vars_mut().set("Tag", "v1.2.3");
    ctx.artifacts
        .add(linux_bin("myapp", "x86_64-unknown-linux-gnu"));

    let yamls = crate::snapcraft_snap_yamls_for_crate(&ctx, "myapp").unwrap();
    assert_eq!(yamls.len(), 1);
    assert!(yamls[0].contains("name: app-v1.2.3"));
}
