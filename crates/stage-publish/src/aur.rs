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
pub struct PkgbuildParams<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub pkgrel: u32,
    pub description: &'a str,
    pub url: &'a str,
    pub license: &'a str,
    pub maintainers: &'a [String],
    pub contributors: &'a [String],
    pub depends: &'a [String],
    pub optdepends: &'a [String],
    pub conflicts: &'a [String],
    pub provides: &'a [String],
    pub replaces: &'a [String],
    pub backup: &'a [String],
    /// `(arch, url, sha256)` tuples — e.g. `("x86_64", url, hash)`.
    pub sources: &'a [(String, String, String)],
    pub binary_name: &'a str,
    /// Custom install template for the `package()` function body.
    /// When `None`, defaults to `install -Dm755 "$srcdir/<binary>" "$pkgdir/usr/bin/<binary>"`.
    /// Use this when the archive places binaries in a subdirectory.
    pub install_template: Option<&'a str>,
    /// Filename for a `.install` file (post-install hooks). When `Some`, the
    /// PKGBUILD will include an `install=<filename>` line.
    pub install_file: Option<&'a str>,
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
pub fn generate_pkgbuild(params: &PkgbuildParams<'_>) -> Result<String> {
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
pub fn generate_srcinfo(params: &PkgbuildParams<'_>) -> Result<String> {
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

/// Resolved AUR `Default()`-time fields: package name, conflicts, provides,
/// and pkgrel. Extracted from `publish_to_aur` so the four GoReleaser-aligned
/// defaults can be exercised in unit tests without standing up a full
/// publish-to-git flow. Mirrors GoReleaser's
/// `internal/pipe/aur/aur.go::Default()` behaviour:
///
/// - `name` defaults to `<crate_name>` with `-bin` suffix appended when the
///   crate name does not already end in `-bin`.
/// - `conflicts` defaults to `[base_name]` when unset/empty (GR aur.go:58-63).
/// - `provides` defaults to `[base_name]` when unset/empty (GR aur.go:58-63).
/// - `pkgrel` defaults to `1` when unset (GR aur.go:64-66).
///
/// `base_name` is the project name when set, otherwise the package name with
/// any trailing `-bin` stripped — see the comment block in `publish_to_aur`
/// for the rationale (covers the edge case where `package_name="foo-bin"`
/// and `project_name="foo-cli"`).
pub(crate) struct AurResolvedDefaults {
    pub package_name: String,
    pub conflicts: Vec<String>,
    pub provides: Vec<String>,
    pub pkgrel: u32,
}

/// Apply the four GoReleaser `Default()` rules to an `AurConfig` for
/// `crate_name` under `project_name` (use `""` when no project name is
/// configured). The returned struct holds the post-default values that
/// `publish_to_aur` would feed into PKGBUILD generation.
pub(crate) fn aur_resolve_defaults(
    aur_cfg: &anodizer_core::config::AurConfig,
    crate_name: &str,
    project_name: &str,
) -> AurResolvedDefaults {
    let package_name = aur_cfg.name.clone().unwrap_or_else(|| {
        if crate_name.ends_with("-bin") {
            crate_name.to_string()
        } else {
            format!("{}-bin", crate_name)
        }
    });

    let base_name = if project_name.is_empty() {
        package_name
            .strip_suffix("-bin")
            .unwrap_or(&package_name)
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
        package_name,
        conflicts,
        provides,
        pkgrel,
    }
}

// ---------------------------------------------------------------------------
// publish_to_aur
// ---------------------------------------------------------------------------

pub fn publish_to_aur(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let (crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "aur")?;

    let aur_cfg = publish
        .aur
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("aur: no aur config for '{}'", crate_name))?;

    // Check skip before doing any work.
    if let Some(ref d) = aur_cfg.skip {
        let off = d
            .try_evaluates_to_true(|tmpl| ctx.render_template(tmpl))
            .with_context(|| format!("aur: render skip template for '{}'", crate_name))?;
        if off {
            log.status(&format!("aur: skipped for '{}'", crate_name));
            return Ok(());
        }
    }

    // Check skip_upload before doing any work.
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
        return Ok(());
    }

    let git_url = aur_cfg
        .git_url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("aur: no git_url config for '{}'", crate_name))?;

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would push AUR PKGBUILD for '{}' to {}",
            crate_name, git_url
        ));
        return Ok(());
    }

    // AUR pkgver does not allow hyphens; replace with underscores.
    let version = ctx.version().replace('-', "_");

    // GR-parity Default() resolution: name auto-suffix `-bin`, conflicts /
    // provides default to [base_name], pkgrel default `"1"`. The first three
    // rely on `project_name` (after defaulting), so we resolve them via
    // `aur_resolve_defaults` for symmetry with `publish_to_aur` callers and
    // to keep the unit tests focused on the default rules.
    let project_name_for_defaults = ctx.config.project_name.as_str();
    let resolved_defaults = aur_resolve_defaults(aur_cfg, crate_name, project_name_for_defaults);
    // Render the resolved name through the template engine — users who set
    // `aur.name: "{{ .ProjectName }}-bin"` rely on this.
    let package_name = ctx
        .render_template(&resolved_defaults.package_name)
        .unwrap_or_else(|_| resolved_defaults.package_name.clone());
    // GoReleaser Pro parity: fall back to project `metadata.*` when aur config unset.
    let description_raw = aur_cfg
        .description
        .as_deref()
        .or_else(|| ctx.config.meta_description())
        .unwrap_or(crate_name);
    let description = ctx
        .render_template(description_raw)
        .unwrap_or_else(|_| description_raw.to_string());
    let license = aur_cfg
        .license
        .clone()
        .or_else(|| ctx.config.meta_license().map(str::to_string))
        .unwrap_or_default();
    // PKGBUILD `url=` resolves through `homepage:` → crate metadata
    // homepage → the derived github release URL.
    let url = aur_cfg
        .homepage
        .as_deref()
        .or_else(|| ctx.config.meta_homepage())
        .map(|s| s.to_string());
    let url = if let Some(u) = url {
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
    let contributors = aur_cfg.contributors.clone().unwrap_or_default();
    let depends = aur_cfg.depends.clone().unwrap_or_default();
    let optdepends = aur_cfg.optdepends.clone().unwrap_or_default();
    // conflicts / provides come from the GR-aligned default resolver above
    // (cf. aur.go:58-63). The post-template `package_name` may differ from
    // the raw resolved name, so we stick with the resolved-defaults output —
    // template rendering only affects the on-PKGBUILD `pkgname=`, not the
    // base used to derive `conflicts`/`provides` defaults.
    let conflicts = resolved_defaults.conflicts;
    let provides = resolved_defaults.provides;
    let replaces = aur_cfg.replaces.clone().unwrap_or_default();
    let backup = aur_cfg.backup.clone().unwrap_or_default();

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
    );

    let url_template = aur_cfg.url_template.as_deref();

    //
    // — empty linux-archive set produces a PKGBUILD with placeholder URL and
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

    let sources: Vec<(String, String, String)> = {
        // Deduplicate by architecture — AUR -bin packages expect one source per
        // architecture. When multiple artifacts share the same arch (e.g.
        // multiple linux-amd64 archives), keep only the first match.
        let mut seen_arches = std::collections::HashSet::new();
        linux_artifacts
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
                        util::render_url_template(
                            tmpl,
                            crate_name,
                            &version,
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
            .collect()
    };

    // pkgrel comes from the GR-aligned default resolver above (cf. aur.go:64-66).
    let pkgrel = resolved_defaults.pkgrel;

    // Compute .install filename: strip trailing "-bin" from the package name.
    let install_base = package_name.strip_suffix("-bin").unwrap_or(&package_name);
    let install_filename = format!("{}.install", install_base);
    let install_file_ref = if aur_cfg.install.is_some() {
        Some(install_filename.as_str())
    } else {
        None
    };

    let pkgbuild_params = PkgbuildParams {
        name: &package_name,
        version: &version,
        pkgrel,
        description: &description,
        url: &url,
        license: &license,
        maintainers: &maintainers,
        contributors: &contributors,
        depends: &depends,
        optdepends: &optdepends,
        conflicts: &conflicts,
        provides: &provides,
        replaces: &replaces,
        backup: &backup,
        sources: &sources,
        binary_name: crate_name,
        install_template: aur_cfg.package.as_deref(),
        install_file: install_file_ref,
    };
    let pkgbuild = generate_pkgbuild(&pkgbuild_params)?;

    // Clone AUR repo, write PKGBUILD, commit, push.
    let tmp_dir = tempfile::tempdir().context("aur: create temp dir")?;
    let repo_path = tmp_dir.path();

    // AUR uses SSH.  When private_key or git_ssh_command is set, use the
    // SSH clone function with those credentials.
    if aur_cfg.private_key.is_some() || aur_cfg.git_ssh_command.is_some() {
        util::clone_repo_ssh(
            git_url,
            aur_cfg.private_key.as_deref(),
            aur_cfg.git_ssh_command.as_deref(),
            repo_path,
            "aur",
            log,
        )?;
    } else {
        // Plain clone (no bearer-token auth for AUR).
        util::clone_repo_with_auth(git_url, None, repo_path, "aur", log)?;
    }

    // Determine output directory (optional subdirectory in the repo).
    // GoReleaser templates the directory field (aur.go:103-108).
    let output_dir = if let Some(ref dir) = aur_cfg.directory {
        let rendered_dir = ctx.render_template(dir).unwrap_or_else(|_| dir.clone());
        let d = repo_path.join(&rendered_dir);
        std::fs::create_dir_all(&d)
            .with_context(|| format!("aur: create directory {}", d.display()))?;
        d
    } else {
        repo_path.to_path_buf()
    };

    let pkgbuild_path = output_dir.join("PKGBUILD");
    std::fs::write(&pkgbuild_path, &pkgbuild)
        .with_context(|| format!("aur: write PKGBUILD {}", pkgbuild_path.display()))?;

    log.status(&format!("wrote AUR PKGBUILD: {}", pkgbuild_path.display()));

    // Write .install file if configured (post-install hooks).
    if let Some(ref install_content) = aur_cfg.install {
        let install_path = output_dir.join(&install_filename);
        std::fs::write(&install_path, install_content).with_context(|| {
            format!("aur: write {} {}", install_filename, install_path.display())
        })?;
        log.status(&format!(
            "wrote AUR install file: {}",
            install_path.display()
        ));
    }

    // Generate .SRCINFO from a Tera template (no makepkg dependency).
    let srcinfo = generate_srcinfo(&pkgbuild_params)?;
    let srcinfo_path = output_dir.join(".SRCINFO");
    std::fs::write(&srcinfo_path, &srcinfo)
        .with_context(|| format!("aur: write .SRCINFO {}", srcinfo_path.display()))?;
    log.status(&format!("wrote AUR .SRCINFO: {}", srcinfo_path.display()));

    let commit_msg = crate::homebrew::render_commit_msg(
        aur_cfg.commit_msg_template.as_deref(),
        &package_name,
        &version,
        "package",
    );
    let commit_opts = util::resolve_commit_opts(aur_cfg.commit_author.as_ref());
    // AUR repositories are always on `master`. Pin the push branch explicitly
    // rather than relying on `git clone`'s default, which varies by git
    // version / config and once surfaced pushes that silently went to `main`
    // on fresh-cloned workspaces. Matches GoReleaser `internal/pipe/aur/aur.go`.
    util::commit_and_push_with_opts(
        repo_path,
        &["."],
        &commit_msg,
        Some("master"),
        "aur",
        &commit_opts,
    )?;

    log.status(&format!(
        "AUR package '{}' pushed to {}",
        package_name, git_url
    ));

    Ok(())
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

        assert!(publish_to_aur(&ctx, "mytool", &log).is_ok());
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
    // against `aur_resolve_defaults` so a regression in the helper trips a
    // unit test instead of slipping through to a malformed PKGBUILD on disk.
    // -----------------------------------------------------------------------

    /// `aur.name` unset → resolved name is `<crate>-bin`. When the crate name
    /// already ends in `-bin` (e.g. `foo-bin`), do NOT double-suffix.
    #[test]
    fn test_aur_default_name_appends_bin_suffix() {
        use anodizer_core::config::AurConfig;

        let cfg = AurConfig::default();

        // Plain crate name → suffix appended.
        let resolved = aur_resolve_defaults(&cfg, "mytool", "mytool");
        assert_eq!(
            resolved.package_name, "mytool-bin",
            "name should default to crate_name + '-bin', got {:?}",
            resolved.package_name,
        );

        // Crate name already ends in `-bin` → no double suffix.
        let resolved_already_bin = aur_resolve_defaults(&cfg, "foo-bin", "foo-bin");
        assert_eq!(
            resolved_already_bin.package_name, "foo-bin",
            "name must not double-suffix when crate already ends in '-bin', got {:?}",
            resolved_already_bin.package_name,
        );

        // Explicit `aur.name` wins over the default.
        let cfg_explicit = AurConfig {
            name: Some("custom-name".to_string()),
            ..Default::default()
        };
        let resolved_explicit = aur_resolve_defaults(&cfg_explicit, "mytool", "mytool");
        assert_eq!(resolved_explicit.package_name, "custom-name");
    }

    /// `aur.conflicts` unset/empty → defaults to `[project_name]` (GR aur.go:58-63).
    /// When `project_name` is empty, falls back to the package name with any
    /// trailing `-bin` stripped.
    #[test]
    fn test_aur_default_conflicts_uses_project_name() {
        use anodizer_core::config::AurConfig;

        // Unset → defaults to [project_name].
        let cfg_unset = AurConfig::default();
        let resolved = aur_resolve_defaults(&cfg_unset, "mytool", "mytool");
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
        let resolved_empty = aur_resolve_defaults(&cfg_empty, "mytool", "mytool");
        assert_eq!(
            resolved_empty.conflicts,
            vec!["mytool".to_string()],
            "conflicts should default to [project_name] when empty",
        );

        // No project_name → fallback to package name with `-bin` stripped.
        let resolved_no_project = aur_resolve_defaults(&cfg_unset, "mytool", "");
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
        let resolved_explicit = aur_resolve_defaults(&cfg_explicit, "mytool", "mytool");
        assert_eq!(resolved_explicit.conflicts, vec!["other-pkg".to_string()]);
    }

    /// `aur.provides` unset/empty → defaults to `[project_name]` (GR aur.go:58-63).
    #[test]
    fn test_aur_default_provides_uses_project_name() {
        use anodizer_core::config::AurConfig;

        let cfg_unset = AurConfig::default();
        let resolved = aur_resolve_defaults(&cfg_unset, "mytool", "mytool");
        assert_eq!(
            resolved.provides,
            vec!["mytool".to_string()],
            "provides should default to [project_name] when unset",
        );

        let cfg_empty = AurConfig {
            provides: Some(vec![]),
            ..Default::default()
        };
        let resolved_empty = aur_resolve_defaults(&cfg_empty, "mytool", "mytool");
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
        let resolved_explicit = aur_resolve_defaults(&cfg_explicit, "mytool", "mytool");
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
        let resolved = aur_resolve_defaults(&cfg_unset, "mytool", "mytool");
        assert_eq!(resolved.pkgrel, 1, "pkgrel should default to 1 when unset");

        // Explicit numeric value passes through.
        let cfg_explicit = AurConfig {
            rel: Some("3".to_string()),
            ..Default::default()
        };
        let resolved_explicit = aur_resolve_defaults(&cfg_explicit, "mytool", "mytool");
        assert_eq!(resolved_explicit.pkgrel, 3);

        // Non-numeric value falls back to 1 (defensive — GR's Default() pins
        // the field as a string, so we accept any unparseable input rather
        // than fail the publish).
        let cfg_bad = AurConfig {
            rel: Some("not-a-number".to_string()),
            ..Default::default()
        };
        let resolved_bad = aur_resolve_defaults(&cfg_bad, "mytool", "mytool");
        assert_eq!(resolved_bad.pkgrel, 1);
    }
}
