use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};
use serde::Serialize;

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::config::{SnapcraftConfig, SnapcraftExtraFileSpec};
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Serde-serializable snapcraft YAML model
// ---------------------------------------------------------------------------

fn is_empty_vec<T>(v: &[T]) -> bool {
    v.is_empty()
}

#[derive(Serialize)]
struct SnapcraftYaml {
    name: String,
    version: String,
    summary: String,
    description: String,
    base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    grade: Option<String>,
    confinement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    assumes: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    architectures: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    apps: HashMap<String, SnapcraftYamlApp>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    plugs: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    slots: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    layouts: HashMap<String, SnapcraftYamlLayout>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    hooks: HashMap<String, serde_json::Value>,
    parts: HashMap<String, SnapcraftYamlPart>,
}

#[derive(Serialize)]
struct SnapcraftYamlApp {
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop-mode")]
    stop_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "restart-condition")]
    restart_condition: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    plugs: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    environment: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    adapter: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    after: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    autostart: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    before: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "bus-name")]
    bus_name: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec", rename = "command-chain")]
    command_chain: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "common-id")]
    common_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    desktop: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    extensions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "install-mode")]
    install_mode: Option<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    passthrough: HashMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "post-stop-command")]
    post_stop_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "refresh-mode")]
    refresh_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "reload-command")]
    reload_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "restart-delay")]
    restart_delay: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    slots: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    sockets: HashMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "start-timeout")]
    start_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop-command")]
    stop_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop-timeout")]
    stop_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "watchdog-timeout")]
    watchdog_timeout: Option<String>,
}

#[derive(Serialize)]
struct SnapcraftYamlLayout {
    #[serde(skip_serializing_if = "Option::is_none")]
    bind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "bind-file")]
    bind_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    symlink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    type_: Option<String>,
}

#[derive(Serialize)]
struct SnapcraftYamlPart {
    plugin: String,
    source: String,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    organize: HashMap<String, String>,
    #[serde(rename = "stage")]
    #[serde(skip_serializing_if = "is_empty_vec")]
    stage_files: Vec<String>,
    #[serde(rename = "prime")]
    #[serde(skip_serializing_if = "is_empty_vec")]
    prime_files: Vec<String>,
}

// ---------------------------------------------------------------------------
// triple_to_snap_arch — map target triple to snapcraft architecture name
// ---------------------------------------------------------------------------

/// Map a Rust target triple to a snapcraft architecture name.
fn triple_to_snap_arch(triple: &str) -> &'static str {
    if triple.contains("x86_64") || triple.contains("amd64") {
        "amd64"
    } else if triple.contains("aarch64") || triple.contains("arm64") {
        "arm64"
    } else if triple.contains("armv7") {
        "armhf"
    } else if triple.contains("i686") || triple.contains("i386") || triple.contains("i586") {
        "i386"
    } else if triple.contains("s390x") {
        "s390x"
    } else if triple.contains("ppc64le") || triple.contains("powerpc64le") {
        "ppc64el"
    } else if triple.contains("riscv64") {
        "riscv64"
    } else {
        "amd64"
    }
}

// ---------------------------------------------------------------------------
// generate_snapcraft_yaml
// ---------------------------------------------------------------------------

/// Generate a snapcraft.yaml string from the anodize snapcraft config.
///
/// `binary_names` is the list of binary filenames to include in this snap.
/// The first binary is used as the default app name when no apps are configured.
/// `target` is the optional target triple, used to set the architectures field.
pub fn generate_snapcraft_yaml(
    config: &SnapcraftConfig,
    version: &str,
    binary_names: &[&str],
    extra_files: Option<&[SnapcraftExtraFileSpec]>,
    target: Option<&str>,
) -> Result<String> {
    let primary_binary = binary_names.first().copied().unwrap_or("binary");
    let name = config
        .name
        .clone()
        .unwrap_or_else(|| primary_binary.to_string());
    let summary = config
        .summary
        .clone()
        .unwrap_or_else(|| format!("{name} snap package"));
    let description = config
        .description
        .clone()
        .unwrap_or_else(|| format!("{name} ��� built with anodize"));
    let base = config.base.clone().unwrap_or_else(|| "core22".to_string());
    let confinement = config
        .confinement
        .clone()
        .unwrap_or_else(|| "strict".to_string());

    // Build apps section — if args is set, append it to command.
    // When no apps are configured, generate a default app entry using the
    // first binary's name (like GoReleaser does).
    let apps: HashMap<String, SnapcraftYamlApp> = if let Some(app_map) = config.apps.as_ref()
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
        // Default app entry: use primary binary name as both app name and command
        let mut default_apps = HashMap::new();
        default_apps.insert(
            primary_binary.to_string(),
            SnapcraftYamlApp {
                command: Some(format!("bin/{primary_binary}")),
                daemon: None,
                stop_mode: None,
                restart_condition: None,
                plugs: Vec::new(),
                environment: HashMap::new(),
                adapter: None,
                after: Vec::new(),
                aliases: Vec::new(),
                autostart: None,
                before: Vec::new(),
                bus_name: None,
                command_chain: Vec::new(),
                common_id: None,
                completer: None,
                desktop: None,
                extensions: Vec::new(),
                install_mode: None,
                passthrough: HashMap::new(),
                post_stop_command: None,
                refresh_mode: None,
                reload_command: None,
                restart_delay: None,
                slots: Vec::new(),
                sockets: HashMap::new(),
                start_timeout: None,
                stop_command: None,
                stop_timeout: None,
                timer: None,
                watchdog_timeout: None,
            },
        );
        default_apps
    };

    // Build layouts section
    let layouts: HashMap<String, SnapcraftYamlLayout> = config
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

    // Build parts section — a single "binary" part that copies all binaries in
    let mut organize = HashMap::new();
    let mut stage_files = Vec::new();
    let mut prime_files = Vec::new();
    for bin_name in binary_names {
        organize.insert(bin_name.to_string(), format!("bin/{bin_name}"));
        stage_files.push(format!("bin/{bin_name}"));
        prime_files.push(format!("bin/{bin_name}"));
    }

    // Include extra files in organize/stage/prime
    if let Some(files) = extra_files {
        for file in files {
            let source = file.source();
            let source_filename = std::path::Path::new(source)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(source);
            // If destination is set, use it; otherwise use the source filename
            let dest = file
                .destination()
                .unwrap_or(source_filename);
            organize.insert(source_filename.to_string(), dest.to_string());
            stage_files.push(dest.to_string());
            prime_files.push(dest.to_string());
        }
    }

    let mut parts = HashMap::new();
    parts.insert(
        "binary".to_string(),
        SnapcraftYamlPart {
            plugin: "dump".to_string(),
            source: ".".to_string(),
            organize,
            stage_files,
            prime_files,
        },
    );

    // Map target triple to snapcraft architecture name
    let architectures: Vec<String> = if let Some(triple) = target {
        let snap_arch = triple_to_snap_arch(triple);
        vec![snap_arch.to_string()]
    } else {
        Vec::new()
    };

    let yaml_model = SnapcraftYaml {
        name,
        version: version.to_string(),
        summary,
        description,
        base,
        grade: config.grade.clone(),
        confinement,
        license: config.license.clone(),
        title: config.title.clone(),
        icon: config.icon.clone(),
        assumes: config.assumes.clone().unwrap_or_default(),
        architectures,
        apps,
        plugs: config.plugs.clone().unwrap_or_default(),
        slots: config.slots.clone().unwrap_or_default(),
        layouts,
        hooks: config.hooks.clone().unwrap_or_default(),
        parts,
    };

    let yaml = serde_yaml_ng::to_string(&yaml_model).context("serialize snapcraft YAML")?;
    Ok(yaml.trim_end().to_string())
}

// ---------------------------------------------------------------------------
// snapcraft_command
// ---------------------------------------------------------------------------

/// Construct the snapcraft pack CLI command arguments.
/// This only builds the snap; publishing is handled separately via
/// `snapcraft_upload_command`.
pub fn snapcraft_command(output_path: &str) -> Vec<String> {
    vec![
        "snapcraft".to_string(),
        "pack".to_string(),
        "--destructive-mode".to_string(),
        "--output".to_string(),
        output_path.to_string(),
    ]
}

/// Construct the snapcraft upload CLI command arguments.
/// When `channels` is non-empty, adds `--release=<comma-separated channels>`.
pub fn snapcraft_upload_command(snap_path: &str, channels: Option<&[String]>) -> Vec<String> {
    let mut args = vec![
        "snapcraft".to_string(),
        "upload".to_string(),
        snap_path.to_string(),
    ];

    if let Some(ch) = channels {
        let non_empty: Vec<&String> = ch.iter().filter(|c| !c.is_empty()).collect();
        if !non_empty.is_empty() {
            let joined: Vec<&str> = non_empty.iter().map(|s| s.as_str()).collect();
            args.push(format!("--release={}", joined.join(",")));
        }
    }

    args
}

// ---------------------------------------------------------------------------
// SnapcraftStage
// ---------------------------------------------------------------------------

pub struct SnapcraftStage;

impl Stage for SnapcraftStage {
    fn name(&self) -> &str {
        "snapcraft"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("snapcraft");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have snapcraft config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.snapcrafts.is_some())
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
        let mut archives_to_remove: Vec<PathBuf> = Vec::new();

        for krate in &crates {
            let snap_configs = krate.snapcrafts.as_ref().unwrap();

            // Collect all Linux binary artifacts for this crate
            let linux_binaries: Vec<_> = ctx
                .artifacts
                .by_kind_and_crate(ArtifactKind::Binary, &krate.name)
                .into_iter()
                .filter(|b| {
                    b.target
                        .as_deref()
                        .map(anodize_core::target::is_linux)
                        .unwrap_or(false)
                })
                .cloned()
                .collect();

            for snap_cfg in snap_configs {
                // Skip disabled configs
                if let Some(ref d) = snap_cfg.disable {
                    if d.is_disabled(|tmpl| ctx.render_template(tmpl)) {
                        log.status(&format!(
                            "skipping disabled snapcraft config for crate {}",
                            krate.name
                        ));
                        continue;
                    }
                }

                // Validate confinement value
                if let Some(conf) = &snap_cfg.confinement {
                    match conf.as_str() {
                        "strict" | "devmode" | "classic" => {}
                        other => anyhow::bail!(
                            "snapcraft: invalid confinement '{}' for crate '{}'. \
                             Valid values are: strict, devmode, classic",
                            other,
                            krate.name
                        ),
                    }
                }

                // Validate grade value
                if let Some(grade) = &snap_cfg.grade {
                    match grade.as_str() {
                        "stable" | "devel" => {}
                        other => anyhow::bail!(
                            "snapcraft: invalid grade '{}' for crate '{}'. \
                             Valid values are: stable, devel",
                            other,
                            krate.name
                        ),
                    }
                }

                // Filter binaries by ids if configured (C2)
                let mut filtered_binaries = linux_binaries.clone();
                if let Some(ref filter_ids) = snap_cfg.ids
                    && !filter_ids.is_empty()
                {
                    filtered_binaries.retain(|b| {
                        b.metadata
                            .get("id")
                            .map(|id| filter_ids.contains(id))
                            .unwrap_or(false)
                            || b.metadata
                                .get("name")
                                .map(|n| filter_ids.contains(n))
                                .unwrap_or(false)
                    });
                }

                // Warn and skip if no linux binaries found
                if filtered_binaries.is_empty() && linux_binaries.is_empty() {
                    log.warn(&format!(
                        "no Linux binaries found for crate '{}'; skipping snapcraft",
                        krate.name
                    ));
                    continue;
                }
                if filtered_binaries.is_empty() {
                    log.warn(&format!(
                        "ids filter {:?} matched no binaries for crate '{}'; skipping",
                        snap_cfg.ids, krate.name
                    ));
                    continue;
                }

                // Group binaries by target triple (platform) — one snap per platform
                let mut by_target: HashMap<String, Vec<&Artifact>> = HashMap::new();
                for b in &filtered_binaries {
                    let target = b.target.clone().unwrap_or_else(|| "unknown".to_string());
                    by_target.entry(target).or_default().push(b);
                }

                for (target_key, target_binaries) in &by_target {
                    let target = if target_key == "unknown" {
                        None
                    } else {
                        Some(target_key.clone())
                    };
                    // Derive Os/Arch from the target triple for template rendering
                    let (os, arch) = target
                        .as_deref()
                        .map(anodize_core::target::map_target)
                        .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()));

                    // Ensure output directory exists
                    let output_dir = dist.join("linux");
                    if !dry_run {
                        fs::create_dir_all(&output_dir).with_context(|| {
                            format!("create snapcraft output dir: {}", output_dir.display())
                        })?;
                    }

                    // Determine output filename from name_template or default
                    let snap_name = snap_cfg.name.as_deref().unwrap_or(&krate.name);
                    let snap_filename = if let Some(tmpl) = &snap_cfg.name_template {
                        // Set Os/Arch/Target in template vars temporarily
                        ctx.template_vars_mut().set("Os", &os);
                        ctx.template_vars_mut().set("Arch", &arch);
                        ctx.template_vars_mut()
                            .set("Target", target.as_deref().unwrap_or(""));
                        let rendered = ctx.render_template(tmpl).with_context(|| {
                            format!(
                                "snapcraft: render name_template for crate {} target {:?}",
                                krate.name, target
                            )
                        })?;
                        if rendered.to_lowercase().ends_with(".snap") {
                            rendered
                        } else {
                            format!("{rendered}.snap")
                        }
                    } else {
                        format!("{snap_name}_{version}_{arch}.snap")
                    };
                    let snap_path = output_dir.join(&snap_filename);

                    // Build artifact metadata (I4)
                    let artifact_metadata = {
                        let mut m = HashMap::new();
                        if let Some(id) = &snap_cfg.id {
                            m.insert("id".to_string(), id.clone());
                        }
                        m
                    };

                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would run: snapcraft pack --output {} for crate {} target {:?}",
                            snap_path.display(),
                            krate.name,
                            target,
                        ));
                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::Snap,
                            name: String::new(),
                            path: snap_path,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            metadata: artifact_metadata,
                            size: None,
                        });

                        // If replace is set, mark archives for this crate+target for removal
                        if snap_cfg.replace.unwrap_or(false) {
                            archives_to_remove.extend(
                                anodize_core::util::collect_replace_archives(
                                    &ctx.artifacts,
                                    &krate.name,
                                    target.as_deref(),
                                ),
                            );
                        }

                        continue;
                    }

                    // Create temp directory for snapcraft build
                    let tmp_dir =
                        tempfile::tempdir().context("create temp dir for snapcraft build")?;
                    let snap_dir = tmp_dir.path().join("snap");
                    fs::create_dir_all(&snap_dir)
                        .with_context(|| format!("create snap dir: {}", snap_dir.display()))?;

                    // Collect all binary names for this platform group
                    let all_binary_names: Vec<String> = target_binaries
                        .iter()
                        .map(|b| {
                            b.path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("binary")
                                .to_string()
                        })
                        .collect();
                    let binary_name_refs: Vec<&str> =
                        all_binary_names.iter().map(|s| s.as_str()).collect();

                    // Generate and write snapcraft.yaml
                    let yaml_content = generate_snapcraft_yaml(
                        snap_cfg,
                        &version,
                        &binary_name_refs,
                        snap_cfg.extra_files.as_deref(),
                        target.as_deref(),
                    )?;
                    let yaml_path = snap_dir.join("snapcraft.yaml");
                    fs::write(&yaml_path, &yaml_content).with_context(|| {
                        format!("write snapcraft.yaml to {}", yaml_path.display())
                    })?;

                    // Copy all binaries for this platform into temp directory
                    for bin_artifact in target_binaries {
                        let bin_name = bin_artifact
                            .path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("binary");
                        let binary_dest = tmp_dir.path().join(bin_name);
                        let bin_path_str = bin_artifact.path.to_string_lossy();
                        fs::copy(&bin_artifact.path, &binary_dest).with_context(|| {
                            format!("copy binary {} to {}", bin_path_str, binary_dest.display())
                        })?;
                    }

                    // Copy extra files into temp directory
                    if let Some(extra_files) = &snap_cfg.extra_files {
                        for extra in extra_files {
                            let src = PathBuf::from(extra.source());
                            let file_name =
                                src.file_name().and_then(|n| n.to_str()).unwrap_or("extra");
                            let dest = tmp_dir.path().join(file_name);
                            fs::copy(&src, &dest).with_context(|| {
                                format!("copy extra file {} to {}", src.display(), dest.display())
                            })?;
                            // Apply file mode if specified
                            #[cfg(unix)]
                            if let Some(mode) = extra.mode() {
                                use std::os::unix::fs::PermissionsExt;
                                let perms = std::fs::Permissions::from_mode(mode);
                                std::fs::set_permissions(&dest, perms).with_context(|| {
                                    format!("set mode {:o} on {}", mode, dest.display())
                                })?;
                            }
                        }
                    }

                    // Apply mod_timestamp if set
                    if let Some(ts) = &snap_cfg.mod_timestamp {
                        anodize_core::util::apply_mod_timestamp(tmp_dir.path(), ts, &log)?;
                    }

                    // Run snapcraft pack
                    let cmd_args = snapcraft_command(&snap_path.to_string_lossy());

                    log.status(&format!("running: {}", cmd_args.join(" ")));

                    let output = Command::new(&cmd_args[0])
                        .args(&cmd_args[1..])
                        .current_dir(tmp_dir.path())
                        .output()
                        .with_context(|| {
                            format!(
                                "execute snapcraft for crate {} target {:?}",
                                krate.name, target
                            )
                        })?;
                    log.check_output(output, "snapcraft pack")?;

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::Snap,
                        name: String::new(),
                        path: snap_path,
                        target: target.clone(),
                        crate_name: krate.name.clone(),
                        metadata: artifact_metadata,
                        size: None,
                    });

                    // If replace is set, mark archives for this crate+target for removal
                    if snap_cfg.replace.unwrap_or(false) {
                        archives_to_remove.extend(anodize_core::util::collect_replace_archives(
                            &ctx.artifacts,
                            &krate.name,
                            target.as_deref(),
                        ));
                    }
                }
            }
        }

        // Clear per-target template vars so they don't leak to downstream stages.
        ctx.template_vars_mut().set("Os", "");
        ctx.template_vars_mut().set("Arch", "");
        ctx.template_vars_mut().set("Target", "");

        // Remove replaced archives
        if !archives_to_remove.is_empty() {
            ctx.artifacts.remove_by_paths(&archives_to_remove);
        }

        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SnapcraftPublishStage — uploads previously built .snap artifacts
// ---------------------------------------------------------------------------

pub struct SnapcraftPublishStage;

impl Stage for SnapcraftPublishStage {
    fn name(&self) -> &str {
        "snapcraft-publish"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("snapcraft-publish");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;

        // Collect crates that have snapcraft config with publish: true
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.snapcrafts.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // Collect all snap artifacts that were built
        let snap_artifacts: Vec<_> = ctx
            .artifacts
            .by_kind(ArtifactKind::Snap)
            .into_iter()
            .cloned()
            .collect();

        if snap_artifacts.is_empty() {
            return Ok(());
        }

        for krate in &crates {
            let snap_configs = krate.snapcrafts.as_ref().unwrap();

            for snap_cfg in snap_configs {
                // Only publish configs that opt in
                if !snap_cfg.publish.unwrap_or(false) {
                    continue;
                }
                // Skip disabled configs
                if let Some(ref d) = snap_cfg.disable {
                    if d.is_disabled(|tmpl| ctx.render_template(tmpl)) {
                        continue;
                    }
                }

                // Find snap artifacts for this crate (optionally filtered by id)
                let matching: Vec<_> = snap_artifacts
                    .iter()
                    .filter(|a| a.crate_name == krate.name)
                    .filter(|a| {
                        if let Some(ref filter_id) = snap_cfg.id {
                            a.metadata
                                .get("id")
                                .map(|id| id == filter_id)
                                .unwrap_or(false)
                        } else {
                            true
                        }
                    })
                    .collect();

                for artifact in &matching {
                    let snap_path = artifact.path.to_string_lossy();

                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would run: snapcraft upload {}",
                            snap_path,
                        ));
                        continue;
                    }

                    let upload_args =
                        snapcraft_upload_command(&snap_path, snap_cfg.channel_templates.as_deref());
                    log.status(&format!("running: {}", upload_args.join(" ")));
                    let upload_output = Command::new(&upload_args[0])
                        .args(&upload_args[1..])
                        .output()
                        .with_context(|| {
                            format!(
                                "execute snapcraft upload for crate {} snap {}",
                                krate.name, snap_path
                            )
                        })?;

                    // Review-pending responses from the Snap Store should be
                    // warnings, not fatal errors — the snap was uploaded
                    // successfully but needs human review.
                    if !upload_output.status.success() {
                        const REVIEW_PENDING_STRINGS: &[&str] = &[
                            "Waiting for previous upload",
                            "A human will soon review your snap",
                            "(NEEDS REVIEW)",
                        ];

                        let stderr = String::from_utf8_lossy(&upload_output.stderr);
                        let stdout = String::from_utf8_lossy(&upload_output.stdout);
                        let combined = format!("{}{}", stdout, stderr);
                        if REVIEW_PENDING_STRINGS.iter().any(|s| combined.contains(s)) {
                            log.warn(&format!("snap upload pending review: {}", combined.trim()));
                        } else {
                            log.check_output(upload_output, "snapcraft upload")?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use anodize_core::config::{
        Config, CrateConfig, SnapcraftApp, SnapcraftConfig, SnapcraftExtraFileSpec,
        SnapcraftLayout, StringOrBool,
    };
    use anodize_core::context::{Context, ContextOptions};
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // generate_snapcraft_yaml tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_snapcraft_yaml_basic() {
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
        let yaml = generate_snapcraft_yaml(&cfg, "1.2.3", &["myapp"], None, None).unwrap();
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
        // Verify parts section with binary
        assert!(yaml.contains("parts:"), "missing parts");
        assert!(yaml.contains("binary:"), "missing binary part");
        assert!(yaml.contains("plugin: dump"), "missing dump plugin");
    }

    #[test]
    fn test_generate_snapcraft_yaml_with_apps() {
        let mut apps = HashMap::new();
        apps.insert(
            "myapp".to_string(),
            SnapcraftApp {
                command: Some("bin/myapp".to_string()),
                daemon: Some("simple".to_string()),
                stop_mode: Some("sigterm".to_string()),
                restart_condition: Some("on-failure".to_string()),
                plugs: Some(vec!["network".to_string(), "home".to_string()]),
                environment: Some(HashMap::from([("LANG".to_string(), "C.UTF-8".to_string())])),
                args: Some("--verbose".to_string()),
                ..Default::default()
            },
        );

        let cfg = SnapcraftConfig {
            name: Some("mysnap".to_string()),
            apps: Some(apps),
            ..Default::default()
        };
        let yaml = generate_snapcraft_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
        assert!(yaml.contains("apps:"), "missing apps section");
        assert!(yaml.contains("myapp:"), "missing app name");
        // S4: args should be appended to command, not a separate field
        assert!(
            yaml.contains("command: bin/myapp --verbose"),
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
    fn test_generate_snapcraft_yaml_with_plugs_slots() {
        let cfg = SnapcraftConfig {
            name: Some("mysnap".to_string()),
            plugs: Some(vec![
                "network".to_string(),
                "home".to_string(),
                "personal-files".to_string(),
            ]),
            slots: Some(vec!["dbus-slot".to_string()]),
            ..Default::default()
        };
        let yaml = generate_snapcraft_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
        assert!(yaml.contains("plugs:"), "missing plugs section");
        assert!(yaml.contains("- network"), "missing network plug");
        assert!(yaml.contains("- home"), "missing home plug");
        assert!(
            yaml.contains("- personal-files"),
            "missing personal-files plug"
        );
        assert!(yaml.contains("slots:"), "missing slots section");
        assert!(yaml.contains("- dbus-slot"), "missing dbus-slot slot");
    }

    #[test]
    fn test_generate_snapcraft_yaml_with_layouts() {
        let mut layouts = HashMap::new();
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
            ..Default::default()
        };
        let yaml = generate_snapcraft_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
        assert!(yaml.contains("layouts:"), "missing layouts section");
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
                ..Default::default()
            };
            let yaml = generate_snapcraft_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
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
            ..Default::default()
        };
        let yaml = generate_snapcraft_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
        // Default base should be core22
        assert!(yaml.contains("base: core22"), "default base not core22");
        // Default confinement should be strict
        assert!(
            yaml.contains("confinement: strict"),
            "default confinement not strict"
        );
    }

    #[test]
    fn test_generate_snapcraft_yaml_minimal() {
        // Completely default config — only binary_name is used
        let cfg = SnapcraftConfig::default();
        let yaml = generate_snapcraft_yaml(&cfg, "0.1.0", &["mytool"], None, None).unwrap();
        // Name falls back to binary_name
        assert!(yaml.contains("name: mytool"), "missing fallback name");
        assert!(yaml.contains("version: 0.1.0"), "missing version");
        assert!(yaml.contains("base: core22"), "missing default base");
        assert!(
            yaml.contains("confinement: strict"),
            "missing default confinement"
        );
        assert!(yaml.contains("parts:"), "missing parts");
        assert!(yaml.contains("plugin: dump"), "missing dump plugin");
        // Summary and description should be auto-generated
        assert!(yaml.contains("summary:"), "missing summary");
        assert!(yaml.contains("description:"), "missing description");
    }

    // -----------------------------------------------------------------------
    // snapcraft_command tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_snapcraft_command_basic() {
        let cmd = snapcraft_command("/tmp/output/mysnap_1.0.0_amd64.snap");
        assert_eq!(cmd[0], "snapcraft");
        assert_eq!(cmd[1], "pack");
        assert_eq!(cmd[2], "--destructive-mode");
        assert_eq!(cmd[3], "--output");
        assert_eq!(cmd[4], "/tmp/output/mysnap_1.0.0_amd64.snap");
        assert_eq!(cmd.len(), 5);
    }

    #[test]
    fn test_snapcraft_command_no_publish_flag() {
        // snapcraft pack should never contain --publish
        let cmd = snapcraft_command("/tmp/out.snap");
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
            disable: Some(StringOrBool::Bool(true)),
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

        // Default filename: {name}_{version}_{arch}.snap
        let path_str = snaps[0].path.to_string_lossy();
        assert!(
            path_str.ends_with("mysnap_1.0.0_amd64.snap"),
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
            path_str.ends_with("myapp_2.0.0_linux_amd64.snap"),
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
    fn test_generate_yaml_with_extra_files() {
        let cfg = SnapcraftConfig {
            name: Some("mysnap".to_string()),
            ..Default::default()
        };
        let extra = vec![
            SnapcraftExtraFileSpec::Source("README.md".to_string()),
            SnapcraftExtraFileSpec::Source("config/defaults.yaml".to_string()),
        ];
        let yaml = generate_snapcraft_yaml(&cfg, "1.0.0", &["myapp"], Some(&extra), None).unwrap();
        // Extra files should appear in the organize mapping
        assert!(
            yaml.contains("README.md"),
            "extra file README.md should be in yaml"
        );
        assert!(
            yaml.contains("defaults.yaml"),
            "extra file defaults.yaml should be in yaml"
        );
        // The binary should still be there
        assert!(yaml.contains("bin/myapp"), "binary should be in yaml");
    }

    #[test]
    fn test_artifact_metadata_includes_id() {
        let tmp = TempDir::new().unwrap();

        let snap_cfg = SnapcraftConfig {
            name: Some("mysnap".to_string()),
            id: Some("main-snap".to_string()),
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
        let config: Config =
            serde_yaml_ng::from_str(yaml).expect("failed to parse snapcraft config");
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
          - network
          - home
        slots:
          - dbus-svc
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
        disable: false
        replace: true
        mod_timestamp: "1704067200"
"#;
        let config: Config =
            serde_yaml_ng::from_str(yaml).expect("failed to parse full snapcraft config");
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
        assert_eq!(snap.plugs.as_ref().unwrap(), &["network", "home"]);
        assert_eq!(snap.slots.as_ref().unwrap(), &["dbus-svc"]);
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
            "C.UTF-8"
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
        assert_eq!(snap.disable, Some(StringOrBool::Bool(false)));
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
            ..Default::default()
        };
        let snap_cfg_classic = SnapcraftConfig {
            name: Some("mysnap-classic".to_string()),
            confinement: Some("classic".to_string()),
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
            disable: Some(StringOrBool::Bool(true)),
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
        let mut apps = HashMap::new();
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
                passthrough: Some(HashMap::from([
                    ("custom-key".to_string(), serde_json::json!("custom-value")),
                ])),
                post_stop_command: Some("bin/cleanup".to_string()),
                refresh_mode: Some("endure".to_string()),
                reload_command: Some("bin/reload".to_string()),
                restart_condition: Some("on-failure".to_string()),
                restart_delay: Some("10s".to_string()),
                slots: Some(vec!["dbus-slot".to_string()]),
                sockets: Some(HashMap::from([
                    (
                        "mysock".to_string(),
                        serde_json::json!({"listen-stream": "$SNAP_DATA/mysock.sock"}),
                    ),
                ])),
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
            ..Default::default()
        };
        let yaml = generate_snapcraft_yaml(&cfg, "2.0.0", &["mydaemon"], None, None).unwrap();

        // Verify all kebab-case fields are present
        assert!(yaml.contains("adapter: none"), "missing adapter\n{yaml}");
        assert!(yaml.contains("- network-manager"), "missing after entry\n{yaml}");
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
        assert!(yaml.contains("command-chain:"), "missing command-chain\n{yaml}");
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
        let mut hooks = HashMap::new();
        hooks.insert(
            "configure".to_string(),
            serde_json::json!({"plugs": ["network"]}),
        );
        hooks.insert(
            "install".to_string(),
            serde_json::json!({"plugs": ["home", "network"]}),
        );

        let cfg = SnapcraftConfig {
            name: Some("mysnap".to_string()),
            hooks: Some(hooks),
            ..Default::default()
        };
        let yaml = generate_snapcraft_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();
        assert!(yaml.contains("hooks:"), "missing hooks section\n{yaml}");
        assert!(yaml.contains("configure:"), "missing configure hook\n{yaml}");
        assert!(yaml.contains("install:"), "missing install hook\n{yaml}");
    }

    #[test]
    fn test_generate_yaml_extra_files_structured() {
        let cfg = SnapcraftConfig {
            name: Some("mysnap".to_string()),
            ..Default::default()
        };
        let extra = vec![
            SnapcraftExtraFileSpec::Source("README.md".to_string()),
            SnapcraftExtraFileSpec::Detailed {
                source: "config/app.conf".to_string(),
                destination: Some("etc/app.conf".to_string()),
                mode: Some(0o644),
            },
        ];
        let yaml = generate_snapcraft_yaml(&cfg, "1.0.0", &["myapp"], Some(&extra), None).unwrap();
        // Simple file: source filename used as-is
        assert!(
            yaml.contains("README.md"),
            "missing simple extra file\n{yaml}"
        );
        // Structured file: destination should be used
        assert!(
            yaml.contains("etc/app.conf"),
            "missing structured extra file destination\n{yaml}"
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
        let config: Config =
            serde_yaml_ng::from_str(yaml).expect("failed to parse config with new fields");
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
        assert_eq!(
            app.completer.as_deref(),
            Some("completions/myapp.bash")
        );
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
        assert_eq!(extra[0], SnapcraftExtraFileSpec::Source("README.md".to_string()));
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
    fn test_new_app_fields_omitted_when_empty() {
        // When new fields are not set, they should NOT appear in generated YAML
        let mut apps = HashMap::new();
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
            ..Default::default()
        };
        let yaml = generate_snapcraft_yaml(&cfg, "1.0.0", &["myapp"], None, None).unwrap();

        // None of the new fields should appear
        assert!(!yaml.contains("adapter:"), "adapter should be omitted\n{yaml}");
        assert!(!yaml.contains("after:"), "after should be omitted\n{yaml}");
        assert!(!yaml.contains("aliases:"), "aliases should be omitted\n{yaml}");
        assert!(!yaml.contains("autostart:"), "autostart should be omitted\n{yaml}");
        assert!(!yaml.contains("before:"), "before should be omitted\n{yaml}");
        assert!(!yaml.contains("bus-name:"), "bus-name should be omitted\n{yaml}");
        assert!(
            !yaml.contains("command-chain:"),
            "command-chain should be omitted\n{yaml}"
        );
        assert!(!yaml.contains("common-id:"), "common-id should be omitted\n{yaml}");
        assert!(!yaml.contains("completer:"), "completer should be omitted\n{yaml}");
        assert!(!yaml.contains("desktop:"), "desktop should be omitted\n{yaml}");
        assert!(!yaml.contains("extensions:"), "extensions should be omitted\n{yaml}");
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
        assert!(!yaml.contains("sockets:"), "sockets should be omitted\n{yaml}");
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
}
