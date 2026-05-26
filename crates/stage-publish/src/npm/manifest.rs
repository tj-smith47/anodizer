//! NPM `package.json` generation + `postinstall.js` shim.
//!
//! GoReleaser Pro's npm pipe ships a single static postinstall script
//! parametrised at render time with the per-target download URL map.
//! Anodizer mirrors that shape: one `package.json` per `npms[]` entry,
//! one `postinstall.js` that selects + downloads the right artifact at
//! install time.

use std::collections::BTreeMap;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::NpmConfig;
use anodizer_core::context::Context;
use anyhow::{Context as _, Result};

use crate::util;

/// Default download archive format when [`NpmConfig::format`] is unset.
pub(crate) const DEFAULT_FORMAT: &str = "tgz";

/// Default dist-tag for `npm publish --tag`.
pub(crate) const DEFAULT_TAG: &str = "latest";

/// Default registry endpoint.
pub(crate) const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

/// Default `extra_files` glob set when the user does not override it
/// (GR Pro parity: `README*`, `LICENSE*`).
pub(crate) const DEFAULT_EXTRA_FILES: &[&str] = &["README*", "LICENSE*"];

/// One platform-specific download entry emitted into `postinstall.js`.
///
/// `os` / `cpu` follow Node's `process.platform` / `process.arch` names
/// (linux/darwin/win32, x64/arm64/ia32). `url` is the resolved release
/// archive URL; `sha256` is the hex digest the postinstall script must
/// verify after download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformBinary {
    /// Node `process.platform` name (linux/darwin/win32/...).
    pub os: String,
    /// Node `process.arch` name (x64/arm64/ia32/...).
    pub cpu: String,
    /// Resolved download URL for the platform binary archive.
    pub url: String,
    /// Hex sha256 the postinstall script verifies against.
    pub sha256: String,
    /// Archive format hint passed to the postinstall script
    /// (`tgz`/`tar.gz`/`zip`/`binary`).
    pub format: String,
}

use serde::Serialize;

/// Map an anodizer `(os, arch)` tuple to Node's `process.platform` /
/// `process.arch` names. Returns `None` for OS/arch combinations npm
/// does not represent (e.g. `freebsd/ppc64`).
pub(crate) fn map_to_node(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    let node_os: &'static str = match os {
        "linux" => "linux",
        "darwin" => "darwin",
        "windows" => "win32",
        "freebsd" => "freebsd",
        "openbsd" => "openbsd",
        "netbsd" => "netbsd",
        "aix" => "aix",
        "android" => "android",
        _ => return None,
    };
    let node_cpu: &'static str = match arch {
        "amd64" => "x64",
        "arm64" => "arm64",
        "386" => "ia32",
        "armv7" | "armv6" => "arm",
        "s390x" => "s390x",
        "ppc64" => "ppc64",
        "ppc64le" => "ppc64",
        "riscv64" => "riscv64",
        _ => return None,
    };
    Some((node_os, node_cpu))
}

/// Resolve the effective dist-tag (configured value or [`DEFAULT_TAG`]).
pub(crate) fn resolve_tag(cfg: &NpmConfig) -> &str {
    cfg.tag
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_TAG)
}

/// Resolve the effective format (configured value or [`DEFAULT_FORMAT`]).
pub(crate) fn resolve_format(cfg: &NpmConfig) -> &str {
    cfg.format
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_FORMAT)
}

/// Resolve the effective registry endpoint, trimming trailing slashes so
/// the publish URL constructor can append `/<path>` without doubling up.
pub(crate) fn resolve_registry(cfg: &NpmConfig) -> String {
    let raw = cfg
        .registry
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REGISTRY);
    raw.trim_end_matches('/').to_string()
}

/// Resolve the effective access value. Scoped packages on the public npm
/// registry default to `restricted`; explicit `public` is required to
/// open the package up.
pub(crate) fn resolve_access(cfg: &NpmConfig) -> Option<String> {
    cfg.access
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Resolve the effective `extra_files` glob set.
pub(crate) fn resolve_extra_files(cfg: &NpmConfig) -> Vec<String> {
    cfg.extra_files
        .clone()
        .unwrap_or_else(|| DEFAULT_EXTRA_FILES.iter().map(|s| s.to_string()).collect())
}

/// Resolve the effective package name, falling back to `crate_name` when
/// `cfg.name` is unset.
pub(crate) fn resolve_name<'a>(cfg: &'a NpmConfig, crate_name: &'a str) -> &'a str {
    cfg.name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(crate_name)
}

/// Resolve the env var NAME (NOT value) that holds the npm auth token
/// for this entry. Currently fixed to `NPM_TOKEN` — the canonical npm
/// convention. Stored in evidence so rollback knows which env var to
/// consult.
pub(crate) fn token_env_var(_cfg: &NpmConfig) -> &'static str {
    "NPM_TOKEN"
}

/// Collect the platform-binary download set for this `npms[]` entry.
///
/// Walks `ctx.artifacts` for `Archive` entries whose `crate_name` (when
/// `ids:` is unset) matches the entry's `name` / first crate, applies
/// the [`map_to_node`] OS/arch filter, and resolves the download URL
/// (via `url_template` or the artifact's `url` metadata).
pub(crate) fn collect_platform_binaries(
    ctx: &Context,
    cfg: &NpmConfig,
    pkg_name: &str,
    version: &str,
) -> Result<Vec<PlatformBinary>> {
    let format = resolve_format(cfg).to_string();
    let id_filter = cfg.ids.as_ref();
    let url_template = cfg.url_template.as_deref();

    let mut out: Vec<PlatformBinary> = Vec::new();
    for art in ctx.artifacts.all() {
        if !matches!(art.kind, ArtifactKind::Archive) {
            continue;
        }
        if let Some(ids) = id_filter
            && !ids.iter().any(|id| id == &art.crate_name)
        {
            continue;
        }
        let target = art.target.as_deref().unwrap_or("");
        let (os, arch) = anodizer_core::target::map_target(target);
        let Some((node_os, node_cpu)) = map_to_node(&os, &arch) else {
            continue;
        };
        let sha256 = art.metadata.get("sha256").cloned().unwrap_or_default();
        let url = resolve_artifact_url(ctx, art, url_template, pkg_name, version, &arch, &os);
        out.push(PlatformBinary {
            os: node_os.to_string(),
            cpu: node_cpu.to_string(),
            url,
            sha256,
            format: format.clone(),
        });
    }
    // Sort for deterministic tarball assembly.
    out.sort_by(|a, b| a.os.cmp(&b.os).then_with(|| a.cpu.cmp(&b.cpu)));
    // Deduplicate identical (os, cpu) tuples — two archives matching the
    // same node platform tuple is config-author's bug, but we silently
    // drop the second so the manifest doesn't carry duplicate entries.
    out.dedup_by(|a, b| a.os == b.os && a.cpu == b.cpu);
    Ok(out)
}

/// Resolve the archive's download URL, honouring `url_template` when set
/// and falling back to the artifact's `url` metadata otherwise.
fn resolve_artifact_url(
    ctx: &Context,
    art: &Artifact,
    url_template: Option<&str>,
    pkg_name: &str,
    version: &str,
    arch: &str,
    os: &str,
) -> String {
    if let Some(tmpl) = url_template {
        return util::render_url_template_with_ctx(ctx, tmpl, pkg_name, version, arch, os);
    }
    art.metadata
        .get("url")
        .cloned()
        .unwrap_or_else(|| art.path.to_string_lossy().into_owned())
}

/// Render the `package.json` content for a single `npms[]` entry.
///
/// Honours the GR Pro fallback rules: `description`/`homepage`/`license`
/// fall back to `metadata.{description,homepage,license}` when unset.
/// `extra:` is shallow-merged into the root object (config-author keys
/// win over anodizer-generated keys).
pub(crate) fn render_package_json(
    ctx: &Context,
    cfg: &NpmConfig,
    pkg_name: &str,
    version: &str,
    binaries: &[PlatformBinary],
) -> Result<String> {
    // BTreeMap so key order is deterministic across runs / platforms.
    let mut root: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    root.insert(
        "name".into(),
        serde_json::Value::String(pkg_name.to_string()),
    );
    root.insert(
        "version".into(),
        serde_json::Value::String(version.to_string()),
    );

    // Description with metadata fallback.
    let description = cfg
        .description
        .as_deref()
        .map(|s| s.to_string())
        .or_else(|| ctx.config.meta_description().map(|s| s.to_string()));
    if let Some(d) = description {
        root.insert("description".into(), serde_json::Value::String(d));
    }

    // Homepage with metadata fallback.
    let homepage = cfg
        .homepage
        .as_deref()
        .map(|s| s.to_string())
        .or_else(|| ctx.config.meta_homepage().map(|s| s.to_string()));
    if let Some(h) = homepage {
        root.insert("homepage".into(), serde_json::Value::String(h));
    }

    // License with metadata fallback.
    let license = cfg
        .license
        .as_deref()
        .map(|s| s.to_string())
        .or_else(|| ctx.config.meta_license().map(|s| s.to_string()));
    if let Some(l) = license {
        root.insert("license".into(), serde_json::Value::String(l));
    }

    if let Some(author) = cfg.author.as_deref() {
        root.insert(
            "author".into(),
            serde_json::Value::String(author.to_string()),
        );
    }

    if let Some(keywords) = cfg.keywords.as_ref() {
        root.insert(
            "keywords".into(),
            serde_json::Value::Array(
                keywords
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
        );
    }

    if let Some(repo_url) = cfg.repository.as_deref() {
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), serde_json::Value::String("git".into()));
        obj.insert(
            "url".into(),
            serde_json::Value::String(repo_url.to_string()),
        );
        root.insert("repository".into(), serde_json::Value::Object(obj));
    }

    if let Some(bugs) = cfg.bugs.as_deref() {
        let mut obj = serde_json::Map::new();
        obj.insert("url".into(), serde_json::Value::String(bugs.to_string()));
        root.insert("bugs".into(), serde_json::Value::Object(obj));
    }

    // The bin entry points at the postinstall-installed binary launcher
    // (`bin/<crate-name>` inside the installed package). The launcher is
    // a small JS shim that invokes the downloaded native binary.
    let bin_basename = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
    let mut bin = serde_json::Map::new();
    bin.insert(
        bin_basename.to_string(),
        serde_json::Value::String(format!("bin/{}.js", bin_basename)),
    );
    root.insert("bin".into(), serde_json::Value::Object(bin));

    // Scripts: postinstall runs the embedded shim.
    let mut scripts = serde_json::Map::new();
    scripts.insert(
        "postinstall".into(),
        serde_json::Value::String("node ./postinstall.js".into()),
    );
    root.insert("scripts".into(), serde_json::Value::Object(scripts));

    // Embedded binary table — consumed by `postinstall.js` to look up
    // the matching download URL for the runtime's platform/cpu.
    let bins_obj = serde_json::Value::Array(
        binaries
            .iter()
            .map(|b| {
                let mut o = serde_json::Map::new();
                o.insert("os".into(), serde_json::Value::String(b.os.clone()));
                o.insert("cpu".into(), serde_json::Value::String(b.cpu.clone()));
                o.insert("url".into(), serde_json::Value::String(b.url.clone()));
                o.insert("sha256".into(), serde_json::Value::String(b.sha256.clone()));
                o.insert("format".into(), serde_json::Value::String(b.format.clone()));
                serde_json::Value::Object(o)
            })
            .collect(),
    );
    let mut anodize = serde_json::Map::new();
    anodize.insert("binaries".into(), bins_obj);
    root.insert("anodize".into(), serde_json::Value::Object(anodize));

    // Apply `extra:` shallow merge. Config-author keys win over the
    // generated root — operators sometimes need to override `bin` /
    // `scripts` / etc.
    if let Some(extra) = cfg.extra.as_ref() {
        for (k, v) in extra {
            root.insert(k.clone(), v.clone());
        }
    }

    // Encode the BTreeMap into a serde_json::Value::Object so the JSON
    // key order is deterministic (alphabetical) — important for the
    // tarball reproducibility test.
    let mut ordered = serde_json::Map::new();
    for (k, v) in root {
        ordered.insert(k, v);
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(ordered))
        .context("npm: serialize package.json")
}

/// Render the `postinstall.js` shim. The script:
///
/// 1. Reads the embedded `anodize.binaries` table from `package.json`.
/// 2. Selects the entry matching `process.platform` + `process.arch`.
/// 3. Downloads the archive, verifies the sha256, extracts the binary
///    into `bin/<name>` (or `bin/<name>.exe` on win32).
/// 4. Errors with a clear "unsupported platform" message if no match.
///
/// The shim is intentionally minimal (no third-party deps) so it works
/// from any installed npm package without an extra dependency tree.
pub(crate) fn render_postinstall_js(pkg_name: &str) -> String {
    let bin_basename = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
    format!(
        r#"#!/usr/bin/env node
// SPDX-License-Identifier: MIT
// Generated by anodizer — do not edit by hand.
//
// Selects the platform-matching binary archive from package.json,
// downloads it, verifies its sha256, and extracts the binary into
// ./bin/{bin_basename}{{.exe?}} so `npx {bin_basename}` works.
const fs = require('fs');
const path = require('path');
const https = require('https');
const crypto = require('crypto');
const {{ execSync }} = require('child_process');

const pkg = require('./package.json');
const binaries = (pkg.anodize && pkg.anodize.binaries) || [];
const target = binaries.find(b => b.os === process.platform && b.cpu === process.arch);
if (!target) {{
  console.error(
    `[anodize/npm] unsupported platform ${{process.platform}}/${{process.arch}}; ` +
    `supported: ${{binaries.map(b => `${{b.os}}/${{b.cpu}}`).join(', ')}}`
  );
  process.exit(1);
}}

const binDir = path.join(__dirname, 'bin');
fs.mkdirSync(binDir, {{ recursive: true }});

const exe = process.platform === 'win32' ? '{bin_basename}.exe' : '{bin_basename}';
const archivePath = path.join(__dirname, `__anodize_${{target.os}}_${{target.cpu}}.${{target.format}}`);

function download(url, dest) {{
  return new Promise((resolve, reject) => {{
    function go(u, redirects) {{
      https.get(u, res => {{
        if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location && redirects > 0) {{
          go(res.headers.location, redirects - 1);
          return;
        }}
        if (res.statusCode !== 200) {{
          reject(new Error(`HTTP ${{res.statusCode}} fetching ${{u}}`));
          return;
        }}
        const f = fs.createWriteStream(dest);
        res.pipe(f);
        f.on('finish', () => f.close(resolve));
        f.on('error', reject);
      }}).on('error', reject);
    }}
    go(url, 5);
  }});
}}

(async () => {{
  await download(target.url, archivePath);
  const buf = fs.readFileSync(archivePath);
  const got = crypto.createHash('sha256').update(buf).digest('hex');
  if (target.sha256 && got !== target.sha256) {{
    console.error(`[anodize/npm] sha256 mismatch: expected ${{target.sha256}}, got ${{got}}`);
    process.exit(1);
  }}
  // Extract the binary. For `binary` format the archive IS the binary.
  if (target.format === 'binary') {{
    fs.copyFileSync(archivePath, path.join(binDir, exe));
  }} else if (target.format === 'zip') {{
    execSync(`unzip -o "${{archivePath}}" -d "${{binDir}}"`, {{ stdio: 'inherit' }});
  }} else {{
    // tgz / tar.gz
    execSync(`tar -xzf "${{archivePath}}" -C "${{binDir}}"`, {{ stdio: 'inherit' }});
  }}
  fs.unlinkSync(archivePath);
  // Ensure executable bit on POSIX.
  if (process.platform !== 'win32') {{
    try {{ fs.chmodSync(path.join(binDir, exe), 0o755); }} catch (_) {{}}
  }}
}})().catch(err => {{
  console.error(`[anodize/npm] postinstall failed: ${{err.message}}`);
  process.exit(1);
}});
"#,
        bin_basename = bin_basename
    )
}

/// Render the `bin/<name>.js` launcher that npm symlinks into
/// `node_modules/.bin/<name>`. The launcher invokes the native binary
/// the postinstall script dropped into `bin/<name>{,.exe}`.
pub(crate) fn render_launcher_js(pkg_name: &str) -> String {
    let bin_basename = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
    format!(
        r#"#!/usr/bin/env node
// SPDX-License-Identifier: MIT
// Generated by anodizer — do not edit by hand.
const path = require('path');
const {{ spawnSync }} = require('child_process');
const exe = process.platform === 'win32' ? '{bin_basename}.exe' : '{bin_basename}';
const target = path.join(__dirname, exe);
const result = spawnSync(target, process.argv.slice(2), {{ stdio: 'inherit' }});
process.exit(result.status === null ? 1 : result.status);
"#,
        bin_basename = bin_basename
    )
}
