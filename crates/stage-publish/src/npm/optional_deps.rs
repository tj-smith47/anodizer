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

use std::collections::{BTreeMap, BTreeSet};

use anodizer_core::artifact::ArtifactKind;
use anodizer_core::build_plan::crate_build_target_entries;
use anodizer_core::config::{NpmConfig, UniversalBinaryConfig};
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result, bail};

use super::manifest::{
    NpmTriple, finalize_package_json, insert_common_metadata, insert_engines, insert_files,
    insert_publish_config, npm_triple, warn_excluded_targets,
};

/// One native binary embedded in a per-platform package: its on-disk source
/// and the package-relative path it lands under (`cli`, `cli.exe`, or
/// `bin/git-cliff` when `platform_bin_dir` is set). A single-command package
/// carries one; a multi-command `bins:` package carries one per command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EmbeddedBinary {
    /// Package-relative path the binary is embedded under.
    pub subpath: String,
    /// On-disk path of the binary to embed (mode `0o755`).
    pub src: std::path::PathBuf,
}

/// One per-platform package emitted in `optional-deps` mode.
///
/// `name` is the full npm name (`<scope>/<bin>-<os>-<cpu>[-<libc>]`).
/// `package_json` is the rendered manifest carrying the `os`/`cpu`/`libc`
/// selectors. `binaries` are every native binary the package embeds — one for
/// a single-command tool, or one per command for a multi-command `bins:` tool
/// whose per-command launcher shims each resolve their own binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlatformPackage {
    /// Full npm package name (e.g. `@scope/cli-linux-x64-musl`).
    pub name: String,
    /// npm selection triple this package targets (derived from the target).
    pub triple: NpmTriple,
    /// Rendered `package.json` for the per-platform package.
    pub package_json: String,
    /// Native binaries embedded in this package (≥1), each at its own subpath.
    pub binaries: Vec<EmbeddedBinary>,
}

/// A per-platform package under construction, before its `package.json` is
/// rendered. Held separately so multi-command `bins:` binaries for the same
/// platform can be MERGED into one package (each command's launcher resolves
/// its own binary) before the `files` allowlist — which must list every
/// embedded binary — is baked into `package.json`.
struct RawPlatform {
    pkg_name: String,
    triple: NpmTriple,
    binaries: Vec<EmbeddedBinary>,
}

/// The rendered metapackage file pair (`package.json` + `shim.js`), grouped so
/// a skipped metapackage is a single `None` rather than two fields that could
/// drift apart.
#[derive(Debug, Clone)]
pub(crate) struct MetapackageFiles {
    /// Rendered metapackage `package.json` (carries `optionalDependencies` +
    /// `bin`).
    pub package_json: String,
    /// Rendered launcher shims — one per emitted command (`shim.js` for the
    /// single-command default, `<command>.js` per entry when `bins` is set).
    pub shims: Vec<NpmShim>,
}

/// One generated metapackage launcher shim: the filename npm's `bin` map points
/// at, plus its rendered JavaScript body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NpmShim {
    /// Shim filename inside the package (`shim.js` or `<command>.js`).
    pub filename: String,
    /// Rendered shim body.
    pub contents: String,
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

/// The trimmed, slash-stripped `platform_bin_dir` (e.g. `bin`), or `None` when
/// unset/blank — the binary then lands at the package root.
fn platform_bin_dir(cfg: &NpmConfig) -> Option<&str> {
    cfg.platform_bin_dir
        .as_deref()
        .map(|s| s.trim().trim_matches('/'))
        .filter(|s| !s.is_empty())
}

/// Join a package-relative binary path from an optional subdir + filename:
/// `Some("bin")` + `git-cliff` → `bin/git-cliff`; `None` → `git-cliff`.
fn join_bin_dir(dir: Option<&str>, name: &str) -> String {
    match dir {
        Some(d) => format!("{d}/{name}"),
        None => name.to_string(),
    }
}

/// One command the metapackage installs: the `bin`-map key, the launcher shim
/// filename that key points at, and the per-platform binary the shim resolves
/// (`None` = the platform package's own embedded binary — the single-command
/// default; `Some(name)` = a specific binary filename from a `bins` entry).
struct MetaCommand {
    /// `bin`-map key (the CLI command name).
    command: String,
    /// Launcher shim filename (`shim.js` or `<command>.js`).
    shim_file: String,
    /// Binary filename to resolve inside the selected platform package.
    target: Option<String>,
}

/// Resolve the command set the metapackage emits: every `bins` entry (each with
/// its own `<command>.js` shim resolving the mapped binary) when set, else the
/// single `bin:`-derived command with the default `shim.js`.
fn resolve_commands(cfg: &NpmConfig, metapackage: &str) -> Vec<MetaCommand> {
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
        return vec![MetaCommand {
            command: resolve_bin(cfg, metapackage).to_string(),
            shim_file: "shim.js".to_string(),
            target: None,
        }];
    }
    bins.into_iter()
        .map(|(cmd, bin)| MetaCommand {
            command: cmd.trim().to_string(),
            shim_file: format!("{}.js", cmd.trim()),
            target: Some(bin.trim().to_string()),
        })
        .collect()
}

const DARWIN_ARM64: &str = "aarch64-apple-darwin";
const DARWIN_X64: &str = "x86_64-apple-darwin";

/// Statically mirror `universal::resolve_default_unibin_ids` for the case
/// where a `universal_binaries[]` entry leaves `ids:` unset (or empty):
/// `ub.id` if explicitly set; else `project_name` when some build entry for
/// this crate resolves to that exact id; else the crate name. Production
/// answers the middle branch by probing already-registered `Binary` artifact
/// metadata; every input that probe reads is already known statically before
/// any build runs — `ub.id`, `project_name`, and this crate's own per-build
/// ids (`resolved_per_build`, already rendered where production would render
/// them) — so the same three-way precedence is fully reproducible here.
fn default_unibin_ids_static(
    ub: &UniversalBinaryConfig,
    crate_name: &str,
    project_name: &str,
    resolved_per_build: &[(String, BTreeSet<String>)],
) -> Vec<String> {
    if let Some(ref id) = ub.id {
        return vec![id.clone()];
    }
    if !project_name.is_empty() && resolved_per_build.iter().any(|(id, _)| id == project_name) {
        return vec![project_name.to_string()];
    }
    vec![crate_name.to_string()]
}

/// Whether a `universal_binaries[]` entry with `replace: true` retires
/// `DARWIN_ARM64` + `DARWIN_X64` from `crate_targets` for THIS entry, mirroring
/// `stage-build/src/universal.rs::build_universal_binary`'s own precondition
/// for running lipo and removing the source artifacts.
///
/// Two static checks, both required, mirroring the real gate exactly:
///
/// 1. **Both-required precondition** (`build_universal_binary`'s early-return
///    when either `arm64`/`x86_64` binary is absent): lipo is a no-op unless
///    the crate's build plan produces BOTH darwin triples at all, so an entry
///    can only retire them when `crate_targets` already contains both.
/// 2. **`ids:` selection** (`effective_ids` filtering `by_kind_and_crate` down
///    to the entry's chosen builds before the arm64/x86_64 search): the
///    narrowing id list — an explicit, non-empty `ub.ids`, or else the
///    statically-resolved default from [`default_unibin_ids_static`] — is
///    matched against each surviving build's id in `resolved_per_build`
///    (already resolved the same way `stage-build` resolves a `Binary`
///    artifact's `id` metadata: an explicit `build.id` verbatim, or the
///    rendered `binary`-fallback — see [`crate_build_target_entries`]'s
///    doc). The pair is only retired when some combination of id-matched
///    builds covers both triples. An explicit `ub.ids: []` is production's
///    "no filter" case (`effective_ids.is_empty()` falls through to the
///    default resolution), so it is treated identically to `None`.
fn universal_replace_retires_darwin_pair(
    ub: &UniversalBinaryConfig,
    crate_name: &str,
    project_name: &str,
    crate_targets: &BTreeSet<String>,
    resolved_per_build: &[(String, BTreeSet<String>)],
) -> bool {
    if ub.replace != Some(true) {
        return false;
    }
    if !(crate_targets.contains(DARWIN_ARM64) && crate_targets.contains(DARWIN_X64)) {
        return false;
    }
    let narrowing_ids: Vec<String> =
        match ub.ids.as_deref().filter(|ids: &&[String]| !ids.is_empty()) {
            Some(ids) => ids.to_vec(),
            None => default_unibin_ids_static(ub, crate_name, project_name, resolved_per_build),
        };
    let (mut arm64_selected, mut x64_selected) = (false, false);
    for (id, targets) in resolved_per_build {
        if narrowing_ids.iter().any(|nid| nid == id) {
            arm64_selected |= targets.contains(DARWIN_ARM64);
            x64_selected |= targets.contains(DARWIN_X64);
        }
    }
    arm64_selected && x64_selected
}

/// The npm platform identities (`platform_suffix` values, e.g.
/// `linux-x64-glibc`) that a completeness gate REQUIRES an artifact for, each
/// mapped to the source target triple(s) it was derived from (so an error
/// message can point the user at the exact `targets:` self-service value).
///
/// Derived from the crate's CONFIGURED build targets — not from whatever
/// binaries happen to be present in `ctx.artifacts` this run — via the same
/// build-planning SSOT ([`crate_build_target_entries`],
/// [`anodizer_core::build_plan::crate_target_list`]'s sibling) that
/// `stage-publish::publisher_helpers::crate_build_targets` composes for every
/// other per-target publisher's `targets:` allowlist check, plus this entry's
/// own `BuildConfig.skip` veto (a skip-truthy build compiles nothing —
/// `stage-build` honors `skip` before ever invoking cargo — so its targets
/// must not be expected either; render failures fall back to "not skipped",
/// matching `cross_requirements.rs`'s hint-over-reports-rather-than-drops
/// posture). Mirrors the entry's own narrowing knobs exactly as
/// [`generate_layout`]'s artifact loop applies them: `ids:` selects which
/// crates' targets count, `targets:` further narrows the triples, and
/// `libc_aware` decides whether a linux target's glibc/musl variant is a
/// distinct platform identity. A triple with no npm os/cpu mapping
/// (`npm_triple` returns `None`) contributes nothing — it is out of npm's
/// scope entirely, not a missing platform.
fn expected_platform_identities(
    ctx: &Context,
    cfg: &NpmConfig,
    libc_aware: bool,
) -> BTreeMap<String, BTreeSet<String>> {
    let default_targets = ctx.config.effective_default_targets();
    let mut targets: BTreeSet<String> = BTreeSet::new();
    for c in ctx.config.crate_universe() {
        let selected = match cfg.ids.as_ref() {
            Some(ids) => ids.iter().any(|id| id == &c.name),
            None => true,
        };
        if !selected {
            continue;
        }
        // A per-crate workspace run only builds and stages artifacts for
        // `ctx.options.selected_crates` (populated by `--crate`); a sibling
        // crate that carries this npm entry via implicit-all (`ids` unset)
        // but was never dispatched this run contributes no artifacts, so its
        // targets must not inflate the expected set either. Empty
        // `selected_crates` means "every crate is in scope" (single-crate and
        // lockstep runs), mirroring `publisher_helpers::effective_publish_crates`.
        if !ctx.options.selected_crates.is_empty() && !ctx.options.selected_crates.contains(&c.name)
        {
            continue;
        }

        let skip_evaluator = |build: &anodizer_core::config::BuildConfig| {
            build
                .skip
                .as_ref()
                .map(|s| {
                    s.try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                        .unwrap_or(false)
                })
                .unwrap_or(false)
        };
        let entries = crate_build_target_entries(c, &default_targets, skip_evaluator);

        let mut crate_targets: BTreeSet<String> = BTreeSet::new();
        let mut per_build: Vec<(String, BTreeSet<String>)> = Vec::new();
        for entry in &entries {
            let mut this_build_targets: BTreeSet<String> = BTreeSet::new();
            for t in &entry.targets {
                if crate::publisher_helpers::target_in_allowlist(cfg.targets.as_ref(), t) {
                    crate_targets.insert(t.clone());
                    this_build_targets.insert(t.clone());
                }
            }
            // Resolved to the exact string `stage-build`'s `artifact_meta`
            // would stamp on this build's `Binary` artifact: an explicit
            // `build.id` verbatim, or the `binary`-fallback id rendered
            // through the active context — only the fallback is ever
            // templated in production, so an `Explicit` id must never be
            // rendered here either.
            let resolved_id = match &entry.id {
                anodizer_core::build_plan::BuildId::Explicit(raw) => raw.clone(),
                anodizer_core::build_plan::BuildId::BinaryFallback(raw) => {
                    ctx.render_template(raw).unwrap_or_else(|_| raw.clone())
                }
            };
            per_build.push((resolved_id, this_build_targets));
        }

        // A `universal_binaries[]` entry with `replace: true` runs `lipo` on
        // the two darwin per-arch binaries and REMOVES them from the artifact
        // registry (`stage-build/src/universal.rs::build_universal_binary`),
        // leaving only a `UniversalBinary`-kind artifact whose target
        // (`darwin-universal`) has no npm os/cpu mapping (`npm_triple`
        // returns `None` for it). Retiring the pair from the expected set is
        // gated by `universal_replace_retires_darwin_pair` mirroring lipo's
        // own both-required + `ids:` preconditions — an entry that cannot
        // possibly reach both darwin triples leaves them expected, since a
        // genuinely dropped darwin shard must still fail the gate.
        for ub in c.universal_binaries.iter().flatten() {
            if universal_replace_retires_darwin_pair(
                ub,
                &c.name,
                ctx.config.project_name.as_str(),
                &crate_targets,
                &per_build,
            ) {
                crate_targets.remove(DARWIN_ARM64);
                crate_targets.remove(DARWIN_X64);
            }
        }

        targets.extend(crate_targets);
    }
    let mut by_identity: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for t in targets {
        if let Some(triple) = npm_triple(&t) {
            by_identity
                .entry(platform_suffix(&triple, libc_aware))
                .or_default()
                .insert(t);
        }
    }
    by_identity
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

/// The libc selector of the dedup-preferred configured triple among the
/// triples an identity maps to (mirrors the `!libc_aware` glibc/musl
/// collapse's own tie-break: glibc wins over musl over anything else).
/// `None` only when none of the triples resolve to a known npm libc, which
/// cannot happen for triples already admitted into `expected_platform_identities`.
fn expected_winning_libc(triples: &BTreeSet<String>) -> Option<String> {
    triples
        .iter()
        .filter_map(|t| npm_triple(t).map(|nt| nt.libc))
        .min_by_key(|libc| libc_dedup_rank(libc))
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

/// True when `part` is a legal npm name part (a scope name or a package
/// name): non-empty, lowercase URL-safe characters (`a-z 0-9 - _ . ~`), no
/// leading `.`/`_`. Shared by package-name and scope validation.
fn npm_name_part_ok(part: &str) -> bool {
    !part.is_empty()
        && !part.starts_with('.')
        && !part.starts_with('_')
        && part.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | '~')
        })
}

/// Validate a configured `scope:` shape once up front: `@` followed by a
/// legal npm name part. Errors blame the scope itself, so a bad scope is
/// caught identically on the default and template naming paths at config
/// time instead of surfacing as a registry 4xx (or a confusing rendered-name
/// error) mid-release.
fn validate_npm_scope(scope: &str) -> Result<()> {
    let ok = scope
        .strip_prefix('@')
        .is_some_and(|rest| !rest.contains('/') && npm_name_part_ok(rest));
    if !ok {
        bail!(
            "npm: `scope:` value '{}' is not a legal npm scope — it must be '@' \
             followed by a lowercase URL-safe name (e.g. scope: \"@acme\")",
            scope
        );
    }
    Ok(())
}

/// Validate `name` against npm's package-name rules: ≤214 chars, lowercase
/// URL-safe characters (`a-z 0-9 - _ . ~`), no leading `.`/`_`, scoped names
/// as `@scope/name` with both parts non-empty.
fn validate_npm_package_name(name: &str) -> Result<()> {
    let part_ok = npm_name_part_ok;
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
/// Beyond the standard release context, seeds the per-platform naming vars
/// from [`NpmTriple::name_template_vars`] (`Os`/`Arch`/`Target` +
/// `NpmOs`/`NpmCpu`/`NpmLibc`). A rendered name without a leading `@` is
/// prefixed with `scope` when one is configured; the final name is validated
/// as a legal npm name.
fn render_platform_name(
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
fn render_platform_json(
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
fn render_metapackage_json(
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
fn render_shim_js(
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
    if let Some(s) = scope {
        validate_npm_scope(s)?;
    }
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
    let mut kind_searched = ArtifactKind::UploadableBinary;
    if binaries.is_empty() {
        binaries = ctx.artifacts.by_kind(ArtifactKind::Binary);
        kind_searched = ArtifactKind::Binary;
    }

    let mut raws: Vec<RawPlatform> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    for art in binaries {
        if let Some(ids) = id_filter
            && !ids.iter().any(|id| id == &art.crate_name)
        {
            continue;
        }
        let target = art.target.as_deref().unwrap_or("");
        // `targets:` allowlist: a triple deliberately left out of scope is
        // silently skipped (NOT routed through warn_excluded_targets, which is
        // for targets npm has no os/cpu/libc mapping for — a different concern).
        if !crate::publisher_helpers::target_in_allowlist(cfg.targets.as_ref(), target) {
            continue;
        }
        // `amd64_variant` / `arm_variant` microarch filter: a tuned build whose
        // variant metadata does not match the configured (or default) variant is
        // dropped so only the chosen microarch lands in each package.
        if !super::manifest::artifact_matches_variant(art, cfg) {
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
        let pkg_name = match naming {
            PlatformNaming::Default { scope } => {
                let suffix = platform_suffix(&triple, libc_aware);
                let name = format!("{}/{}-{}", scope, bin, suffix);
                validate_npm_package_name(&name)?;
                name
            }
            PlatformNaming::Template { template, scope } => {
                render_platform_name(ctx, template, scope, target, &triple)?
            }
        };
        let subpath = join_bin_dir(platform_bin_dir(cfg), &art.name);
        raws.push(RawPlatform {
            pkg_name,
            triple,
            binaries: vec![EmbeddedBinary {
                subpath,
                src: art.path.clone(),
            }],
        });
    }
    warn_excluded_targets(log, &excluded);

    // ORDER-COUPLED PASSES: the merge, the libc collapse, and the collision
    // bail below all assume this name-sorted order — each only compares
    // ADJACENT entries, so reordering or skipping the sort silently breaks all
    // three.
    // Sort by name, breaking ties on libc so the collapse below has a
    // deterministic winner instead of one defined by artifact-insertion order.
    // When not libc-aware, a linux musl and glibc binary share the same package
    // name; `libc_dedup_rank` ranks glibc ahead of musl so the collapse (which
    // keeps the first of each run) always keeps the glibc binary. glibc is the
    // broadest-compatibility default for a single fallback linux package.
    raws.sort_by(|a, b| {
        a.pkg_name
            .cmp(&b.pkg_name)
            .then_with(|| libc_dedup_rank(&a.triple.libc).cmp(&libc_dedup_rank(&b.triple.libc)))
    });
    // Merge adjacent packages that share a (name, triple): a multi-command
    // `bins:` tool emits one binary artifact per command for the SAME platform,
    // and they must co-locate in ONE npm package (each command's launcher shim
    // resolves its own binary) rather than collide as duplicate package names.
    // Distinct subpaths embed side by side; a repeated subpath (a duplicate
    // artifact) is kept once. `Vec::dedup_by` can only drop, so the merge is
    // hand-rolled.
    let mut merged: Vec<RawPlatform> = Vec::with_capacity(raws.len());
    for raw in raws {
        if let Some(last) = merged.last_mut()
            && last.pkg_name == raw.pkg_name
            && last.triple == raw.triple
        {
            for b in raw.binaries {
                if !last.binaries.iter().any(|e| e.subpath == b.subpath) {
                    last.binaries.push(b);
                }
            }
            continue;
        }
        merged.push(raw);
    }
    // When not libc-aware, two linux packages (musl + glibc) collapse to the
    // same name; drop the duplicate so optionalDependencies has no colliding
    // key. The sort above guarantees glibc precedes musl, so the glibc package
    // (with its full binary set) is retained. The collapse only spans a libc
    // difference — same-name packages for DIFFERENT os/cpu pairs are a naming
    // bug caught below, not silently merged.
    if !libc_aware {
        merged.dedup_by(|a, b| {
            a.pkg_name == b.pkg_name && a.triple.os == b.triple.os && a.triple.cpu == b.triple.cpu
        });
    }
    // Any duplicate name that survives the collapse above means two distinct
    // platforms rendered the same package name — with the default scheme that
    // is impossible, so this is a platform_name_template that omits a
    // distinguishing var (e.g. NpmLibc while libc_aware is true).
    let mut colliding: Vec<&str> = merged
        .windows(2)
        .filter(|w| w[0].pkg_name == w[1].pkg_name)
        .map(|w| w[0].pkg_name.as_str())
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

    if merged.is_empty() {
        bail!(
            "npm: metapackage '{}' has no binary artifacts matching any npm platform; \
             nothing to publish (optional-deps mode requires per-target binaries)",
            metapackage
        );
    }

    // Completeness gate: every platform the crate's CONFIGURED targets expect
    // must have a matching artifact, or the metapackage would publish with a
    // narrower `optionalDependencies` than what the config promises — and npm
    // versions are immutable, so a partial platform set at this version can
    // never be repaired. Runs before any package.json is rendered or staged.
    let expected = expected_platform_identities(ctx, cfg, libc_aware);
    // `merged` is already collapsed to at most one entry per identity (the
    // `!libc_aware` glibc/musl dedup above keeps the glibc entry when both
    // are present), so this libc is whichever triple is actually backing the
    // identity right now — not necessarily the one the config promises.
    let actual_libc_by_identity: BTreeMap<String, String> = merged
        .iter()
        .map(|r| {
            (
                platform_suffix(&r.triple, libc_aware),
                r.triple.libc.clone(),
            )
        })
        .collect();
    // An identity is a gap either when no artifact backs it at all, or when
    // it maps more than one configured triple (the `!libc_aware` collapse)
    // and the artifact actually present is not the dedup-preferred one — a
    // dropped glibc shard backfilled by a surviving musl artifact for the
    // same identity must still fail the gate, since the config's presence of
    // a glibc target promises glibc content, and npm's immutable versions
    // mean that promise can never be silently downgraded to musl.
    let missing: Vec<(&String, &BTreeSet<String>)> = expected
        .iter()
        .filter(
            |(identity, triples)| match actual_libc_by_identity.get(*identity) {
                None => true,
                Some(actual_libc) => expected_winning_libc(triples)
                    .is_some_and(|winning_libc| *actual_libc != winning_libc),
            },
        )
        .collect();
    if !missing.is_empty() {
        let list = missing
            .iter()
            .map(|(identity, triples)| {
                let triple_list = triples
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("/");
                format!("{identity} ({triple_list})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "npm: metapackage '{}' is missing per-platform package(s) for the configured \
             target(s) {list} — no matching artifact found under {kind_searched:?}. Possible \
             causes: a dist merge dropped the shard; the build never produced it; a variant \
             filter (upx/strip/etc.) excluded it; or the narrowing was intentional. If it was \
             intentional, self-service by narrowing this npm entry's own `targets:` allowlist \
             (using the triple(s) shown in parentheses above) or `ids:` crate list to match; \
             refusing to publish a metapackage with incomplete optionalDependencies, since npm \
             versions are immutable and a partial platform set can never be repaired at this \
             version",
            metapackage
        );
    }

    // Render each `package.json` now that every package's full binary set — and
    // thus its `files` allowlist — is final.
    let mut platforms: Vec<PlatformPackage> = Vec::with_capacity(merged.len());
    for raw in merged {
        let subpaths: Vec<String> = raw.binaries.iter().map(|b| b.subpath.clone()).collect();
        let package_json = render_platform_json(
            ctx,
            cfg,
            &raw.pkg_name,
            crate_name,
            &subpaths,
            version,
            &raw.triple,
            libc_aware,
            provenance_override,
        )?;
        platforms.push(PlatformPackage {
            name: raw.pkg_name,
            triple: raw.triple,
            package_json,
            binaries: raw.binaries,
        });
    }

    let metapackage_files = if skip_metapackage {
        None
    } else {
        let commands = resolve_commands(cfg, &metapackage);
        let package_json = render_metapackage_json(
            ctx,
            cfg,
            &metapackage,
            crate_name,
            &commands,
            version,
            &platforms,
            provenance_override,
        )?;
        let bin_dir = platform_bin_dir(cfg);
        let shims = commands
            .iter()
            .map(|c| NpmShim {
                filename: c.shim_file.clone(),
                contents: render_shim_js(cfg, &c.command, c.target.as_deref(), bin_dir, &platforms),
            })
            .collect();
        Some(MetapackageFiles {
            package_json,
            shims,
        })
    };

    Ok(OptionalDepsLayout {
        metapackage,
        metapackage_files,
        platforms,
    })
}
