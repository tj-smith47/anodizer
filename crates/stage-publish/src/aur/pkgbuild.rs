use super::*;

// ---------------------------------------------------------------------------
// pkgdesc quoting helper
// ---------------------------------------------------------------------------

/// Quote a PKGBUILD `pkgdesc` value, choosing the appropriate quoting style
/// to handle embedded single or double quotes.
pub(crate) fn quote_pkgdesc(s: &str) -> String {
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
    /// Rendered pacman `license=()` array entries (already SPDX-id-split for
    /// dual-licensed crates). Built by [`aur_license_array`] from the resolved
    /// license string; empty when no license is configured.
    pub(crate) license: &'a [String],
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
    /// When `None`, defaults to `install -Dm755 "$srcdir/<binary>" "$pkgdir/usr/bin/<binary>"`
    /// followed by any [`Self::extra_install_lines`].
    /// Use this when the archive places binaries in a subdirectory. A custom
    /// template fully replaces the body (including the extra-install lines).
    pub(crate) install_template: Option<&'a str>,
    /// Additional `package()` install lines (LICENSE, man pages, shell
    /// completions the archive bundles) appended after the default binary
    /// install. Each is gated on the artifact existing inside `$srcdir` so a
    /// crate that ships no LICENSE/man/completions emits none. Ignored when
    /// [`Self::install_template`] overrides the whole body.
    pub(crate) extra_install_lines: &'a [String],
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
pub(crate) fn extract_archive_extension(url: &str) -> &str {
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

pub(crate) const PKGBUILD_TEMPLATE: &str = r#"{% for m in maintainers %}# Maintainer: {{ m }}
{% endfor %}{% for c in contributors %}# Contributor: {{ c }}
{% endfor %}{% if maintainers | length > 0 or contributors | length > 0 %}
{% endif %}pkgname='{{ name }}'
pkgver={{ version }}
pkgrel={{ pkgrel }}
pkgdesc={{ quoted_description }}
arch=({% for a in arches %}'{{ a }}'{% if not loop.last %} {% endif %}{% endfor %})
url="{{ url }}"
license=({% for l in license %}'{{ l }}'{% if not loop.last %} {% endif %}{% endfor %})
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
    // automatically uses the pkgver variable.
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
        // Default body: install the binary, then any LICENSE/man/completion
        // lines the archive bundles (each already `$srcdir`-existence-gated by
        // the caller). Joined with the template's 4-space `package()` indent so
        // `bash -n` and namcap see a well-formed function body.
        let mut body = vec![format!(
            "install -Dm755 \"$srcdir/{}\" \"$pkgdir/usr/bin/{}\"",
            params.binary_name, params.binary_name
        )];
        body.extend(params.extra_install_lines.iter().cloned());
        body.join("\n    ")
    };
    ctx.insert("install_line", &install_line);

    anodizer_core::template::render_static(&tera, "pkgbuild", &ctx, "aur")
}

// ---------------------------------------------------------------------------
// generate_srcinfo (template-based, no makepkg dependency)
// ---------------------------------------------------------------------------

pub(crate) const SRCINFO_TEMPLATE: &str = r#"pkgbase = {{ name }}
	pkgdesc = {{ description }}
	pkgver = {{ version }}
	pkgrel = {{ pkgrel }}
{% if url %}	url = {{ url }}
{% endif %}{% for l in license %}	license = {{ l }}
{% endfor %}
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
