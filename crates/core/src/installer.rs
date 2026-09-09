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

/// The template-var keys the templatefiles stage binds [`InstallerCases`] to
/// ([`InstallerCases::bind`]) — the full surface through which a
/// `template_files:` entry can consume the engine-derived installer tables.
pub const INSTALLER_TEMPLATE_VARS: &[&str] = &[
    "InstallerAssetCases",
    "InstallerDetectOsCases",
    "InstallerDetectArchCases",
    "InstallerSupportedPlatforms",
    "InstallerDetectLibc",
    "InstallerAssetCaseSubject",
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
    /// The reachable asset-arm keys — the `os-arch` pairs the detect arms can
    /// actually emit — space-joined in arm order (bind to
    /// `InstallerSupportedPlatforms`). Lets the script's error paths list the
    /// prebuilt binaries that DO exist instead of a bare "unsupported
    /// platform".
    pub supported_platforms: String,
    /// The distinct archive formats the emitted arms select, so a generator
    /// can refuse a combination the script cannot honour (a single-file
    /// format alongside several binaries) before writing the script.
    pub formats: BTreeSet<String>,
    /// A POSIX-`sh` block setting `LIBC` to `gnu` or `musl`, emitted only when
    /// some platform ships both (bind to `InstallerDetectLibc`). Empty
    /// otherwise, so a single-libc release keeps the script it had.
    pub detect_libc: String,
    /// The word the asset `case` matches on — `${OS}-${ARCH}`, or
    /// `${OS}-${ARCH}-${LIBC}` once any platform splits by libc (bind to
    /// `InstallerAssetCaseSubject`).
    pub asset_case_subject: String,
}

impl InstallerCases {
    /// Bind the case tables to their template vars — the keys listed in
    /// [`INSTALLER_TEMPLATE_VARS`] — so the templatefiles stage and any
    /// consumption probe share one binding surface.
    pub fn bind(&self, vars: &mut crate::template::TemplateVars) {
        vars.set("InstallerAssetCases", &self.asset_cases);
        vars.set("InstallerDetectOsCases", &self.detect_os_cases);
        vars.set("InstallerDetectArchCases", &self.detect_arch_cases);
        vars.set("InstallerSupportedPlatforms", &self.supported_platforms);
        vars.set("InstallerDetectLibc", &self.detect_libc);
        vars.set("InstallerAssetCaseSubject", &self.asset_case_subject);
    }
}

/// Whether any `template_files:` entry actually READS one of the installer
/// case vars ([`INSTALLER_TEMPLATE_VARS`]).
///
/// Each entry is rendered twice through the real entry renderer
/// ([`crate::template_file_render::render_templated_file_entry`], so skip
/// semantics and template-dialect translation match the templatefiles stage
/// exactly) — once with the installer vars bound empty, once bound to a
/// sentinel. Differing output means the entry consumes at least one of the
/// vars; probing through the render replaces any regex over template syntax.
/// An entry that fails to render (missing src, template error) counts as
/// consuming: non-consumption cannot be proven, and staying loud is the
/// fail-safe for a gate that silences a 404-class validation.
///
/// All touched vars are restored, so the caller's template scope is
/// unchanged.
pub fn template_files_consume_installer_vars(ctx: &mut Context) -> bool {
    let Some(entries) = ctx.config.template_files.clone() else {
        return false;
    };
    entries
        .iter()
        .any(|entry| entry_reads_installer_vars(ctx, entry))
}

fn entry_reads_installer_vars(
    ctx: &mut Context,
    entry: &crate::config::TemplateFileConfig,
) -> bool {
    const PROBE: &str = "\u{1}anodizer-installer-consumption-probe\u{1}";
    let prior: Vec<(&str, Option<String>)> = INSTALLER_TEMPLATE_VARS
        .iter()
        .map(|k| (*k, ctx.template_vars().get(k).cloned()))
        .collect();
    let render_with = |ctx: &mut Context, value: &str| {
        for k in INSTALLER_TEMPLATE_VARS {
            ctx.template_vars_mut().set(k, value);
        }
        crate::template_file_render::render_templated_file_entry(
            ctx,
            entry,
            "installer consumption probe",
        )
    };
    let base = render_with(ctx, "");
    let probe = render_with(ctx, PROBE);
    for (k, value) in prior {
        match value {
            Some(v) => ctx.template_vars_mut().set(k, &v),
            None => {
                ctx.template_vars_mut().unset(k);
            }
        }
    }
    match (base, probe) {
        (Ok(Some(a)), Ok(Some(b))) => {
            a.rendered_contents != b.rendered_contents || a.rendered_dst != b.rendered_dst
        }
        (Ok(a), Ok(b)) => a.is_some() != b.is_some(),
        _ => true,
    }
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
/// Two families are undetectable by construction and get NO detect arm: the
/// mips family (`uname -m` reports `mips`/`mips64` for both endiannesses, so
/// an arm could fetch the wrong-endian binary) and illumos (`uname -s` is
/// `SunOS`, mapped to `solaris` only). Releasing such a target leaves its
/// asset arm unreachable; a default-visible warning names the stranded
/// target(s) so the release operator knows those hosts fall through to the
/// unsupported-platform error.
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
        supported_platforms: String::new(),
        formats: BTreeSet::new(),
        detect_libc: String::new(),
        asset_case_subject: ASSET_CASE_SUBJECT_PLAIN.to_string(),
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

    // Collapse target triples to the installer's `os-arch` key vocabulary,
    // keeping the libc class alongside it. `*-linux-gnu` and `*-linux-musl`
    // both reduce to `linux-amd64` but are NOT interchangeable — a glibc binary
    // cannot run on a musl host — so each keeps its own arm and the script
    // probes the host's libc to choose. What genuinely aliases is the `all`
    // universal fanning into amd64/arm64; [`record_arm`] gives it the worst
    // rank, so it only fills a key no arch-specific build claimed, since no
    // `uname -m` can ever produce "all".
    let mut arms: BTreeMap<(String, &'static str), (u8, String, String)> = BTreeMap::new();
    let mut released_os: BTreeSet<String> = BTreeSet::new();
    let mut released_arch: BTreeSet<String> = BTreeSet::new();
    for (target, asset) in &assets {
        let (os, arch) = map_target(target);
        if arch == "all" {
            continue;
        }
        released_os.insert(os.clone());
        released_arch.insert(arch.clone());
        record_arm(
            &mut arms,
            (format!("{os}-{arch}"), libc_class(target)),
            RANK_ARCH_SPECIFIC,
            &asset.asset_name,
            &asset.format,
        );
    }
    for (target, asset) in &assets {
        let (os, arch) = map_target(target);
        if arch != "all" {
            continue;
        }
        released_os.insert(os.clone());
        for cpu in ["amd64", "arm64"] {
            released_arch.insert(cpu.to_string());
            record_arm(
                &mut arms,
                (format!("{os}-{cpu}"), libc_class(target)),
                RANK_UNIVERSAL,
                &asset.asset_name,
                &asset.format,
            );
        }
    }

    // Which released tokens the generated detect arms can actually emit. A
    // token with no row in the detection tables (the mips family, illumos)
    // makes every asset arm keyed by it unreachable — the release ships the
    // asset, but no host can ever select it.
    let detectable_os: BTreeSet<&str> = UNAME_OS_CASES.iter().map(|(_, t)| *t).collect();
    let detectable_arch: BTreeSet<&str> = UNAME_ARCH_CASES.iter().map(|(_, t)| *t).collect();
    let key_is_reachable = |key: &str| -> bool {
        // Keys are `{os}-{arch}` and neither token contains `-`.
        key.split_once('-')
            .is_some_and(|(os, arch)| detectable_os.contains(os) && detectable_arch.contains(arch))
    };

    // A platform ships two libc classes when both a gnu-ish and a musl triple
    // reduce to the same `os-arch` key. Only then do the arms need a libc
    // suffix — and only then does the script need a probe to produce one.
    let mut per_key: BTreeMap<&str, usize> = BTreeMap::new();
    for (key, _) in arms.keys() {
        *per_key.entry(key.as_str()).or_default() += 1;
    }
    let any_split = per_key.values().any(|n| *n > 1);
    // With a suffixed subject every arm must carry a third segment, so the
    // platforms that did NOT split match any libc the probe can report.
    let arm_key = |key: &str, libc: &'static str| -> String {
        match (per_key.get(key).copied().unwrap_or(0) > 1, any_split) {
            (true, _) => format!("{key}-{libc}"),
            (false, true) => format!("{key}-*"),
            (false, false) => key.to_string(),
        }
    };
    // The operator-facing list names the split platforms by libc but leaves
    // the rest unsuffixed: a `*` glob is arm syntax, not a platform.
    let listed_key = |key: &str, libc: &'static str| -> String {
        if per_key.get(key).copied().unwrap_or(0) > 1 {
            format!("{key}-{libc}")
        } else {
            key.to_string()
        }
    };

    let stranded: Vec<&str> = assets
        .iter()
        .filter(|(target, _)| {
            let (os, arch) = map_target(target);
            arch != "all" && !key_is_reachable(&format!("{os}-{arch}"))
        })
        .map(|(target, _)| target.as_str())
        .collect();
    if !stranded.is_empty() {
        ctx.logger("templatefiles").warn(&format!(
            "installer script cannot detect released target(s) {}: their uname \
             tokens are ambiguous or unmapped (`uname -m` reports the same \
             token for both mips endiannesses; illumos reports `SunOS`), so \
             their asset arms are unreachable — matching hosts get the \
             unsupported-platform error",
            stranded.join(", ")
        ));
    }

    let supported: Vec<String> = arms
        .keys()
        .filter(|(key, _)| key_is_reachable(key))
        .map(|(key, libc)| listed_key(key, libc))
        .collect();

    // Each arm bakes the archive FORMAT the engine resolved for that target
    // alongside the asset name. The script therefore never re-derives a format
    // by sniffing the filename extension — a `binary`-format asset carries the
    // executable's own name and no extension at all.
    let lines: Vec<String> = arms
        .iter()
        .map(|((key, libc), (_, asset, format))| {
            let key = arm_key(key, libc);
            format!(
                "    {key})\n        ARCHIVE=\"{asset}\"\n        FORMAT=\"{format}\"\n        ;;"
            )
        })
        .collect();
    let formats: BTreeSet<String> = arms.values().map(|(_, _, format)| format.clone()).collect();
    Ok(InstallerCases {
        asset_cases: lines.join("\n"),
        detect_os_cases: render_uname_cases(UNAME_OS_CASES, &released_os),
        detect_arch_cases: render_uname_cases(UNAME_ARCH_CASES, &released_arch),
        supported_platforms: supported.join(" "),
        formats,
        detect_libc: if any_split {
            DETECT_LIBC_BLOCK.to_string()
        } else {
            String::new()
        },
        asset_case_subject: if any_split {
            ASSET_CASE_SUBJECT_LIBC.to_string()
        } else {
            ASSET_CASE_SUBJECT_PLAIN.to_string()
        },
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

/// The libc class an installer arm is keyed by, or `""` for a platform that
/// has none.
///
/// Only Linux ships two interchangeable-looking C libraries for one
/// `os-arch` pair. A `*-musl*` triple (including `musleabihf`) is the static
/// one; every other Linux triple links glibc. Non-Linux targets collapse to
/// the empty class so their arms stay unsuffixed.
fn libc_class(target: &str) -> &'static str {
    let (os, _) = map_target(target);
    if os != "linux" {
        ""
    } else if target.contains("musl") {
        "musl"
    } else {
        "gnu"
    }
}

/// The `case` subject when no platform ships two libcs — the shape every
/// release had before libc-aware arms existed.
const ASSET_CASE_SUBJECT_PLAIN: &str = "${OS}-${ARCH}";

/// The `case` subject once some platform ships both libcs: every arm then
/// carries a third segment, so the detect block below must always set `LIBC`.
const ASSET_CASE_SUBJECT_LIBC: &str = "${OS}-${ARCH}-${LIBC}";

/// The runtime libc probe, emitted only alongside [`ASSET_CASE_SUBJECT_LIBC`].
///
/// `ldd --version` names its implementation on both glibc and musl, and is the
/// reliable answer where it exists. Where it does not (a minimal container),
/// the loader that is actually installed answers instead. A host that reveals
/// neither keeps `gnu`, which is what such a host received before the split.
const DETECT_LIBC_BLOCK: &str = "\n\
# This release ships both glibc and musl builds for at least one platform, so\n\
# the asset arms below are keyed by libc as well as os and arch.\n\
LIBC=gnu\n\
if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then\n\
\tLIBC=musl\n\
elif ls /lib/ld-musl-* >/dev/null 2>&1 && ! ls /lib/ld-linux-*.so.* >/dev/null 2>&1; then\n\
\tLIBC=musl\n\
fi\n";

/// The archive formats that produce a single file rather than a container.
///
/// A `curl | sh` installer can only give that file one name, so the generated
/// script renames it to the binary it holds — well-defined only when the
/// install script carries exactly one binary. The generator refuses the
/// combination rather than emitting a script that guesses.
pub const SINGLE_FILE_ARCHIVE_FORMATS: &[&str] = &["gz", "xz", "binary"];

/// Rank of an asset built for one concrete architecture — the rank every
/// release target carries, since libc variants now key separate arms.
const RANK_ARCH_SPECIFIC: u8 = 1;

/// Rank the `all` universal asset when it fans out onto an `amd64`/`arm64`
/// key: strictly worse than any arch-specific build, so a universal only fills
/// a key no native build claimed — the arch-specific-wins precedence a
/// `curl | sh` user relies on.
const RANK_UNIVERSAL: u8 = 2;

/// Record `asset` under `key`, keeping the lowest-ranked asset when several
/// release targets collapse onto one arm. The only remaining collision is a
/// darwin universal asset landing on a key a native build already claimed; an
/// equal-rank collision holds the incumbent, which is the
/// lexicographically-smaller target (assets arrive in sorted-target order).
fn record_arm(
    arms: &mut BTreeMap<(String, &'static str), (u8, String, String)>,
    key: (String, &'static str),
    rank: u8,
    asset: &str,
    format: &str,
) {
    match arms.entry(key) {
        std::collections::btree_map::Entry::Vacant(slot) => {
            slot.insert((rank, asset.to_string(), format.to_string()));
        }
        std::collections::btree_map::Entry::Occupied(mut slot) if rank < slot.get().0 => {
            slot.insert((rank, asset.to_string(), format.to_string()));
        }
        std::collections::btree_map::Entry::Occupied(_) => {}
    }
}

/// The crate whose primary archive the remote installer downloads: the crate
/// that builds a binary named after the project (`config.project_name`) — the
/// binary the `curl | sh` script installs as `${PROJECT}` — and exposes a
/// binstallable (`tar.gz` / `zip` / …) archive.
///
/// Searches top-level `crates:` first, then every `workspaces[].crates[]`, and
/// returns the first match. `None` when no such crate exists (e.g. a pure
/// library workspace), in which case the installer renders no asset arms.
/// Public so emission validation can tell whether a crate's derived asset
/// names feed the installer's case table.
pub fn installer_crate(config: &Config) -> Option<CrateConfig> {
    let project = config.project_name.as_str();
    let produces_project_binary = |c: &CrateConfig| -> bool {
        crate::build_plan::planned_builds(c)
            .map(|builds| builds.iter().any(|b| b.binary.as_deref() == Some(project)))
            .unwrap_or(false)
    };

    config
        .crate_universe()
        .into_iter()
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
        // Variant derivation consults the process env at every cargo flags
        // tier; seal so an exported RUSTFLAGS cannot leak into fixtures.
        ctx.set_env_source(crate::MapEnvSource::new());
        ctx.template_vars_mut().set("ProjectName", "anodizer");
        ctx.template_vars_mut().set("Version", "0.13.0");
        ctx
    }

    /// [`InstallerCases::bind`] must write exactly the keys listed in
    /// [`INSTALLER_TEMPLATE_VARS`] — the const is the consumption probe's
    /// view of the binding surface, so drift between the two silently blinds
    /// the probe.
    #[test]
    fn bind_writes_exactly_the_installer_template_vars() {
        let cases = InstallerCases {
            asset_cases: "a".to_string(),
            detect_os_cases: "b".to_string(),
            detect_arch_cases: "c".to_string(),
            supported_platforms: "d".to_string(),
            formats: BTreeSet::new(),
            detect_libc: "e".to_string(),
            asset_case_subject: "f".to_string(),
        };
        let mut vars = crate::template::TemplateVars::new();
        cases.bind(&mut vars);
        for k in INSTALLER_TEMPLATE_VARS {
            assert!(
                vars.get(k).is_some_and(|v| !v.is_empty()),
                "{k} must be bound"
            );
        }
        assert_eq!(
            vars.all().len(),
            INSTALLER_TEMPLATE_VARS.len(),
            "bind must write no keys beyond INSTALLER_TEMPLATE_VARS"
        );
    }

    /// The consumption probe: an entry that interpolates an installer var
    /// consumes; one that renders none of them does not; a missing src file
    /// stays conservatively "consuming" (fail-loud). The caller's template
    /// scope survives the probe.
    #[test]
    fn template_files_consumption_probe_detects_real_bindings() {
        use crate::config::TemplateFileConfig;
        let tmp = tempfile::tempdir().unwrap();
        let installer = tmp.path().join("install.sh.tera");
        std::fs::write(&installer, "{{ InstallerAssetCases }}\n").unwrap();
        let plain = tmp.path().join("notes.md.tera");
        std::fs::write(&plain, "release {{ Version }} of {{ ProjectName }}\n").unwrap();

        let entry = |src: &std::path::Path| TemplateFileConfig {
            src: src.to_string_lossy().into_owned(),
            dst: "out.txt".to_string(),
            ..Default::default()
        };

        let mut ctx = anodize_ctx(None);
        ctx.template_vars_mut()
            .set("InstallerAssetCases", "stale-from-stage");
        ctx.config.template_files = Some(vec![entry(&plain)]);
        assert!(
            !template_files_consume_installer_vars(&mut ctx),
            "a template binding no Installer* var must not count as a consumer"
        );

        ctx.config.template_files = Some(vec![entry(&plain), entry(&installer)]);
        assert!(
            template_files_consume_installer_vars(&mut ctx),
            "a template interpolating an installer var is a consumer"
        );

        ctx.config.template_files = Some(vec![entry(&tmp.path().join("missing.tera"))]);
        assert!(
            template_files_consume_installer_vars(&mut ctx),
            "an unreadable entry cannot prove non-consumption — stay loud"
        );

        assert_eq!(
            ctx.template_vars()
                .get("InstallerAssetCases")
                .map(String::as_str),
            Some("stale-from-stage"),
            "the probe must restore the caller's bindings"
        );
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

    /// A v3-tuned linux build (`RUSTFLAGS -Ctarget-cpu=x86-64-v3` in the
    /// per-target build env) must surface the SAME `amd64v3` suffix in the
    /// installer's asset arm the archive stage bakes into the uploaded name —
    /// with zero overrides. Untuned targets keep the baseline names.
    #[test]
    fn installer_arms_carry_config_declared_amd64_variant() {
        let mut ctx = anodize_ctx(None);
        let mut env = std::collections::HashMap::new();
        env.insert(
            "x86_64-unknown-linux-gnu".to_string(),
            std::collections::HashMap::from([(
                "RUSTFLAGS".to_string(),
                "-Ctarget-cpu=x86-64-v3".to_string(),
            )]),
        );
        ctx.config.crates[0].builds.as_mut().unwrap()[0].env = Some(env);

        let table = render_installer_cases(&mut ctx).unwrap().asset_cases;
        let arms = parse_arms(&table);
        assert_eq!(
            arms.get("linux-amd64").map(String::as_str),
            Some("anodizer_0.13.0_linux_amd64v3.tar.gz"),
            "tuned target's arm must carry the micro-arch suffix"
        );
        assert_eq!(
            arms.get("darwin-amd64").map(String::as_str),
            Some("anodizer_0.13.0_darwin_amd64.tar.gz"),
            "untuned amd64 target stays baseline"
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
        assert_eq!(cases.supported_platforms, "");
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

    /// A release shipping BOTH a gnu and a musl build for the same os+arch
    /// gets one arm per libc plus a runtime probe to choose between them.
    /// Neither asset may be dropped: a glibc binary cannot run on a musl host,
    /// so collapsing the pair onto one key strands whichever half loses.
    #[test]
    fn dual_libc_targets_split_arms_and_emit_libc_probe() {
        let name_template = "{{ ProjectName }}-{{ Version }}-{{ Target }}";
        let mut ctx = anodize_ctx(Some(name_template));
        ctx.config.defaults.as_mut().unwrap().targets = Some(vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "x86_64-unknown-linux-musl".to_string(),
        ]);
        let cases = render_installer_cases(&mut ctx).unwrap();
        let arms = parse_arms(&cases.asset_cases);

        assert_eq!(arms.len(), 2, "each libc keeps its own arm: {arms:?}");
        assert_eq!(
            arms.get("linux-amd64-gnu").map(String::as_str),
            Some("anodizer-0.13.0-x86_64-unknown-linux-gnu.tar.gz"),
        );
        assert_eq!(
            arms.get("linux-amd64-musl").map(String::as_str),
            Some("anodizer-0.13.0-x86_64-unknown-linux-musl.tar.gz"),
        );
        assert_eq!(cases.asset_case_subject, "${OS}-${ARCH}-${LIBC}");
        assert!(
            cases.detect_libc.contains("LIBC=musl"),
            "split arms are unreachable without the probe that sets LIBC: {}",
            cases.detect_libc
        );
    }

    /// A release with one libc per platform keeps the arms and the `case`
    /// subject it had, and emits no probe — the split is not a tax on projects
    /// that do not need it.
    #[test]
    fn single_libc_does_not_split_or_probe() {
        let mut ctx = anodize_ctx(None);
        let cases = render_installer_cases(&mut ctx).unwrap();
        let arms = parse_arms(&cases.asset_cases);
        assert!(
            arms.contains_key("linux-amd64"),
            "an unsplit key stays unsuffixed: {arms:?}"
        );
        assert_eq!(cases.asset_case_subject, "${OS}-${ARCH}");
        assert_eq!(cases.detect_libc, "");
    }

    /// A musl-only release does not split: a static musl binary runs on glibc
    /// hosts too, so there is nothing to choose between.
    #[test]
    fn musl_only_does_not_split() {
        let name_template = "{{ ProjectName }}-{{ Version }}-{{ Target }}";
        let mut ctx = anodize_ctx(Some(name_template));
        ctx.config.defaults.as_mut().unwrap().targets = Some(vec![
            "x86_64-unknown-linux-musl".to_string(),
            "aarch64-unknown-linux-musl".to_string(),
        ]);
        let cases = render_installer_cases(&mut ctx).unwrap();
        let arms = parse_arms(&cases.asset_cases);
        assert_eq!(
            arms.get("linux-amd64").map(String::as_str),
            Some("anodizer-0.13.0-x86_64-unknown-linux-musl.tar.gz"),
            "a musl-only release keeps the plain key: {arms:?}"
        );
        assert_eq!(cases.detect_libc, "");
    }

    /// The libc class is read from the whole triple, not a `-gnu`/`-musl`
    /// suffix: the 32-bit arm pair spells it `gnueabihf` / `musleabihf`.
    #[test]
    fn armhf_gnueabihf_musleabihf_split() {
        let name_template = "{{ ProjectName }}-{{ Version }}-{{ Target }}";
        let mut ctx = anodize_ctx(Some(name_template));
        ctx.config.defaults.as_mut().unwrap().targets = Some(vec![
            "armv7-unknown-linux-gnueabihf".to_string(),
            "armv7-unknown-linux-musleabihf".to_string(),
        ]);
        let cases = render_installer_cases(&mut ctx).unwrap();
        let arms = parse_arms(&cases.asset_cases);
        assert_eq!(arms.len(), 2, "armhf splits by libc too: {arms:?}");
        assert!(
            arms.keys()
                .all(|k| k.ends_with("-gnu") || k.ends_with("-musl")),
            "both arms carry a libc suffix: {arms:?}"
        );
    }

    /// Once a platform splits, every other arm must still match: the `case`
    /// subject now carries a third segment, so unsplit keys are emitted as a
    /// glob that any probed libc satisfies.
    #[test]
    fn unsplit_keys_glob_the_libc_segment_once_any_platform_splits() {
        let name_template = "{{ ProjectName }}-{{ Version }}-{{ Target }}";
        let mut ctx = anodize_ctx(Some(name_template));
        ctx.config.defaults.as_mut().unwrap().targets = Some(vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "x86_64-unknown-linux-musl".to_string(),
            "aarch64-apple-darwin".to_string(),
        ]);
        let cases = render_installer_cases(&mut ctx).unwrap();
        let arms = parse_arms(&cases.asset_cases);
        assert!(
            arms.contains_key("darwin-arm64-*"),
            "an unsplit platform must still match the suffixed subject: {arms:?}"
        );
    }

    /// `format: binary` uploads the executable itself, so the installer arm
    /// must name the binary (plus `.exe` on Windows) — never `<stem>.binary`,
    /// a filename no release ever produces. The default template for that
    /// format is keyed on `{{ .Binary }}`, the same rule the archive stage
    /// applies.
    #[test]
    fn binary_format_arm_names_the_executable_not_a_dot_binary_file() {
        let mut ctx = anodize_ctx(None);
        {
            let ArchivesConfig::Configs(configs) = &mut ctx.config.crates[0].archives else {
                unreachable!("the fixture configures explicit archives");
            };
            configs.truncate(1);
            configs[0].formats = Some(vec!["binary".to_string()]);
            configs[0].format_overrides = None;
        }
        // A `ProjectName` distinct from the binary name is what separates the
        // two default templates: the wrong one renders the project's stem.
        ctx.template_vars_mut()
            .set("ProjectName", "anodizer-project");
        ctx.config.defaults.as_mut().unwrap().targets = Some(vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "x86_64-pc-windows-msvc".to_string(),
        ]);

        let cases = render_installer_cases(&mut ctx).unwrap();
        let arms = parse_arms(&cases.asset_cases);
        assert_eq!(
            arms.get("linux-amd64").map(String::as_str),
            Some("anodizer_0.13.0_linux_amd64"),
            "a binary-format asset carries no format extension: {arms:?}"
        );
        assert_eq!(
            arms.get("windows-amd64").map(String::as_str),
            Some("anodizer_0.13.0_windows_amd64.exe"),
            "a Windows binary-format asset carries the .exe the archive stage writes"
        );
        assert!(
            cases.formats.contains("binary"),
            "the resolved format must reach the generator so it can gate on it"
        );
    }

    /// The same libc split, but with the installer crate reachable only
    /// through `workspaces[].crates[]` (the per-crate config mode) instead of
    /// top-level `crates:` — the split operates on the derived asset map
    /// regardless of which config mode surfaced the crate.
    #[test]
    fn dual_libc_split_holds_under_workspaces_config() {
        let name_template = "{{ ProjectName }}-{{ Version }}-{{ Target }}";
        let mut ctx = anodize_ctx(Some(name_template));
        ctx.config.defaults.as_mut().unwrap().targets = Some(vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "x86_64-unknown-linux-musl".to_string(),
        ]);
        let moved: Vec<CrateConfig> = std::mem::take(&mut ctx.config.crates);
        ctx.config.workspaces = Some(vec![crate::config::WorkspaceConfig {
            name: "cli".to_string(),
            crates: moved,
            ..Default::default()
        }]);

        let cases = render_installer_cases(&mut ctx).unwrap();
        let arms = parse_arms(&cases.asset_cases);
        assert_eq!(
            arms.get("linux-amd64-musl").map(String::as_str),
            Some("anodizer-0.13.0-x86_64-unknown-linux-musl.tar.gz"),
            "per-crate (workspaces) config must split by libc too: {arms:?}"
        );
        assert_eq!(
            arms.get("linux-amd64-gnu").map(String::as_str),
            Some("anodizer-0.13.0-x86_64-unknown-linux-gnu.tar.gz"),
        );
    }

    /// The operator-facing platform list names each libc a split platform
    /// ships, so the unsupported-platform error stays accurate.
    #[test]
    fn supported_platforms_lists_split_keys() {
        let name_template = "{{ ProjectName }}-{{ Version }}-{{ Target }}";
        let mut ctx = anodize_ctx(Some(name_template));
        ctx.config.defaults.as_mut().unwrap().targets = Some(vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "x86_64-unknown-linux-musl".to_string(),
            "aarch64-apple-darwin".to_string(),
        ]);
        let cases = render_installer_cases(&mut ctx).unwrap();
        assert_eq!(
            cases.supported_platforms,
            "darwin-arm64 linux-amd64-gnu linux-amd64-musl"
        );
    }

    /// `supported_platforms` lists every reachable arm key, space-joined in
    /// arm (BTreeMap) order — the value the script's error paths print.
    #[test]
    fn supported_platforms_lists_reachable_keys() {
        let mut ctx = anodize_ctx(None);
        let cases = render_installer_cases(&mut ctx).unwrap();
        assert_eq!(
            cases.supported_platforms,
            "darwin-amd64 darwin-arm64 linux-amd64 linux-arm64 \
             windows-amd64 windows-arm64"
        );
    }

    /// A released mips-family target has an asset arm but no detect arm
    /// (`uname -m` is endian-ambiguous): the render warns naming the stranded
    /// target and excludes its key from `supported_platforms`.
    #[test]
    fn undetectable_mips_target_warns_and_is_not_listed_supported() {
        let mut ctx = anodize_ctx(None);
        ctx.config.defaults.as_mut().unwrap().targets = Some(vec![
            "x86_64-unknown-linux-gnu".to_string(),
            "mips64el-unknown-linux-gnuabi64".to_string(),
        ]);
        let capture = crate::log::LogCapture::new();
        ctx.with_log_capture(capture.clone());

        let cases = render_installer_cases(&mut ctx).unwrap();

        // The asset arm exists (the release ships it) …
        assert!(
            parse_arms(&cases.asset_cases).contains_key("linux-mips64el"),
            "asset arm for the mips target must still be emitted"
        );
        // … but it is unreachable, so it is not advertised as supported …
        assert_eq!(cases.supported_platforms, "linux-amd64");
        // … and the operator is told which target is stranded.
        assert_eq!(capture.warn_count(), 1, "exactly one stranded-target warn");
        assert!(
            capture
                .warn_messages()
                .iter()
                .any(|m| m.contains("mips64el-unknown-linux-gnuabi64")),
            "warn must name the stranded target: {:?}",
            capture.warn_messages()
        );
    }

    /// A fully-detectable release (the standard 6-triple matrix) renders no
    /// stranded-target warning.
    #[test]
    fn detectable_targets_render_no_stranded_warning() {
        let mut ctx = anodize_ctx(None);
        let capture = crate::log::LogCapture::new();
        ctx.with_log_capture(capture.clone());
        let _ = render_installer_cases(&mut ctx).unwrap();
        assert_eq!(
            capture.warn_count(),
            0,
            "no warning for detectable targets: {:?}",
            capture.warn_messages()
        );
    }
}
