use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context as _, Result};
use serde::Serialize;

use anodize_core::artifact::{Artifact, ArtifactKind};
use anodize_core::context::Context;
use anodize_core::stage::Stage;

// ---------------------------------------------------------------------------
// Architecture mapping
// ---------------------------------------------------------------------------

/// Map a Go-style or Rust-style architecture name to the Flatpak equivalent.
/// Only x86_64 and aarch64 are supported by Flatpak.
fn arch_to_flatpak(arch: &str) -> Option<&'static str> {
    match arch {
        "amd64" | "x86_64" => Some("x86_64"),
        "arm64" | "aarch64" => Some("aarch64"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Default name template
// ---------------------------------------------------------------------------

/// Default output filename template for Flatpak bundles.
const DEFAULT_NAME_TEMPLATE: &str = "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}.flatpak";

// ---------------------------------------------------------------------------
// Manifest JSON structures
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Manifest {
    id: String,
    runtime: String,
    #[serde(rename = "runtime-version")]
    runtime_version: String,
    sdk: String,
    command: String,
    #[serde(rename = "finish-args", skip_serializing_if = "Vec::is_empty")]
    finish_args: Vec<String>,
    modules: Vec<ManifestModule>,
}

#[derive(Serialize)]
struct ManifestModule {
    name: String,
    buildsystem: String,
    #[serde(rename = "build-commands")]
    build_commands: Vec<String>,
    sources: Vec<ManifestSource>,
}

#[derive(Serialize)]
struct ManifestSource {
    #[serde(rename = "type")]
    type_: String,
    path: String,
    #[serde(rename = "dest-filename", skip_serializing_if = "Option::is_none")]
    dest_filename: Option<String>,
}

// ---------------------------------------------------------------------------
// FlatpakStage
// ---------------------------------------------------------------------------

pub struct FlatpakStage;

/// Parse Os and Arch from a Rust target triple using the shared mapping.
fn os_arch_from_target(target: Option<&str>) -> (String, String) {
    target
        .map(anodize_core::target::map_target)
        .unwrap_or_else(|| ("linux".to_string(), "amd64".to_string()))
}

impl Stage for FlatpakStage {
    fn name(&self) -> &str {
        "flatpak"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let log = ctx.logger("flatpak");
        let selected = ctx.options.selected_crates.clone();
        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();

        // Collect crates that have flatpaks config
        let crates: Vec<_> = ctx
            .config
            .crates
            .iter()
            .filter(|c| selected.is_empty() || selected.contains(&c.name))
            .filter(|c| c.flatpaks.is_some())
            .cloned()
            .collect();

        if crates.is_empty() {
            return Ok(());
        }

        // Check if any flatpak config is actually enabled before requiring tools
        let has_enabled = crates.iter().any(|c| {
            c.flatpaks.as_ref().is_some_and(|cfgs| {
                cfgs.iter().any(|cfg| {
                    cfg.disable
                        .as_ref()
                        .is_none_or(|d| !d.is_disabled(|s| ctx.render_template(s)))
                })
            })
        });
        if !has_enabled {
            return Ok(());
        }

        // Check tool availability once for the entire stage
        if !dry_run {
            if !anodize_core::util::find_binary("flatpak-builder") {
                anyhow::bail!(
                    "flatpak-builder not found on PATH; install Flatpak to create Flatpak bundles"
                );
            }
            if !anodize_core::util::find_binary("flatpak") {
                anyhow::bail!(
                    "flatpak not found on PATH; install Flatpak to create Flatpak bundles"
                );
            }
        }

        // Resolve version from template vars
        let version = ctx
            .template_vars()
            .get("Version")
            .cloned()
            .unwrap_or_else(|| {
                log.warn(
                    "no Version template variable set; using 0.0.0 for Flatpak bundle version",
                );
                "0.0.0".to_string()
            });

        let mut new_artifacts: Vec<Artifact> = Vec::new();
        let mut archives_to_remove: Vec<PathBuf> = Vec::new();

        for krate in &crates {
            let flatpak_configs = krate.flatpaks.as_ref().unwrap();

            // Collect Linux binary artifacts for this crate
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

            for flatpak_cfg in flatpak_configs {
                // Skip disabled configs (supports bool or template string)
                if let Some(ref d) = flatpak_cfg.disable
                    && d.is_disabled(|s| ctx.render_template(s))
                {
                    log.status(&format!(
                        "skipping disabled flatpak config for crate {}",
                        krate.name
                    ));
                    continue;
                }

                // Validate required fields
                let app_id = flatpak_cfg
                    .app_id
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("flatpak: app_id is required for crate '{}'", krate.name)
                    })?;

                let runtime = flatpak_cfg
                    .runtime
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("flatpak: runtime is required for crate '{}'", krate.name)
                    })?;

                let runtime_version = flatpak_cfg
                    .runtime_version
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "flatpak: runtime_version is required for crate '{}'",
                            krate.name
                        )
                    })?;

                let sdk = flatpak_cfg
                    .sdk
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        anyhow::anyhow!("flatpak: sdk is required for crate '{}'", krate.name)
                    })?;

                // Filter by build IDs if specified
                let mut filtered = linux_binaries.clone();
                if let Some(ref filter_ids) = flatpak_cfg.ids
                    && !filter_ids.is_empty()
                {
                    filtered.retain(|b| {
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

                // Warn and skip if no Linux binaries found
                if filtered.is_empty() && linux_binaries.is_empty() {
                    log.warn(&format!(
                        "no Linux binary artifacts found for crate '{}'; \
                         skipping Flatpak generation (expected binaries targeting linux)",
                        krate.name
                    ));
                    continue;
                }
                if filtered.is_empty() {
                    log.warn(&format!(
                        "ids filter {:?} matched no binaries for crate '{}'; skipping",
                        flatpak_cfg.ids, krate.name
                    ));
                    continue;
                }

                // Filter to only supported architectures (amd64/arm64)
                let effective_binaries: Vec<(Option<String>, PathBuf, String)> = filtered
                    .iter()
                    .filter_map(|b| {
                        let (_, arch) = os_arch_from_target(b.target.as_deref());
                        arch_to_flatpak(&arch).map(|flatpak_arch| {
                            (b.target.clone(), b.path.clone(), flatpak_arch.to_string())
                        })
                    })
                    .collect();

                if effective_binaries.is_empty() {
                    log.warn(&format!(
                        "no supported architectures (amd64/arm64) found for crate '{}'; skipping Flatpak",
                        krate.name
                    ));
                    continue;
                }

                for (target, binary_path, flatpak_arch) in &effective_binaries {
                    // Derive Os/Arch from the target triple for template rendering
                    let (os, arch) = os_arch_from_target(target.as_deref());

                    // Set Os/Arch/Target in template vars for this iteration
                    ctx.template_vars_mut().set("Os", &os);
                    ctx.template_vars_mut().set("Arch", &arch);
                    ctx.template_vars_mut()
                        .set("Target", target.as_deref().unwrap_or(""));

                    // Determine output filename from name template or default
                    let name_template = flatpak_cfg
                        .name_template
                        .as_deref()
                        .unwrap_or(DEFAULT_NAME_TEMPLATE);

                    let output_name = ctx.render_template(name_template).with_context(|| {
                        format!(
                            "flatpak: render name template for crate {} target {:?}",
                            krate.name, target
                        )
                    })?;

                    // Ensure the filename ends with .flatpak
                    let output_name = if output_name.to_lowercase().ends_with(".flatpak") {
                        output_name
                    } else {
                        format!("{output_name}.flatpak")
                    };

                    // Output goes in a clean flat directory: dist/flatpak/
                    let output_dir = dist.join("flatpak");
                    let output_path = output_dir.join(&output_name);

                    // Build work happens in a separate subdirectory
                    let work_dir = dist.join("flatpak").join(&krate.name).join(flatpak_arch);

                    // Derive the binary name from the file path
                    let binary_name = binary_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&krate.name);

                    // Determine command: config command or binary name
                    let command = flatpak_cfg.command.as_deref().unwrap_or(binary_name);

                    // Build finish_args
                    let finish_args = flatpak_cfg.finish_args.clone().unwrap_or_default();

                    // Resolve extra_files globs to concrete file names
                    let mut extra_file_names: Vec<String> = Vec::new();
                    if let Some(extra_files) = &flatpak_cfg.extra_files {
                        for spec in extra_files {
                            let pattern = spec.glob();
                            match glob::glob(pattern) {
                                Ok(entries) => {
                                    for entry in entries.flatten() {
                                        if entry.is_file() {
                                            let dst_name = spec
                                                .name_template()
                                                .map(|s| s.to_string())
                                                .or_else(|| {
                                                    entry
                                                        .file_name()
                                                        .and_then(|n| n.to_str())
                                                        .map(|s| s.to_string())
                                                })
                                                .unwrap_or_else(|| "extra".to_string());
                                            extra_file_names.push(dst_name);
                                        }
                                    }
                                }
                                Err(e) => {
                                    log.warn(&format!(
                                        "invalid extra_files glob pattern '{}': {}",
                                        pattern, e
                                    ));
                                }
                            }
                        }
                    }

                    // Build manifest sources and build_commands
                    let mut sources = vec![ManifestSource {
                        type_: "file".to_string(),
                        path: binary_name.to_string(),
                        dest_filename: None,
                    }];
                    let mut build_commands = vec![format!(
                        "install -Dm755 {binary_name} /app/bin/{binary_name}"
                    )];

                    for extra_name in &extra_file_names {
                        sources.push(ManifestSource {
                            type_: "file".to_string(),
                            path: extra_name.clone(),
                            dest_filename: None,
                        });
                        build_commands.push(format!(
                            "install -Dm644 {extra_name} /app/share/{app_id}/{extra_name}"
                        ));
                    }

                    // Build the manifest
                    let manifest = Manifest {
                        id: app_id.to_string(),
                        runtime: runtime.to_string(),
                        runtime_version: runtime_version.to_string(),
                        sdk: sdk.to_string(),
                        command: command.to_string(),
                        finish_args,
                        modules: vec![ManifestModule {
                            name: app_id.to_string(),
                            buildsystem: "simple".to_string(),
                            build_commands,
                            sources,
                        }],
                    };

                    if dry_run {
                        log.status(&format!(
                            "(dry-run) would create Flatpak bundle {} for crate {} target {:?}",
                            output_name, krate.name, target
                        ));

                        if let Some(ts) = &flatpak_cfg.mod_timestamp {
                            log.status(&format!("(dry-run) would apply mod_timestamp={ts}"));
                        }

                        new_artifacts.push(Artifact {
                            kind: ArtifactKind::Flatpak,
                            name: String::new(),
                            path: output_path,
                            target: target.clone(),
                            crate_name: krate.name.clone(),
                            metadata: {
                                let mut m =
                                    HashMap::from([("format".to_string(), "flatpak".to_string())]);
                                if let Some(id) = &flatpak_cfg.id {
                                    m.insert("id".to_string(), id.clone());
                                }
                                m
                            },
                            size: None,
                        });

                        // If replace is set, mark archives for this crate+target for removal
                        if flatpak_cfg.replace.unwrap_or(false) {
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

                    // Live mode — create working directory and output directory
                    fs::create_dir_all(&work_dir).with_context(|| {
                        format!("create Flatpak work dir: {}", work_dir.display())
                    })?;
                    fs::create_dir_all(&output_dir).with_context(|| {
                        format!("create Flatpak output dir: {}", output_dir.display())
                    })?;

                    // Copy binary to working dir
                    let staged_binary = work_dir.join(binary_name);
                    fs::copy(binary_path, &staged_binary).with_context(|| {
                        format!(
                            "copy binary {} to {}",
                            binary_path.display(),
                            staged_binary.display()
                        )
                    })?;

                    // Set permissions to 0o755 (rwxr-xr-x), consistent with install -Dm755
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let perms = std::fs::Permissions::from_mode(0o755);
                        std::fs::set_permissions(&staged_binary, perms).with_context(|| {
                            format!(
                                "flatpak: set executable permission on {}",
                                staged_binary.display()
                            )
                        })?;
                    }

                    // Copy extra files into working dir
                    if let Some(extra_files) = &flatpak_cfg.extra_files {
                        for spec in extra_files {
                            let pattern = spec.glob();
                            match glob::glob(pattern) {
                                Ok(entries) => {
                                    for entry in entries.flatten() {
                                        if entry.is_file() {
                                            let dst_name = spec
                                                .name_template()
                                                .map(|s| s.to_string())
                                                .or_else(|| {
                                                    entry
                                                        .file_name()
                                                        .and_then(|n| n.to_str())
                                                        .map(|s| s.to_string())
                                                })
                                                .unwrap_or_else(|| "extra".to_string());
                                            let dst = work_dir.join(&dst_name);
                                            fs::copy(&entry, &dst).with_context(|| {
                                                format!(
                                                    "copy extra file {} to work dir",
                                                    entry.display()
                                                )
                                            })?;
                                        }
                                    }
                                }
                                Err(e) => {
                                    log.warn(&format!(
                                        "invalid extra_files glob pattern '{}': {}",
                                        pattern, e
                                    ));
                                }
                            }
                        }
                    }

                    // Write manifest as {app_id}.json in working dir
                    let manifest_json = serde_json::to_string_pretty(&manifest)
                        .context("flatpak: serialize manifest JSON")?;
                    let manifest_path = work_dir.join(format!("{app_id}.json"));
                    fs::write(&manifest_path, &manifest_json).with_context(|| {
                        format!("flatpak: write manifest to {}", manifest_path.display())
                    })?;

                    // Apply mod_timestamp if set (template-rendered, to work dir contents)
                    if let Some(ref ts_tmpl) = flatpak_cfg.mod_timestamp {
                        let ts = ctx
                            .render_template(ts_tmpl)
                            .with_context(|| "flatpak: render mod_timestamp template")?;
                        anodize_core::util::apply_mod_timestamp(&work_dir, &ts, &log)?;
                    }

                    // Run flatpak-builder
                    let builder_args = [
                        "flatpak-builder".to_string(),
                        "--force-clean".to_string(),
                        format!("--arch={flatpak_arch}"),
                        format!("--default-branch={version}"),
                        "--repo=repo".to_string(),
                        "build".to_string(),
                        format!("{app_id}.json"),
                    ];

                    log.status(&format!("running: {}", builder_args.join(" ")));

                    let output = Command::new(&builder_args[0])
                        .args(&builder_args[1..])
                        .current_dir(&work_dir)
                        .output()
                        .with_context(|| {
                            format!(
                                "execute flatpak-builder for crate {} target {:?}",
                                krate.name, target
                            )
                        })?;
                    log.check_output(output, "flatpak-builder")?;

                    // Run flatpak build-bundle — output to the clean output dir
                    let output_path_str = output_path.to_string_lossy().into_owned();
                    let bundle_args = [
                        "flatpak".to_string(),
                        "build-bundle".to_string(),
                        format!("--arch={flatpak_arch}"),
                        "repo".to_string(),
                        output_path_str,
                        app_id.to_string(),
                        version.clone(),
                    ];

                    log.status(&format!("running: {}", bundle_args.join(" ")));

                    let output = Command::new(&bundle_args[0])
                        .args(&bundle_args[1..])
                        .current_dir(&work_dir)
                        .output()
                        .with_context(|| {
                            format!(
                                "execute flatpak build-bundle for crate {} target {:?}",
                                krate.name, target
                            )
                        })?;
                    log.check_output(output, "flatpak build-bundle")?;

                    // Apply mod_timestamp to the output .flatpak if set
                    if let Some(ref ts_tmpl) = flatpak_cfg.mod_timestamp
                        && output_path.exists()
                    {
                        let ts = ctx
                            .render_template(ts_tmpl)
                            .with_context(|| "flatpak: render mod_timestamp template for output")?;
                        let mtime = anodize_core::util::parse_mod_timestamp(&ts)?;
                        anodize_core::util::set_file_mtime(&output_path, mtime)?;
                        log.status(&format!(
                            "applied mod_timestamp={ts} to {}",
                            output_path.display()
                        ));
                    }

                    log.status(&format!(
                        "created Flatpak bundle {} for crate {} target {:?}",
                        output_name, krate.name, target
                    ));

                    new_artifacts.push(Artifact {
                        kind: ArtifactKind::Flatpak,
                        name: String::new(),
                        path: output_path,
                        target: target.clone(),
                        crate_name: krate.name.clone(),
                        metadata: {
                            let mut m =
                                HashMap::from([("format".to_string(), "flatpak".to_string())]);
                            if let Some(id) = &flatpak_cfg.id {
                                m.insert("id".to_string(), id.clone());
                            }
                            m
                        },
                        size: None,
                    });

                    // If replace is set, mark archives for this crate+target for removal
                    if flatpak_cfg.replace.unwrap_or(false) {
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

        // Register new Flatpak artifacts
        for artifact in new_artifacts {
            ctx.artifacts.add(artifact);
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
    use std::path::PathBuf;

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
        use anodize_core::config::FlatpakConfig;

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
        use anodize_core::config::FlatpakConfig;

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
        assert!(config.disable.is_none());
        assert!(config.id.is_none());
    }

    // -----------------------------------------------------------------------
    // Required field validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_config_required_field_validation() {
        use anodize_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::config::{Config, CrateConfig, FlatpakConfig, StringOrBool};
        use anodize_core::context::{Context, ContextOptions};

        let tmp = tempfile::TempDir::new().unwrap();

        // Disable via bool
        {
            let flatpak_cfg = FlatpakConfig {
                disable: Some(StringOrBool::Bool(true)),
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
                disable: Some(StringOrBool::String("{{ IsSnapshot }}".to_string())),
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
        use anodize_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        assert_eq!(
            DEFAULT_NAME_TEMPLATE,
            "{{ ProjectName }}_{{ Version }}_{{ Os }}_{{ Arch }}.flatpak"
        );
    }

    // -----------------------------------------------------------------------
    // Stage no-op when no flatpak config
    // -----------------------------------------------------------------------

    #[test]
    fn test_stage_skips_when_no_flatpak_config() {
        use anodize_core::config::Config;
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodize_core::context::{Context, ContextOptions};

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
    // Custom name_template rendering
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatpak_dry_run_custom_name_template() {
        use anodize_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodize_core::context::{Context, ContextOptions};

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
        use anodize_core::config::{Config, CrateConfig, FlatpakConfig};
        use anodize_core::context::{Context, ContextOptions};

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
}
