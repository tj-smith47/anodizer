//! NPM `package.json` generation + `postinstall.js` shim (postinstall mode).
//!
//! In `postinstall` mode the publisher emits one `package.json` per `npms[]`
//! entry plus a `postinstall.js` that selects + downloads the OS/arch-matching
//! release archive at install time. The `optional-deps` mode (the default)
//! lives in [`super::optional_deps`].
//!
//! The target→npm-triple mapping ([`npm_triple`]) is shared by both modes:
//! npm's `os`/`cpu`/`libc` selectors are DERIVED from each artifact's real
//! target triple, never hand-written, so `npm install` resolves the right
//! package on the host.

use std::collections::BTreeMap;

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::NpmConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};
use serde::Serialize;

use crate::util;

/// Default download archive format when [`NpmConfig::format`] is unset.
pub(crate) const DEFAULT_FORMAT: &str = "tgz";

/// Default dist-tag for `npm publish --tag`.
pub(crate) const DEFAULT_TAG: &str = "latest";

/// Default registry endpoint.
pub(crate) const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

/// Default `extra_files` glob set when the user does not override it
/// (`README*`, `LICENSE*`).
pub(crate) const DEFAULT_EXTRA_FILES: &[&str] = &["README*", "LICENSE*"];

/// npm's platform-selection triple for one built target.
///
/// `os` / `cpu` follow npm's `process.platform` / `process.arch` names
/// (linux/darwin/win32, x64/arm64/ia32). `libc` is npm's `libc` selector
/// (`musl` / `glibc`) for linux targets, or empty when the OS has no libc
/// distinction. Every field is DERIVED from the artifact's real target
/// triple — see [`npm_triple`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NpmTriple {
    /// npm `os` name (linux/darwin/win32/...).
    pub os: String,
    /// npm `cpu` name (x64/arm64/ia32/...).
    pub cpu: String,
    /// npm `libc` name (`musl` / `glibc`) or empty when not applicable.
    pub libc: String,
}

impl NpmTriple {
    /// The per-platform naming template variables for one built target:
    /// anodizer's `Os`/`Arch` mapping (from a single
    /// [`anodizer_core::target::map_target`] call), the raw `Target` triple,
    /// and the npm selector vars (`NpmOs`/`NpmCpu`/`NpmLibc`) from this
    /// triple. Lives beside [`npm_triple`] so the target→npm naming authority
    /// is single-sourced; `platform_name_template` rendering consumes these
    /// pairs verbatim.
    pub(crate) fn name_template_vars(&self, target: &str) -> Vec<(&'static str, String)> {
        let (os, arch) = anodizer_core::target::map_target(target);
        vec![
            ("Os", os),
            ("Arch", arch),
            ("Target", target.to_string()),
            ("NpmOs", self.os.clone()),
            ("NpmCpu", self.cpu.clone()),
            ("NpmLibc", self.libc.clone()),
        ]
    }
}

/// One platform-specific download entry emitted into `postinstall.js`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformBinary {
    /// npm `process.platform` name (linux/darwin/win32/...).
    pub os: String,
    /// npm `process.arch` name (x64/arm64/ia32/...).
    pub cpu: String,
    /// Resolved download URL for the platform binary archive.
    pub url: String,
    /// Hex sha256 the postinstall script verifies against.
    pub sha256: String,
    /// Archive format hint passed to the postinstall script
    /// (`tgz`/`tar.gz`/`tar`/`zip`/`binary`).
    pub format: String,
}

/// Derive npm's `os`/`cpu`/`libc` selectors from a real Rust target triple.
///
/// Maps anodizer's internal os/arch (from [`anodizer_core::target::map_target`])
/// to npm's naming and reads the libc from
/// [`anodizer_core::target::libc_from_target`], mapping `gnu`→`glibc` (npm
/// names the GNU libc `glibc`, not `gnu`). Returns `None` for OS/arch
/// combinations npm does not represent (e.g. `freebsd/ppc64`), so the caller
/// skips them rather than emitting a package npm can never install.
pub(crate) fn npm_triple(target: &str) -> Option<NpmTriple> {
    let (os, arch) = anodizer_core::target::map_target(target);
    let npm_os: &str = match os.as_str() {
        "linux" => "linux",
        // `is_macos` (genuine `*-apple-darwin` only): map_target folds
        // `*-apple-watchos`/`-tvos` into os="darwin", but an npm package with
        // `os: ["darwin"]` built from a watchOS archive would be selected by
        // `npm install` on a real macOS host and fail. Excluded like ios
        // (os="ios", already unmapped). Mirrors homebrew/nix/krew eligibility.
        "darwin" if anodizer_core::target::is_macos(target) => "darwin",
        "windows" => "win32",
        "freebsd" => "freebsd",
        "openbsd" => "openbsd",
        "netbsd" => "netbsd",
        "aix" => "aix",
        "android" => "android",
        _ => return None,
    };
    let npm_cpu: &str = match arch.as_str() {
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
    // npm only models a `libc` selector on linux. `gnu`→`glibc` is npm's name.
    let npm_libc = if npm_os == "linux" {
        match anodizer_core::target::libc_from_target(target) {
            "musl" => "musl",
            "gnu" => "glibc",
            _ => "",
        }
    } else {
        ""
    };
    Some(NpmTriple {
        os: npm_os.to_string(),
        cpu: npm_cpu.to_string(),
        libc: npm_libc.to_string(),
    })
}

/// Warn that `targets` (deduplicated, sorted) were excluded from npm coverage
/// because [`npm_triple`] has no mapping for them. Shared by both modes so the
/// operator is never silently left with a platform gap — notably
/// `darwin-universal` (npm has no universal arch) and exotic arches
/// (loong64/mips/sparc64/riscv edge combos, solaris/illumos/ios). No-op when
/// nothing was excluded.
pub(crate) fn warn_excluded_targets(log: &StageLogger, excluded: &[String]) {
    if excluded.is_empty() {
        return;
    }
    let mut uniq: Vec<&String> = excluded.iter().collect();
    uniq.sort();
    uniq.dedup();
    let list = uniq
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    log.warn(&format!(
        "{} target(s) have no npm os/cpu/libc mapping and were excluded from \
         npm packages: {}. Consumers on those platforms will not be able to \
         `npm install` this package (npm has no selector for them — e.g. macOS \
         universal binaries, or exotic arches).",
        uniq.len(),
        list
    ));
}

/// Resolve the effective dist-tag (configured value or [`DEFAULT_TAG`]).
pub(crate) fn resolve_tag(ctx: &Context, cfg: &NpmConfig) -> anyhow::Result<String> {
    let raw = cfg
        .tag
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_TAG);
    ctx.render_template(raw)
        .with_context(|| format!("npm: render tag template {raw:?}"))
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
pub(crate) fn resolve_registry(ctx: &Context, cfg: &NpmConfig) -> anyhow::Result<String> {
    let raw = cfg
        .registry
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REGISTRY);
    let rendered = ctx
        .render_template(raw)
        .with_context(|| format!("npm: render registry template {raw:?}"))?;
    Ok(rendered.trim_end_matches('/').to_string())
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

/// Resolve the version the `postinstall` download URL is rendered with:
/// `cfg.binary_version` (templated) when set, else the release `version`. Lets
/// the npm package version and the fetched native-binary release version
/// diverge (e.g. re-publishing the wrapper against a pinned binary release).
pub(crate) fn resolve_binary_version(
    ctx: &Context,
    cfg: &NpmConfig,
    version: &str,
) -> anyhow::Result<String> {
    match cfg
        .binary_version
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => ctx
            .render_template(raw)
            .with_context(|| format!("npm: render binary_version template {raw:?}")),
        None => Ok(version.to_string()),
    }
}

/// Resolve the env var NAME (NOT value) that holds the npm auth token.
/// Fixed to `NPM_TOKEN` — the canonical npm convention. Stored in evidence
/// so rollback knows which env var to consult.
pub(crate) fn token_env_var(_cfg: &NpmConfig) -> &'static str {
    "NPM_TOKEN"
}

/// Collect the platform-binary download set for one `npms[]` entry
/// (postinstall mode).
///
/// Walks `ctx.artifacts` for `Archive` entries (filtered by `ids:` when set),
/// derives npm os/cpu via [`npm_triple`], and resolves the download URL (via
/// `url_template` or the artifact's `url` metadata).
pub(crate) fn collect_platform_binaries(
    ctx: &Context,
    cfg: &NpmConfig,
    pkg_name: &str,
    version: &str,
    log: &StageLogger,
) -> Result<Vec<PlatformBinary>> {
    let format = resolve_format(cfg).to_string();
    // A `binary`-format archive is a single raw binary per platform; it cannot
    // satisfy a multi-command `bins:` map (only one command's binary would ship,
    // and the other launchers would resolve a file that was never written).
    if format == "binary" {
        let commands = postinstall_commands(cfg, pkg_name);
        if commands.len() > 1 {
            bail!(
                "npm: postinstall entry '{}' uses `format: binary` (one raw binary per \
                 platform) but its `bins:` map declares {} commands — a single binary \
                 download cannot provide them all; use a tar/zip archive format",
                pkg_name,
                commands.len()
            );
        }
    }
    let id_filter = cfg.ids.as_ref();
    let url_template = cfg.url_template.as_deref();

    let mut out: Vec<PlatformBinary> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
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
        // `targets:` allowlist: silently skip targets left out of scope (the
        // no-npm-mapping warning below is a distinct concern).
        if !crate::publisher_helpers::target_in_allowlist(cfg.targets.as_ref(), target) {
            continue;
        }
        // `amd64_variant` / `arm_variant` microarch filter — same as optional-deps
        // mode, so a tuned build ships the chosen microarch in both modes.
        if !artifact_matches_variant(art, cfg) {
            continue;
        }
        let Some(triple) = npm_triple(target) else {
            excluded.push(if target.is_empty() {
                "<no target>".to_string()
            } else {
                target.to_string()
            });
            continue;
        };
        let (os, arch) = anodizer_core::target::map_target(target);
        let sha256 = art.metadata.get("sha256").cloned().unwrap_or_default();
        let url = resolve_artifact_url(ctx, art, url_template, pkg_name, version, &arch, &os);
        out.push(PlatformBinary {
            os: triple.os,
            cpu: triple.cpu,
            url,
            sha256,
            format: format.clone(),
        });
    }
    warn_excluded_targets(log, &excluded);
    out.sort_by(|a, b| a.os.cmp(&b.os).then_with(|| a.cpu.cmp(&b.cpu)));
    // Two archives mapping to the same (os, cpu) is a config bug; drop the
    // duplicate so the manifest doesn't carry colliding entries.
    out.dedup_by(|a, b| a.os == b.os && a.cpu == b.cpu);
    Ok(out)
}

/// Whether an artifact passes the `amd64_variant` / `arm_variant` microarch
/// filter for its arch. Mirrors the homebrew/nix/krew peers: an artifact with
/// no variant metadata always passes; a tuned artifact is kept only when its
/// metadata matches the configured (or default) variant. amd64 defaults to
/// `v1`, arm (32-bit) to `6`. Shared by both npm modes (optional-deps +
/// postinstall) so a tuned build ships the chosen microarch in either.
pub(crate) fn artifact_matches_variant(art: &Artifact, cfg: &NpmConfig) -> bool {
    let amd64_variant = cfg.amd64_variant.map_or("v1", |v| v.as_str());
    let arm_variant = cfg.arm_variant.as_deref().unwrap_or("6");
    let target = art.target.as_deref().unwrap_or("");
    let (_, arch) = anodizer_core::target::map_target(target);
    if arch == "amd64" {
        return art
            .metadata
            .get("amd64_variant")
            .is_none_or(|v| v == amd64_variant);
    }
    if arch.starts_with("arm") && arch != "arm64" {
        return art
            .metadata
            .get("arm_variant")
            .is_none_or(|v| v == arm_variant);
    }
    true
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

/// Insert the shared metadata fields (`description`/`homepage`/`license`/
/// `author`/`keywords`/`repository`/`bugs`/`man`/`contributors`) into a
/// `package.json` root map, honouring the metadata fallbacks. Shared by
/// postinstall + optional-deps metapackage generation.
///
/// `crate_name` selects the owning crate so the per-crate `meta_*_for`
/// resolvers add a `Cargo.toml [package]` fallback: a plain Rust crate (no
/// top-level `metadata:` YAML block) still emits real
/// description/homepage/license/author. In a workspace per-crate config each
/// crate resolves its OWN metadata, never a shared/global value.
pub(crate) fn insert_common_metadata(
    root: &mut BTreeMap<String, serde_json::Value>,
    ctx: &Context,
    cfg: &NpmConfig,
    crate_name: &str,
) {
    let render = |s: &str| ctx.render_template(s).unwrap_or_else(|_| s.to_string());

    let description = cfg.description.as_deref().map(&render).or_else(|| {
        ctx.config
            .meta_description_for(crate_name)
            .map(str::to_string)
    });
    if let Some(d) = description {
        root.insert("description".into(), serde_json::Value::String(d));
    }

    let homepage = cfg
        .homepage
        .as_deref()
        .map(&render)
        .or_else(|| ctx.config.meta_homepage_for(crate_name).map(str::to_string));
    if let Some(h) = homepage {
        root.insert("homepage".into(), serde_json::Value::String(h));
    }

    let license = cfg
        .license
        .as_deref()
        .map(&render)
        .or_else(|| ctx.config.meta_license_for(crate_name).map(str::to_string));
    if let Some(l) = license {
        root.insert("license".into(), serde_json::Value::String(l));
    }

    // Honour the documented `author` fallback: explicit config, else the
    // project's `metadata.maintainers[0]`, else the crate's
    // `Cargo.toml [package].authors[0]` (both via `meta_first_maintainer_for`).
    let author = cfg.author.as_deref().map(&render).or_else(|| {
        ctx.config
            .meta_first_maintainer_for(crate_name)
            .map(str::to_string)
    });
    if let Some(a) = author {
        root.insert("author".into(), serde_json::Value::String(a));
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

    // Repository URL feeds npm provenance validation: when `provenance: true`,
    // npm rejects (E422) any package whose `repository.url` does not match the
    // OIDC-claimed repository. Fall back to the crate's
    // `Cargo.toml [package].repository` so the field is correct by default and
    // never requires the operator to restate it in the publisher config.
    let repository = cfg.repository_url.as_deref().map(&render).or_else(|| {
        ctx.config
            .meta_repository_for(crate_name)
            .map(str::to_string)
    });
    if let Some(repo_url) = repository {
        let mut obj = serde_json::Map::new();
        obj.insert("type".into(), serde_json::Value::String("git".into()));
        obj.insert("url".into(), serde_json::Value::String(repo_url));
        root.insert("repository".into(), serde_json::Value::Object(obj));
    }

    if let Some(bugs) = cfg.bugs.as_deref() {
        let mut obj = serde_json::Map::new();
        obj.insert("url".into(), serde_json::Value::String(render(bugs)));
        root.insert("bugs".into(), serde_json::Value::Object(obj));
    }

    if let Some(man) = cfg.man.as_ref() {
        root.insert(
            "man".into(),
            serde_json::Value::Array(
                man.iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
        );
    }

    if let Some(contributors) = cfg.contributors.as_ref() {
        root.insert(
            "contributors".into(),
            serde_json::Value::Array(
                contributors
                    .iter()
                    .map(|s| serde_json::Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
}

/// Build the two launcher-shim JS fragments for [`NpmConfig::shim_env`]: the
/// `const SHIM_ENV = { ... }` declaration (empty when `shim_env` is unset) and
/// the `spawnSync` options object literal. With no `shim_env` the options are
/// the historical `{ stdio: 'inherit' }`; with it, the child env is
/// `{ ...process.env, ...SHIM_ENV }` so the configured vars win over the
/// inherited environment. Keys/values are JSON-encoded (valid JS literals);
/// the `BTreeMap` ordering keeps the emitted object deterministic. Shared by
/// the optional-deps shim and the postinstall launcher.
pub(crate) fn shim_env_fragments(cfg: &NpmConfig) -> (String, String) {
    match cfg.shim_env.as_ref().filter(|m| !m.is_empty()) {
        Some(env) => {
            let mut items = String::new();
            for (k, v) in env {
                let key = serde_json::to_string(k).unwrap_or_else(|_| format!("{k:?}"));
                let val = serde_json::to_string(v).unwrap_or_else(|_| format!("{v:?}"));
                items.push_str(&format!("  {key}: {val},\n"));
            }
            let decl = format!("const SHIM_ENV = {{\n{items}}};\n");
            let opts = "{ stdio: 'inherit', env: { ...process.env, ...SHIM_ENV } }".to_string();
            (decl, opts)
        }
        None => (String::new(), "{ stdio: 'inherit' }".to_string()),
    }
}

/// Default npm `engines.node` floor when [`NpmConfig::engines`] is unset —
/// the constraint every leading native-CLI wrapper (esbuild, biome, swc)
/// declares as its lower bound.
pub(crate) const DEFAULT_ENGINES_NODE: &str = ">=18";

/// Insert the `engines` map: explicit `cfg.engines` (verbatim, including an
/// empty map → field suppressed) else a derived `{ node: ">=18" }`. The
/// derived default is overridable but never required of the operator.
pub(crate) fn insert_engines(root: &mut BTreeMap<String, serde_json::Value>, cfg: &NpmConfig) {
    let engines: BTreeMap<String, String> = match cfg.engines.as_ref() {
        Some(e) => e.clone(),
        None => {
            let mut d = BTreeMap::new();
            d.insert("node".to_string(), DEFAULT_ENGINES_NODE.to_string());
            d
        }
    };
    if engines.is_empty() {
        return;
    }
    let mut obj = serde_json::Map::new();
    for (k, v) in engines {
        obj.insert(k, serde_json::Value::String(v));
    }
    root.insert("engines".into(), serde_json::Value::Object(obj));
}

/// Insert `publishConfig.provenance`: explicit `cfg.provenance` else `true`
/// (the npm supply-chain norm biome and swc both set). When an `access`
/// value is resolvable it is co-located under the same `publishConfig` object
/// (matching swc's `publishConfig{access,provenance}` shape); `npm publish`
/// still honours the per-run `.npmrc` `access`, so this is purely declarative.
///
/// `provenance_override` lets the live publish path force the emitted value:
/// `Some(v)` writes `v` regardless of `cfg.provenance` (used to degrade to
/// `false` on a runner that cannot mint an npm provenance attestation — see
/// [`runner_supports_npm_provenance`]), while `None` emits the configured
/// value unchanged (the manifest-only / non-publish path keeps the operator's
/// choice). The override is publish-time only and never reaches the
/// byte-compared determinism dist.
pub(crate) fn insert_publish_config(
    root: &mut BTreeMap<String, serde_json::Value>,
    cfg: &NpmConfig,
    provenance_override: Option<bool>,
) {
    let provenance = provenance_override.unwrap_or_else(|| cfg.provenance.unwrap_or(true));
    let mut obj = serde_json::Map::new();
    if let Some(access) = resolve_access(cfg) {
        obj.insert("access".into(), serde_json::Value::String(access));
    }
    obj.insert("provenance".into(), serde_json::Value::Bool(provenance));
    root.insert("publishConfig".into(), serde_json::Value::Object(obj));
}

/// Whether the current runner can produce an npm provenance / Trusted
/// Publishing attestation that the npm registry will accept.
///
/// npm provenance is minted from a GitHub Actions OIDC token and the registry
/// only verifies the sigstore bundle for **GitHub-hosted** runners; on a
/// self-hosted runner the publish is rejected with an `E422 Unprocessable
/// Entity` whose body reads `Error verifying sigstore provenance bundle:
/// Unsupported GitHub Actions runner`. GitHub Actions sets
/// `RUNNER_ENVIRONMENT=github-hosted` on hosted runners and `self-hosted` on
/// self-hosted ones.
///
/// Conservative: only the proven-incompatible case is reported unsupported —
/// running under GitHub Actions (`GITHUB_ACTIONS == "true"`) with
/// `RUNNER_ENVIRONMENT` set to anything other than `github-hosted`. Every other
/// environment (GitHub-hosted, or any non-GitHub-Actions CI / local run) is
/// left as configured so other ecosystems are never over-degraded.
pub(crate) fn runner_supports_npm_provenance(
    env: &dyn anodizer_core::env_source::EnvSource,
) -> bool {
    if env.var("GITHUB_ACTIONS").as_deref() != Some("true") {
        return true;
    }
    match env.var("RUNNER_ENVIRONMENT") {
        Some(v) => v == "github-hosted",
        // GITHUB_ACTIONS=true but RUNNER_ENVIRONMENT unset: not a known-hosted
        // runner. Treat as unsupported so a misreporting self-hosted runner
        // (which is the env this guard exists for) cannot 422 the release.
        None => false,
    }
}

/// Resolve the provenance value the live publish should emit for `pkg`,
/// applying the runner-capability gate.
///
/// Returns `Some(override)` to force the emitted `publishConfig.provenance`
/// when the configured request must be downgraded, or `None` to emit the
/// configured value unchanged. Provenance is downgraded to `false` (with an
/// actionable `log.warn`) only when it was *requested* (explicit `true` or the
/// unset default) AND [`runner_supports_npm_provenance`] is false; an explicit
/// `provenance: false` stays false with no spurious warning.
pub(crate) fn effective_provenance_override(
    ctx: &Context,
    cfg: &NpmConfig,
    pkg: &str,
    log: &StageLogger,
) -> Option<bool> {
    let requested = cfg.provenance.unwrap_or(true);
    if !requested {
        return None;
    }
    if runner_supports_npm_provenance(ctx.env_source()) {
        return None;
    }
    let runner_env = ctx
        .env_source()
        .var("RUNNER_ENVIRONMENT")
        .unwrap_or_else(|| "<unset>".to_string());
    log.warn(&format!(
        "npm provenance requested but unsupported on this runner \
         (RUNNER_ENVIRONMENT={runner_env}); npm provenance/Trusted Publishing \
         requires a GitHub-hosted runner. Publishing '{pkg}' WITHOUT provenance. \
         Run the publish on a GitHub-hosted runner to retain provenance."
    ));
    Some(false)
}

/// Resolve the `files` allowlist for a package: explicit `cfg.files`
/// (verbatim, including an empty list → field suppressed) else
/// `derived_entries` (the binary / shim / launcher this package actually
/// ships) unioned with the basenames of any `extra_files` globs, sorted +
/// de-duplicated for deterministic emission.
pub(crate) fn insert_files(
    root: &mut BTreeMap<String, serde_json::Value>,
    cfg: &NpmConfig,
    derived_entries: &[String],
) {
    let files: Vec<String> = match cfg.files.as_ref() {
        Some(f) => f.clone(),
        None => {
            let mut set: std::collections::BTreeSet<String> =
                derived_entries.iter().cloned().collect();
            for pattern in resolve_extra_files(cfg) {
                // The published basename of an `extra_files` glob (e.g.
                // `README*` / `LICENSE*`) is what lands in the package dir;
                // emit the trailing path component, dropping any glob dir.
                let base = pattern.rsplit('/').next().unwrap_or(&pattern);
                set.insert(base.to_string());
            }
            set.into_iter().collect()
        }
    };
    if files.is_empty() {
        return;
    }
    root.insert(
        "files".into(),
        serde_json::Value::Array(files.into_iter().map(serde_json::Value::String).collect()),
    );
}

/// Serialize a `package.json` root map deterministically (alphabetical key
/// order via the `BTreeMap`), applying the `extra:` shallow-merge last so
/// config-author keys win over generated ones.
pub(crate) fn finalize_package_json(
    mut root: BTreeMap<String, serde_json::Value>,
    cfg: &NpmConfig,
) -> Result<String> {
    if let Some(extra) = cfg.extra.as_ref() {
        for (k, v) in extra {
            root.insert(k.clone(), v.clone());
        }
    }
    let mut ordered = serde_json::Map::new();
    for (k, v) in root {
        ordered.insert(k, v);
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(ordered))
        .context("npm: serialize package.json")
}

/// Resolve the postinstall command set as `(command, target-binary-basename)`
/// pairs: every [`NpmConfig::bins`] entry when set (each command running its
/// mapped binary), else the single package-basename command running the
/// same-named binary. The launcher for `command` lives at `bin/<command>.js`
/// and execs `<target>{,.exe}` from the extracted `bin/` directory.
pub(crate) fn postinstall_commands(cfg: &NpmConfig, pkg_name: &str) -> Vec<(String, String)> {
    let bins: Vec<(&String, &String)> = cfg
        .bins
        .as_ref()
        .map(|m| {
            m.iter()
                .filter(|(k, v)| !k.trim().is_empty() && !v.trim().is_empty())
        })
        .into_iter()
        .flatten()
        .collect();
    if bins.is_empty() {
        let base = pkg_name.rsplit('/').next().unwrap_or(pkg_name).to_string();
        return vec![(base.clone(), base)];
    }
    bins.into_iter()
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

/// Render the `package.json` content for one `npms[]` entry (postinstall mode).
///
/// `crate_name` selects the owning crate for the per-crate metadata resolvers
/// (`pkg_name` is the published npm name, which may be a scoped alias that
/// shares nothing with the crate name). `provenance_override` is threaded into
/// [`insert_publish_config`] so the live publish can degrade provenance on a
/// runner that cannot mint an attestation.
pub(crate) fn render_package_json(
    ctx: &Context,
    cfg: &NpmConfig,
    pkg_name: &str,
    crate_name: &str,
    version: &str,
    binaries: &[PlatformBinary],
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

    // `bin` map: each command points at its postinstall-installed launcher.
    // BTreeMap keeps the emitted map sorted for determinism.
    let commands = postinstall_commands(cfg, pkg_name);
    let mut bin_deps: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for (command, _target) in &commands {
        bin_deps.insert(
            command.clone(),
            serde_json::Value::String(format!("bin/{}.js", command)),
        );
    }
    let mut bin = serde_json::Map::new();
    for (k, v) in bin_deps {
        bin.insert(k, v);
    }
    root.insert("bin".into(), serde_json::Value::Object(bin));

    // Postinstall packages ship: package.json (implicit), the postinstall
    // script, and one launcher under bin/ per command. `files` makes that
    // explicit.
    let mut derived_files = vec!["postinstall.js".to_string()];
    for (command, _target) in &commands {
        derived_files.push(format!("bin/{}.js", command));
    }
    insert_files(&mut root, cfg, &derived_files);

    let mut scripts = serde_json::Map::new();
    scripts.insert(
        "postinstall".into(),
        serde_json::Value::String("node ./postinstall.js".into()),
    );
    root.insert("scripts".into(), serde_json::Value::Object(scripts));

    // Embedded binary table consumed by `postinstall.js` to look up the
    // download URL for the runtime's platform/cpu.
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

    finalize_package_json(root, cfg)
}

/// Render the `postinstall.js` shim (postinstall mode). The script reads the
/// embedded `anodize.binaries` table, selects the `process.platform` +
/// `process.arch` entry, downloads + sha256-verifies the archive, and extracts
/// every command binary in `targets` into `bin/<target>{,.exe}` so each
/// `npx <command>` works. `targets` is the set of binary basenames the package
/// installs (one per `bins` command, or the single package basename). No
/// third-party deps.
pub(crate) fn render_postinstall_js(targets: &[String]) -> String {
    // A `binary`-format archive carries exactly one binary, so it is written
    // under the first target's name; multi-command packages ship an archive
    // (tar/zip) whose extraction drops every target binary into bin/.
    let targets_js = targets
        .iter()
        .map(|t| serde_json::to_string(t).unwrap_or_else(|_| format!("{t:?}")))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"#!/usr/bin/env node
// SPDX-License-Identifier: MIT
// Generated by anodizer (https://github.com/tj-smith47/anodizer) — do not edit by hand.
//
// Selects the platform-matching binary archive from package.json, downloads
// it, verifies its sha256, and extracts each command binary into
// ./bin/<target>{{.exe?}} so every `npx <command>` works.
const fs = require('fs');
const path = require('path');
const https = require('https');
const crypto = require('crypto');
const {{ execSync }} = require('child_process');

const TARGETS = [{targets_js}];
const exeSuffix = process.platform === 'win32' ? '.exe' : '';

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
  // A `binary`-format archive IS a single binary → name it after the first
  // target; archive formats extract every target binary into bin/.
  if (target.format === 'binary') {{
    fs.copyFileSync(archivePath, path.join(binDir, TARGETS[0] + exeSuffix));
  }} else if (target.format === 'zip') {{
    execSync(`unzip -o "${{archivePath}}" -d "${{binDir}}"`, {{ stdio: 'inherit' }});
  }} else if (target.format === 'tar') {{
    execSync(`tar -xf "${{archivePath}}" -C "${{binDir}}"`, {{ stdio: 'inherit' }});
  }} else {{
    execSync(`tar -xzf "${{archivePath}}" -C "${{binDir}}"`, {{ stdio: 'inherit' }});
  }}
  fs.unlinkSync(archivePath);
  if (process.platform !== 'win32') {{
    for (const t of TARGETS) {{
      try {{ fs.chmodSync(path.join(binDir, t + exeSuffix), 0o755); }} catch (_) {{}}
    }}
  }}
}})().catch(err => {{
  console.error(`[anodize/npm] postinstall failed: ${{err.message}}`);
  process.exit(1);
}});
"#,
        targets_js = targets_js
    )
}

/// Render the `bin/<command>.js` launcher that npm symlinks into
/// `node_modules/.bin/<command>` (postinstall mode). It invokes the native
/// `target` binary the postinstall script dropped into `bin/<target>{,.exe}`,
/// injecting [`NpmConfig::shim_env`] into the spawned child's environment
/// (shared `shim_env_fragments` idiom with the optional-deps shim).
pub(crate) fn render_launcher_js(cfg: &NpmConfig, command: &str, target: &str) -> String {
    let (shim_env_decl, spawn_opts) = shim_env_fragments(cfg);
    // JSON-encode the user-supplied `bins` key/value into JS string literals so
    // a quote/backslash/backtick in a command or target name can never produce
    // broken or injectable JS (mirrors the TARGETS encoding in postinstall.js).
    let command_js = serde_json::to_string(command).unwrap_or_else(|_| format!("{command:?}"));
    let target_js = serde_json::to_string(target).unwrap_or_else(|_| format!("{target:?}"));
    format!(
        r#"#!/usr/bin/env node
// SPDX-License-Identifier: MIT
// Generated by anodizer (https://github.com/tj-smith47/anodizer) — do not edit by hand.
const path = require('path');
const {{ spawnSync }} = require('child_process');
{shim_env_decl}
const CMD = {command_js};
const base = {target_js};
const exe = process.platform === 'win32' ? base + '.exe' : base;
const bin = path.join(__dirname, exe);
const result = spawnSync(bin, process.argv.slice(2), {spawn_opts});
if (result.error) {{
  console.error(
    `[${{CMD}}] failed to launch ${{bin}}: ${{result.error.message}}; ` +
    `the postinstall step may not have completed — try reinstalling the package`
  );
  process.exit(1);
}}
process.exit(result.status === null ? 1 : result.status);
"#,
        shim_env_decl = shim_env_decl,
        spawn_opts = spawn_opts,
        command_js = command_js,
        target_js = target_js,
    )
}
