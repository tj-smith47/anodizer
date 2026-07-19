use std::collections::BTreeMap;

use serde::Serialize;

use anodizer_core::config::SnapcraftConfig;
use anodizer_core::context::Context;
use anodizer_core::template::assert_no_unrendered_logged;
use anyhow::{Context as _, Result};

use crate::arch::{is_valid_snap_arch, triple_to_snap_arch};
use crate::build_stage::{
    filter_binaries_by_ids, group_binaries_by_target, linux_binaries_for_crate,
};
use crate::gate::snap_cfg_skipped;
use crate::generate::generate_snap_yaml;

// The default snap name template — core's default asset-name template
// verbatim (`ProjectName` is rebound to the snap name before rendering), so
// the Os/Arch stem and the Arm/Mips/Amd64 variant suffixes cannot drift from
// the names every sibling artifact carries for the same target.
pub(super) const DEFAULT_SNAP_NAME_TEMPLATE: &str =
    anodizer_core::archive_name::DEFAULT_NAME_TEMPLATE;

// ---------------------------------------------------------------------------
// Serde-serializable snapcraft YAML model
// ---------------------------------------------------------------------------

pub(super) fn is_empty_vec<T>(v: &[T]) -> bool {
    v.is_empty()
}

#[derive(Serialize)]
pub(super) struct SnapcraftYaml {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grade: Option<String>,
    pub confinement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub assumes: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub architectures: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub apps: BTreeMap<String, SnapcraftYamlApp>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub plugs: BTreeMap<String, serde_json::Value>,
    #[serde(rename = "layout")]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub layouts: BTreeMap<String, SnapcraftYamlLayout>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<String, serde_json::Value>,
}

#[derive(Default, Serialize)]
pub(super) struct SnapcraftYamlApp {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop-mode")]
    pub stop_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "restart-condition")]
    pub restart_condition: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub plugs: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub after: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autostart: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub before: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "bus-name")]
    pub bus_name: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec", rename = "command-chain")]
    pub command_chain: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "common-id")]
    pub common_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub extensions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "install-mode")]
    pub install_mode: Option<String>,
    #[serde(flatten)]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub passthrough: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "post-stop-command")]
    pub post_stop_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "refresh-mode")]
    pub refresh_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "reload-command")]
    pub reload_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "restart-delay")]
    pub restart_delay: Option<String>,
    #[serde(skip_serializing_if = "is_empty_vec")]
    pub slots: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub sockets: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "start-timeout")]
    pub start_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop-command")]
    pub stop_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "stop-timeout")]
    pub stop_timeout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "watchdog-timeout")]
    pub watchdog_timeout: Option<String>,
}

#[derive(Serialize)]
pub(super) struct SnapcraftYamlLayout {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "bind-file")]
    pub bind_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symlink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub type_: Option<String>,
}

/// Render the snap.yaml metadata a build would write to
/// `prime/meta/snap.yaml` for one snap config on one target.
///
/// Renders the config's templated fields (summary / description / grade,
/// with the project-description fallback and the 78-char summary cap) and
/// hands them to [`generate_snap_yaml`]. This is the single source of truth
/// the build's prime-dir staging and the offline schema validator both call,
/// so a validated document is byte-for-byte the metadata a release ships.
///
/// `binary_names` are the binary filenames staged into the prime root (the
/// first names the default app when no `apps:` are configured); `target` is
/// the optional triple driving the `architectures:` field.
pub(crate) fn render_snap_yaml(
    ctx: &Context,
    snap_cfg: &SnapcraftConfig,
    crate_name: &str,
    version: &str,
    binary_names: &[&str],
    target: Option<&str>,
    project_name: Option<&str>,
) -> Result<String> {
    let rendered_cfg = render_snap_cfg(ctx, snap_cfg, crate_name)?;
    let yaml = generate_snap_yaml(&rendered_cfg, version, binary_names, target, project_name)?;
    // Final chokepoint: catch any user-supplied field that reached the manifest
    // without template rendering. Strict fails the build before publish; lenient
    // warns with the residual already redacted.
    let log = ctx.logger("snapcraft");
    assert_no_unrendered_logged(
        &yaml,
        "snapcraft.yaml",
        ctx.render_is_strict(),
        |s| ctx.redact(s),
        |msg| log.warn(msg),
    )?;
    Ok(yaml)
}

/// Render every snap.yaml a build would emit for one crate, mirroring the
/// build's per-target run walk — without staging files or spawning snapcraft.
///
/// Returns `Ok(vec![])` (nothing to validate) when the crate carries no
/// snapcraft config, when a config's `skip:` / `if:` gate suppresses it, or
/// when no Linux binaries were built for the crate in this snapshot shard
/// (the same shard-tolerance case the build's "no Linux binaries → skip"
/// guard hits). Otherwise groups the crate's Linux binaries by target via the
/// same helpers the build loop uses and returns one rendered snap.yaml per
/// (config, target) pair, each stamped with the run's resolved version.
pub fn snapcraft_snap_yamls_for_crate(ctx: &Context, crate_name: &str) -> Result<Vec<String>> {
    let log = ctx.logger("snapcraft");
    let Some(krate) = ctx.config.find_crate(crate_name) else {
        return Ok(Vec::new());
    };
    let Some(snap_configs) = krate.snapcrafts.as_ref() else {
        return Ok(Vec::new());
    };

    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_else(|| "0.0.0".to_string());
    let project_name = ctx.config.project_name.clone();

    let linux_binaries = linux_binaries_for_crate(ctx, crate_name);

    let mut yamls = Vec::new();
    for snap_cfg in snap_configs {
        if snap_cfg_skipped(ctx, &log, snap_cfg, crate_name)? {
            continue;
        }

        let filtered = filter_binaries_by_ids(&linux_binaries, snap_cfg.ids.as_ref());
        // No Linux binary for this crate in this shard (or the `ids` filter
        // admitted none) — nothing to render. The live build's
        // "no Linux binaries → skip" guard hits the same case.
        if filtered.is_empty() {
            continue;
        }

        let by_target = group_binaries_by_target(&filtered);
        // Grouping keys on the variant to match the live build's per-variant
        // snap builds 1:1; the variant disambiguates only the `.snap` FILENAME
        // (`compute_snap_filename`), not the manifest — the snap `name:` is the
        // package name — so the rendered YAML is variant-independent here.
        for ((target_key, _amd64_variant), target_binaries) in &by_target {
            let target = if target_key == "unknown" {
                None
            } else {
                Some(target_key.as_str())
            };

            // Mirror the build's per-target arch gate: `process_snap_target`
            // refuses to stage a target whose snap arch is unsupported by the
            // store (e.g. riscv64). Skip the same targets here so the validated
            // (target → snap.yaml) set is byte-identical to the built set.
            if let Some(t) = target
                && !is_valid_snap_arch(triple_to_snap_arch(t))
            {
                continue;
            }

            let binary_names: Vec<String> = target_binaries
                .iter()
                .map(|b| {
                    b.path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("binary")
                        .to_string()
                })
                .collect();
            let binary_name_refs: Vec<&str> = binary_names.iter().map(|s| s.as_str()).collect();

            yamls.push(render_snap_yaml(
                ctx,
                snap_cfg,
                crate_name,
                &version,
                &binary_name_refs,
                target,
                Some(project_name.as_str()),
            )?);
        }
    }

    Ok(yamls)
}

/// Clone `snap_cfg` and pre-render its summary / description / grade
/// fields through the template engine. Fall back
/// to project `metadata.description` when snapcraft's `description` is
/// unset.
fn render_snap_cfg(
    ctx: &Context,
    snap_cfg: &SnapcraftConfig,
    krate_name: &str,
) -> Result<SnapcraftConfig> {
    let mut rendered_cfg = snap_cfg.clone();
    if rendered_cfg.description.is_none() {
        rendered_cfg.description = ctx
            .config
            .meta_description_for(krate_name)
            .map(str::to_string);
    }
    // `summary` is a snapcraft-required short tagline with no Cargo.toml
    // counterpart. Fall back to the (possibly Cargo.toml-derived)
    // description so a plain Rust project that declares only
    // `package.description` does not hard-error on "summary is required".
    if rendered_cfg.summary.is_none() {
        rendered_cfg.summary = rendered_cfg.description.clone();
    }
    if let Some(ref s) = rendered_cfg.summary {
        let rendered = ctx
            .render_template(s)
            .with_context(|| format!("snapcraft: render summary for crate {}", krate_name))?;
        rendered_cfg.summary = Some(truncate_snap_summary(&rendered));
    }
    if let Some(ref d) = rendered_cfg.description {
        rendered_cfg.description =
            Some(ctx.render_template(d).with_context(|| {
                format!("snapcraft: render description for crate {}", krate_name)
            })?);
    }
    if let Some(ref g) = rendered_cfg.grade {
        rendered_cfg.grade = Some(
            ctx.render_template(g)
                .with_context(|| format!("snapcraft: render grade for crate {}", krate_name))?,
        );
    }
    // The remaining user-supplied string fields are templatable too (GoReleaser
    // templates these); without rendering, a value like `title: "{{ .Tag }}"`
    // would ship the literal delimiters into snap.yaml.
    if let Some(ref n) = rendered_cfg.name {
        rendered_cfg.name = Some(
            ctx.render_template(n)
                .with_context(|| format!("snapcraft: render name for crate {}", krate_name))?,
        );
    }
    if let Some(ref b) = rendered_cfg.base {
        rendered_cfg.base = Some(
            ctx.render_template(b)
                .with_context(|| format!("snapcraft: render base for crate {}", krate_name))?,
        );
    }
    if let Some(ref c) = rendered_cfg.confinement {
        rendered_cfg.confinement =
            Some(ctx.render_template(c).with_context(|| {
                format!("snapcraft: render confinement for crate {}", krate_name)
            })?);
    }
    // Derive the SPDX license from the crate's Cargo.toml when the config
    // omits it, mirroring every other publisher's `meta_license_for` fallback
    // so a dual-licensed project does not have to hardcode it.
    if rendered_cfg.license.is_none() {
        rendered_cfg.license = ctx.config.meta_license_for(krate_name).map(str::to_string);
    }
    if let Some(ref l) = rendered_cfg.license {
        rendered_cfg.license = Some(
            ctx.render_template(l)
                .with_context(|| format!("snapcraft: render license for crate {}", krate_name))?,
        );
    }
    if let Some(ref t) = rendered_cfg.title {
        rendered_cfg.title = Some(
            ctx.render_template(t)
                .with_context(|| format!("snapcraft: render title for crate {}", krate_name))?,
        );
    }
    // App `command`/`args` are user-templatable (GoReleaser renders them, e.g.
    // `command: myapp-{{ .Version }}`); without rendering, the literal
    // delimiters would ship into snap.yaml — caught by the residual-delimiter
    // guard at the YAML chokepoint, so failing to render here is a hard error
    // under strict mode.
    if let Some(apps) = rendered_cfg.apps.as_mut() {
        for (app_name, app) in apps.iter_mut() {
            if let Some(ref c) = app.command {
                app.command = Some(ctx.render_template(c).with_context(|| {
                    format!("snapcraft: render app '{app_name}' command for crate {krate_name}")
                })?);
            }
            if let Some(ref a) = app.args {
                app.args = Some(ctx.render_template(a).with_context(|| {
                    format!("snapcraft: render app '{app_name}' args for crate {krate_name}")
                })?);
            }
        }
    }
    Ok(rendered_cfg)
}

/// snapcraft's `summary` is hard-capped at 78 characters; a longer value
/// fails at `snapcraft pack`. Deriving the summary from an arbitrarily long
/// `package.description` (or a user-supplied over-long summary) can exceed
/// it, so truncate to 78 characters here — the single point where the
/// effective summary is finalised — applying the cap to derived and
/// user-set summaries alike.
fn truncate_snap_summary(summary: &str) -> String {
    const MAX_SUMMARY_CHARS: usize = 78;
    if summary.chars().count() <= MAX_SUMMARY_CHARS {
        return summary.to_string();
    }
    summary.chars().take(MAX_SUMMARY_CHARS).collect()
}

#[cfg(test)]
mod summary_tests {
    use super::*;
    use anodizer_core::config::{Config, CrateConfig};
    use anodizer_core::test_helpers::TestContextBuilder;

    /// Build a Context whose single crate's `Cargo.toml [package].description`
    /// supplies derived metadata, with NO top-level `metadata:` block.
    fn ctx_with_cargo_description(description: &str) -> (Context, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("demo");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            format!("[package]\nname = \"demo\"\ndescription = \"{description}\"\n"),
        )
        .unwrap();
        let mut ctx = TestContextBuilder::new().build();
        assert!(ctx.config.metadata.is_none(), "no metadata: block present");
        ctx.config.crates = vec![CrateConfig {
            name: "demo".to_string(),
            path: "demo".to_string(),
            ..Default::default()
        }];
        ctx.config.populate_derived_metadata(tmp.path());
        (ctx, tmp)
    }

    fn ctx_with_cargo_license(license: &str) -> (Context, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("demo");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            format!("[package]\nname = \"demo\"\nlicense = \"{license}\"\n"),
        )
        .unwrap();
        let mut ctx = TestContextBuilder::new().build();
        assert!(ctx.config.metadata.is_none(), "no metadata: block present");
        ctx.config.crates = vec![CrateConfig {
            name: "demo".to_string(),
            path: "demo".to_string(),
            ..Default::default()
        }];
        ctx.config.populate_derived_metadata(tmp.path());
        (ctx, tmp)
    }

    #[test]
    fn license_resolves_from_cargo_toml_when_config_omits_it() {
        // snapcraft's `license` must derive from the crate's Cargo.toml SPDX
        // license (like every other publisher) when the config omits it, so a
        // dual-licensed project does not need to hardcode it.
        let (ctx, _tmp) = ctx_with_cargo_license("MIT OR Apache-2.0");
        let snap_cfg = SnapcraftConfig::default();
        assert!(snap_cfg.license.is_none());

        let rendered = render_snap_cfg(&ctx, &snap_cfg, "demo").expect("render snap cfg");
        assert_eq!(rendered.license.as_deref(), Some("MIT OR Apache-2.0"));
    }

    #[test]
    fn emitted_snap_yaml_carries_derived_license() {
        // End-to-end: resolve the config (derive license from Cargo.toml) then
        // generate the snap.yaml, proving the emitted manifest — not just the
        // intermediate struct — carries `license: MIT OR Apache-2.0`.
        let (ctx, _tmp) = ctx_with_cargo_license("MIT OR Apache-2.0");
        let snap_cfg = SnapcraftConfig {
            summary: Some("a demo".to_string()),
            description: Some("a demo description".to_string()),
            ..Default::default()
        };
        assert!(snap_cfg.license.is_none());
        let resolved = render_snap_cfg(&ctx, &snap_cfg, "demo").expect("render snap cfg");
        let yaml = generate_snap_yaml(
            &resolved,
            "0.9.1",
            &["demo"],
            Some("x86_64-unknown-linux-gnu"),
            Some("demo"),
        )
        .expect("generate snap.yaml");
        assert!(
            yaml.contains("license: MIT OR Apache-2.0"),
            "emitted snap.yaml must carry the derived license: {yaml}"
        );
    }

    #[test]
    fn explicit_license_wins_over_derived() {
        // An explicit config license overrides the Cargo.toml-derived value.
        let (ctx, _tmp) = ctx_with_cargo_license("MIT OR Apache-2.0");
        let snap_cfg = SnapcraftConfig {
            license: Some("GPL-3.0".to_string()),
            ..Default::default()
        };
        let rendered = render_snap_cfg(&ctx, &snap_cfg, "demo").expect("render snap cfg");
        assert_eq!(rendered.license.as_deref(), Some("GPL-3.0"));
    }

    #[test]
    fn summary_resolves_from_cargo_toml_description() {
        // Previously: snapcraft "summary is required" — now the summary falls
        // back to the Cargo.toml description.
        let (ctx, _tmp) = ctx_with_cargo_description("a concise demo summary");
        let snap_cfg = SnapcraftConfig::default();
        assert!(snap_cfg.summary.is_none());

        let rendered = render_snap_cfg(&ctx, &snap_cfg, "demo").expect("render snap cfg");
        assert_eq!(rendered.summary.as_deref(), Some("a concise demo summary"));
    }

    #[test]
    fn derived_over_long_summary_is_capped_at_78_chars() {
        // A >78-char description must not produce a summary that fails at
        // `snapcraft pack`; the cap applies to the derived summary.
        let long = "x".repeat(120);
        let (ctx, _tmp) = ctx_with_cargo_description(&long);
        let snap_cfg = SnapcraftConfig::default();

        let rendered = render_snap_cfg(&ctx, &snap_cfg, "demo").unwrap();
        let summary = rendered.summary.expect("summary derived");
        assert_eq!(
            summary.chars().count(),
            78,
            "derived summary must be capped at 78 chars; got {} chars",
            summary.chars().count()
        );
    }

    #[test]
    fn user_set_over_long_summary_is_capped_at_78_chars() {
        // The cap is applied consistently to a user-supplied over-long summary.
        let mut ctx = TestContextBuilder::new().build();
        ctx.config = Config::default();
        let snap_cfg = SnapcraftConfig {
            summary: Some("y".repeat(100)),
            ..Default::default()
        };
        let rendered = render_snap_cfg(&ctx, &snap_cfg, "demo").unwrap();
        let summary = rendered.summary.expect("summary present");
        assert!(
            summary.chars().count() <= 78,
            "user-set summary must be capped at <= 78 chars; got {} chars",
            summary.chars().count()
        );
    }

    #[test]
    fn short_summary_is_left_unchanged() {
        let summary = truncate_snap_summary("short and fine");
        assert_eq!(summary, "short and fine");
    }
}
