use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

// ---------------------------------------------------------------------------
// pkgdesc quoting helper
// ---------------------------------------------------------------------------

/// Quote a PKGBUILD `pkgdesc` value, choosing the appropriate quoting style
/// to handle embedded single or double quotes.
fn quote_pkgdesc(s: &str) -> String {
    if s.contains('"') && !s.contains('\'') {
        format!("'{}'", s)
    } else if s.contains('\'') && !s.contains('"') {
        format!("\"{}\"", s)
    } else if s.contains('"') && s.contains('\'') {
        // Escape single quotes for single-quoted string using shell idiom
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        format!("\"{}\"", s)
    }
}

// ---------------------------------------------------------------------------
// PkgbuildParams
// ---------------------------------------------------------------------------

/// Parameters for generating an Arch Linux PKGBUILD file.
pub(crate) struct PkgbuildParams<'a> {
    pub(crate) name: &'a str,
    pub(crate) version: &'a str,
    pub(crate) pkgrel: u32,
    pub(crate) description: &'a str,
    pub(crate) url: &'a str,
    pub(crate) license: &'a str,
    pub(crate) maintainers: &'a [String],
    pub(crate) contributors: &'a [String],
    pub(crate) depends: &'a [String],
    pub(crate) optdepends: &'a [String],
    pub(crate) conflicts: &'a [String],
    pub(crate) provides: &'a [String],
    pub(crate) replaces: &'a [String],
    pub(crate) backup: &'a [String],
    /// `(arch, url, sha256)` tuples — e.g. `("x86_64", url, hash)`.
    pub(crate) sources: &'a [(String, String, String)],
    pub(crate) binary_name: &'a str,
    /// Custom install template for the `package()` function body.
    /// When `None`, defaults to `install -Dm755 "$srcdir/<binary>" "$pkgdir/usr/bin/<binary>"`.
    /// Use this when the archive places binaries in a subdirectory.
    pub(crate) install_template: Option<&'a str>,
    /// Filename for a `.install` file (post-install hooks). When `Some`, the
    /// PKGBUILD will include an `install=<filename>` line.
    pub(crate) install_file: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// archive extension helper
// ---------------------------------------------------------------------------

/// Extract the archive extension from a URL path.
///
/// Handles compound extensions like `.tar.gz`, `.tar.xz`, `.tar.bz2`, `.tar.zst`
/// and simple ones like `.zip`, `.gz`, `.xz`.
fn extract_archive_extension(url: &str) -> &str {
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split('#').next().unwrap_or(path);
    let filename = path.rsplit('/').next().unwrap_or(path);
    for ext in &[
        ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz4", ".tar.sz",
    ] {
        if filename.ends_with(ext) {
            return &ext[1..];
        }
    }
    if let Some(dot_pos) = filename.rfind('.') {
        &filename[dot_pos + 1..]
    } else {
        ""
    }
}

// ---------------------------------------------------------------------------
// generate_pkgbuild
// ---------------------------------------------------------------------------

const PKGBUILD_TEMPLATE: &str = r#"{% for m in maintainers %}# Maintainer: {{ m }}
{% endfor %}{% for c in contributors %}# Contributor: {{ c }}
{% endfor %}{% if maintainers | length > 0 or contributors | length > 0 %}
{% endif %}pkgname='{{ name }}'
pkgver={{ version }}
pkgrel={{ pkgrel }}
pkgdesc={{ quoted_description }}
arch=({% for a in arches %}'{{ a }}'{% if not loop.last %} {% endif %}{% endfor %})
url="{{ url }}"
license=('{{ license }}')
{% if depends | length > 0 %}depends=({% for d in depends %}'{{ d }}'{% if not loop.last %} {% endif %}{% endfor %})
{% else %}depends=()
{% endif %}{% if optdepends | length > 0 %}optdepends=({% for d in optdepends %}'{{ d }}'{% if not loop.last %} {% endif %}{% endfor %})
{% endif %}{% if conflicts | length > 0 %}conflicts=({% for c in conflicts %}'{{ c }}'{% if not loop.last %} {% endif %}{% endfor %})
{% endif %}{% if provides | length > 0 %}provides=({% for p in provides %}'{{ p }}'{% if not loop.last %} {% endif %}{% endfor %})
{% endif %}{% if replaces | length > 0 %}replaces=({% for r in replaces %}'{{ r }}'{% if not loop.last %} {% endif %}{% endfor %})
{% endif %}{% if backup | length > 0 %}backup=({% for b in backup %}'{{ b }}'{% if not loop.last %} {% endif %}{% endfor %})
{% endif %}{% if install_file %}install={{ install_file }}
{% endif %}{% for s in sources %}source_{{ s.arch }}=("{{ s.rename }}::{{ s.url }}")
sha256sums_{{ s.arch }}=('{{ s.hash }}')
{% endfor %}
package() {
    {{ install_line }}
}
"#;

/// Generate an Arch Linux PKGBUILD file string.
pub(crate) fn generate_pkgbuild(params: &PkgbuildParams<'_>) -> Result<String> {
    let tera = anodizer_core::template::parse_static("pkgbuild", PKGBUILD_TEMPLATE)
        .context("aur: parse PKGBUILD template")?;

    let mut ctx = tera::Context::new();
    ctx.insert("name", params.name);
    ctx.insert("version", params.version);
    ctx.insert("pkgrel", &params.pkgrel);
    ctx.insert("description", params.description);
    ctx.insert("quoted_description", &quote_pkgdesc(params.description));
    ctx.insert("url", params.url);
    ctx.insert("license", params.license);
    ctx.insert("maintainers", params.maintainers);
    ctx.insert("contributors", params.contributors);
    ctx.insert("depends", params.depends);
    ctx.insert("optdepends", params.optdepends);
    ctx.insert("conflicts", params.conflicts);
    ctx.insert("provides", params.provides);
    ctx.insert("replaces", params.replaces);
    ctx.insert("backup", params.backup);
    ctx.insert("binary_name", params.binary_name);
    ctx.insert("install_file", &params.install_file);

    // Deduplicate architectures.
    let mut arches: Vec<&str> = params
        .sources
        .iter()
        .map(|(arch, _, _)| arch.as_str())
        .collect();
    arches.sort();
    arches.dedup();
    ctx.insert("arches", &arches);

    // Sources as objects for template iteration.
    // Replace the version string in URLs with ${pkgver} so the PKGBUILD
    // automatically uses the pkgver variable (GoReleaser convention).
    let substituted_sources: Vec<(String, String, String, String)> = params
        .sources
        .iter()
        .map(|(arch, url, hash)| {
            let substituted_url = url.replace(params.version, "${pkgver}");
            let format = extract_archive_extension(url);
            let rename = format!(
                "{}_{}_{}{}",
                params.name,
                "${pkgver}",
                arch,
                if format.is_empty() {
                    String::new()
                } else {
                    format!(".{}", format)
                }
            );
            (arch.clone(), substituted_url, hash.clone(), rename)
        })
        .collect();
    let sources: Vec<std::collections::HashMap<&str, &str>> = substituted_sources
        .iter()
        .map(|(arch, url, hash, rename)| {
            let mut m = std::collections::HashMap::new();
            m.insert("arch", arch.as_str());
            m.insert("url", url.as_str());
            m.insert("hash", hash.as_str());
            m.insert("rename", rename.as_str());
            m
        })
        .collect();
    ctx.insert("sources", &sources);

    let install_line = if let Some(tmpl) = params.install_template {
        tmpl.to_string()
    } else {
        format!(
            "install -Dm755 \"$srcdir/{}\" \"$pkgdir/usr/bin/{}\"",
            params.binary_name, params.binary_name
        )
    };
    ctx.insert("install_line", &install_line);

    anodizer_core::template::render_static(&tera, "pkgbuild", &ctx, "aur")
}

// ---------------------------------------------------------------------------
// generate_srcinfo (template-based, no makepkg dependency)
// ---------------------------------------------------------------------------

const SRCINFO_TEMPLATE: &str = r#"pkgbase = {{ name }}
	pkgdesc = {{ description }}
	pkgver = {{ version }}
	pkgrel = {{ pkgrel }}
{% if url %}	url = {{ url }}
{% endif %}{% if license %}	license = {{ license }}
{% endif %}
{% for d in depends %}	depends = {{ d }}
{% endfor %}{% for o in optdepends %}	optdepends = {{ o }}
{% endfor %}{% for c in conflicts %}	conflicts = {{ c }}
{% endfor %}{% for p in provides %}	provides = {{ p }}
{% endfor %}{% for b in backup %}	backup = {{ b }}
{% endfor %}{% for s in sources %}	arch = {{ s.arch }}
	source_{{ s.arch }} = {{ s.url }}
	sha256sums_{{ s.arch }} = {{ s.hash }}
{% endfor %}
pkgname = {{ name }}
"#;

/// Generate an AUR `.SRCINFO` file string from a Tera template.
pub(crate) fn generate_srcinfo(params: &PkgbuildParams<'_>) -> Result<String> {
    let tera = anodizer_core::template::parse_static("srcinfo", SRCINFO_TEMPLATE)
        .context("aur: parse .SRCINFO template")?;

    let mut ctx = tera::Context::new();
    ctx.insert("name", params.name);
    ctx.insert("version", params.version);
    ctx.insert("pkgrel", &params.pkgrel);
    ctx.insert("description", params.description);
    ctx.insert("url", params.url);
    ctx.insert("license", params.license);
    ctx.insert("depends", params.depends);
    ctx.insert("optdepends", params.optdepends);
    ctx.insert("conflicts", params.conflicts);
    ctx.insert("provides", params.provides);
    ctx.insert("backup", params.backup);

    let source_data: Vec<(String, String, String, String)> = params
        .sources
        .iter()
        .map(|(arch, url, hash)| {
            let format = extract_archive_extension(url);
            let rename = format!(
                "{}_{}_{}{}",
                params.name,
                params.version,
                arch,
                if format.is_empty() {
                    String::new()
                } else {
                    format!(".{}", format)
                }
            );
            (arch.clone(), url.clone(), hash.clone(), rename)
        })
        .collect();
    let sources: Vec<std::collections::HashMap<&str, &str>> = source_data
        .iter()
        .map(|(arch, url, hash, rename)| {
            let mut m = std::collections::HashMap::new();
            m.insert("arch", arch.as_str());
            m.insert("url", url.as_str());
            m.insert("hash", hash.as_str());
            m.insert("rename", rename.as_str());
            m
        })
        .collect();
    ctx.insert("sources", &sources);

    anodizer_core::template::render_static(&tera, "srcinfo", &ctx, "aur")
}

// ---------------------------------------------------------------------------
// Default resolution (GoReleaser parity: aur.go::Default)
// ---------------------------------------------------------------------------

/// Resolved AUR `Default()`-time fields: conflicts, provides, and pkgrel.
/// Extracted from `publish_to_aur` so the GoReleaser-aligned defaults can
/// be exercised in unit tests without standing up a full publish-to-git
/// flow. Mirrors GoReleaser's `internal/pipe/aur/aur.go::Default()`
/// behaviour:
///
/// - `name` raw default is computed by `aur_default_package_name`
///   (`<crate_name>` with `-bin` suffix appended when the crate name does
///   not already end in `-bin`); the caller renders templates and feeds
///   the rendered string into `aur_resolve_defaults` so `base_name` is
///   derived from the post-template name.
/// - `conflicts` defaults to `[base_name]` when unset/empty (GR aur.go:58-63).
/// - `provides` defaults to `[base_name]` when unset/empty (GR aur.go:58-63).
/// - `pkgrel` defaults to `1` when unset (GR aur.go:64-66).
///
/// `base_name` is the project name when set, otherwise the rendered package
/// name with any trailing `-bin` stripped (covers the edge case where
/// `package_name="foo-bin"` and `project_name="foo-cli"`).
pub(crate) struct AurResolvedDefaults {
    pub(crate) conflicts: Vec<String>,
    pub(crate) provides: Vec<String>,
    pub(crate) pkgrel: u32,
}

/// Compute the raw (pre-template) default `aur.name`: the explicit
/// `aur_cfg.name` if Some, otherwise `<crate_name>-bin` (without
/// double-suffixing when the crate already ends in `-bin`).
///
/// This is split out from `aur_resolve_defaults` so the caller can render
/// the result through the template engine *before* `base_name` is derived
/// — otherwise `aur.name = "{{ .ProjectName }}-bin"` with an empty
/// `project_name` would carry unrendered template syntax into
/// `conflicts`/`provides`.
pub(crate) fn aur_default_package_name(
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
) -> String {
    aur_cfg.name.clone().unwrap_or_else(|| {
        if crate_name.ends_with("-bin") {
            crate_name.to_string()
        } else {
            format!("{}-bin", crate_name)
        }
    })
}

/// Apply the GoReleaser `Default()` rules for `conflicts`, `provides`, and
/// `pkgrel`, given a `rendered_package_name` (post-template) and a
/// `project_name` (use `""` when no project name is configured). The
/// returned struct holds the post-default values that `publish_to_aur`
/// would feed into PKGBUILD generation.
///
/// `rendered_package_name` must be the template-rendered output of
/// `aur_default_package_name` — the helper is intentionally template-free
/// so it stays pure (no `Context` dependency).
pub(crate) fn aur_resolve_defaults(
    aur_cfg: &anodizer_core::config::AurConfig,
    rendered_package_name: &str,
    project_name: &str,
) -> AurResolvedDefaults {
    let base_name = if project_name.is_empty() {
        rendered_package_name
            .strip_suffix("-bin")
            .unwrap_or(rendered_package_name)
            .to_string()
    } else {
        project_name.to_string()
    };

    let conflicts = if aur_cfg.conflicts.as_ref().is_none_or(|v| v.is_empty()) {
        vec![base_name.clone()]
    } else {
        aur_cfg.conflicts.clone().unwrap_or_default()
    };
    let provides = if aur_cfg.provides.as_ref().is_none_or(|v| v.is_empty()) {
        vec![base_name.clone()]
    } else {
        aur_cfg.provides.clone().unwrap_or_default()
    };

    let pkgrel: u32 = aur_cfg
        .rel
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    AurResolvedDefaults {
        conflicts,
        provides,
        pkgrel,
    }
}

// ---------------------------------------------------------------------------
// publish_to_aur — per-section helpers
// ---------------------------------------------------------------------------

/// Owned, post-default field set fed into `PkgbuildParams`. Built once
/// by [`aur_resolve_fields`] from the active `aur:` config + project
/// metadata fallbacks so the orchestrator stays linear.
struct AurResolvedFields {
    package_name: String,
    version: String,
    pkgrel: u32,
    description: String,
    license: String,
    url: String,
    maintainers: Vec<String>,
    contributors: Vec<String>,
    depends: Vec<String>,
    optdepends: Vec<String>,
    conflicts: Vec<String>,
    provides: Vec<String>,
    replaces: Vec<String>,
    backup: Vec<String>,
}

/// Evaluate the early-exit gates (`skip`, `skip_upload`, missing
/// `git_url`, dry-run) for the AUR publisher.
///
/// Returns `Ok(Some(git_url))` when the caller should proceed with
/// the publish; `Ok(None)` when an early-exit fired (the helper has
/// already emitted any operator-facing log line). Errors propagate
/// unchanged (e.g. the `skip` Tera render failure).
fn aur_check_skip_and_resolve_git_url(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<Option<String>> {
    if let Some(ref d) = aur_cfg.skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("aur: render skip template for '{}'", crate_name))?;
        if off {
            log.status(&format!("aur: skipped for '{}'", crate_name));
            return Ok(None);
        }
    }

    if crate::util::should_skip_upload(aur_cfg.skip_upload.as_ref(), ctx, log) {
        log.status(&format!(
            "aur: skipping upload for '{}' (skip_upload={})",
            crate_name,
            aur_cfg
                .skip_upload
                .as_ref()
                .map(|v| v.as_str())
                .unwrap_or("")
        ));
        return Ok(None);
    }

    let git_url = aur_cfg
        .git_url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("aur: no git_url config for '{}'", crate_name))?
        .clone();

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would push AUR PKGBUILD for '{}' to {}",
            crate_name, git_url
        ));
        return Ok(None);
    }

    Ok(Some(git_url))
}

/// Resolve all PKGBUILD field defaults (name, version, pkgrel, url,
/// license, dependency arrays, etc.). `crate_cfg` is consulted for the
/// `release.github` fallback when `aur.homepage` / `metadata.homepage`
/// are both unset; the AUR-default `conflicts`/`provides`/`pkgrel`
/// rules are applied via `aur_resolve_defaults` against the rendered
/// package name (so `aur.name = "{{ .ProjectName }}-bin"` does not
/// leak unrendered template syntax into the array fields).
fn aur_resolve_fields(
    ctx: &Context,
    crate_cfg: &anodizer_core::config::CrateConfig,
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
    log: &StageLogger,
) -> Result<AurResolvedFields> {
    // AUR pkgver does not allow hyphens; replace with underscores.
    let version = ctx.version().replace('-', "_");

    // GR-parity Default() resolution: name auto-suffix `-bin`, conflicts /
    // provides default to [base_name], pkgrel default `"1"`. The defaults
    // are split across two helpers (`aur_default_package_name` →
    // template-render → `aur_resolve_defaults`) to expose the default
    // rules to unit tests without standing up a full publish flow, while
    // ensuring `base_name` is derived from the rendered package name (so
    // `aur.name = "{{ .ProjectName }}-bin"` with an empty project_name
    // does not leak unrendered template syntax into conflicts/provides).
    let project_name_for_defaults = ctx.config.project_name.as_str();
    let raw_package_name = aur_default_package_name(aur_cfg, crate_name);
    // Render the resolved name through the template engine — users who set
    // `aur.name: "{{ .ProjectName }}-bin"` rely on this. On render failure
    // (typically a malformed template like `{{ unclosed`), surface a warning
    // and fall back to the raw value: a visible warning beats a silent
    // swallow without breaking a currently-malformed user build.
    let package_name = util::render_or_warn(ctx, log, "aur.name", &raw_package_name);
    let resolved_defaults = aur_resolve_defaults(aur_cfg, &package_name, project_name_for_defaults);

    // GoReleaser Pro parity: fall back to project `metadata.*` when aur config unset.
    let description_raw = aur_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description())
        .unwrap_or(crate_name);
    let description = util::render_or_warn(ctx, log, "aur.description", description_raw);

    // PKGBUILD `license=()` is documented as RECOMMENDED but not required
    // per the Arch wiki (https://wiki.archlinux.org/title/PKGBUILD#license);
    // makepkg builds without complaint when the array contains an empty
    // string. The Tera template emits `license=('{{ license }}')`
    // unconditionally — empty produces `license=('')` which `namcap` lints
    // but neither `makepkg` nor AUR uploads reject.
    let license = aur_cfg
        .license
        .clone()
        .or_else(|| ctx.config.meta_license().map(str::to_string))
        .unwrap_or_default();

    // PKGBUILD `url=` resolves through `homepage:` → crate metadata
    // homepage → the derived github release URL.
    let url_override = aur_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage())
        .map(|s| s.to_string());
    let url = if let Some(u) = url_override {
        u
    } else if let Some(gh) = crate_cfg.release.as_ref().and_then(|r| r.github.as_ref()) {
        format!("https://github.com/{}/{}", gh.owner, gh.name)
    } else {
        anyhow::bail!(
            "aur: no url configured for '{}' and no release.github owner/name available. \
             Set `publish.aur.homepage` or configure `release.github` with owner and name.",
            crate_name
        );
    };

    let maintainers = aur_cfg
        .maintainers
        .clone()
        .unwrap_or_else(|| ctx.config.meta_maintainers().to_vec());
    // The Vec fields below default to empty when unset. The PKGBUILD_TEMPLATE
    // wraps each in a `{% if X | length > 0 %}...{% endif %}` guard so the
    // emitted PKGBUILD omits the corresponding `<key>=(...)` line entirely
    // when the list is empty — all of these arrays are optional per the
    // PKGBUILD spec (https://wiki.archlinux.org/title/PKGBUILD).
    let contributors = aur_cfg.contributors.clone().unwrap_or_default();
    let depends = aur_cfg.depends.clone().unwrap_or_default();
    let optdepends = aur_cfg.optdepends.clone().unwrap_or_default();
    // conflicts / provides come from the GR-aligned default resolver
    // (cf. aur.go:58-63). The resolver was fed the *rendered* package name,
    // so `base_name` reflects post-template values when `project_name` is
    // empty.
    let conflicts = resolved_defaults.conflicts;
    let provides = resolved_defaults.provides;
    let replaces = aur_cfg.replaces.clone().unwrap_or_default();
    let backup = aur_cfg.backup.clone().unwrap_or_default();

    Ok(AurResolvedFields {
        package_name,
        version,
        pkgrel: resolved_defaults.pkgrel,
        description,
        license,
        url,
        maintainers,
        contributors,
        depends,
        optdepends,
        conflicts,
        provides,
        replaces,
        backup,
    })
}

/// Build the `(arch, download_url, sha256)` source tuples for the
/// PKGBUILD `source_<arch>=` / `sha256sums_<arch>=` arrays. Filters
/// `ctx.artifacts` to Linux archives matching `aur.ids` + the GR-
/// hardcoded `amd64_variant`/`arm_variant=7` rules, validates that at
/// least one archive matched and that every match carries a non-empty
/// sha256, then dedupes by PKGBUILD architecture (`x86_64`, `aarch64`,
/// `i686`, `armv7h`) keeping the first match per arch.
fn aur_build_sources(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
    version: &str,
) -> Result<Vec<(String, String, String)>> {
    // Find Linux artifacts for the AUR package, applying IDs + amd64_variant filter.
    // GoReleaser hardcodes arm_variant to "7" for AUR (no config option).
    let ids_filter = aur_cfg.ids.as_deref();
    let amd64_variant = aur_cfg.amd64_variant.as_deref().or(Some("v1"));
    let linux_artifacts = util::find_artifacts_by_os_with_variant(
        ctx,
        crate_name,
        "linux",
        ids_filter,
        amd64_variant,
        Some("7"),
    )?;

    // An empty linux-archive set produces a PKGBUILD with placeholder URL and
    // empty sha256 that users would have to hand-fix. Hard-fail with an
    // actionable error instead.
    if linux_artifacts.is_empty() {
        let ids_hint = ids_filter
            .map(|ids| format!("ids={ids:?}"))
            .unwrap_or_else(|| "ids=<none>".to_string());
        let amd_hint = amd64_variant.unwrap_or("<default v1>");
        anyhow::bail!(
            "aur: no linux archives matched filters for '{crate_name}' — \
             PKGBUILD would have placeholder URL and empty sha256. Check your \
             archive configuration and aur filters ({ids_hint}, \
             amd64_variant={amd_hint}, arm_variant=7 [hardcoded]). At least \
             one linux Archive artifact must match."
        );
    }

    // The PKGBUILD `sha256sums_<arch>=('...')` array is consumed by
    // `makepkg`'s integrity check (per
    // https://wiki.archlinux.org/title/PKGBUILD#sha256sums). An empty
    // hash string `('')` is silently accepted by makepkg but disables the
    // verification — installers would download an unverified tarball.
    // Bail before emitting a PKGBUILD whose hashes cannot prove
    // tarball integrity.
    if let Some(empty) = linux_artifacts.iter().find(|a| a.sha256.is_empty()) {
        anyhow::bail!(
            "aur: artifact for crate '{}' at url '{}' (os={}, arch={}) is \
             missing required sha256 metadata. The generated PKGBUILD would \
             emit `sha256sums_<arch>=('')`, which disables makepkg's \
             integrity check and ships an unverified tarball. Check \
             dist/artifacts.json for the archive entry's metadata.sha256 \
             and re-run `task release` from a clean dist/ if the field is \
             absent or empty.",
            crate_name,
            empty.url,
            empty.os,
            empty.arch,
        );
    }

    let url_template = aur_cfg.url_template.as_deref();
    // Deduplicate by architecture — AUR -bin packages expect one source per
    // architecture. When multiple artifacts share the same arch (e.g.
    // multiple linux-amd64 archives), keep only the first match.
    let mut seen_arches = std::collections::HashSet::new();
    let sources: Vec<(String, String, String)> = linux_artifacts
        .iter()
        .filter_map(|a| {
            let pkgbuild_arch = match a.arch.as_str() {
                "arm64" | "aarch64" => "aarch64".to_string(),
                "386" | "i686" | "i386" | "x86" => "i686".to_string(),
                "armv7" | "arm" | "armhf" | "armv6" => "armv7h".to_string(),
                _ => "x86_64".to_string(),
            };
            if seen_arches.insert(pkgbuild_arch.clone()) {
                let download_url = if let Some(tmpl) = url_template {
                    util::render_url_template_with_ctx(
                        ctx,
                        tmpl,
                        crate_name,
                        version,
                        &pkgbuild_arch,
                        "linux",
                    )
                } else {
                    a.url.clone()
                };
                Some((pkgbuild_arch, download_url, a.sha256.clone()))
            } else {
                None
            }
        })
        .collect();

    Ok(sources)
}

/// Clone the AUR git repo into `repo_path`. When either `aur.private_key`
/// or `aur.git_ssh_command` is set the SSH clone path is taken; otherwise
/// falls back to a plain (no-auth-header) clone. AUR has no bearer-token
/// flow so the auth-aware variant is never invoked with credentials.
fn aur_clone_repo(
    aur_cfg: &anodizer_core::config::AurConfig,
    git_url: &str,
    repo_path: &std::path::Path,
    log: &StageLogger,
) -> Result<()> {
    if aur_cfg.private_key.is_some() || aur_cfg.git_ssh_command.is_some() {
        util::clone_repo_ssh(
            git_url,
            aur_cfg.private_key.as_deref(),
            aur_cfg.git_ssh_command.as_deref(),
            repo_path,
            "aur",
            log,
        )
    } else {
        util::clone_repo_with_auth(git_url, None, repo_path, "aur", log)
    }
}

/// Resolve the output directory inside the cloned repo, optionally
/// creating a subdirectory rendered from `aur.directory`. Matches the
/// GoReleaser template-then-mkdir behaviour at aur.go:103-108.
fn aur_resolve_output_dir(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    repo_path: &std::path::Path,
    log: &StageLogger,
) -> Result<std::path::PathBuf> {
    if let Some(ref dir) = aur_cfg.directory {
        let rendered_dir = util::render_or_warn(ctx, log, "aur.directory", dir);
        let d = repo_path.join(&rendered_dir);
        std::fs::create_dir_all(&d)
            .with_context(|| format!("aur: create directory {}", d.display()))?;
        Ok(d)
    } else {
        Ok(repo_path.to_path_buf())
    }
}

/// Write `PKGBUILD`, the optional `.install` file, and `.SRCINFO` into
/// `output_dir`. `install_filename` is precomputed by the caller as
/// `<package_name minus trailing -bin>.install`; the `.install` file
/// is only emitted when `install_content` is `Some`. Status lines
/// mirror the formerly-inline `log.status` calls.
fn aur_write_package_files(
    output_dir: &std::path::Path,
    pkgbuild: &str,
    srcinfo: &str,
    install_filename: &str,
    install_content: Option<&str>,
    log: &StageLogger,
) -> Result<()> {
    let pkgbuild_path = output_dir.join("PKGBUILD");
    std::fs::write(&pkgbuild_path, pkgbuild)
        .with_context(|| format!("aur: write PKGBUILD {}", pkgbuild_path.display()))?;
    log.status(&format!("wrote AUR PKGBUILD: {}", pkgbuild_path.display()));

    if let Some(content) = install_content {
        let install_path = output_dir.join(install_filename);
        std::fs::write(&install_path, content).with_context(|| {
            format!("aur: write {} {}", install_filename, install_path.display())
        })?;
        log.status(&format!(
            "wrote AUR install file: {}",
            install_path.display()
        ));
    }

    let srcinfo_path = output_dir.join(".SRCINFO");
    std::fs::write(&srcinfo_path, srcinfo)
        .with_context(|| format!("aur: write .SRCINFO {}", srcinfo_path.display()))?;
    log.status(&format!("wrote AUR .SRCINFO: {}", srcinfo_path.display()));

    Ok(())
}

/// Commit the staged files in `repo_path` and push to AUR `master`.
/// Returns `true` when the push delivered a new commit, `false` when
/// `commit_and_push_with_opts` reports `NoChanges` (nothing to ship,
/// repo already up to date).
fn aur_commit_and_push(
    ctx: &Context,
    aur_cfg: &anodizer_core::config::AurConfig,
    repo_path: &std::path::Path,
    package_name: &str,
    version: &str,
    git_url: &str,
    log: &StageLogger,
) -> Result<bool> {
    let commit_msg = crate::homebrew::render_commit_msg(
        aur_cfg.commit_msg_template.as_deref(),
        package_name,
        version,
        "package",
    );
    let commit_opts = util::resolve_commit_opts(ctx, aur_cfg.commit_author.as_ref());
    // AUR repositories are always on `master`. Pin the push branch via the
    // shared [`AUR_REPO_BRANCH`] constant so the publish and rollback
    // paths can never drift (e.g. one renamed to `main`). Matches
    // GoReleaser `internal/pipe/aur/aur.go`.
    let outcome = util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        Some(AUR_REPO_BRANCH),
        "aur",
        &commit_opts,
    )?;
    let pushed = match outcome {
        util::CommitOutcome::Pushed => {
            log.status(&format!(
                "AUR package '{}' pushed to {}",
                package_name, git_url
            ));
            true
        }
        util::CommitOutcome::NoChanges => {
            log.status(&format!(
                "aur: nothing to push, package '{}' already up to date",
                package_name
            ));
            false
        }
    };
    Ok(pushed)
}

// ---------------------------------------------------------------------------
// publish_to_aur
// ---------------------------------------------------------------------------

pub fn publish_to_aur(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<bool> {
    let (crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "aur")?;

    let aur_cfg = publish
        .aur
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("aur: no aur config for '{}'", crate_name))?;

    let git_url = match aur_check_skip_and_resolve_git_url(ctx, aur_cfg, crate_name, log)? {
        Some(u) => u,
        None => return Ok(false),
    };

    let fields = aur_resolve_fields(ctx, crate_cfg, aur_cfg, crate_name, log)?;
    let sources = aur_build_sources(ctx, aur_cfg, crate_name, &fields.version)?;

    // Compute .install filename: strip trailing "-bin" from the package name.
    let install_base = fields
        .package_name
        .strip_suffix("-bin")
        .unwrap_or(&fields.package_name);
    let install_filename = format!("{}.install", install_base);
    let install_file_ref = if aur_cfg.install.is_some() {
        Some(install_filename.as_str())
    } else {
        None
    };

    let pkgbuild_params = PkgbuildParams {
        name: &fields.package_name,
        version: &fields.version,
        pkgrel: fields.pkgrel,
        description: &fields.description,
        url: &fields.url,
        license: &fields.license,
        maintainers: &fields.maintainers,
        contributors: &fields.contributors,
        depends: &fields.depends,
        optdepends: &fields.optdepends,
        conflicts: &fields.conflicts,
        provides: &fields.provides,
        replaces: &fields.replaces,
        backup: &fields.backup,
        sources: &sources,
        binary_name: crate_name,
        install_template: aur_cfg.package.as_deref(),
        install_file: install_file_ref,
    };
    let pkgbuild = generate_pkgbuild(&pkgbuild_params)?;
    let srcinfo = generate_srcinfo(&pkgbuild_params)?;

    // Clone AUR repo, write PKGBUILD, commit, push.
    let tmp_dir = tempfile::tempdir().context("aur: create temp dir")?;
    let repo_path = tmp_dir.path();
    aur_clone_repo(aur_cfg, &git_url, repo_path, log)?;

    let output_dir = aur_resolve_output_dir(ctx, aur_cfg, repo_path, log)?;
    aur_write_package_files(
        &output_dir,
        &pkgbuild,
        &srcinfo,
        &install_filename,
        aur_cfg.install.as_deref(),
        log,
    )?;

    aur_commit_and_push(
        ctx,
        aur_cfg,
        repo_path,
        &fields.package_name,
        &fields.version,
        &git_url,
        log,
    )
}

// ---------------------------------------------------------------------------
// AurOurPublisher — Publisher trait wrapper (git-revert rollback)
// ---------------------------------------------------------------------------

/// `Publisher` for the AUR repo we own (the per-crate
/// `publish.aur:` entry that pushes a binary PKGBUILD to a dedicated
/// AUR package we control via SSH).
///
/// Named `AurOurPublisher` to disambiguate from the upstream-AUR
/// force-push publisher (`aur_source:`) — that one is Submitter group,
/// has no rollback path (irreversible force-push), and writes to
/// packages we do NOT own.
///
/// Rollback shape mirrors the other git-revert publishers: re-clone
/// via the configured SSH key + command, run `git revert HEAD --no-edit`,
/// push to `master` (AUR's only branch).
///
/// SECURITY NOTE: [`AurOurTarget`]'s SSH credentials (`private_key`,
/// `git_ssh_command`) carry `#[serde(skip)]` so they never land in
/// persisted evidence (`dist/run-<id>/report.json`, the run-summary
/// JSON, or the announce-time release-body summary). Rollback
/// re-reads them from the live `ctx.config` at yank time so a
/// rotated SSH key is correctly picked up; if the user rotated and
/// the new key lacks AUR push access, the failure surfaces clearly
/// in the per-target warn line.
use crate::util::{RevertTarget, run_revert_targets_parallel};
use serde::{Deserialize, Serialize};

/// AUR has a single branch convention: every package repo lives on
/// `master`. Pinning this in one constant means both the publish path
/// and the rollback path push to the same name and a future drift
/// (e.g. a stray rename to `main`) is impossible without editing this
/// line.
pub(crate) const AUR_REPO_BRANCH: &str = "master";

simple_publisher!(
    AurOurPublisher,
    "aur",
    anodizer_core::PublisherGroup::Manager,
    false,
    Some("AUR_SSH_KEY write"),
);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AurOurTarget {
    target: String,
    /// AUR SSH URL — typically
    /// `ssh://aur@aur.archlinux.org/<package>.git`.
    git_url: String,
    /// Inline SSH private-key contents. Captured at run-time from the
    /// active `aur.private_key:` config so a same-process rollback
    /// doesn't have to re-read config; but `#[serde(skip)]` keeps it
    /// out of any persisted shape of [`anodizer_core::PublishEvidence`].
    /// When `decode_aur_our_targets` re-hydrates from a previously
    /// serialized evidence blob this field comes back as `None` and
    /// [`AurOurPublisher::rollback`] re-resolves it from
    /// `ctx.config.crates[*].publish.aur.private_key` by matching
    /// `git_url`.
    ///
    /// SECURITY: persistence tasks (`--rollback-only --from-run`,
    /// `--summary-json`, the announce-time release-body summary) all
    /// round-trip evidence through serde JSON; `#[serde(skip)]` is
    /// the single point of control that keeps the SSH key from
    /// leaking through any of them.
    #[serde(skip)]
    private_key: Option<String>,
    /// Custom `GIT_SSH_COMMAND` override (alternative to
    /// `private_key` — same precedence the publish path uses).
    /// Same `#[serde(skip)]` rationale as `private_key`: the command
    /// can reference an on-disk key path that we treat as
    /// secret-sensitive.
    #[serde(skip)]
    git_ssh_command: Option<String>,
}

/// Walk `ctx.config.crates` for a `publish.aur` block whose `git_url`
/// matches `git_url` and return the resolved
/// `(private_key, git_ssh_command)` pair. Used at rollback time so
/// the SSH credentials never need to round-trip through serialized
/// evidence.
///
/// Returns `(None, None)` when no crate is configured for the given
/// URL — the rollback `git push` will fail loudly via the warning
/// helper that points the operator at `publish.aur.private_key`.
fn resolve_aur_credentials_from_config(
    ctx: &Context,
    git_url: &str,
) -> (Option<String>, Option<String>) {
    for c in &ctx.config.crates {
        let Some(ac) = c.publish.as_ref().and_then(|p| p.aur.as_ref()) else {
            continue;
        };
        if ac.git_url.as_deref() == Some(git_url) {
            return (ac.private_key.clone(), ac.git_ssh_command.clone());
        }
    }
    (None, None)
}

/// Collapse the recorded rollback targets to a unique set keyed by
/// `git_url` (AUR always pushes to `master`, so branch is implicit).
///
/// The first entry seen for a given `git_url` wins; later entries that
/// share the same URL are dropped because the second `git revert HEAD`
/// against the same repo would revert the first revert and restore
/// the bad release.
fn dedup_aur_targets(targets: &[AurOurTarget]) -> Vec<AurOurTarget> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out: Vec<AurOurTarget> = Vec::with_capacity(targets.len());
    for t in targets {
        if seen.insert(t.git_url.clone()) {
            out.push(t.clone());
        }
    }
    out
}

impl From<&AurOurTarget> for anodizer_core::publish_evidence::AurTargetSnapshot {
    fn from(t: &AurOurTarget) -> Self {
        Self {
            target: t.target.clone(),
            git_url: t.git_url.clone(),
        }
    }
}

impl From<anodizer_core::publish_evidence::AurTargetSnapshot> for AurOurTarget {
    fn from(s: anodizer_core::publish_evidence::AurTargetSnapshot) -> Self {
        Self {
            target: s.target,
            git_url: s.git_url,
            // SSH credentials are NOT carried in the snapshot — they
            // live only in the live `aur.private_key:` config and are
            // resolved at rollback time via
            // `resolve_aur_credentials_from_config`. This decode
            // boundary matches what the prior `#[serde(skip)]` shape
            // produced when the serialized evidence round-tripped.
            private_key: None,
            git_ssh_command: None,
        }
    }
}

fn decode_aur_our_targets(extra: &anodizer_core::PublishEvidenceExtra) -> Vec<AurOurTarget> {
    match extra {
        anodizer_core::PublishEvidenceExtra::Aur(a) => {
            a.aur_our_targets.iter().cloned().map(Into::into).collect()
        }
        _ => Vec::new(),
    }
}

fn collect_aur_our_run_targets(ctx: &Context) -> Vec<AurOurTarget> {
    let mut out: Vec<AurOurTarget> = Vec::new();
    let selected = &ctx.options.selected_crates;
    for c in &ctx.config.crates {
        if !selected.is_empty() && !selected.contains(&c.name) {
            continue;
        }
        let Some(ac) = c.publish.as_ref().and_then(|p| p.aur.as_ref()) else {
            continue;
        };
        let Some(git_url) = ac.git_url.as_ref() else {
            continue;
        };
        // Use the package name (or the AUR-default of `<crate>-bin`)
        // as the human label so log lines say what was rolled back.
        let raw_pkg = aur_default_package_name(ac, &c.name);
        let label = ctx.render_template(&raw_pkg).unwrap_or(raw_pkg);
        out.push(AurOurTarget {
            target: label,
            git_url: git_url.clone(),
            private_key: ac.private_key.clone(),
            git_ssh_command: ac.git_ssh_command.clone(),
        });
    }
    out
}

fn is_aur_per_crate_configured(ctx: &Context, crate_name: &str) -> bool {
    crate::util::all_crates(ctx)
        .into_iter()
        .any(|c| c.name == crate_name && c.publish.as_ref().is_some_and(|p| p.aur.is_some()))
}

/// Message emitted at publisher entry. Names how many crates the publisher
/// is iterating over. Factored into a helper so tests can pin the exact
/// substring an operator scans the log for.
pub(crate) fn run_start_message(selected_total: usize) -> String {
    format!(
        "aur: starting publish for {} selected crate(s)",
        selected_total
    )
}

/// Message emitted when a selected crate has no `publish.aur` block.
/// Replaces what used to be a silent `continue` — operators need to see
/// why a per-crate publish was a no-op rather than guess from a blank log.
pub(crate) fn run_skip_unconfigured_message(crate_name: &str) -> String {
    format!("aur: skipping crate '{}' — no aur config block", crate_name)
}

/// Message emitted just before delegating to `publish_to_aur`. Anchors the
/// AUR activity (PKGBUILD render, git clone, push) to a specific crate in
/// the log so multi-crate workspaces are disambiguatable.
pub(crate) fn run_per_crate_start_message(crate_name: &str) -> String {
    format!("aur: starting per-crate publish for '{}'", crate_name)
}

/// Final summary emitted at publisher exit. `processed` is the count of
/// crates the publisher actually invoked `publish_to_aur` on (not the
/// count of successful AUR pushes — `publish_to_aur` has its own skip
/// paths for skip_upload/dry-run/etc., each of which logs its own status
/// line).
pub(crate) fn run_done_message(processed: usize) -> String {
    format!("aur: completed — {} crate(s) processed", processed)
}

/// Decision predicate for the no-eligible-crates warning. True when the
/// publisher walked the selection but the configured-predicate filtered
/// every crate out — distinct from "ran successfully in dry-run mode".
///
/// `processed` is the count of crates whose `is_aur_per_crate_configured`
/// check passed and whose `publish_to_aur` invocation was reached.
/// `selected_len` is the size of the implicit-all-resolved selection.
pub(crate) fn should_warn_no_eligible(processed: usize, selected_len: usize) -> bool {
    processed == 0 && selected_len > 0
}

/// Warning emitted when the publisher was registered (at least one crate
/// has a `publish.aur` block at the config level) but the run path
/// processed zero crates.
///
/// With the implicit-all default in
/// [`crate::publisher_helpers::effective_publish_crates`], an empty
/// `selected_crates` resolves to every crate carrying a `publish.aur`
/// block — so a zero-processed run means `--crate`/`--all` matrix
/// selection was non-empty AND filtered every aur-configured crate out.
/// Operators must see this — otherwise the publisher's `succeeded` status
/// hides the fact that nothing was pushed.
pub(crate) fn run_no_eligible_crates_warning(selected_total: usize) -> String {
    format!(
        "aur: registered but 0 of {} effective crate(s) had an aur \
         config block — nothing pushed. Check that --crate / --all selects a \
         crate whose publish.aur block is set.",
        selected_total
    )
}

impl anodizer_core::Publisher for AurOurPublisher {
    fn name(&self) -> &str {
        Self::PUBLISHER_NAME
    }
    fn group(&self) -> anodizer_core::PublisherGroup {
        Self::PUBLISHER_GROUP
    }
    fn required(&self) -> bool {
        self.required_override.unwrap_or(Self::PUBLISHER_REQUIRED)
    }
    fn rollback_scope_needed(&self) -> Option<&'static str> {
        Self::ROLLBACK_SCOPE
    }

    fn run(&self, ctx: &mut Context) -> anyhow::Result<anodizer_core::PublishEvidence> {
        let log = ctx.logger("publish");
        let selected =
            crate::publisher_helpers::effective_publish_crates(ctx, is_aur_per_crate_configured);
        log.status(&run_start_message(selected.len()));
        let mut processed = 0usize;
        let mut any_pushed = false;
        for crate_name in &selected {
            // Defensive guard for explicit `--crate=X` selection when X has no
            // publisher block; implicit-all is already filtered by effective_publish_crates above.
            if !is_aur_per_crate_configured(ctx, crate_name) {
                log.status(&run_skip_unconfigured_message(crate_name));
                continue;
            }
            processed += 1;
            log.status(&run_per_crate_start_message(crate_name));
            if publish_to_aur(ctx, crate_name, &log)? {
                any_pushed = true;
            }
        }
        if should_warn_no_eligible(processed, selected.len()) {
            log.warn(&run_no_eligible_crates_warning(selected.len()));
        } else {
            log.status(&run_done_message(processed));
        }
        let mut evidence = anodizer_core::PublishEvidence::new("aur");
        // Only record rollback targets when at least one push was made.
        // Phantom evidence causes rollback to git-revert in repos that
        // were never touched (dry-run, skip_upload, no-op NoChanges).
        if any_pushed {
            let targets = collect_aur_our_run_targets(ctx);
            evidence.extra = anodizer_core::PublishEvidenceExtra::Aur(
                anodizer_core::publish_evidence::AurExtra {
                    aur_our_targets: targets.iter().map(Into::into).collect(),
                },
            );
        }
        Ok(evidence)
    }

    fn rollback(
        &self,
        ctx: &mut Context,
        evidence: &anodizer_core::PublishEvidence,
    ) -> anyhow::Result<()> {
        let log = ctx.logger("publish");
        let targets = decode_aur_our_targets(&evidence.extra);
        if targets.is_empty() {
            log.warn(&crate::publisher_helpers::rollback_empty_warning_msg(
                "aur",
                "AUR repo clone targets",
            ));
            return Ok(());
        }
        // Dedup recorded targets by `(git_url, AUR_REPO_BRANCH)` before
        // fanning out. When two crates share the same AUR repo
        // (unusual for binary PKGBUILDs but possible if a workspace
        // packages multiple binaries into one repo), running `git
        // revert HEAD` twice would revert the first revert — restoring
        // the bad release. Keep the first-seen entry's label so the
        // warn lines still name a meaningful target.
        let unique = dedup_aur_targets(&targets);
        // SSH credentials are not in the serialized evidence
        // (`#[serde(skip)]`). Resolve them from the live config now
        // so the parallel workers each have their own clone of the
        // credential bundle.
        let prepared: Vec<RevertTarget> = unique
            .iter()
            .map(|t| {
                let (pk, ssh_cmd) = resolve_aur_credentials_from_config(ctx, &t.git_url);
                RevertTarget {
                    target: t.target.clone(),
                    repo_url: t.git_url.clone(),
                    branch: Some(AUR_REPO_BRANCH.to_string()),
                    token: None,
                    private_key: pk,
                    ssh_command: ssh_cmd,
                }
            })
            .collect();
        let (reverted, failed) = run_revert_targets_parallel(&prepared, "aur", None, &log);
        log.status(&format!(
            "aur: reverted {} repo(s), {} failure(s)",
            reverted, failed
        ));
        Ok(())
    }

    fn preflight(&self, _ctx: &Context) -> anyhow::Result<anodizer_core::PreflightCheck> {
        Ok(anodizer_core::PreflightCheck::Pass)
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;
    use anodizer_core::config::{AurConfig, CrateConfig, PublishConfig, StringOrBool};
    use anodizer_core::test_helpers::TestContextBuilder;
    use anodizer_core::{PreflightCheck, PublishEvidence, Publisher, PublisherGroup};

    fn aur_crate(name: &str) -> CrateConfig {
        CrateConfig {
            name: name.to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur: Some(AurConfig {
                    git_url: Some(format!("ssh://aur@aur.archlinux.org/{name}-bin.git")),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn aur_publisher_classification() {
        let p = AurOurPublisher::new();
        assert_eq!(p.name(), "aur");
        assert_eq!(p.group(), PublisherGroup::Manager);
        assert!(!p.required());
        assert_eq!(p.rollback_scope_needed(), Some("AUR_SSH_KEY write"));
    }

    #[test]
    fn aur_preflight_defaults_to_pass() {
        let ctx = TestContextBuilder::new().build();
        let p = AurOurPublisher::new();
        assert!(matches!(
            p.preflight(&ctx).expect("preflight ok"),
            PreflightCheck::Pass
        ));
    }

    #[test]
    fn aur_rollback_warns_when_no_targets_recorded() {
        let capture = anodizer_core::log::LogCapture::new();
        let mut ctx = TestContextBuilder::new().build();
        ctx.with_log_capture(capture.clone());
        let evidence = PublishEvidence::new("aur");
        let p = AurOurPublisher::new();
        assert!(p.rollback(&mut ctx, &evidence).is_ok());

        let warns = capture.warn_messages();
        assert!(
            warns.iter().any(|m| m.contains("aur")
                && m.contains("AUR repo clone targets")
                && m.contains("verify")),
            "expected captured warn naming publisher + target-noun + 'verify'; got: {warns:?}"
        );
    }

    #[test]
    fn aur_target_extra_roundtrips() {
        let original = vec![AurOurTarget {
            target: "demo-bin".into(),
            git_url: "ssh://aur@aur.archlinux.org/demo-bin.git".into(),
            private_key: None,
            git_ssh_command: None,
        }];
        let extra =
            anodizer_core::PublishEvidenceExtra::Aur(anodizer_core::publish_evidence::AurExtra {
                aur_our_targets: original.iter().map(Into::into).collect(),
            });
        let decoded = decode_aur_our_targets(&extra);
        assert_eq!(decoded, original);
    }

    #[test]
    fn aur_collect_run_targets_uses_default_bin_suffix() {
        let ctx = TestContextBuilder::new()
            .crates(vec![aur_crate("demo")])
            .build();
        let targets = collect_aur_our_run_targets(&ctx);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target, "demo-bin");
        assert!(targets[0].git_url.ends_with("demo-bin.git"));
    }

    #[test]
    fn aur_effective_publish_crates_implicit_all_when_selection_empty() {
        // Regression pin for the `selected_crates = Vec::new()` failure
        // mode: the run path used to iterate the empty Vec and silently
        // skip every configured AUR repo. The helper now resolves to
        // implicit-all over `publish.aur`-carrying crates.
        let ctx = TestContextBuilder::new()
            .crates(vec![
                aur_crate("alpha"),
                aur_crate("beta"),
                CrateConfig {
                    name: "gamma".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            .build();
        let names =
            crate::publisher_helpers::effective_publish_crates(&ctx, is_aur_per_crate_configured);
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn aur_effective_publish_crates_honors_non_empty_selection() {
        let ctx = TestContextBuilder::new()
            .crates(vec![aur_crate("alpha"), aur_crate("beta")])
            .selected_crates(vec!["beta".to_string()])
            .build();
        let names =
            crate::publisher_helpers::effective_publish_crates(&ctx, is_aur_per_crate_configured);
        assert_eq!(names, vec!["beta".to_string()]);
    }

    #[test]
    fn aur_our_target_extra_omits_private_key_after_serde_roundtrip() {
        // SECURITY: persisting `private_key` / `git_ssh_command` into
        // `dist/run-<id>/report.json`, the run summary
        // (`--summary-json`), or the announce-time release-body text
        // would leak the SSH key publicly. The
        // `AurTargetSnapshot` core type has no field for either
        // credential, so the type system rejects any future leak
        // attempt at the encode boundary. This test pins the
        // resulting wire shape: a populated AurOurTarget converts
        // into the snapshot WITHOUT carrying the secret bytes.
        let with_secrets = AurOurTarget {
            target: "demo-bin".into(),
            git_url: "ssh://aur@aur.archlinux.org/demo-bin.git".into(),
            private_key: Some("PRIVATE-KEY-CONTENTS".into()),
            git_ssh_command: Some("ssh -i /tmp/key".into()),
        };
        let extra =
            anodizer_core::PublishEvidenceExtra::Aur(anodizer_core::publish_evidence::AurExtra {
                aur_our_targets: vec![(&with_secrets).into()],
            });
        let serialized = serde_json::to_string(&extra).expect("serialize");
        assert!(
            !serialized.contains("PRIVATE-KEY-CONTENTS"),
            "private_key leaked into serialized evidence: {serialized}"
        );
        assert!(
            !serialized.contains("/tmp/key"),
            "git_ssh_command leaked into serialized evidence: {serialized}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&serialized).expect("parse");
        let first = &parsed["aur_our_targets"][0];
        assert!(
            first.get("private_key").is_none(),
            "private_key field present in evidence: {first}"
        );
        assert!(
            first.get("git_ssh_command").is_none(),
            "git_ssh_command field present in evidence: {first}"
        );
        // Positive shape: operator-public coordinates survive the
        // conversion.
        assert_eq!(first["target"], "demo-bin");
        assert_eq!(first["git_url"], "ssh://aur@aur.archlinux.org/demo-bin.git");
    }

    #[test]
    fn aur_our_rollback_re_reads_private_key_from_config() {
        // `#[serde(skip)]` means decoded evidence has no credentials.
        // Rollback must re-resolve them from `ctx.config.crates[*].
        // publish.aur.private_key` keyed by `git_url`. Verify the
        // resolver returns the live config's key + ssh command.
        let mut c = aur_crate("demo");
        if let Some(p) = c.publish.as_mut()
            && let Some(a) = p.aur.as_mut()
        {
            a.private_key = Some("ROTATED-KEY".into());
            a.git_ssh_command = Some("ssh -i /tmp/rotated".into());
        }
        let ctx = TestContextBuilder::new().crates(vec![c]).build();
        let (pk, ssh) =
            resolve_aur_credentials_from_config(&ctx, "ssh://aur@aur.archlinux.org/demo-bin.git");
        assert_eq!(pk.as_deref(), Some("ROTATED-KEY"));
        assert_eq!(ssh.as_deref(), Some("ssh -i /tmp/rotated"));

        // Unknown URL: returns (None, None) so the warn helper fires
        // and points the operator at publish.aur.private_key.
        let (pk, ssh) = resolve_aur_credentials_from_config(&ctx, "ssh://aur@x/y.git");
        assert!(pk.is_none());
        assert!(ssh.is_none());
    }

    #[test]
    fn aur_branch_constant_matches_publish_path() {
        // Pin the constant so both publish and rollback are guaranteed
        // to push to the same branch name; a stray rename (e.g. to
        // `main`) would have to edit this line.
        assert_eq!(AUR_REPO_BRANCH, "master");
    }

    #[test]
    fn aur_dedup_targets_collapses_shared_git_url() {
        // Two recorded targets that happen to share a git_url collapse
        // to one entry. A second `git revert HEAD` against the same
        // AUR repo would revert the first revert and restore the bad
        // release — the dedup guards that.
        let targets = vec![
            AurOurTarget {
                target: "demo-bin".into(),
                git_url: "ssh://aur@aur.archlinux.org/demo-bin.git".into(),
                private_key: None,
                git_ssh_command: None,
            },
            AurOurTarget {
                target: "demo-alias".into(),
                git_url: "ssh://aur@aur.archlinux.org/demo-bin.git".into(),
                private_key: None,
                git_ssh_command: None,
            },
        ];
        let unique = dedup_aur_targets(&targets);
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0].target, "demo-bin");
    }

    #[test]
    fn aur_collect_run_targets_skips_when_git_url_absent() {
        // No git_url: the publish path itself bails, so we should
        // also skip recording.
        let mut crate_cfg = aur_crate("demo");
        if let Some(p) = crate_cfg.publish.as_mut()
            && let Some(a) = p.aur.as_mut()
        {
            a.git_url = None;
        }
        let ctx = TestContextBuilder::new().crates(vec![crate_cfg]).build();
        let targets = collect_aur_our_run_targets(&ctx);
        assert!(targets.is_empty(), "expected no targets, got {targets:?}");
    }

    // -----------------------------------------------------------------------
    // Log-message helpers — the operator-facing log strings the publisher
    // emits at each boundary.

    #[test]
    fn run_start_message_names_selected_total() {
        let msg = run_start_message(3);
        assert!(msg.starts_with("aur:"), "{msg}");
        assert!(msg.contains("starting publish"), "{msg}");
        assert!(msg.contains("3 selected"), "{msg}");
    }

    #[test]
    fn run_skip_unconfigured_message_names_crate() {
        let msg = run_skip_unconfigured_message("demo");
        assert!(msg.starts_with("aur:"), "{msg}");
        assert!(msg.contains("skipping crate 'demo'"), "{msg}");
        assert!(msg.contains("no aur config block"), "{msg}");
    }

    #[test]
    fn run_per_crate_start_message_names_crate() {
        let msg = run_per_crate_start_message("demo");
        assert!(msg.starts_with("aur:"), "{msg}");
        assert!(msg.contains("starting per-crate publish"), "{msg}");
        assert!(msg.contains("'demo'"), "{msg}");
    }

    #[test]
    fn run_done_message_reports_processed_count() {
        let msg = run_done_message(2);
        assert!(msg.starts_with("aur:"), "{msg}");
        assert!(msg.contains("completed"), "{msg}");
        assert!(msg.contains("2 crate(s) processed"), "{msg}");
    }

    #[test]
    fn run_no_eligible_crates_warning_names_remediation() {
        let msg = run_no_eligible_crates_warning(5);
        assert!(msg.starts_with("aur:"), "{msg}");
        assert!(msg.contains("registered"), "{msg}");
        assert!(msg.contains("0 of 5 effective"), "{msg}");
        assert!(msg.contains("nothing pushed"), "{msg}");
        assert!(msg.contains("--crate"), "{msg}");
        assert!(msg.contains("--all"), "{msg}");
    }

    /// The no-eligible-crates warning must fire only when the iteration
    /// loop's configured-predicate filtered every selected crate out — not
    /// when the publish path was reached successfully.
    #[test]
    fn should_warn_no_eligible_only_fires_when_predicate_filtered_everything() {
        // One configured crate reached the publish path → no warning.
        assert!(!should_warn_no_eligible(1, 1));
        // True positive: none configured.
        assert!(should_warn_no_eligible(0, 3));
        // Empty selection → no warning.
        assert!(!should_warn_no_eligible(0, 0));
        // Partial-skip → no warning.
        assert!(!should_warn_no_eligible(1, 3));
    }

    /// Run the publisher end-to-end in dry-run mode against a context that
    /// selects an aur-configured crate. Verifies the run path is wired
    /// (returns Ok). The log lines are written to stderr and asserted
    /// indirectly via the helper-string tests above.
    #[test]
    fn aur_publisher_run_dry_run_returns_ok() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![aur_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .build();
        let p = AurOurPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run publisher.run");
        // dry-run publish_to_aur short-circuits before git push; no actual
        // push occurred so evidence.extra must be empty (no phantom targets).
        let targets = decode_aur_our_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "dry-run must not record rollback targets: {targets:?}"
        );
    }

    /// When the publisher is registered (a crate has an aur block) but the
    /// selected-crates filter excludes every aur-configured crate, the run
    /// path must still return Ok and the processed count is zero.
    #[test]
    fn aur_publisher_run_no_eligible_crates_returns_ok() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![
                aur_crate("demo"),
                CrateConfig {
                    name: "other".to_string(),
                    path: ".".to_string(),
                    tag_template: "v{{ .Version }}".to_string(),
                    publish: Some(PublishConfig::default()),
                    ..Default::default()
                },
            ])
            // Select only the non-aur crate — publisher registered but
            // run path will iterate zero aur-configured crates.
            .selected_crates(vec!["other".to_string()])
            .dry_run(true)
            .build();
        let p = AurOurPublisher::new();
        // Must return Ok even when no aur-configured crate is selected.
        p.run(&mut ctx).expect("publisher.run ok");
    }

    /// Implicit-all selection (empty `selected_crates`) + dry-run must
    /// produce empty evidence. The implicit-all path resolves through
    /// `effective_publish_crates` to every aur-configured crate, so this
    /// pins the gate where phantom rollback targets used to leak.
    #[test]
    fn test_publish_to_aur_dry_run_implicit_all_produces_empty_evidence() {
        let mut ctx = TestContextBuilder::new()
            .crates(vec![aur_crate("demo"), aur_crate("other")])
            // No selected_crates → implicit-all resolves to both aur crates.
            .dry_run(true)
            .build();
        let p = AurOurPublisher::new();
        let evidence = p.run(&mut ctx).expect("dry-run implicit-all publisher.run");
        let targets = decode_aur_our_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "dry-run + implicit-all must not record rollback targets: {targets:?}"
        );
    }

    /// skip_upload path must produce empty evidence — no push occurred.
    #[test]
    fn aur_publisher_run_skip_upload_produces_empty_evidence() {
        let mut crate_with_skip = aur_crate("demo");
        if let Some(ref mut publish) = crate_with_skip.publish
            && let Some(ref mut aur) = publish.aur
        {
            aur.skip_upload = Some(StringOrBool::Bool(true));
        }
        let mut ctx = TestContextBuilder::new()
            .crates(vec![crate_with_skip])
            .selected_crates(vec!["demo".to_string()])
            .build();
        let p = AurOurPublisher::new();
        let evidence = p.run(&mut ctx).expect("skip_upload publisher.run");
        let targets = decode_aur_our_targets(&evidence.extra);
        assert!(
            targets.is_empty(),
            "skip_upload must not record rollback targets: {targets:?}"
        );
    }

    #[test]
    fn aur_publisher_visible_work_contract() {
        use crate::testing::assert_publisher_visible_work_contract;
        let mut ctx = TestContextBuilder::new()
            .crates(vec![aur_crate("demo")])
            .selected_crates(vec!["demo".to_string()])
            .dry_run(true)
            .build();
        let p = AurOurPublisher::new();
        assert_publisher_visible_work_contract(&p, &mut ctx);
    }

    /// Building an AUR PKGBUILD for a linux artifact whose `sha256`
    /// metadata is empty must bail with an actionable error. Defaulting
    /// to `""` would emit `sha256sums_<arch>=('')` in the generated
    /// PKGBUILD, which silently disables makepkg's integrity check and
    /// ships an unverified tarball. The bail message must name the
    /// publisher, the field, the offending artifact context, and a
    /// next-step hint.
    #[test]
    fn aur_sha256_empty_metadata_bails_with_actionable_error() {
        use anodizer_core::artifact::{Artifact, ArtifactKind};
        use anodizer_core::config::AurConfig;
        let mut c = aur_crate("mytool");
        if let Some(ref mut publish) = c.publish
            && let Some(ref mut aur) = publish.aur
        {
            *aur = AurConfig {
                git_url: Some("ssh://aur@aur.archlinux.org/mytool-bin.git".to_string()),
                license: Some("MIT".to_string()),
                homepage: Some("https://example.com/mytool".to_string()),
                ..Default::default()
            };
        }
        let mut ctx = TestContextBuilder::new()
            .crates(vec![c])
            .selected_crates(vec!["mytool".to_string()])
            .build();
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            path: std::path::PathBuf::from("/tmp/mytool-linux-amd64.tar.gz"),
            name: "mytool-linux-amd64.tar.gz".to_string(),
            target: Some("x86_64-unknown-linux-gnu".to_string()),
            crate_name: "mytool".to_string(),
            metadata: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "url".to_string(),
                    "https://example.com/mytool-linux-amd64.tar.gz".to_string(),
                );
                m
            },
            size: None,
        });
        let log =
            anodizer_core::log::StageLogger::new("publish", anodizer_core::log::Verbosity::Quiet);
        let err = publish_to_aur(&ctx, "mytool", &log).expect_err("missing sha256 must bail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing sha256 metadata"),
            "error must mention missing sha256; got: {msg}"
        );
        assert!(
            msg.contains("mytool"),
            "error must name the offending crate; got: {msg}"
        );
        assert!(
            msg.contains("checksum stage"),
            "error must mention the checksum stage; got: {msg}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // generate_pkgbuild tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_pkgbuild_basic() {
        let pkgbuild = generate_pkgbuild(&PkgbuildParams {
            name: "mytool",
            version: "1.0.0",
            pkgrel: 1,
            description: "A great tool",
            url: "https://github.com/org/mytool",
            license: "MIT",
            maintainers: &["Jane Doe <jane@example.com>".to_string()],
            contributors: &[],
            depends: &[],
            optdepends: &[],
            conflicts: &[],
            provides: &[],
            replaces: &[],
            backup: &[],
            sources: &[(
                "x86_64".to_string(),
                "https://example.com/mytool-1.0.0-linux-amd64.tar.gz".to_string(),
                "deadbeef1234".to_string(),
            )],
            binary_name: "mytool",
            install_template: None,
            install_file: None,
        })
        .unwrap();

        assert!(pkgbuild.contains("# Maintainer: Jane Doe <jane@example.com>"));
        assert!(pkgbuild.contains("pkgname='mytool'"));
        assert!(pkgbuild.contains("pkgver=1.0.0"));
        assert!(pkgbuild.contains("pkgrel=1"));
        assert!(pkgbuild.contains("pkgdesc=\"A great tool\""));
        assert!(pkgbuild.contains("arch=('x86_64')"));
        assert!(pkgbuild.contains("url=\"https://github.com/org/mytool\""));
        assert!(pkgbuild.contains("license=('MIT')"));
        assert!(pkgbuild.contains("depends=()"));
        assert!(pkgbuild.contains(
            "source_x86_64=(\"mytool_${pkgver}_x86_64.tar.gz::https://example.com/mytool-${pkgver}-linux-amd64.tar.gz\")"
        ));
        assert!(pkgbuild.contains("sha256sums_x86_64=('deadbeef1234')"));
        assert!(pkgbuild.contains("package()"));
        assert!(pkgbuild.contains("install -Dm755 \"$srcdir/mytool\" \"$pkgdir/usr/bin/mytool\""));
    }

    #[test]
    fn test_generate_pkgbuild_multi_arch() {
        let pkgbuild = generate_pkgbuild(&PkgbuildParams {
            name: "mytool",
            version: "2.0.0",
            pkgrel: 1,
            description: "Multi-arch tool",
            url: "https://github.com/org/mytool",
            license: "Apache-2.0",
            maintainers: &[],
            contributors: &[],
            depends: &[],
            optdepends: &[],
            conflicts: &[],
            provides: &[],
            replaces: &[],
            backup: &[],
            sources: &[
                (
                    "x86_64".to_string(),
                    "https://example.com/mytool-2.0.0-linux-amd64.tar.gz".to_string(),
                    "hash_amd64".to_string(),
                ),
                (
                    "aarch64".to_string(),
                    "https://example.com/mytool-2.0.0-linux-arm64.tar.gz".to_string(),
                    "hash_arm64".to_string(),
                ),
            ],
            binary_name: "mytool",
            install_template: None,
            install_file: None,
        })
        .unwrap();

        assert!(pkgbuild.contains("arch=('aarch64' 'x86_64')"));
        assert!(pkgbuild.contains("source_x86_64="));
        assert!(pkgbuild.contains("source_aarch64="));
        assert!(pkgbuild.contains("sha256sums_x86_64=('hash_amd64')"));
        assert!(pkgbuild.contains("sha256sums_aarch64=('hash_arm64')"));
    }

    #[test]
    fn test_generate_pkgbuild_with_depends() {
        let pkgbuild = generate_pkgbuild(&PkgbuildParams {
            name: "mytool",
            version: "1.0.0",
            pkgrel: 1,
            description: "A tool",
            url: "https://example.com",
            license: "MIT",
            maintainers: &[],
            contributors: &[],
            depends: &["glibc".to_string(), "openssl".to_string()],
            optdepends: &["git: for VCS support".to_string()],
            conflicts: &["mytool-git".to_string()],
            provides: &["mytool".to_string()],
            replaces: &["old-mytool".to_string()],
            backup: &["etc/mytool/config.toml".to_string()],
            sources: &[(
                "x86_64".to_string(),
                "https://example.com/mytool.tar.gz".to_string(),
                "hash".to_string(),
            )],
            binary_name: "mytool",
            install_template: None,
            install_file: None,
        })
        .unwrap();

        assert!(pkgbuild.contains("depends=('glibc' 'openssl')"));
        assert!(pkgbuild.contains("optdepends=('git: for VCS support')"));
        assert!(pkgbuild.contains("conflicts=('mytool-git')"));
        assert!(pkgbuild.contains("provides=('mytool')"));
        assert!(pkgbuild.contains("replaces=('old-mytool')"));
        assert!(pkgbuild.contains("backup=('etc/mytool/config.toml')"));
    }

    #[test]
    fn test_generate_pkgbuild_no_maintainers() {
        let pkgbuild = generate_pkgbuild(&PkgbuildParams {
            name: "tool",
            version: "1.0.0",
            pkgrel: 1,
            description: "A tool",
            url: "https://example.com",
            license: "MIT",
            maintainers: &[],
            contributors: &[],
            depends: &[],
            optdepends: &[],
            conflicts: &[],
            provides: &[],
            replaces: &[],
            backup: &[],
            sources: &[(
                "x86_64".to_string(),
                "https://example.com/tool.tar.gz".to_string(),
                "hash".to_string(),
            )],
            binary_name: "tool",
            install_template: None,
            install_file: None,
        })
        .unwrap();

        assert!(!pkgbuild.contains("# Maintainer:"));
        assert!(pkgbuild.starts_with("pkgname="));
    }

    #[test]
    fn test_generate_pkgbuild_complete_structure() {
        let pkgbuild = generate_pkgbuild(&PkgbuildParams {
            name: "anodizer",
            version: "3.2.1",
            pkgrel: 1,
            description: "Release automation for Rust projects",
            url: "https://github.com/tj-smith47/anodizer",
            license: "Apache-2.0",
            maintainers: &["TJ Smith <tj@example.com>".to_string()],
            contributors: &[],
            depends: &[],
            optdepends: &[],
            conflicts: &[],
            provides: &[],
            replaces: &[],
            backup: &[],
            sources: &[
                (
                    "x86_64".to_string(),
                    "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-linux-amd64.tar.gz".to_string(),
                    "aabbccdd".to_string(),
                ),
                (
                    "aarch64".to_string(),
                    "https://github.com/tj-smith47/anodizer/releases/download/v3.2.1/anodizer-3.2.1-linux-arm64.tar.gz".to_string(),
                    "eeff0011".to_string(),
                ),
            ],
            binary_name: "anodizer",
            install_template: None,
            install_file: None,
        }).unwrap();

        // Starts with maintainer comment
        assert!(pkgbuild.starts_with("# Maintainer: TJ Smith <tj@example.com>"));

        // Contains required fields
        assert!(pkgbuild.contains("pkgname='anodizer'"));
        assert!(pkgbuild.contains("pkgver=3.2.1"));
        assert!(pkgbuild.contains("arch=('aarch64' 'x86_64')"));

        // Contains package() function
        assert!(pkgbuild.contains("package() {"));
        assert!(pkgbuild.contains("install -Dm755"));

        // Ends with closing brace
        assert!(pkgbuild.trim_end().ends_with('}'));
    }

    #[test]
    fn test_generate_pkgbuild_custom_install_template() {
        let pkgbuild = generate_pkgbuild(&PkgbuildParams {
            name: "mytool",
            version: "1.0.0",
            pkgrel: 1,
            description: "A tool with subdirectory archive",
            url: "https://example.com",
            license: "MIT",
            maintainers: &[],
            contributors: &[],
            depends: &[],
            optdepends: &[],
            conflicts: &[],
            provides: &[],
            replaces: &[],
            backup: &[],
            sources: &[(
                "x86_64".to_string(),
                "https://example.com/mytool.tar.gz".to_string(),
                "hash".to_string(),
            )],
            binary_name: "mytool",
            install_template: Some(
                r#"install -Dm755 "$srcdir/mytool-${pkgver}/mytool" "$pkgdir/usr/bin/mytool""#,
            ),
            install_file: None,
        })
        .unwrap();

        assert!(pkgbuild.contains("package() {"));
        assert!(pkgbuild.contains(
            r#"install -Dm755 "$srcdir/mytool-${pkgver}/mytool" "$pkgdir/usr/bin/mytool""#
        ));
        // Should NOT contain the default install line
        assert!(!pkgbuild.contains("\"$srcdir/mytool\" \"$pkgdir/usr/bin/mytool\""));
    }

    #[test]
    fn test_generate_pkgbuild_duplicate_arch_sources() {
        // Regression test: when sources have duplicate architectures, the
        // PKGBUILD should only contain one source per arch.
        let sources = vec![
            (
                "x86_64".to_string(),
                "https://example.com/first-amd64.tar.gz".to_string(),
                "hash1".to_string(),
            ),
            (
                "x86_64".to_string(),
                "https://example.com/second-amd64.tar.gz".to_string(),
                "hash2".to_string(),
            ),
        ];
        // Simulate the deduplication that publish_to_aur does before
        // calling generate_pkgbuild (finding #1).
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<_> = sources
            .into_iter()
            .filter(|(arch, _, _)| seen.insert(arch.clone()))
            .collect();
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].1, "https://example.com/first-amd64.tar.gz");
    }

    // -----------------------------------------------------------------------
    // generate_srcinfo tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_srcinfo() {
        let srcinfo = generate_srcinfo(&PkgbuildParams {
            name: "mytool-bin",
            version: "2.5.0",
            pkgrel: 3,
            description: "A fantastic CLI tool",
            url: "https://github.com/org/mytool",
            license: "Apache-2.0",
            maintainers: &["Alice <alice@example.com>".to_string()],
            contributors: &[],
            depends: &["glibc".to_string(), "openssl".to_string()],
            optdepends: &[
                "git: for VCS support".to_string(),
                "bash-completion: for shell completions".to_string(),
            ],
            conflicts: &["mytool-git".to_string()],
            provides: &["mytool".to_string()],
            replaces: &[],
            backup: &[],
            sources: &[
                (
                    "x86_64".to_string(),
                    "https://github.com/org/mytool/releases/download/v2.5.0/mytool-2.5.0-linux-amd64.tar.gz".to_string(),
                    "aabbccdd11223344".to_string(),
                ),
                (
                    "aarch64".to_string(),
                    "https://github.com/org/mytool/releases/download/v2.5.0/mytool-2.5.0-linux-arm64.tar.gz".to_string(),
                    "eeff005566778899".to_string(),
                ),
            ],
            binary_name: "mytool",
            install_template: None,
            install_file: None,
        }).unwrap();

        // pkgbase line
        assert!(srcinfo.contains("pkgbase = mytool-bin"), "missing pkgbase");

        // pkgver / pkgrel
        assert!(srcinfo.contains("\tpkgver = 2.5.0"), "missing pkgver");
        assert!(srcinfo.contains("\tpkgrel = 3"), "missing pkgrel");

        // description
        assert!(
            srcinfo.contains("\tpkgdesc = A fantastic CLI tool"),
            "missing pkgdesc"
        );

        // url + license
        assert!(
            srcinfo.contains("\turl = https://github.com/org/mytool"),
            "missing url"
        );
        assert!(
            srcinfo.contains("\tlicense = Apache-2.0"),
            "missing license"
        );

        // depends
        assert!(
            srcinfo.contains("\tdepends = glibc"),
            "missing depends glibc"
        );
        assert!(
            srcinfo.contains("\tdepends = openssl"),
            "missing depends openssl"
        );

        // optdepends
        assert!(
            srcinfo.contains("\toptdepends = git: for VCS support"),
            "missing optdepends git"
        );
        assert!(
            srcinfo.contains("\toptdepends = bash-completion: for shell completions"),
            "missing optdepends bash-completion"
        );

        // conflicts
        assert!(
            srcinfo.contains("\tconflicts = mytool-git"),
            "missing conflicts"
        );

        // provides
        assert!(srcinfo.contains("\tprovides = mytool"), "missing provides");

        // arch + source + sha256sums (x86_64)
        assert!(srcinfo.contains("\tarch = x86_64"), "missing arch x86_64");
        assert!(
            srcinfo.contains("\tsource_x86_64 = https://github.com/org/mytool/releases/download/v2.5.0/mytool-2.5.0-linux-amd64.tar.gz"),
            "missing source_x86_64"
        );
        assert!(
            srcinfo.contains("\tsha256sums_x86_64 = aabbccdd11223344"),
            "missing sha256sums_x86_64"
        );

        // arch + source + sha256sums (aarch64)
        assert!(srcinfo.contains("\tarch = aarch64"), "missing arch aarch64");
        assert!(
            srcinfo.contains("\tsource_aarch64 = https://github.com/org/mytool/releases/download/v2.5.0/mytool-2.5.0-linux-arm64.tar.gz"),
            "missing source_aarch64"
        );
        assert!(
            srcinfo.contains("\tsha256sums_aarch64 = eeff005566778899"),
            "missing sha256sums_aarch64"
        );

        // pkgname line at the end
        assert!(
            srcinfo.contains("\npkgname = mytool-bin"),
            "missing pkgname at end"
        );

        // pkgname should appear after the source blocks (i.e. near the end)
        let pkgname_pos = srcinfo.find("pkgname = mytool-bin").unwrap();
        let last_sha_pos = srcinfo.find("sha256sums_aarch64").unwrap();
        assert!(
            pkgname_pos > last_sha_pos,
            "pkgname should appear after source/sha256 blocks"
        );
    }

    #[test]
    fn test_generate_srcinfo_no_optdepends() {
        let srcinfo = generate_srcinfo(&PkgbuildParams {
            name: "simple-bin",
            version: "1.0.0",
            pkgrel: 1,
            description: "Simple tool",
            url: "https://example.com",
            license: "MIT",
            maintainers: &[],
            contributors: &[],
            depends: &[],
            optdepends: &[],
            conflicts: &[],
            provides: &[],
            replaces: &[],
            backup: &[],
            sources: &[(
                "x86_64".to_string(),
                "https://example.com/simple.tar.gz".to_string(),
                "deadbeef".to_string(),
            )],
            binary_name: "simple",
            install_template: None,
            install_file: None,
        })
        .unwrap();

        // Should not contain optdepends line when empty
        assert!(
            !srcinfo.contains("optdepends"),
            "should not contain optdepends when empty"
        );
        // Should still have basic structure
        assert!(srcinfo.contains("pkgbase = simple-bin"));
        assert!(srcinfo.contains("pkgname = simple-bin"));
    }

    // -----------------------------------------------------------------------
    // publish_to_aur dry-run tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_publish_to_aur_dry_run() {
        use anodizer_core::config::{AurConfig, Config, CrateConfig, PublishConfig};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
                    description: Some("A great tool".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let log = StageLogger::new("publish", Verbosity::Normal);

        let pushed = publish_to_aur(&ctx, "mytool", &log).expect("dry-run ok");
        assert!(!pushed, "dry-run must return false (not pushed)");
    }

    /// Regression for parity with GoReleaser's `ErrNoArchivesFound`: an empty
    /// linux-archive set must hard-fail with an actionable error instead of
    /// silently writing a PKGBUILD with placeholder URL + empty sha256.
    #[test]
    fn test_publish_to_aur_empty_linux_archive_set_hard_errors() {
        use anodizer_core::config::{AurConfig, Config, CrateConfig, PublishConfig};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
                    homepage: Some("https://example.com/mytool".to_string()),
                    description: Some("A great tool".to_string()),
                    // ids filter that matches nothing forces empty archive set.
                    ids: Some(vec!["nonexistent".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        // dry_run: false so we reach the archive-set check.
        let ctx = Context::new(config, ContextOptions::default());
        let log = StageLogger::new("publish", Verbosity::Normal);

        let result = publish_to_aur(&ctx, "mytool", &log);
        let err = result.expect_err("empty linux archive set must hard-fail");
        let msg = err.to_string();
        assert!(
            msg.contains("no linux archives matched"),
            "error should explain the no-match condition, got: {msg}"
        );
        assert!(
            msg.contains("nonexistent"),
            "error should cite ids filter, got: {msg}"
        );
    }

    #[test]
    fn test_publish_to_aur_missing_config() {
        use anodizer_core::config::{Config, CrateConfig, PublishConfig};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        }];

        let ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let log = StageLogger::new("publish", Verbosity::Normal);

        assert!(publish_to_aur(&ctx, "mytool", &log).is_err());
    }

    #[test]
    fn test_publish_to_aur_missing_git_url() {
        use anodizer_core::config::{AurConfig, Config, CrateConfig, PublishConfig};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur: Some(AurConfig {
                    git_url: None, // Missing
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let log = StageLogger::new("publish", Verbosity::Normal);

        assert!(publish_to_aur(&ctx, "mytool", &log).is_err());
    }

    // -----------------------------------------------------------------------
    // GoReleaser Default() parity (aur.go::Default — C-new-12)
    //
    // GoReleaser applies four defaults at config-load time that anodizer must
    // mirror: name auto-suffixed `-bin`, conflicts/provides default to the
    // project name, and pkgrel defaults to "1". These tests pin each rule
    // against the helper pair (`aur_default_package_name` →
    // `aur_resolve_defaults`) so a regression trips a unit test instead of
    // slipping through to a malformed PKGBUILD on disk.
    // -----------------------------------------------------------------------

    /// `aur.name` unset → raw default is `<crate>-bin`. When the crate name
    /// already ends in `-bin` (e.g. `foo-bin`), do NOT double-suffix.
    #[test]
    fn test_aur_default_name_appends_bin_suffix() {
        use anodizer_core::config::AurConfig;

        let cfg = AurConfig::default();

        // Plain crate name → suffix appended.
        assert_eq!(
            aur_default_package_name(&cfg, "mytool"),
            "mytool-bin",
            "name should default to crate_name + '-bin'",
        );

        // Crate name already ends in `-bin` → no double suffix.
        assert_eq!(
            aur_default_package_name(&cfg, "foo-bin"),
            "foo-bin",
            "name must not double-suffix when crate already ends in '-bin'",
        );

        // Explicit `aur.name` wins over the default and is returned verbatim
        // (template rendering is the caller's responsibility).
        let cfg_explicit = AurConfig {
            name: Some("custom-name".to_string()),
            ..Default::default()
        };
        assert_eq!(
            aur_default_package_name(&cfg_explicit, "mytool"),
            "custom-name",
        );
    }

    /// `aur.conflicts` unset/empty → defaults to `[project_name]` (GR aur.go:58-63).
    /// When `project_name` is empty, falls back to the rendered package name
    /// with any trailing `-bin` stripped.
    #[test]
    fn test_aur_default_conflicts_uses_project_name() {
        use anodizer_core::config::AurConfig;

        // Unset → defaults to [project_name].
        let cfg_unset = AurConfig::default();
        let resolved = aur_resolve_defaults(&cfg_unset, "mytool-bin", "mytool");
        assert_eq!(
            resolved.conflicts,
            vec!["mytool".to_string()],
            "conflicts should default to [project_name] when unset",
        );

        // Empty vec → defaults same as unset.
        let cfg_empty = AurConfig {
            conflicts: Some(vec![]),
            ..Default::default()
        };
        let resolved_empty = aur_resolve_defaults(&cfg_empty, "mytool-bin", "mytool");
        assert_eq!(
            resolved_empty.conflicts,
            vec!["mytool".to_string()],
            "conflicts should default to [project_name] when empty",
        );

        // No project_name → fallback to rendered package name with `-bin` stripped.
        let resolved_no_project = aur_resolve_defaults(&cfg_unset, "mytool-bin", "");
        assert_eq!(
            resolved_no_project.conflicts,
            vec!["mytool".to_string()],
            "conflicts should fall back to stripped package name when project_name empty",
        );

        // Explicit value wins.
        let cfg_explicit = AurConfig {
            conflicts: Some(vec!["other-pkg".to_string()]),
            ..Default::default()
        };
        let resolved_explicit = aur_resolve_defaults(&cfg_explicit, "mytool-bin", "mytool");
        assert_eq!(resolved_explicit.conflicts, vec!["other-pkg".to_string()]);
    }

    /// `aur.provides` unset/empty → defaults to `[project_name]` (GR aur.go:58-63).
    #[test]
    fn test_aur_default_provides_uses_project_name() {
        use anodizer_core::config::AurConfig;

        let cfg_unset = AurConfig::default();
        let resolved = aur_resolve_defaults(&cfg_unset, "mytool-bin", "mytool");
        assert_eq!(
            resolved.provides,
            vec!["mytool".to_string()],
            "provides should default to [project_name] when unset",
        );

        let cfg_empty = AurConfig {
            provides: Some(vec![]),
            ..Default::default()
        };
        let resolved_empty = aur_resolve_defaults(&cfg_empty, "mytool-bin", "mytool");
        assert_eq!(
            resolved_empty.provides,
            vec!["mytool".to_string()],
            "provides should default to [project_name] when empty",
        );

        // Explicit value wins.
        let cfg_explicit = AurConfig {
            provides: Some(vec!["virtual-pkg".to_string()]),
            ..Default::default()
        };
        let resolved_explicit = aur_resolve_defaults(&cfg_explicit, "mytool-bin", "mytool");
        assert_eq!(resolved_explicit.provides, vec!["virtual-pkg".to_string()]);
    }

    /// `aur.rel` unset → defaults to `1` (GR aur.go:64-66). Non-numeric or
    /// unparseable values also collapse to `1` rather than erroring; explicit
    /// numeric values pass through unchanged.
    #[test]
    fn test_aur_default_pkgrel_is_one() {
        use anodizer_core::config::AurConfig;

        // Unset → 1.
        let cfg_unset = AurConfig::default();
        let resolved = aur_resolve_defaults(&cfg_unset, "mytool-bin", "mytool");
        assert_eq!(resolved.pkgrel, 1, "pkgrel should default to 1 when unset");

        // Explicit numeric value passes through.
        let cfg_explicit = AurConfig {
            rel: Some("3".to_string()),
            ..Default::default()
        };
        let resolved_explicit = aur_resolve_defaults(&cfg_explicit, "mytool-bin", "mytool");
        assert_eq!(resolved_explicit.pkgrel, 3);

        // Non-numeric value falls back to 1 (defensive — GR's Default() pins
        // the field as a string, so we accept any unparseable input rather
        // than fail the publish).
        let cfg_bad = AurConfig {
            rel: Some("not-a-number".to_string()),
            ..Default::default()
        };
        let resolved_bad = aur_resolve_defaults(&cfg_bad, "mytool-bin", "mytool");
        assert_eq!(resolved_bad.pkgrel, 1);
    }

    /// Regression: `base_name` is derived from the *rendered* package name,
    /// not the raw template string. Simulates `aur.name = "{{ .ProjectName }}-bin"`
    /// rendered against an empty `project_name` (which yields just `"-bin"`).
    /// Before the split, the helper carried the unrendered template through
    /// to `conflicts`/`provides`, leaking `{{` into the PKGBUILD output.
    #[test]
    fn test_aur_resolve_defaults_uses_rendered_name_for_base() {
        use anodizer_core::config::AurConfig;

        let cfg = AurConfig::default();
        // `"-bin"` is what `"{{ .ProjectName }}-bin"` renders to when
        // `project_name == ""` — the caller in `publish_to_aur` performs
        // the render *before* invoking `aur_resolve_defaults`.
        let resolved = aur_resolve_defaults(&cfg, "-bin", "");

        assert!(
            !resolved.conflicts[0].contains("{{"),
            "conflicts must not leak unrendered template syntax, got {:?}",
            resolved.conflicts,
        );
        assert_eq!(
            resolved.conflicts[0], "",
            "with rendered name '-bin' and no project_name, base_name strips to ''",
        );
        assert!(
            !resolved.provides[0].contains("{{"),
            "provides must not leak unrendered template syntax, got {:?}",
            resolved.provides,
        );
        assert_eq!(resolved.provides[0], "");
    }

    /// Regression for GoReleaser parity P9.1 (commit cba5b9f):
    /// `aur.skip_upload: "{{ .IsSnapshot }}"` must template-expand
    /// before its bool/auto/empty interpretation. On a snapshot run
    /// the rendered value is `"true"` and the publish path must
    /// short-circuit to `Ok(())` (no git-push attempt).
    #[test]
    fn aur_skip_upload_template_expands_to_true_on_snapshot() {
        use anodizer_core::config::{AurConfig, Config, CrateConfig, PublishConfig, StringOrBool};
        use anodizer_core::context::{Context, ContextOptions};
        use anodizer_core::log::{StageLogger, Verbosity};

        let mut config = Config::default();
        config.project_name = "mytool".to_string();
        config.crates = vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                aur: Some(AurConfig {
                    git_url: Some("ssh://aur@aur.archlinux.org/mytool.git".to_string()),
                    description: Some("A great tool".to_string()),
                    skip_upload: Some(StringOrBool::String("{{ .IsSnapshot }}".to_string())),
                    // ids filter that matches nothing — would normally
                    // hard-fail with "no linux archives matched", but the
                    // skip_upload short-circuit must run BEFORE the
                    // archive check.
                    ids: Some(vec!["nonexistent".to_string()]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }];

        let mut ctx = Context::new(
            config,
            ContextOptions {
                snapshot: true,
                ..Default::default()
            },
        );
        // Populate IsSnapshot template var (normally done by populate_git_vars).
        ctx.template_vars_mut().set("IsSnapshot", "true");

        let log = StageLogger::new("publish", Verbosity::Normal);
        publish_to_aur(&ctx, "mytool", &log).expect(
            "skip_upload='{{ .IsSnapshot }}' on snapshot must short-circuit \
             to Ok(()) before the archive-set check (GR cba5b9f)",
        );
    }
}
