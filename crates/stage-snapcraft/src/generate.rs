use std::collections::BTreeMap;

use anyhow::{Context as _, Result};

use anodizer_core::config::SnapcraftConfig;

use crate::arch::triple_to_snap_arch;
use crate::yaml::{SnapcraftYaml, SnapcraftYamlApp, SnapcraftYamlLayout};

// ---------------------------------------------------------------------------
// generate_snap_yaml
// ---------------------------------------------------------------------------

/// Generate a snap.yaml metadata string from the anodizer snapcraft config.
///
/// this generates the `snap.yaml` file that goes into
/// `prime/meta/snap.yaml` — it is *not* a `snapcraft.yaml` build recipe.
/// Binaries and extra files are staged into the `prime/` directory by the
/// caller; this function only produces the metadata.
///
/// `binary_names` is the list of binary filenames to include in this snap.
/// The first binary is used as the default app name when no apps are configured.
/// `target` is the optional target triple, used to set the architectures field.
pub fn generate_snap_yaml(
    config: &SnapcraftConfig,
    version: &str,
    binary_names: &[&str],
    target: Option<&str>,
    project_name: Option<&str>,
) -> Result<String> {
    let primary_binary = binary_names.first().copied().unwrap_or("binary");
    // GoReleaser defaults snap name to ctx.Config.ProjectName, not binary name.
    let name = config
        .name
        .clone()
        .unwrap_or_else(|| project_name.unwrap_or(primary_binary).to_string());
    // summary and description are
    // required fields; error instead of silently defaulting.
    let summary = config
        .summary
        .clone()
        .ok_or_else(|| anyhow::anyhow!("snapcraft: summary is required for snap '{}'", name))?;
    let description = config
        .description
        .clone()
        .ok_or_else(|| anyhow::anyhow!("snapcraft: description is required for snap '{}'", name))?;
    // Do NOT default `base:`; only emit it when the user supplied one.
    // Forcing `base: core22` breaks classic-confinement snaps (which want
    // no base at all) and modern snaps that need `core24`.
    let base = config.base.clone().filter(|s| !s.is_empty());
    let confinement = config
        .confinement
        .clone()
        .unwrap_or_else(|| "strict".to_string());

    // Build apps section — if args is set, append it to command.
    // When no apps are configured, generate a default app entry using the
    // first binary's name (like GoReleaser does).
    let apps: BTreeMap<String, SnapcraftYamlApp> = if let Some(app_map) = config.apps.as_ref()
        && !app_map.is_empty()
    {
        app_map
            .iter()
            .map(|(app_name, app_cfg)| {
                let command = match (&app_cfg.command, &app_cfg.args) {
                    (Some(cmd), Some(args)) => Some(format!("{cmd} {args}")),
                    (cmd, _) => cmd.clone(),
                };
                let yaml_app = SnapcraftYamlApp {
                    command,
                    daemon: app_cfg.daemon.clone(),
                    stop_mode: app_cfg.stop_mode.clone(),
                    restart_condition: app_cfg.restart_condition.clone(),
                    plugs: app_cfg.plugs.clone().unwrap_or_default(),
                    environment: app_cfg.environment.clone().unwrap_or_default(),
                    adapter: app_cfg.adapter.clone(),
                    after: app_cfg.after.clone().unwrap_or_default(),
                    aliases: app_cfg.aliases.clone().unwrap_or_default(),
                    autostart: app_cfg.autostart.clone(),
                    before: app_cfg.before.clone().unwrap_or_default(),
                    bus_name: app_cfg.bus_name.clone(),
                    command_chain: app_cfg.command_chain.clone().unwrap_or_default(),
                    common_id: app_cfg.common_id.clone(),
                    completer: app_cfg.completer.clone(),
                    desktop: app_cfg.desktop.clone(),
                    extensions: app_cfg.extensions.clone().unwrap_or_default(),
                    install_mode: app_cfg.install_mode.clone(),
                    passthrough: app_cfg.passthrough.clone().unwrap_or_default(),
                    post_stop_command: app_cfg.post_stop_command.clone(),
                    refresh_mode: app_cfg.refresh_mode.clone(),
                    reload_command: app_cfg.reload_command.clone(),
                    restart_delay: app_cfg.restart_delay.clone(),
                    slots: app_cfg.slots.clone().unwrap_or_default(),
                    sockets: app_cfg.sockets.clone().unwrap_or_default(),
                    start_timeout: app_cfg.start_timeout.clone(),
                    stop_command: app_cfg.stop_command.clone(),
                    stop_timeout: app_cfg.stop_timeout.clone(),
                    timer: app_cfg.timer.clone(),
                    watchdog_timeout: app_cfg.watchdog_timeout.clone(),
                };
                (app_name.clone(), yaml_app)
            })
            .collect()
    } else {
        // when no apps are configured,
        // use snap.Name as the app name key (falling back to the binary filename).
        // The command is always the binary basename — binaries sit at the prime root.
        let default_app_name = config
            .name
            .as_deref()
            .filter(|n| !n.is_empty())
            .unwrap_or(primary_binary);
        let mut default_apps = BTreeMap::new();
        default_apps.insert(
            default_app_name.to_string(),
            SnapcraftYamlApp {
                command: Some(primary_binary.to_string()),
                ..Default::default()
            },
        );
        default_apps
    };

    // Build layouts section
    let layouts: BTreeMap<String, SnapcraftYamlLayout> = config
        .layouts
        .as_ref()
        .map(|layout_map| {
            layout_map
                .iter()
                .map(|(path, layout_cfg)| {
                    let yaml_layout = SnapcraftYamlLayout {
                        bind: layout_cfg.bind.clone(),
                        bind_file: layout_cfg.bind_file.clone(),
                        symlink: layout_cfg.symlink.clone(),
                        type_: layout_cfg.type_.clone(),
                    };
                    (path.clone(), yaml_layout)
                })
                .collect()
        })
        .unwrap_or_default();

    // Map target triple to snapcraft architecture name
    let architectures: Vec<String> = if let Some(triple) = target {
        let snap_arch = triple_to_snap_arch(triple);
        vec![snap_arch.to_string()]
    } else {
        Vec::new()
    };

    // assumes, hooks, and plugs are
    // populated inside the `for name, config := range snap.Apps` loop. When the
    // apps map is empty, those fields remain zero-valued and `omitempty` drops
    // them from the emitted YAML. Mirror that here.
    let has_apps = config.apps.as_ref().map(|m| !m.is_empty()).unwrap_or(false);

    let yaml_model = SnapcraftYaml {
        name,
        version: version.to_string(),
        summary,
        description,
        base,
        grade: Some(config.grade.clone().unwrap_or_else(|| "stable".to_string())),
        confinement,
        license: config.license.clone(),
        title: config.title.clone(),
        icon: config.icon.clone(),
        assumes: if has_apps {
            config.assumes.clone().unwrap_or_default()
        } else {
            Vec::new()
        },
        architectures,
        apps,
        plugs: if has_apps {
            config.plugs.clone().unwrap_or_default()
        } else {
            BTreeMap::new()
        },
        // Snapcraft has no top-level `slots:` concept; app-scoped slots live
        // under `apps.<name>.slots` and are emitted via the apps walker above.
        layouts,
        hooks: if has_apps {
            config.hooks.clone().unwrap_or_default()
        } else {
            BTreeMap::new()
        },
    };

    let yaml = serde_yaml_ng::to_string(&yaml_model).context("serialize snapcraft YAML")?;
    Ok(yaml.trim_end().to_string())
}
