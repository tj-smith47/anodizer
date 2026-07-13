//! The `install-script` stage: emit a deterministic POSIX `install.sh`
//! (`curl | sh`) release asset.
//!
//! The per-platform `os-arch → asset` case table, the `uname -s`/`uname -m`
//! detection arms, and the supported-platform list are ALL derived from the
//! release's configured targets by the engine
//! ([`anodizer_core::installer::render_installer_cases`]) — the same SSOT that
//! keeps cargo-binstall's `pkg_url` from 404ing. The stage never reads produced
//! artifacts and never hand-rolls a `uname`/os/arch mapping, so its output is
//! byte-identical on every determinism shard regardless of which binaries that
//! shard compiled.
//!
//! At run time (`curl -fsSL .../install.sh | sh`) the script detects the host
//! OS + architecture, maps it to the matching release archive, downloads and
//! (by default) sha256-verifies it, extracts the binary(ies), and installs
//! them — defaulting the release version to the latest GitHub release,
//! overridable via a `VERSION=` environment variable.
//!
//! Everything the tool can compute is derived: the repository slug from the
//! git `origin` remote, the installed binary names from the project name, the
//! asset names from the configured targets, and the checksums filename and tag
//! prefix from the flagship crate that builds the project binary. A bare
//! `install_scripts: {}` produces a working installer with no required input.

use std::collections::HashSet;
use std::fs;

use anyhow::{Context as _, Result};

use anodizer_core::artifact::{Artifact, ArtifactKind};
use anodizer_core::config::{ChecksumConfig, CrateConfig, InstallScriptConfig};
use anodizer_core::context::Context;
use anodizer_core::installer::{InstallerCases, installer_crate, render_installer_cases};
use anodizer_core::stage::Stage;

/// The POSIX script skeleton. `@MARKER@` placeholders are replaced with baked
/// values at render time; the marker syntax cannot collide with shell `${...}`
/// expansions in the body.
const SCRIPT_TEMPLATE: &str = include_str!("install.sh.tmpl");

/// Default output filename when a config sets no `filename:`.
const DEFAULT_FILENAME: &str = "install.sh";

/// Default install directory when a config sets no `install_dir:`.
const DEFAULT_INSTALL_DIR: &str = "/usr/local/bin";

/// Default download + REST-API base when a config sets no `base_url:`.
const DEFAULT_BASE_URL: &str = "https://github.com";

/// The literal the engine bakes into the asset/checksums names in place of the
/// concrete release version, so the generated script resolves the version at
/// run time (latest release or a `VERSION=` pin) and substitutes it into
/// `${version}` — one asset table serves every release the script is fetched
/// from.
const VERSION_PLACEHOLDER: &str = "${version}";

/// The `install-script` pipeline stage.
pub struct InstallScriptStage;

impl Stage for InstallScriptStage {
    fn name(&self) -> &str {
        "install-script"
    }

    fn run(&self, ctx: &mut Context) -> Result<()> {
        let configs = ctx.config.install_scripts.clone();
        if configs.is_empty() {
            return Ok(());
        }

        validate_unique_ids(&configs)?;

        let dry_run = ctx.options.dry_run;
        let dist = ctx.config.dist.clone();
        let project_name = ctx.config.project_name.clone();
        let repo_default = default_repo(ctx);

        // The engine-derived tables (asset arms, uname detection arms,
        // supported-platform list) plus the checksums filename and tag prefix
        // are config-INDEPENDENT — they depend only on the release's targets
        // and the flagship crate — so derive them once for all configs. The
        // flagship crate (`installer_crate`) is the SSOT for both the checksums
        // filename and the tag prefix (per-crate, not the global defaults).
        let crate_cfg = installer_crate(&ctx.config);
        let derived = match &crate_cfg {
            Some(c) => Some(derive_engine_tables(ctx, c)?),
            None => None,
        };

        let mut seen_filenames = HashSet::new();
        let mut artifacts: Vec<Artifact> = Vec::new();

        for cfg in &configs {
            let id = cfg.id.as_deref().unwrap_or("default");
            let filename = cfg
                .filename
                .as_deref()
                .unwrap_or(DEFAULT_FILENAME)
                .to_string();

            if let Some(ref skip) = cfg.skip
                && skip
                    .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
                    .with_context(|| "install-script: render skip template")?
            {
                ctx.logger("install-script")
                    .verbose(&format!("install-script config '{id}' skipped"));
                continue;
            }

            if !seen_filenames.insert(filename.clone()) {
                anyhow::bail!("install-script: duplicate filename '{}'", filename);
            }

            // No flagship crate builds a binstallable archive for the project
            // binary (e.g. a pure-library workspace, or a restricted build that
            // filtered the flagship crate out): there is nothing for the script
            // to fetch, so step aside with a status line rather than emit an
            // installer whose case table is empty.
            let Some(derived) = &derived else {
                ctx.logger("install-script").status(&format!(
                    "skipped install script '{filename}' — no crate builds a binstallable \
                     archive for the '{project_name}' binary"
                ));
                continue;
            };

            let repo = cfg
                .repo
                .clone()
                .or_else(|| repo_default.clone())
                .with_context(|| {
                    format!(
                        "install-script: config '{id}' needs a `repo:` (owner/name) — could not \
                         derive one from the git origin remote"
                    )
                })?;

            let binaries = cfg
                .binaries
                .clone()
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| vec![project_name.clone()]);
            let base_url = cfg
                .base_url
                .as_deref()
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string();
            let install_dir = cfg
                .install_dir
                .as_deref()
                .unwrap_or(DEFAULT_INSTALL_DIR)
                .to_string();
            let verify_checksum = cfg.verify_checksum.unwrap_or(true);
            let name = cfg.name.clone().unwrap_or_else(|| project_name.clone());

            let script = render_script(&ScriptParams {
                repo: &repo,
                base_url: &base_url,
                binaries: &binaries,
                install_dir: &install_dir,
                verify_checksum,
                name: &name,
                description: cfg.description.as_deref().unwrap_or(""),
                homepage: cfg.homepage.as_deref().unwrap_or(""),
                filename: &filename,
                checksums: &derived.checksums_filename,
                tag_prefix: &derived.tag_prefix,
                cases: &derived.cases,
            });

            if dry_run {
                ctx.logger("install-script")
                    .status(&format!("(dry-run) would build install script {filename}"));
                continue;
            }

            let output_path = dist.join(&filename);
            fs::write(&output_path, script.as_bytes())
                .with_context(|| format!("install-script: write {}", output_path.display()))?;

            ctx.logger("install-script")
                .status(&format!("built install script {filename}"));

            artifacts.push(Artifact {
                kind: ArtifactKind::InstallScript,
                name: filename,
                path: output_path,
                target: None,
                crate_name: project_name.clone(),
                metadata: std::collections::HashMap::from([("id".to_string(), id.to_string())]),
                size: None,
            });
        }

        for artifact in artifacts {
            ctx.artifacts.add(artifact);
        }

        Ok(())
    }
}

/// Build-time environment requirements. The install-script stage only writes a
/// text file — it spawns no external tool — so it needs none. Present for
/// symmetry with the other packaging stages and wired into `preflight`.
pub fn env_requirements(_ctx: &Context) -> Vec<anodizer_core::EnvRequirement> {
    Vec::new()
}

/// The engine-derived, config-independent pieces of a generated installer.
struct DerivedTables {
    /// The `os-arch → asset`, `uname` detection, and supported-platform case
    /// tables, with each asset name carrying [`VERSION_PLACEHOLDER`] in place
    /// of the concrete release version.
    cases: InstallerCases,
    /// The checksums filename the checksum stage produces for the flagship
    /// crate, version-templated to `${version}`.
    checksums_filename: String,
    /// The literal tag prefix from the flagship crate's tag template
    /// (`v{{ Version }}` → `v`; empty when the template is a bare version).
    tag_prefix: String,
}

/// Derive the case tables, checksums filename, and tag prefix for the flagship
/// crate, all rendered with the release version expressed as the shell
/// `${version}` placeholder so the emitted script is version-agnostic.
///
/// The `Version` template var is stamped to [`VERSION_PLACEHOLDER`] for the
/// duration and restored afterward, so no concrete release version leaks into
/// the surrounding pipeline's template scope.
fn derive_engine_tables(ctx: &mut Context, crate_cfg: &CrateConfig) -> Result<DerivedTables> {
    let prior_version = ctx.template_vars().get("Version").cloned();
    ctx.template_vars_mut().set("Version", VERSION_PLACEHOLDER);

    let result = (|| -> Result<DerivedTables> {
        let cases =
            render_installer_cases(ctx).context("install-script: derive installer case tables")?;
        let checksums_filename = resolve_checksums_filename(ctx, crate_cfg)?;
        let tag_prefix = resolve_tag_prefix(ctx, crate_cfg)?;
        Ok(DerivedTables {
            cases,
            checksums_filename,
            tag_prefix,
        })
    })();

    match prior_version {
        Some(v) => ctx.template_vars_mut().set("Version", &v),
        None => {
            ctx.template_vars_mut().unset("Version");
        }
    }
    result
}

/// Resolve the flagship crate's combined-checksums filename, rendered with the
/// `${version}` placeholder already stamped on `Version`.
///
/// The template string is resolved through
/// [`ChecksumConfig::resolve_combined_name_template`] — the single source of
/// truth shared with the checksum stage — so the crate's own
/// `checksum.name_template` wins, then `defaults.checksum.name_template`, then
/// the canonical [`ChecksumConfig::DEFAULT_NAME_TEMPLATE`].
///
/// The template is rendered under the AMBIENT template scope, deliberately NOT
/// binding `CrateName`, so the filename is byte-identical to the one the
/// checksum stage's `write_combined_file` writes (which also renders under
/// ambient vars). Binding `CrateName` here would produce a different name than
/// the file actually written, and the installer's checksum fetch would 404.
fn resolve_checksums_filename(ctx: &mut Context, crate_cfg: &CrateConfig) -> Result<String> {
    // Resolve the template string first (drops the immutable `ctx.config`
    // borrow before the mutable `render_template` borrow).
    let global = ctx
        .config
        .defaults
        .as_ref()
        .and_then(|d| d.checksum.as_ref());
    let template =
        ChecksumConfig::resolve_combined_name_template(crate_cfg.checksum.as_ref(), global)
            .to_string();

    // Render under the AMBIENT template scope — deliberately NOT binding
    // `CrateName` — so the filename is byte-identical to the one the checksum
    // stage's `write_combined_file` writes (which also renders under ambient
    // vars). `Version` is already stamped to the `${version}` placeholder by
    // the caller.
    ctx.render_template(&template)
        .with_context(|| "install-script: render checksums filename")
}

/// Resolve the flagship crate's literal tag prefix.
///
/// Renders the crate's tag template (a `release.tag` override wins, then the
/// crate's `tag_template`, then the canonical `v{{ Version }}`) with `Version`
/// stamped to `${version}`, then strips the trailing `${version}` — so
/// `v{{ Version }}` → `v`, `release-{{ Version }}` → `release-`, and a bare
/// `{{ Version }}` → `` (empty). This is the W2 fix: the script's
/// `tag="${TAG_PREFIX}${version}"` no longer hardcodes a `v` prefix.
///
/// The rendered tag MUST end with the version placeholder: a `curl | sh`
/// installer reconstructs the release tag from a runtime-resolved version and
/// can only invert a version-SUFFIXED template (any prefix is fine). A
/// version-infix template (e.g. `{{ Version }}-stable`) is rejected with a
/// clear error rather than silently baking a broken `tag=` into the script.
fn resolve_tag_prefix(ctx: &mut Context, crate_cfg: &CrateConfig) -> Result<String> {
    let template = crate_cfg
        .release
        .as_ref()
        .and_then(|r| r.tag.clone())
        .filter(|t| !t.is_empty())
        .or_else(|| Some(crate_cfg.tag_template.clone()).filter(|t| !t.is_empty()))
        .unwrap_or_else(|| "v{{ Version }}".to_string());

    let rendered = ctx
        .render_template(&template)
        .with_context(|| format!("install-script: render tag template '{template}'"))?;
    match rendered.strip_suffix(VERSION_PLACEHOLDER) {
        Some(prefix) => Ok(prefix.to_string()),
        None => anyhow::bail!(
            "install-script: tag template '{template}' must end with the version \
             (e.g. `v{{{{ Version }}}}`), but rendered to '{rendered}', which places \
             text after the version. A `curl | sh` installer reconstructs the release \
             tag from a runtime-resolved version and cannot invert a version-infix \
             template. Use a version-suffixed `release.tag` / crate `tag_template`, \
             or skip the install-script stage for this project."
        ),
    }
}

/// Parameters passed to [`render_script`].
struct ScriptParams<'a> {
    repo: &'a str,
    base_url: &'a str,
    binaries: &'a [String],
    install_dir: &'a str,
    verify_checksum: bool,
    name: &'a str,
    description: &'a str,
    homepage: &'a str,
    filename: &'a str,
    checksums: &'a str,
    tag_prefix: &'a str,
    cases: &'a InstallerCases,
}

/// Render the POSIX install script from baked values. Pure and deterministic:
/// identical inputs always yield byte-identical output.
///
/// Renders in a SINGLE pass ([`single_pass_replace`]) under a categorized escape
/// policy, so every `@MARKER@` position is substituted exactly once and no
/// replacement value can be re-scanned as a marker (re-substitution is
/// structurally impossible). Two DATA categories are escaped for their shell
/// context — free-text metadata (`@NAME@` via [`shell_dq_escape`],
/// `@DESCRIPTION@` / `@HOMEPAGE@` / `@FILENAME@` via [`comment_sanitize`]) and
/// structured identifiers baked into double-quoted assignments (`@REPO@`,
/// `@BASE_URL@`, `@BINARIES@` via [`shell_dq_escape`]) — so a `"`, `$`,
/// `` ` ``, `\`, or newline cannot break out and inject shell text. Engine-
/// rendered shell fragments (the case tables, tag prefix, checksums filename)
/// and the shell-expandable `@INSTALL_DIR@` pass through verbatim by design.
fn render_script(params: &ScriptParams) -> String {
    // Human free-text metadata: escaped for its shell context (@NAME@ lands in
    // double-quoted strings and a comment; description/homepage are comment-only).
    let name = shell_dq_escape(params.name);
    let description = comment_sanitize(params.description);
    let homepage = comment_sanitize(params.homepage);
    let filename = comment_sanitize(params.filename);
    // Structured identifiers baked into double-quoted assignments. A slug / URL /
    // binary-name never legitimately contains `"`, `$`, backtick, or a newline,
    // so escaping them is zero-cost defense-in-depth and keeps the escape policy
    // consistent across every double-quoted DATA value.
    let repo = shell_dq_escape(params.repo);
    let base_url = shell_dq_escape(params.base_url);
    let binaries = shell_dq_escape(&params.binaries.join(" "));
    let verify = if params.verify_checksum {
        "true"
    } else {
        "false"
    };

    // Markers NOT escaped, BY DESIGN:
    //   @INSTALL_DIR@   — a shell-expandable default: `install_dir: "$HOME/bin"`
    //                     and the runtime `${INSTALL_DIR:-…}` override both rely
    //                     on the value expanding, so it must pass through live.
    //   @CHECKSUMS@     — an engine-rendered filename that embeds a live
    //                     `${version}` expansion (resolved at install time).
    //   @TAG_PREFIX@ / @VERIFY_CHECKSUM@ / @DETECT_*_CASES@ / @ASSET_CASES@ /
    //   @SUPPORTED_PLATFORMS@ — engine-generated shell code / control values,
    //                     not user free-text.
    let map: &[(&str, &str)] = &[
        ("@REPO@", &repo),
        ("@BASE_URL@", &base_url),
        ("@BINARIES@", &binaries),
        ("@INSTALL_DIR@", params.install_dir),
        ("@VERIFY_CHECKSUM@", verify),
        ("@FILENAME@", &filename),
        ("@CHECKSUMS@", params.checksums),
        ("@TAG_PREFIX@", params.tag_prefix),
        ("@DETECT_OS_CASES@", &params.cases.detect_os_cases),
        ("@DETECT_ARCH_CASES@", &params.cases.detect_arch_cases),
        ("@ASSET_CASES@", &params.cases.asset_cases),
        ("@SUPPORTED_PLATFORMS@", &params.cases.supported_platforms),
        ("@NAME@", &name),
        ("@DESCRIPTION@", &description),
        ("@HOMEPAGE@", &homepage),
    ];
    single_pass_replace(SCRIPT_TEMPLATE, map)
}

/// Escape a value baked into a double-quoted POSIX shell string: strip newlines
/// (they would break out of the string) and backslash-escape the four
/// characters the shell still interprets inside double quotes.
fn shell_dq_escape(s: &str) -> String {
    let one_line: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    one_line
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

/// Collapse newlines to spaces so a value baked into a `#` comment line cannot
/// break out of the comment and inject script text.
fn comment_sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

/// Replace every `@MARKER@` token in `template` using `map` in ONE pass: each
/// marker position is substituted exactly once from `map`, so no replacement
/// value can contain a marker that a later pass would rewrite — re-substitution
/// is structurally impossible in either direction. An `@...@` run that matches
/// no known marker is emitted verbatim. UTF-8 safe: all slicing lands on the
/// ASCII `@` byte or a marker's byte length.
fn single_pass_replace(template: &str, map: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 512);
    let mut rest = template;
    while let Some(at) = rest.find('@') {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        if let Some((marker, val)) = map.iter().find(|(m, _)| after.starts_with(*m)) {
            out.push_str(val);
            rest = &after[marker.len()..];
        } else {
            out.push('@');
            rest = &after[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Derive the default `owner/name` repo slug from the git `origin` remote,
/// returning `None` when detection fails (no remote / not a repo / a
/// non-GitHub host) so a config without an explicit `repo:` surfaces a clear
/// error instead. The generated script talks only to `github.com` /
/// `api.github.com` by default, so a GitHub-aware resolver is used
/// deliberately: a GitLab/Gitea origin fails closed (forcing an explicit
/// `repo:`) rather than baking a non-GitHub slug into GitHub URLs that 404 at
/// install time.
fn default_repo(ctx: &Context) -> Option<String> {
    let root = ctx
        .options
        .project_root
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    anodizer_core::git::resolve_github_slug_in(None, None, &root)
        .ok()
        .map(|slug| slug.slug().to_string())
}

/// Reject duplicate config IDs (mirrors the makeself/nfpm validation).
fn validate_unique_ids(configs: &[InstallScriptConfig]) -> Result<()> {
    let mut seen = HashSet::new();
    for cfg in configs {
        let id = cfg.id.as_deref().unwrap_or("default");
        if !seen.insert(id.to_string()) {
            anyhow::bail!("install-script: duplicate id '{}'", id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
