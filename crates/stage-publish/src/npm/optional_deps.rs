//! NPM `optionalDependencies` layout generation (the default `optional-deps`
//! mode).
//!
//! The modern pattern that leading Rust CLIs (biome's `generate-packages.mjs`,
//! git-cliff) use to ship binaries through npm: instead of a postinstall
//! download shim, anodizer emits one thin per-platform package per built
//! target plus a metapackage. The per-platform packages carry `os`/`cpu`/`libc`
//! selectors DERIVED from the target triple ([`super::manifest::npm_triple`]),
//! so npm's native resolution installs only the one matching the host — no
//! download, no postinstall. The metapackage lists every per-platform package
//! under `optionalDependencies` and ships a `bin` shim that resolves the
//! installed one via `require.resolve`.

use std::collections::BTreeMap;

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::config::NpmConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

use super::manifest::{
    NpmTriple, finalize_package_json, insert_common_metadata, insert_engines, insert_files,
    insert_publish_config, npm_triple, warn_excluded_targets,
};

/// One per-platform package emitted in `optional-deps` mode.
///
/// `name` is the full npm name (`<scope>/<bin>-<os>-<cpu>[-<libc>]`).
/// `package_json` is the rendered manifest carrying the `os`/`cpu`/`libc`
/// selectors. `binary_src` is the on-disk path of the binary to embed (mode
/// `0o755`), and `binary_name` is the filename it lands under inside the
/// package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlatformPackage {
    /// Full npm package name (e.g. `@scope/cli-linux-x64-musl`).
    pub name: String,
    /// npm selection triple this package targets (derived from the target).
    pub triple: NpmTriple,
    /// Rendered `package.json` for the per-platform package.
    pub package_json: String,
    /// On-disk path of the binary to embed.
    pub binary_src: std::path::PathBuf,
    /// Filename the binary is embedded under (e.g. `cli` / `cli.exe`).
    pub binary_name: String,
}

/// The rendered metapackage file pair (`package.json` + `shim.js`), grouped so
/// a skipped metapackage is a single `None` rather than two fields that could
/// drift apart.
#[derive(Debug, Clone)]
pub(crate) struct MetapackageFiles {
    /// Rendered metapackage `package.json` (carries `optionalDependencies` +
    /// `bin`).
    pub package_json: String,
    /// Rendered `shim.js` for the metapackage `bin`.
    pub shim_js: String,
}

/// The full set of packages an `optional-deps` entry emits: the per-platform
/// packages plus (unless `skip_metapackage`) one metapackage.
#[derive(Debug, Clone)]
pub(crate) struct OptionalDepsLayout {
    /// Metapackage name (what users `npm install`). Resolved even when the
    /// metapackage is skipped, for logging and provenance probing.
    pub metapackage: String,
    /// Rendered metapackage files; `None` when `skip_metapackage` is truthy.
    pub metapackage_files: Option<MetapackageFiles>,
    /// Per-platform packages, sorted by name for deterministic emission.
    pub platforms: Vec<PlatformPackage>,
}

/// Resolve the metapackage name: `metapackage:` → `name:` → `crate_name`.
pub(crate) fn resolve_metapackage<'a>(cfg: &'a NpmConfig, crate_name: &'a str) -> &'a str {
    cfg.metapackage
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.name.as_deref().map(str::trim).filter(|s| !s.is_empty()))
        .unwrap_or(crate_name)
}

/// Resolve the command name: `bin:` → metapackage basename (scope-stripped).
pub(crate) fn resolve_bin<'a>(cfg: &'a NpmConfig, metapackage: &'a str) -> &'a str {
    cfg.bin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| metapackage.rsplit('/').next().unwrap_or(metapackage))
}

/// Tie-break rank for the not-libc-aware linux dedup: lower sorts first, and
/// `dedup_by` keeps the first of each same-name run. glibc (rank 0) wins over
/// musl (rank 1) so the retained single linux package is the broadest-
/// compatibility build; any other/absent libc (rank 2) loses to both.
fn libc_dedup_rank(libc: &str) -> u8 {
    match libc {
        "glibc" => 0,
        "musl" => 1,
        _ => 2,
    }
}

/// Build the per-platform package name suffix from a triple, honouring
/// `libc_aware`. With `libc_aware`, a linux package gains `-<libc>` so musl
/// and glibc are distinct packages; without it the libc selector is dropped
/// and a single linux package per cpu is emitted.
fn platform_suffix(triple: &NpmTriple, libc_aware: bool) -> String {
    if libc_aware && !triple.libc.is_empty() {
        format!("{}-{}-{}", triple.os, triple.cpu, triple.libc)
    } else {
        format!("{}-{}", triple.os, triple.cpu)
    }
}

/// How per-platform package names are derived: the default
/// `<scope>/<bin>-<suffix>` scheme, or a user-supplied
/// `platform_name_template` (with which `scope` is optional).
enum PlatformNaming<'a> {
    /// Default `<scope>/<bin>-<os>-<cpu>[-<libc>]` naming.
    Default { scope: &'a str },
    /// `platform_name_template` naming; `scope` (when set) prefixes rendered
    /// names that do not already carry a `@scope/`.
    Template {
        template: &'a str,
        scope: Option<&'a str>,
    },
}

/// Validate `name` against npm's package-name rules: ≤214 chars, lowercase
/// URL-safe characters (`a-z 0-9 - _ . ~`), no leading `.`/`_`, scoped names
/// as `@scope/name` with both parts non-empty.
fn validate_npm_package_name(name: &str) -> Result<()> {
    let part_ok = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && !part.starts_with('_')
            && part.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | '~')
            })
    };
    let valid = name.len() <= 214
        && match name.strip_prefix('@') {
            Some(rest) => match rest.split_once('/') {
                Some((scope, pkg)) => part_ok(scope) && part_ok(pkg),
                None => false,
            },
            None => part_ok(name),
        };
    if !valid {
        bail!(
            "npm: rendered platform package name '{}' is not a legal npm package \
             name (lowercase URL-safe characters, no leading '.'/'_', at most 214 \
             chars; scoped names as '@scope/name')",
            name
        );
    }
    Ok(())
}

/// Render one per-platform package name from `platform_name_template`.
///
/// Beyond the standard release context, seeds `Os`/`Arch`/`Target` from
/// anodizer's target mapping and `NpmOs`/`NpmCpu`/`NpmLibc` from the npm
/// triple. A rendered name without a leading `@` is prefixed with `scope`
/// when one is configured; the final name is validated as a legal npm name.
fn render_platform_name(
    ctx: &Context,
    template: &str,
    scope: Option<&str>,
    target: &str,
    triple: &NpmTriple,
) -> Result<String> {
    let mut vars = ctx.template_vars().clone();
    let (os, arch) = anodizer_core::target::map_target(target);
    vars.set("Os", &os);
    vars.set("Arch", &arch);
    vars.set("Target", target);
    vars.set("NpmOs", &triple.os);
    vars.set("NpmCpu", &triple.cpu);
    vars.set("NpmLibc", &triple.libc);
    let rendered = anodizer_core::template::render(template, &vars).with_context(|| {
        format!("npm: render platform_name_template {template:?} for target '{target}'")
    })?;
    let rendered = rendered.trim();
    let full = match scope {
        Some(scope) if !rendered.starts_with('@') => format!("{}/{}", scope, rendered),
        _ => rendered.to_string(),
    };
    validate_npm_package_name(&full)?;
    Ok(full)
}

/// Render a per-platform `package.json`: `name`/`version` plus the npm
/// `os`/`cpu`/`libc` selectors (libc only when `libc_aware` and present).
///
/// `crate_name` drives the per-crate metadata resolvers; `binary_name` is the
/// embedded binary filename, emitted as the package's `files` allowlist.
// Each parameter is an independent render input (context, config, the three
// identity strings, version, the derived triple, the libc toggle); bundling
// them into a struct would only relocate the arity, not reduce coupling.
#[allow(clippy::too_many_arguments)]
fn render_platform_json(
    ctx: &Context,
    cfg: &NpmConfig,
    pkg_name: &str,
    crate_name: &str,
    binary_name: &str,
    version: &str,
    triple: &NpmTriple,
    libc_aware: bool,
    provenance_override: Option<bool>,
) -> Result<String> {
    let mut root: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    root.insert(
        "name".into(),
        serde_json::Value::String(pkg_name.to_string()),
    );
    root.insert(
        "version".into(),
        serde_json::Value::String(version.to_string()),
    );
    insert_common_metadata(&mut root, ctx, cfg, crate_name);
    insert_engines(&mut root, cfg);
    insert_publish_config(&mut root, cfg, provenance_override);
    insert_files(&mut root, cfg, &[binary_name.to_string()]);
    root.insert(
        "os".into(),
        serde_json::Value::Array(vec![serde_json::Value::String(triple.os.clone())]),
    );
    root.insert(
        "cpu".into(),
        serde_json::Value::Array(vec![serde_json::Value::String(triple.cpu.clone())]),
    );
    // Only emit a `libc` selector when libc-aware AND the target has one
    // (linux musl/glibc). darwin/win32 have no libc selector.
    if libc_aware && !triple.libc.is_empty() {
        root.insert(
            "libc".into(),
            serde_json::Value::Array(vec![serde_json::Value::String(triple.libc.clone())]),
        );
    }
    finalize_package_json(root, cfg)
}

/// Render the metapackage `package.json`: shared metadata, the `bin` map
/// pointing at `shim.js`, and `optionalDependencies` listing every
/// per-platform package at the same version.
#[allow(clippy::too_many_arguments)]
fn render_metapackage_json(
    ctx: &Context,
    cfg: &NpmConfig,
    metapackage: &str,
    crate_name: &str,
    bin: &str,
    version: &str,
    platforms: &[PlatformPackage],
    provenance_override: Option<bool>,
) -> Result<String> {
    let mut root: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    root.insert(
        "name".into(),
        serde_json::Value::String(metapackage.to_string()),
    );
    root.insert(
        "version".into(),
        serde_json::Value::String(version.to_string()),
    );
    insert_common_metadata(&mut root, ctx, cfg, crate_name);
    insert_engines(&mut root, cfg);
    insert_publish_config(&mut root, cfg, provenance_override);
    // The metapackage ships only the `bin` shim (binaries live in the
    // per-platform optionalDependencies).
    insert_files(&mut root, cfg, &["shim.js".to_string()]);

    let mut bin_map = serde_json::Map::new();
    bin_map.insert(bin.to_string(), serde_json::Value::String("shim.js".into()));
    root.insert("bin".into(), serde_json::Value::Object(bin_map));

    // optionalDependencies — BTreeMap keeps the keys sorted for determinism.
    let mut opt_deps: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for p in platforms {
        opt_deps.insert(
            p.name.clone(),
            serde_json::Value::String(version.to_string()),
        );
    }
    let mut opt_obj = serde_json::Map::new();
    for (k, v) in opt_deps {
        opt_obj.insert(k, v);
    }
    root.insert(
        "optionalDependencies".into(),
        serde_json::Value::Object(opt_obj),
    );

    finalize_package_json(root, cfg)
}

/// Render the metapackage `shim.js`. The shim builds a `PLATFORMS` table
/// mapping `<platform>-<arch>[-<libc>]` to the per-platform package name,
/// detects musl-vs-glibc on linux, resolves the matching package's binary via
/// `require.resolve`, and `spawnSync`s it (honouring a `BINARY_OVERRIDE` env
/// var). No download, no third-party deps.
fn render_shim_js(bin: &str, platforms: &[PlatformPackage]) -> String {
    // PLATFORMS entries: key is `<os>-<cpu>` or `<os>-<cpu>-<libc>` when the
    // per-platform package carries a libc selector; value is the package name.
    let mut entries: Vec<String> = platforms
        .iter()
        .map(|p| {
            let key = if p.triple.libc.is_empty() {
                format!("{}-{}", p.triple.os, p.triple.cpu)
            } else {
                format!("{}-{}-{}", p.triple.os, p.triple.cpu, p.triple.libc)
            };
            format!(
                "  {:?}: {{ pkg: {:?}, bin: {:?} }},",
                key, p.name, p.binary_name
            )
        })
        .collect();
    entries.sort();
    let platforms_table = entries.join("\n");

    format!(
        r#"#!/usr/bin/env node
// SPDX-License-Identifier: MIT
// Generated by anodizer (https://github.com/tj-smith47/anodizer) — do not edit by hand.
//
// Resolves the platform-matching optional dependency via require.resolve and
// execs its binary. npm's os/cpu/libc resolution installs exactly one of the
// optionalDependencies; this shim finds it and runs it.
const {{ spawnSync }} = require('child_process');
const fs = require('fs');

// Detect glibc vs musl on linux. The presence of /lib/ld-musl-* (or a
// musl-tagged ldd) means musl; otherwise glibc.
function linuxLibc() {{
  try {{
    const files = fs.readdirSync('/lib');
    if (files.some(f => f.startsWith('ld-musl-'))) return 'musl';
  }} catch (_) {{}}
  try {{
    const files = fs.readdirSync('/usr/lib');
    if (files.some(f => f.startsWith('libc.musl-'))) return 'musl';
  }} catch (_) {{}}
  return 'glibc';
}}

const PLATFORMS = {{
{platforms_table}
}};

function selectKey() {{
  const os = process.platform;
  const arch = process.arch;
  if (os === 'linux') {{
    const libc = linuxLibc();
    const withLibc = `${{os}}-${{arch}}-${{libc}}`;
    if (PLATFORMS[withLibc]) return withLibc;
  }}
  return `${{os}}-${{arch}}`;
}}

function resolveBinary() {{
  // Explicit override wins (useful for local testing / packaging).
  if (process.env.BINARY_OVERRIDE) return process.env.BINARY_OVERRIDE;
  const key = selectKey();
  const entry = PLATFORMS[key];
  if (!entry) {{
    const supported = Object.keys(PLATFORMS).join(', ');
    throw new Error(
      `[{bin}] unsupported platform ${{key}}; supported: ${{supported}}`
    );
  }}
  return require.resolve(`${{entry.pkg}}/${{entry.bin}}`);
}}

const target = resolveBinary();
const result = spawnSync(target, process.argv.slice(2), {{ stdio: 'inherit' }});
if (result.error) {{
  console.error(
    `[{bin}] failed to launch ${{target}}: ${{result.error.message}}; ` +
    `the platform package may be missing or not executable — try reinstalling`
  );
  process.exit(1);
}}
process.exit(result.status === null ? 1 : result.status);
"#,
        platforms_table = platforms_table,
        bin = bin,
    )
}

/// Generate the full `optional-deps` layout for one `npms[]` entry.
///
/// Sources binaries from per-target `UploadableBinary` (falling back to
/// `Binary`) artifacts, derives each one's npm triple from its target, and
/// builds one per-platform package plus the metapackage. With `libc_aware`,
/// linux musl and glibc binaries become distinct packages; without it they
/// collapse to a single linux package per cpu (the glibc build wins the dedup
/// deterministically — see [`libc_dedup_rank`]).
///
/// `platform_name_template` overrides the default per-platform naming (see
/// [`render_platform_name`]); a truthy `skip_metapackage` suppresses the
/// metapackage files entirely (per-platform packages only).
///
/// Errors when no platform binary maps to an npm triple — emitting an empty
/// `optionalDependencies` set would make `npm install` of the metapackage
/// silently install nothing.
///
/// `provenance_override` is applied uniformly to every per-platform package
/// and the metapackage so the whole `optional-deps` set publishes with a
/// consistent `publishConfig.provenance` (see [`super::manifest::insert_publish_config`]).
pub(crate) fn generate_layout(
    ctx: &Context,
    cfg: &NpmConfig,
    crate_name: &str,
    version: &str,
    provenance_override: Option<bool>,
    log: &StageLogger,
) -> Result<OptionalDepsLayout> {
    let metapackage = resolve_metapackage(cfg, crate_name).to_string();
    let bin = resolve_bin(cfg, &metapackage).to_string();
    let libc_aware = cfg.libc_aware;
    let scope = cfg
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/'));
    let name_template = cfg
        .platform_name_template
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let naming = match (name_template, scope) {
        (Some(template), scope) => PlatformNaming::Template { template, scope },
        (None, Some(scope)) => PlatformNaming::Default { scope },
        (None, None) => bail!(
            "npm: entry for '{}' uses optional-deps mode but `scope:` is unset — \
             per-platform packages need a scope (e.g. scope: \"@acme\"), or set \
             `platform_name_template:` to name them without one",
            metapackage
        ),
    };
    let skip_metapackage = match cfg.skip_metapackage.as_ref() {
        Some(s) => s
            .try_evaluates_to_true(|t| ctx.render_template(t))
            .context("npm: render skip_metapackage template")?,
        None => false,
    };

    let id_filter = cfg.ids.as_ref();
    // Source per-target binaries: prefer UploadableBinary (the
    // checksummed/signed/released build output), fall back to raw Binary.
    let mut binaries = ctx.artifacts.by_kind(ArtifactKind::UploadableBinary);
    if binaries.is_empty() {
        binaries = ctx.artifacts.by_kind(ArtifactKind::Binary);
    }

    let mut platforms: Vec<PlatformPackage> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    for art in binaries {
        if let Some(ids) = id_filter
            && !ids.iter().any(|id| id == &art.crate_name)
        {
            continue;
        }
        let target = art.target.as_deref().unwrap_or("");
        let Some(triple) = npm_triple(target) else {
            excluded.push(if target.is_empty() {
                "<no target>".to_string()
            } else {
                target.to_string()
            });
            continue;
        };
        let pkg_name = match naming {
            PlatformNaming::Default { scope } => {
                let suffix = platform_suffix(&triple, libc_aware);
                format!("{}/{}-{}", scope, bin, suffix)
            }
            PlatformNaming::Template { template, scope } => {
                render_platform_name(ctx, template, scope, target, &triple)?
            }
        };
        let binary_name = art.name.clone();
        let package_json = render_platform_json(
            ctx,
            cfg,
            &pkg_name,
            crate_name,
            &binary_name,
            version,
            &triple,
            libc_aware,
            provenance_override,
        )?;
        platforms.push(PlatformPackage {
            name: pkg_name,
            triple,
            package_json,
            binary_src: art.path.clone(),
            binary_name,
        });
    }
    warn_excluded_targets(log, &excluded);

    // Sort by name, breaking ties on libc so the dedup below has a
    // deterministic winner instead of one defined by artifact-insertion order.
    // When not libc-aware, a linux musl and glibc binary share the same package
    // name; `libc_dedup_rank` ranks glibc ahead of musl so `dedup_by` (which
    // keeps the first of each run) always keeps the glibc binary. glibc is the
    // broadest-compatibility default for a single fallback linux package.
    platforms.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| libc_dedup_rank(&a.triple.libc).cmp(&libc_dedup_rank(&b.triple.libc)))
    });
    // Identical (name, triple) pairs are the same package emitted twice (e.g.
    // duplicate artifacts); keep one.
    platforms.dedup_by(|a, b| a.name == b.name && a.triple == b.triple);
    // When not libc-aware, two linux binaries (musl + glibc) collapse to the
    // same package name; drop the duplicate so optionalDependencies has no
    // colliding key. The sort above guarantees glibc precedes musl, so the
    // glibc binary is the one retained. The collapse only spans a libc
    // difference — same-name packages for DIFFERENT os/cpu pairs are a naming
    // bug caught below, not silently merged.
    if !libc_aware {
        platforms.dedup_by(|a, b| {
            a.name == b.name && a.triple.os == b.triple.os && a.triple.cpu == b.triple.cpu
        });
    }
    // Any duplicate name that survives the collapses above means two distinct
    // platforms rendered the same package name — with the default scheme that
    // is impossible, so this is a platform_name_template that omits a
    // distinguishing var (e.g. NpmLibc while libc_aware is true).
    let mut colliding: Vec<&str> = platforms
        .windows(2)
        .filter(|w| w[0].name == w[1].name)
        .map(|w| w[0].name.as_str())
        .collect();
    colliding.dedup();
    if !colliding.is_empty() {
        bail!(
            "npm: platform_name_template renders the same package name for \
             multiple platforms: {} — include enough platform vars (NpmOs / \
             NpmCpu / NpmLibc) to make every per-platform name unique",
            colliding.join(", ")
        );
    }

    if platforms.is_empty() {
        bail!(
            "npm: metapackage '{}' has no binary artifacts matching any npm platform; \
             nothing to publish (optional-deps mode requires per-target binaries)",
            metapackage
        );
    }

    let metapackage_files = if skip_metapackage {
        None
    } else {
        let package_json = render_metapackage_json(
            ctx,
            cfg,
            &metapackage,
            crate_name,
            &bin,
            version,
            &platforms,
            provenance_override,
        )?;
        let shim_js = render_shim_js(&bin, &platforms);
        Some(MetapackageFiles {
            package_json,
            shim_js,
        })
    };

    Ok(OptionalDepsLayout {
        metapackage,
        metapackage_files,
        platforms,
    })
}
