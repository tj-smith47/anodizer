use super::*;

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
    /// One download per pacman architecture.
    pub(crate) sources: &'a [AurSource],
    /// The name the binary is installed under (`/usr/bin/<binary_name>`), and
    /// the in-archive file name of a source that does not name its own.
    pub(crate) binary_name: &'a str,
    /// Custom install template for the `package()` function body.
    /// When `None`, the body is derived per source: the binary is installed
    /// from where its download puts it (the archive's `wrap_in_directory`, or
    /// the renamed download itself for a raw binary), followed by any
    /// [`Self::extra_install_lines`]. Sources whose derived bodies differ
    /// are dispatched on `${CARCH}`. A custom template fully replaces the
    /// body (including the extra-install lines).
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

/// One `source_<arch>=` entry of a binary AUR package: the download and
/// the layout the `package()` body needs to find the binary in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AurSource {
    /// pacman architecture name (`x86_64`, `aarch64`, `armv7h`, `i686`).
    pub(crate) arch: String,
    /// Download URL of the archive or raw binary.
    pub(crate) url: String,
    /// Hex sha256 of the download.
    pub(crate) sha256: String,
    /// Top-level directory the archive wraps its contents in, when it has
    /// one.
    pub(crate) wrap_in_directory: Option<String>,
    /// The binary's file name inside the archive, when the artifact records
    /// one that differs from the crate name.
    pub(crate) binary: Option<String>,
    /// `true` when the download is the executable itself rather than an
    /// archive: makepkg then leaves it under the renamed download alias.
    pub(crate) is_binary: bool,
    /// The archive format the archive stage recorded, when it did. The
    /// download alias ends in it so makepkg extracts the file by that name;
    /// a `url_template` may point at a URL whose last segment carries no
    /// extension at all.
    pub(crate) format: Option<String>,
}

impl AurSource {
    /// A flat archive download with no layout detail.
    #[cfg(test)]
    pub(crate) fn new(arch: &str, url: &str, sha256: &str) -> Self {
        Self {
            arch: arch.to_string(),
            url: url.to_string(),
            sha256: sha256.to_string(),
            wrap_in_directory: None,
            binary: None,
            is_binary: false,
            format: None,
        }
    }

    /// The makepkg download alias, `<name>_<version>_<arch>[.<format>]`;
    /// `version` is `${pkgver}` in the PKGBUILD and the literal version in
    /// the `.SRCINFO`.
    fn alias(&self, name: &str, version: &str) -> String {
        // A raw binary carries no archive format, whatever dots its file name
        // holds (`mytool-1.0.0-linux-amd64`).
        let format = if self.is_binary {
            ""
        } else {
            self.format
                .as_deref()
                .unwrap_or_else(|| extract_archive_extension(&self.url))
        };
        if format.is_empty() {
            format!("{name}_{version}_{}", self.arch)
        } else {
            format!("{name}_{version}_{}.{format}", self.arch)
        }
    }

    /// The `package()` lines that install this source's binary and the
    /// extra files beside it, relative to `$srcdir`.
    fn package_lines(&self, name: &str, binary_name: &str, extra: &[String]) -> Vec<String> {
        let binary = self.binary.as_deref().unwrap_or(binary_name);
        if self.is_binary {
            return vec![format!(
                "install -Dm755 \"$srcdir/{}\" \"$pkgdir/usr/bin/{binary}\"",
                self.alias(name, "${pkgver}")
            )];
        }
        let root = match self.wrap_in_directory.as_deref() {
            Some(dir) => format!("$srcdir/{dir}"),
            None => "$srcdir".to_string(),
        };
        let mut lines = vec![format!(
            "install -Dm755 \"{root}/{binary}\" \"$pkgdir/usr/bin/{binary}\""
        )];
        lines.extend(extra.iter().map(|l| l.replace("$srcdir", &root)));
        lines
    }
}

/// The default `package()` body over every source: one body when every
/// architecture installs the same way, otherwise a `case "${CARCH}"` with
/// one arm per architecture.
fn default_package_body(params: &PkgbuildParams<'_>) -> Vec<String> {
    let mut per_arch: Vec<(&str, Vec<String>)> = params
        .sources
        .iter()
        .map(|s| {
            (
                s.arch.as_str(),
                s.package_lines(params.name, params.binary_name, params.extra_install_lines),
            )
        })
        .collect();
    per_arch.sort_by(|a, b| a.0.cmp(b.0));
    match per_arch.first() {
        None => vec![format!(
            "install -Dm755 \"$srcdir/{0}\" \"$pkgdir/usr/bin/{0}\"",
            params.binary_name
        )],
        Some((_, first)) if per_arch.iter().all(|(_, lines)| lines == first) => first.clone(),
        Some(_) => {
            let mut body = vec!["case \"${CARCH}\" in".to_string()];
            for (arch, lines) in &per_arch {
                body.push(format!("{arch})"));
                body.extend(lines.iter().map(|l| format!("    {l}")));
                body.push("    ;;".to_string());
            }
            body.push("esac".to_string());
            body
        }
    }
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
{% endif %}pkgname={{ name }}
pkgver={{ version }}
pkgrel={{ pkgrel }}
pkgdesc={{ description }}
arch=({% for a in arches %}{{ a }}{% if not loop.last %} {% endif %}{% endfor %})
url={{ url }}
license=({% for l in license %}{{ l }}{% if not loop.last %} {% endif %}{% endfor %})
{% if depends | length > 0 %}depends=({% for d in depends %}{{ d }}{% if not loop.last %} {% endif %}{% endfor %})
{% else %}depends=()
{% endif %}{% if optdepends | length > 0 %}optdepends=({% for d in optdepends %}{{ d }}{% if not loop.last %} {% endif %}{% endfor %})
{% endif %}{% if conflicts | length > 0 %}conflicts=({% for c in conflicts %}{{ c }}{% if not loop.last %} {% endif %}{% endfor %})
{% endif %}{% if provides | length > 0 %}provides=({% for p in provides %}{{ p }}{% if not loop.last %} {% endif %}{% endfor %})
{% endif %}{% if replaces | length > 0 %}replaces=({% for r in replaces %}{{ r }}{% if not loop.last %} {% endif %}{% endfor %})
{% endif %}{% if backup | length > 0 %}backup=({% for b in backup %}{{ b }}{% if not loop.last %} {% endif %}{% endfor %})
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

    // Every metadata field is single-quoted here rather than in the template:
    // the value is already rendered at this point, so quoting it now is what
    // keeps a `'`, a `$HOME` or a backtick in a description literal instead of
    // letting makepkg expand or mis-parse it.
    let q = anodizer_core::shell::shell_single_quote;
    let quote_all = |vs: &[String]| -> Vec<String> { vs.iter().map(|v| q(v)).collect() };

    let mut ctx = tera::Context::new();
    ctx.insert("name", &q(params.name));
    ctx.insert("version", params.version);
    ctx.insert("pkgrel", &params.pkgrel);
    ctx.insert("description", &q(params.description));
    ctx.insert("url", &q(params.url));
    ctx.insert("license", &quote_all(params.license));
    ctx.insert("maintainers", params.maintainers);
    ctx.insert("contributors", params.contributors);
    ctx.insert("depends", &quote_all(params.depends));
    ctx.insert("optdepends", &quote_all(params.optdepends));
    ctx.insert("conflicts", &quote_all(params.conflicts));
    ctx.insert("provides", &quote_all(params.provides));
    ctx.insert("replaces", &quote_all(params.replaces));
    ctx.insert("backup", &quote_all(params.backup));
    ctx.insert("binary_name", params.binary_name);
    ctx.insert("install_file", &params.install_file);

    // Deduplicate architectures.
    let mut arches: Vec<&str> = params.sources.iter().map(|s| s.arch.as_str()).collect();
    arches.sort();
    arches.dedup();
    let quoted_arches: Vec<String> = arches.iter().map(|a| q(a)).collect();
    ctx.insert("arches", &quoted_arches);

    // The version inside a URL becomes `${pkgver}` so a pkgver bump moves the
    // download with it. An empty version matches between every character, so
    // it is left alone.
    let pkgver_url = |url: &str| {
        if params.version.is_empty() {
            url.to_string()
        } else {
            url.replace(params.version, "${pkgver}")
        }
    };
    let sources: Vec<std::collections::HashMap<&str, String>> = params
        .sources
        .iter()
        .map(|s| {
            std::collections::HashMap::from([
                ("arch", s.arch.clone()),
                ("url", pkgver_url(&s.url)),
                ("hash", s.sha256.clone()),
                ("rename", s.alias(params.name, "${pkgver}")),
            ])
        })
        .collect();
    ctx.insert("sources", &sources);

    // The body lines are joined with the template's 4-space `package()`
    // indent so `bash -n` and namcap see a well-formed function body.
    let install_line = match params.install_template {
        Some(tmpl) => tmpl.to_string(),
        None => default_package_body(params).join("\n    "),
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
{% endfor %}{% for r in replaces %}	replaces = {{ r }}
{% endfor %}{% for b in backup %}	backup = {{ b }}
{% endfor %}{% if install_file %}	install = {{ install_file }}
{% endif %}{% for s in sources %}	arch = {{ s.arch }}
	source_{{ s.arch }} = {{ s.rename }}::{{ s.url }}
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
    ctx.insert("replaces", params.replaces);
    ctx.insert("install_file", &params.install_file);

    let sources: Vec<std::collections::HashMap<&str, String>> = params
        .sources
        .iter()
        .map(|s| {
            std::collections::HashMap::from([
                ("arch", s.arch.clone()),
                ("url", s.url.clone()),
                ("hash", s.sha256.clone()),
                ("rename", s.alias(params.name, params.version)),
            ])
        })
        .collect();
    ctx.insert("sources", &sources);

    anodizer_core::template::render_static(&tera, "srcinfo", &ctx, "aur")
}
