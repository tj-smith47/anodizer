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

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::config::{Config, CrateConfig};
use crate::context::Context;
use crate::target::map_target;

/// `uname -s` glob patterns → the OS token [`map_target`] emits, so the
/// installer's `detect_os` case arms are generated from the same vocabulary
/// that keys the asset arms — a hand-written shell copy of this mapping is the
/// drift channel that strands a released target behind an "unsupported
/// platform" error.
///
/// `SunOS` maps to `solaris` only: illumos hosts also report `SunOS`, so an
/// illumos-only release stays undetectable rather than mislabeling a Solaris
/// host. Android/iOS have no `curl | sh` story and get no arm.
const UNAME_OS_CASES: &[(&str, &str)] = &[
    ("Linux*", "linux"),
    ("Darwin*", "darwin"),
    ("MINGW*|MSYS*|CYGWIN*", "windows"),
    ("FreeBSD*", "freebsd"),
    ("NetBSD*", "netbsd"),
    ("OpenBSD*", "openbsd"),
    ("SunOS*", "solaris"),
    ("AIX*", "aix"),
];

/// `uname -m` alias patterns → the arch token [`map_target`] emits.
///
/// The mips family is deliberately absent: `uname -m` reports `mips`/`mips64`
/// on both endiannesses, so a generated arm could fetch the wrong-endian
/// binary — worse than the explicit "unsupported" fallthrough. `all` (the
/// darwin-universal synthetic) never appears here either; universal assets are
/// fanned out to the amd64/arm64 keys at render time instead.
const UNAME_ARCH_CASES: &[(&str, &str)] = &[
    ("x86_64|amd64", "amd64"),
    ("aarch64|arm64", "arm64"),
    ("armv7l|armv7", "armv7"),
    ("armv6l|armv6", "armv6"),
    ("i686|i586|i386", "386"),
    ("riscv64", "riscv64"),
    ("ppc64le", "ppc64le"),
    ("ppc64", "ppc64"),
    ("s390x", "s390x"),
    ("loongarch64", "loong64"),
    ("sparc64", "sparc64"),
];

/// Template-var keys [`render_installer_cases`] mutates while rendering each
/// target's asset name (via `seed_target_vars` inside
/// [`crate::archive_name::render_archive_asset_name`]); snapshotted and restored
/// so a sibling `template_files:` entry that reads `{{ Os }}` / `{{ Arch }}`
/// still sees its own values, not the last target's.
const SEEDED_VARS: &[&str] = &[
    "Os", "Arch", "Target", "Arm", "Arm64", "Amd64", "Mips", "I386",
];

/// The engine-generated `case` arm snippets a `curl | sh` installer template
/// consumes — asset names AND the `uname`→token detection vocabulary, all
/// derived from the release's own targets so neither half can drift from the
/// other.
pub struct InstallerCases {
    /// `case "${OS}-${ARCH}"` arms mapping each released pair to its exact
    /// asset filename (bind to `InstallerAssetCases`).
    pub asset_cases: String,
    /// `case "$(uname -s)"` arms mapping host kernel names to the OS tokens
    /// the asset arms are keyed by (bind to `InstallerDetectOsCases`).
    pub detect_os_cases: String,
    /// `case "$(uname -m)"` arms mapping machine names to the arch tokens the
    /// asset arms are keyed by (bind to `InstallerDetectArchCases`).
    pub detect_arch_cases: String,
}

/// Render the installer's POSIX-`sh` case-arm snippets: the `os-arch` →
/// asset-filename table plus the `uname -s`/`uname -m` detection arms for
/// every released target, baked with the version vars currently set on `ctx`.
///
/// The asset value is the same one
/// [`crate::binstall::crate_archive_asset_names`] derives for cargo-binstall's
/// `pkg_url`, so a `curl | sh` download URL built from it resolves to a real
/// asset by construction — the installer can never hardcode a name that 404s
/// when the archive `name_template` / `format_overrides` change. The detection
/// arms are generated from the SAME target set and the same [`map_target`]
/// vocabulary that keys the asset arms, so a released target's arm cannot be
/// stranded behind a hand-written shell mapping that never emits its key, and
/// a vocabulary rename in [`map_target`] moves both halves together.
///
/// A `darwin-universal` build's asset is fanned out to the `darwin-amd64` /
/// `darwin-arm64` keys (real `uname -m` values), with arch-specific assets
/// taking precedence — a universal-only release is installable on both CPU
/// families instead of erroring on an unmatchable `darwin-all` key.
///
/// No snippet carries a trailing `*)` arm; the installer template owns each
/// fallback error arm. All three strings are empty when no installer crate /
/// asset set resolves, leaving non-installer template files unaffected.
pub fn render_installer_cases(ctx: &mut Context) -> Result<InstallerCases> {
    let empty = || InstallerCases {
        asset_cases: String::new(),
        detect_os_cases: String::new(),
        detect_arch_cases: String::new(),
    };
    let Some(crate_cfg) = installer_crate(&ctx.config) else {
        return Ok(empty());
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
        return Ok(empty());
    };

    // Collapse target triples to the installer's `os-arch` key vocabulary.
    // Arch-specific assets are inserted first (first-writer-wins within them
    // is unambiguous — only `all` could alias two triples onto one key);
    // universal (`all`) assets then fan out into any amd64/arm64 keys still
    // missing, since no `uname -m` can ever produce "all".
    let mut arms: BTreeMap<String, String> = BTreeMap::new();
    let mut released_os: BTreeSet<String> = BTreeSet::new();
    let mut released_arch: BTreeSet<String> = BTreeSet::new();
    for (target, asset) in &assets {
        let (os, arch) = map_target(target);
        if arch == "all" {
            continue;
        }
        released_os.insert(os.clone());
        released_arch.insert(arch.clone());
        arms.entry(format!("{os}-{arch}"))
            .or_insert_with(|| asset.asset_name.clone());
    }
    for (target, asset) in &assets {
        let (os, arch) = map_target(target);
        if arch != "all" {
            continue;
        }
        released_os.insert(os.clone());
        for cpu in ["amd64", "arm64"] {
            released_arch.insert(cpu.to_string());
            arms.entry(format!("{os}-{cpu}"))
                .or_insert_with(|| asset.asset_name.clone());
        }
    }

    let lines: Vec<String> = arms
        .iter()
        .map(|(key, asset)| format!("    {key})\n        ARCHIVE=\"{asset}\"\n        ;;"))
        .collect();
    Ok(InstallerCases {
        asset_cases: lines.join("\n"),
        detect_os_cases: render_uname_cases(UNAME_OS_CASES, &released_os),
        detect_arch_cases: render_uname_cases(UNAME_ARCH_CASES, &released_arch),
    })
}

/// Render the `uname` case arms for the released tokens only, in the fixed
/// table order (deterministic output for the determinism harness).
fn render_uname_cases(table: &[(&str, &str)], released: &BTreeSet<String>) -> String {
    table
        .iter()
        .filter(|(_, token)| released.contains(*token))
        .map(|(pattern, token)| format!("        {pattern}) echo \"{token}\" ;;"))
        .collect::<Vec<_>>()
        .join("\n")
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
        let table = render_installer_cases(&mut ctx).unwrap().asset_cases;
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
        let table = render_installer_cases(&mut ctx).unwrap().asset_cases;
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
        let _ = render_installer_cases(&mut ctx).unwrap();
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
        let cases = render_installer_cases(&mut ctx).unwrap();
        assert_eq!(cases.asset_cases, "");
        assert_eq!(cases.detect_os_cases, "");
        assert_eq!(cases.detect_arch_cases, "");
    }

    /// Every `uname -m` alias must round-trip through `map_target` to the
    /// exact arch token its case arm echoes — the coupling that keeps a
    /// generated asset arm reachable (a vocabulary rename in `map_target`
    /// moves the detect arms with it, or this test names the alias that
    /// stopped matching).
    #[test]
    fn uname_arch_aliases_round_trip_through_map_target() {
        for (pattern, token) in UNAME_ARCH_CASES {
            for alias in pattern.split('|') {
                let (_, arch) = map_target(&format!("{alias}-unknown-linux-gnu"));
                assert_eq!(
                    &arch, token,
                    "uname alias '{alias}' must map_target to its case token '{token}'"
                );
            }
        }
    }

    /// Every OS token a detect arm echoes must be exactly what `map_target`
    /// derives for a triple of that OS — the other half of the key coupling.
    #[test]
    fn uname_os_tokens_round_trip_through_map_target() {
        for (_, token) in UNAME_OS_CASES {
            let triple = match *token {
                "darwin" => "x86_64-apple-darwin".to_string(),
                "windows" => "x86_64-pc-windows-msvc".to_string(),
                "aix" => "powerpc64-ibm-aix".to_string(),
                other => format!("x86_64-unknown-{other}"),
            };
            let (os, _) = map_target(&triple);
            assert_eq!(
                &os, token,
                "uname OS token '{token}' must equal map_target's OS for {triple}"
            );
        }
    }

    /// Detect arms are restricted to the released targets and rendered as
    /// ready-to-paste case arms; every asset arm key must be reachable through
    /// them (no released target stranded behind "unsupported platform").
    #[test]
    fn detect_cases_cover_every_asset_arm_key() {
        let mut ctx = anodize_ctx(None);
        let cases = render_installer_cases(&mut ctx).unwrap();

        assert_eq!(
            cases.detect_os_cases,
            "        Linux*) echo \"linux\" ;;\n\
             \x20       Darwin*) echo \"darwin\" ;;\n\
             \x20       MINGW*|MSYS*|CYGWIN*) echo \"windows\" ;;"
        );
        assert_eq!(
            cases.detect_arch_cases,
            "        x86_64|amd64) echo \"amd64\" ;;\n\
             \x20       aarch64|arm64) echo \"arm64\" ;;"
        );

        let os_tokens: Vec<&str> = cases
            .detect_os_cases
            .lines()
            .filter_map(|l| l.split('"').nth(1))
            .collect();
        let arch_tokens: Vec<&str> = cases
            .detect_arch_cases
            .lines()
            .filter_map(|l| l.split('"').nth(1))
            .collect();
        for key in parse_arms(&cases.asset_cases).keys() {
            let (os, arch) = key.split_once('-').expect("key is os-arch");
            assert!(os_tokens.contains(&os), "asset arm OS '{os}' undetectable");
            assert!(
                arch_tokens.contains(&arch),
                "asset arm arch '{arch}' undetectable"
            );
        }
    }

    /// A universal (darwin-all) asset fans out to the real `uname -m` keys —
    /// amd64/arm64 hosts can install it — while arch-specific assets keep
    /// precedence for their own key.
    #[test]
    fn universal_asset_fans_out_to_amd64_and_arm64_keys() {
        let mut ctx = anodize_ctx(Some("{{ ProjectName }}-{{ Version }}-{{ Os }}-{{ Arch }}"));
        ctx.config.defaults.as_mut().unwrap().targets = Some(vec![
            "darwin-universal".to_string(),
            "aarch64-apple-darwin".to_string(),
        ]);
        let cases = render_installer_cases(&mut ctx).unwrap();
        let arms = parse_arms(&cases.asset_cases);

        assert!(!arms.contains_key("darwin-all"), "no unmatchable 'all' key");
        assert_eq!(
            arms.get("darwin-amd64").map(String::as_str),
            Some("anodizer-0.13.0-darwin-all.tar.gz"),
            "amd64 hosts fall back to the universal asset"
        );
        assert_eq!(
            arms.get("darwin-arm64").map(String::as_str),
            Some("anodizer-0.13.0-darwin-arm64.tar.gz"),
            "the arch-specific asset wins its own key over the universal"
        );
    }
}
