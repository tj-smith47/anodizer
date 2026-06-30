//! Remote `curl | sh` installer support — derive the per-target archive asset
//! filenames the installer downloads from the engine, so the script never
//! hardcodes (and silently drifts from) the names the archive stage uploads.
//!
//! A remote installer's whole job is to fetch the right release asset for the
//! user's machine. The machine half (`uname -s`/`uname -m` → `os`/`arch`) must
//! stay in shell, but the NAME of each asset is something anodize already knows
//! exactly — it is whatever the archive stage renders from
//! `archive.name_template` + `format_overrides`. Hand-rolling that name in shell
//! is the same defect class as a hand-written cargo-binstall `pkg_url`: the
//! moment the template or a format override changes, every `curl | sh` user
//! gets a 404. This module renders the names through the one engine SSOT so the
//! installer's URLs resolve by construction.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::config::{Config, CrateConfig};
use crate::context::Context;
use crate::target::map_target;

/// Template-var keys [`render_asset_case_table`] mutates while rendering each
/// target's asset name (via `seed_target_vars` inside
/// [`crate::archive_name::render_archive_asset_name`]); snapshotted and restored
/// so a sibling `template_files:` entry that reads `{{ Os }}` / `{{ Arch }}`
/// still sees its own values, not the last target's.
const SEEDED_VARS: &[&str] = &[
    "Os", "Arch", "Target", "Arm", "Arm64", "Amd64", "Mips", "I386",
];

/// Render a POSIX-`sh` `case "${OS}-${ARCH}"` arm list mapping every released
/// `os-arch` pair to the exact archive asset filename anodize uploads for that
/// target, baked with the version vars currently set on `ctx`.
///
/// The value is the same one [`crate::binstall::crate_archive_asset_names`]
/// derives for cargo-binstall's `pkg_url`, so a `curl | sh` download URL built
/// from it resolves to a real asset by construction — the installer can never
/// hardcode a name that 404s when the archive `name_template` /
/// `format_overrides` change. The snippet carries no trailing `*)` arm; the
/// installer template owns the fallback error arm. Returns an empty string when
/// no installer crate / asset set resolves, leaving non-installer template
/// files unaffected.
///
/// Intended to be bound to the `InstallerAssetCases` template variable before a
/// `template_files:` entry is rendered.
pub fn render_asset_case_table(ctx: &mut Context) -> Result<String> {
    let Some(crate_cfg) = installer_crate(&ctx.config) else {
        return Ok(String::new());
    };
    let default_targets = ctx.config.effective_default_targets();

    // Snapshot the per-target seed vars so rendering the table cannot leak the
    // last target's `Os`/`Arch`/… into the surrounding template_files render.
    let prior: Vec<(&str, Option<String>)> = SEEDED_VARS
        .iter()
        .map(|k| (*k, ctx.template_vars().get(k).cloned()))
        .collect();
    let assets = crate::binstall::crate_archive_asset_names(&crate_cfg, &default_targets, ctx);
    for (key, value) in prior {
        match value {
            Some(v) => ctx.template_vars_mut().set(key, &v),
            None => {
                ctx.template_vars_mut().unset(key);
            }
        }
    }
    let Some(assets) = assets? else {
        return Ok(String::new());
    };

    // Collapse target triples to the installer's `os-arch` uname vocab. Two
    // triples can only collide on one key via the synthetic universal `all`
    // arch, which no `uname -m` emits, so first-writer-wins is unambiguous.
    let mut arms: BTreeMap<String, String> = BTreeMap::new();
    for (target, asset) in &assets {
        let (os, arch) = map_target(target);
        arms.entry(format!("{os}-{arch}"))
            .or_insert_with(|| asset.asset_name.clone());
    }

    let lines: Vec<String> = arms
        .iter()
        .map(|(key, asset)| format!("    {key})\n        ARCHIVE=\"{asset}\"\n        ;;"))
        .collect();
    Ok(lines.join("\n"))
}

/// The crate whose primary archive the remote installer downloads: the crate
/// that builds a binary named after the project (`config.project_name`) — the
/// binary the `curl | sh` script installs as `${PROJECT}` — and exposes a
/// binstallable (`tar.gz` / `zip` / …) archive.
///
/// Searches top-level `crates:` first, then every `workspaces[].crates[]`, and
/// returns the first match. `None` when no such crate exists (e.g. a pure
/// library workspace), in which case the installer renders no asset arms.
fn installer_crate(config: &Config) -> Option<CrateConfig> {
    let project = config.project_name.as_str();
    let produces_project_binary = |c: &CrateConfig| -> bool {
        crate::build_plan::planned_builds(c)
            .map(|builds| builds.iter().any(|b| b.binary.as_deref() == Some(project)))
            .unwrap_or(false)
    };

    config
        .crates
        .iter()
        .chain(
            config
                .workspaces
                .iter()
                .flatten()
                .flat_map(|w| w.crates.iter()),
        )
        .find(|c| produces_project_binary(c) && crate::binstall::binstallable_archive(c).is_some())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_name::render_archive_asset_name;
    use crate::config::{
        ArchiveConfig, ArchivesConfig, BuildConfig, Config, Defaults, FormatOverride,
    };
    use crate::context::{Context, ContextOptions};

    /// The six lockstep triples anodize releases, paired with the installer
    /// `os-arch` key `map_target` reduces each to.
    const ANODIZE_TARGETS: &[&str] = &[
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
    ];

    /// Build a context shaped like anodize's own (lockstep) config: one crate
    /// named after the project that builds the `anodize` binary, with a single
    /// primary archive carrying `name_template` + a `windows → zip` override,
    /// plus the second `-extra` archive that must be ignored (binstallable
    /// archive selection picks the first tar.gz/zip entry).
    fn anodize_ctx(name_template: Option<&str>) -> Context {
        let primary = ArchiveConfig {
            id: Some("default".to_string()),
            name_template: name_template.map(str::to_string),
            formats: Some(vec!["tar.gz".to_string()]),
            format_overrides: Some(vec![FormatOverride {
                os: "windows".to_string(),
                formats: Some(vec!["zip".to_string()]),
            }]),
            ids: Some(vec!["anodizer".to_string()]),
            ..Default::default()
        };
        let extra = ArchiveConfig {
            id: Some("extra".to_string()),
            name_template: Some(
                "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}-extra".to_string(),
            ),
            formats: Some(vec!["tar.xz".to_string(), "tar.zst".to_string()]),
            ids: Some(vec!["anodizer".to_string()]),
            ..Default::default()
        };
        let crate_cfg = CrateConfig {
            name: "anodizer".to_string(),
            path: "crates/cli".to_string(),
            builds: Some(vec![BuildConfig {
                id: Some("anodizer".to_string()),
                binary: Some("anodizer".to_string()),
                ..Default::default()
            }]),
            archives: ArchivesConfig::Configs(vec![primary, extra]),
            ..Default::default()
        };
        let config = Config {
            project_name: "anodizer".to_string(),
            defaults: Some(Defaults {
                targets: Some(ANODIZE_TARGETS.iter().map(|s| s.to_string()).collect()),
                ..Default::default()
            }),
            crates: vec![crate_cfg],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ProjectName", "anodizer");
        ctx.template_vars_mut().set("Version", "0.13.0");
        ctx
    }

    /// Parse the rendered `case` arms back into `os-arch -> ARCHIVE` so a test
    /// can compare each arm against the engine's own asset name.
    fn parse_arms(table: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let mut key: Option<String> = None;
        for line in table.lines() {
            let t = line.trim();
            if let Some(k) = t.strip_suffix(')') {
                key = Some(k.to_string());
            } else if let Some(rest) = t.strip_prefix("ARCHIVE=\"") {
                let asset = rest.trim_end_matches('"');
                if let Some(k) = key.take() {
                    out.insert(k, asset.to_string());
                }
            }
        }
        out
    }

    /// Every rendered installer arm must equal `render_archive_asset_name` for
    /// the target it serves — the agreement that keeps a `curl | sh` URL from
    /// 404ing. Exercised against anodize's real hyphen `name_template` (the
    /// current shipping config: R8 here is hardening, the names already match).
    #[test]
    fn installer_arms_match_engine_asset_names_hyphen_template() {
        let name_template = "{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}";
        let mut ctx = anodize_ctx(Some(name_template));
        let table = render_asset_case_table(&mut ctx).unwrap();
        let arms = parse_arms(&table);

        assert_eq!(arms.len(), ANODIZE_TARGETS.len(), "one arm per target");
        for target in ANODIZE_TARGETS {
            let (os, arch) = map_target(target);
            let key = format!("{os}-{arch}");
            let format = if os == "windows" { "zip" } else { "tar.gz" };
            let expected =
                render_archive_asset_name(&mut ctx, name_template, target, format).unwrap();
            assert_eq!(
                arms.get(&key),
                Some(&expected),
                "installer arm '{key}' must equal the archive stage's asset name"
            );
        }
        // Concrete proof of the shipping names.
        assert_eq!(
            arms.get("linux-amd64").map(String::as_str),
            Some("anodizer-0.13.0-linux-amd64.tar.gz")
        );
        assert_eq!(
            arms.get("windows-amd64").map(String::as_str),
            Some("anodizer-0.13.0-windows-amd64.zip")
        );
    }

    /// The whole point of engine-derivation: with the DEFAULT (underscore)
    /// `name_template`, the installer follows the engine to underscore asset
    /// names — it does NOT keep emitting the old hardcoded hyphen form. A future
    /// `name_template` change can never silently leave the installer 404ing.
    #[test]
    fn installer_arms_follow_engine_default_underscore_template() {
        let mut ctx = anodize_ctx(None);
        let table = render_asset_case_table(&mut ctx).unwrap();
        let arms = parse_arms(&table);

        for target in ANODIZE_TARGETS {
            let (os, arch) = map_target(target);
            let key = format!("{os}-{arch}");
            let format = if os == "windows" { "zip" } else { "tar.gz" };
            let expected = render_archive_asset_name(
                &mut ctx,
                crate::archive_name::DEFAULT_NAME_TEMPLATE,
                target,
                format,
            )
            .unwrap();
            assert_eq!(arms.get(&key), Some(&expected));
        }
        // Underscores, not the hardcoded hyphen form the old shell carried.
        assert_eq!(
            arms.get("linux-amd64").map(String::as_str),
            Some("anodizer_0.13.0_linux_amd64.tar.gz")
        );
    }

    /// Rendering the table must not leak per-target seed vars (`Os`/`Arch`/…)
    /// into the surrounding template_files render.
    #[test]
    fn render_restores_seed_vars() {
        let mut ctx = anodize_ctx(Some("{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"));
        ctx.template_vars_mut().set("Os", "sentinel-os");
        let _ = render_asset_case_table(&mut ctx).unwrap();
        assert_eq!(
            ctx.template_vars().get("Os").map(String::as_str),
            Some("sentinel-os"),
            "Os must be restored after rendering the case table"
        );
        // `Target` was never set before the render, so it must be unset again.
        assert!(
            ctx.template_vars().get("Target").is_none(),
            "Target must be cleared back to unset after rendering"
        );
    }

    /// A pure-library workspace (no crate builds the project binary) yields no
    /// arms — the installer template falls through to its own error arm.
    #[test]
    fn no_installer_crate_yields_empty_table() {
        let config = Config {
            project_name: "anodizer".to_string(),
            crates: vec![CrateConfig {
                name: "anodizer-core".to_string(),
                path: "crates/core".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut ctx = Context::new(config, ContextOptions::default());
        ctx.template_vars_mut().set("ProjectName", "anodizer");
        ctx.template_vars_mut().set("Version", "0.13.0");
        assert_eq!(render_asset_case_table(&mut ctx).unwrap(), "");
    }
}
