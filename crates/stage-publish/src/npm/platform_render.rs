//! NPM `optional-deps` rendering: per-platform package names and
//! `package.json`, the metapackage `package.json`, and the launcher shim JS.

use std::collections::BTreeMap;

use anodizer_core::config::NpmConfig;
use anodizer_core::context::Context;
use anyhow::{Context as _, Result};

use super::manifest::{
    NpmTriple, finalize_package_json, insert_common_metadata, insert_engines, insert_files,
    insert_publish_config,
};
use super::optional_deps::{MetaCommand, PlatformPackage, join_bin_dir, validate_npm_package_name};

/// Render one per-platform package name from `platform_name_template`.
///
/// Beyond the standard release context, seeds the per-platform naming vars
/// from [`NpmTriple::name_template_vars`] (`Os`/`Arch`/`Target` +
/// `NpmOs`/`NpmCpu`/`NpmLibc`). A rendered name without a leading `@` is
/// prefixed with `scope` when one is configured; the final name is validated
/// as a legal npm name.
pub(super) fn render_platform_name(
    ctx: &Context,
    template: &str,
    scope: Option<&str>,
    target: &str,
    triple: &NpmTriple,
) -> Result<String> {
    let rendered =
        crate::util::render_with_ctx_vars(ctx, template, &triple.name_template_vars(target))
            .with_context(|| {
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
/// `crate_name` drives the per-crate metadata resolvers; `bin_subpaths` are the
/// package-relative paths the embedded binaries land under, emitted as the
/// package's `files` allowlist (so every `platform_bin_dir` binary is included
/// at its subdir — one entry for a single-command tool, one per command for a
/// multi-command `bins:` package).
// Each parameter is an independent render input (context, config, the three
// identity strings, version, the derived triple, the libc toggle); bundling
// them into a struct would only relocate the arity, not reduce coupling.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_platform_json(
    ctx: &Context,
    cfg: &NpmConfig,
    pkg_name: &str,
    crate_name: &str,
    bin_subpaths: &[String],
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
    insert_files(&mut root, cfg, bin_subpaths);
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
/// pointing every command at its launcher shim, and `optionalDependencies`
/// listing every per-platform package at the same version.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_metapackage_json(
    ctx: &Context,
    cfg: &NpmConfig,
    metapackage: &str,
    crate_name: &str,
    commands: &[MetaCommand],
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
    // The metapackage ships only the launcher shim(s) (binaries live in the
    // per-platform optionalDependencies).
    let shim_files: Vec<String> = commands.iter().map(|c| c.shim_file.clone()).collect();
    insert_files(&mut root, cfg, &shim_files);

    // `bin` map: one command → its shim file. BTreeMap keeps it sorted.
    let mut bin_deps: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for c in commands {
        bin_deps.insert(
            c.command.clone(),
            serde_json::Value::String(c.shim_file.clone()),
        );
    }
    let mut bin_map = serde_json::Map::new();
    for (k, v) in bin_deps {
        bin_map.insert(k, v);
    }
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

/// Render one metapackage launcher shim. The shim builds a `PLATFORMS` table
/// mapping `<platform>-<arch>[-<libc>]` to the per-platform package name,
/// detects musl-vs-glibc on linux, resolves the matching package's binary via
/// `require.resolve`, and `spawnSync`s it (honouring a `BINARY_OVERRIDE` env
/// var). No download, no third-party deps.
///
/// `bin` labels the command in error messages. `target` overrides which binary
/// filename inside the resolved package to run (a `bins` command resolving a
/// specific binary at `<platform_bin_dir>/<target>`); `None` resolves the
/// platform package's own embedded binary (`bin_subpath`). `shim_env` is
/// injected into the spawned child's environment.
pub(super) fn render_shim_js(
    cfg: &NpmConfig,
    bin: &str,
    target: Option<&str>,
    bin_dir: Option<&str>,
    platforms: &[PlatformPackage],
) -> String {
    // PLATFORMS entries: key is `<os>-<cpu>` or `<os>-<cpu>-<libc>` when the
    // per-platform package carries a libc selector; `bin` is the package-
    // relative binary path (the command's `target` under platform_bin_dir, or
    // the platform package's own embedded binary).
    let mut entries: Vec<String> = platforms
        .iter()
        .map(|p| {
            let key = if p.triple.libc.is_empty() {
                format!("{}-{}", p.triple.os, p.triple.cpu)
            } else {
                format!("{}-{}-{}", p.triple.os, p.triple.cpu, p.triple.libc)
            };
            let bin_path = match target {
                Some(t) => join_bin_dir(bin_dir, t),
                None => p
                    .binaries
                    .first()
                    .map(|b| b.subpath.clone())
                    .unwrap_or_default(),
            };
            format!("  {:?}: {{ pkg: {:?}, bin: {:?} }},", key, p.name, bin_path)
        })
        .collect();
    entries.sort();
    let platforms_table = entries.join("\n");
    let (shim_env_decl, spawn_opts) = super::manifest::shim_env_fragments(cfg);
    // JSON-encode the command label so a quote/backtick in a `bins` key cannot
    // break the generated JS (mirrors render_launcher_js / postinstall TARGETS).
    let bin_js = serde_json::to_string(bin).unwrap_or_else(|_| format!("{bin:?}"));

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
{shim_env_decl}
const CMD = {bin_js};

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
      `[${{CMD}}] unsupported platform ${{key}}; supported: ${{supported}}`
    );
  }}
  return require.resolve(`${{entry.pkg}}/${{entry.bin}}`);
}}

const target = resolveBinary();
const result = spawnSync(target, process.argv.slice(2), {spawn_opts});
if (result.error) {{
  console.error(
    `[${{CMD}}] failed to launch ${{target}}: ${{result.error.message}}; ` +
    `the platform package may be missing or not executable — try reinstalling`
  );
  process.exit(1);
}}
process.exit(result.status === null ? 1 : result.status);
"#,
        platforms_table = platforms_table,
        shim_env_decl = shim_env_decl,
        spawn_opts = spawn_opts,
        bin_js = bin_js,
    )
}
