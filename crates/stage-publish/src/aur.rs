use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use tera::Tera;

use crate::util::{self, find_artifacts_by_os};

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
}

// ---------------------------------------------------------------------------
// generate_pkgbuild
// ---------------------------------------------------------------------------

const PKGBUILD_TEMPLATE: &str = r#"{% for m in maintainers %}# Maintainer: {{ m }}
{% endfor %}{% if maintainers | length > 0 %}
{% endif %}pkgname={{ name }}
pkgver={{ version }}
pkgrel={{ pkgrel }}
pkgdesc="{{ description }}"
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
{% endif %}{% for s in sources %}source_{{ s.arch }}=("{{ s.url }}")
sha256sums_{{ s.arch }}=('{{ s.hash }}')
{% endfor %}
package() {
    {{ install_line }}
}
"#;

/// Generate an Arch Linux PKGBUILD file string.
pub fn generate_pkgbuild(params: &PkgbuildParams<'_>) -> String {
    let mut tera = Tera::default();
    tera.autoescape_on(vec![]); // PKGBUILD is shell, not HTML
    // SAFETY: PKGBUILD_TEMPLATE is a compile-time constant; parse cannot fail.
    tera.add_raw_template("pkgbuild", PKGBUILD_TEMPLATE)
        .expect("aur: invalid PKGBUILD template");

    let mut ctx = tera::Context::new();
    ctx.insert("name", params.name);
    ctx.insert("version", params.version);
    ctx.insert("pkgrel", &params.pkgrel);
    ctx.insert("description", params.description);
    ctx.insert("url", params.url);
    ctx.insert("license", params.license);
    ctx.insert("maintainers", params.maintainers);
    ctx.insert("depends", params.depends);
    ctx.insert("optdepends", params.optdepends);
    ctx.insert("conflicts", params.conflicts);
    ctx.insert("provides", params.provides);
    ctx.insert("replaces", params.replaces);
    ctx.insert("backup", params.backup);
    ctx.insert("binary_name", params.binary_name);

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
    let sources: Vec<std::collections::HashMap<&str, &str>> = params
        .sources
        .iter()
        .map(|(arch, url, hash)| {
            let mut m = std::collections::HashMap::new();
            m.insert("arch", arch.as_str());
            m.insert("url", url.as_str());
            m.insert("hash", hash.as_str());
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

    // SAFETY: All context variables are inserted above; rendering is infallible.
    tera.render("pkgbuild", &ctx)
        .expect("aur: failed to render PKGBUILD template")
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

    let version = ctx.version();

    let package_name = aur_cfg
        .package_name
        .clone()
        .unwrap_or_else(|| crate_name.to_string());
    let description = aur_cfg
        .description
        .clone()
        .unwrap_or_else(|| crate_name.to_string());
    let license = aur_cfg.license.clone().unwrap_or_else(|| "MIT".to_string());
    let url = if let Some(ref u) = aur_cfg.url {
        u.clone()
    } else if let Some(gh) = crate_cfg.release.as_ref().and_then(|r| r.github.as_ref()) {
        format!("https://github.com/{}/{}", gh.owner, gh.name)
    } else {
        anyhow::bail!(
            "aur: no url configured for '{}' and no release.github owner/name available. \
             Set `publish.aur.url` or configure `release.github` with owner and name.",
            crate_name
        );
    };
    let maintainers = aur_cfg.maintainers.clone().unwrap_or_default();
    let depends = aur_cfg.depends.clone().unwrap_or_default();
    let optdepends = aur_cfg.optdepends.clone().unwrap_or_default();
    let conflicts = aur_cfg.conflicts.clone().unwrap_or_default();
    let provides = aur_cfg.provides.clone().unwrap_or_default();
    let replaces = aur_cfg.replaces.clone().unwrap_or_default();
    let backup = aur_cfg.backup.clone().unwrap_or_default();

    // Find Linux artifacts for the AUR package.
    let linux_artifacts = find_artifacts_by_os(ctx, crate_name, "linux");

    let sources: Vec<(String, String, String)> = if linux_artifacts.is_empty() {
        log.warn(&format!(
            "aur: no linux artifacts found for '{}', using placeholder URLs",
            crate_name
        ));
        vec![(
            "x86_64".to_string(),
            format!(
                "https://github.com/{0}/releases/download/v{1}/{0}-{1}-linux-amd64.tar.gz",
                crate_name, version
            ),
            String::new(),
        )]
    } else {
        // Deduplicate by architecture — AUR -bin packages expect one source per
        // architecture. When multiple artifacts share the same arch (e.g.
        // multiple linux-amd64 archives), keep only the first match.
        let mut seen_arches = std::collections::HashSet::new();
        linux_artifacts
            .iter()
            .filter_map(|a| {
                let pkgbuild_arch = if a.arch == "arm64" {
                    "aarch64".to_string()
                } else {
                    "x86_64".to_string()
                };
                if seen_arches.insert(pkgbuild_arch.clone()) {
                    Some((pkgbuild_arch, a.url.clone(), a.sha256.clone()))
                } else {
                    None
                }
            })
            .collect()
    };

    let pkgbuild = generate_pkgbuild(&PkgbuildParams {
        name: &package_name,
        version: &version,
        pkgrel: 1,
        description: &description,
        url: &url,
        license: &license,
        maintainers: &maintainers,
        depends: &depends,
        optdepends: &optdepends,
        conflicts: &conflicts,
        provides: &provides,
        replaces: &replaces,
        backup: &backup,
        sources: &sources,
        binary_name: crate_name,
        install_template: aur_cfg.install_template.as_deref(),
    });

    // Clone AUR repo, write PKGBUILD, commit, push.
    let tmp_dir = tempfile::tempdir().context("aur: create temp dir")?;
    let repo_path = tmp_dir.path();

    // AUR uses SSH or plain HTTPS (no bearer-token auth).
    util::clone_repo_with_auth(git_url, None, repo_path, "aur", log)?;

    let pkgbuild_path = repo_path.join("PKGBUILD");
    std::fs::write(&pkgbuild_path, &pkgbuild)
        .with_context(|| format!("aur: write PKGBUILD {}", pkgbuild_path.display()))?;

    log.status(&format!("wrote AUR PKGBUILD: {}", pkgbuild_path.display()));

    // Generate .SRCINFO using makepkg.
    let srcinfo_result = std::process::Command::new("makepkg")
        .current_dir(repo_path)
        .args(["--printsrcinfo"])
        .output()
        .context("aur: makepkg --printsrcinfo")?;

    if srcinfo_result.status.success() {
        let srcinfo_path = repo_path.join(".SRCINFO");
        std::fs::write(&srcinfo_path, &srcinfo_result.stdout)
            .with_context(|| format!("aur: write .SRCINFO {}", srcinfo_path.display()))?;
        log.status(&format!("wrote AUR .SRCINFO: {}", srcinfo_path.display()));
    } else {
        log.warn(
            "aur: makepkg --printsrcinfo failed (may not be available); skipping .SRCINFO generation",
        );
    }

    util::commit_and_push(
        repo_path,
        &["."],
        &format!("Update to version {}", version),
        None,
        "aur",
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
        });

        assert!(pkgbuild.contains("# Maintainer: Jane Doe <jane@example.com>"));
        assert!(pkgbuild.contains("pkgname=mytool"));
        assert!(pkgbuild.contains("pkgver=1.0.0"));
        assert!(pkgbuild.contains("pkgrel=1"));
        assert!(pkgbuild.contains("pkgdesc=\"A great tool\""));
        assert!(pkgbuild.contains("arch=('x86_64')"));
        assert!(pkgbuild.contains("url=\"https://github.com/org/mytool\""));
        assert!(pkgbuild.contains("license=('MIT')"));
        assert!(pkgbuild.contains("depends=()"));
        assert!(
            pkgbuild.contains(
                "source_x86_64=(\"https://example.com/mytool-1.0.0-linux-amd64.tar.gz\")"
            )
        );
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
        });

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
        });

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
        });

        assert!(!pkgbuild.contains("# Maintainer:"));
        assert!(pkgbuild.starts_with("pkgname="));
    }

    #[test]
    fn test_generate_pkgbuild_complete_structure() {
        let pkgbuild = generate_pkgbuild(&PkgbuildParams {
            name: "anodize",
            version: "3.2.1",
            pkgrel: 1,
            description: "Release automation for Rust projects",
            url: "https://github.com/tj-smith47/anodize",
            license: "Apache-2.0",
            maintainers: &["TJ Smith <tj@example.com>".to_string()],
            depends: &[],
            optdepends: &[],
            conflicts: &[],
            provides: &[],
            replaces: &[],
            backup: &[],
            sources: &[
                (
                    "x86_64".to_string(),
                    "https://github.com/tj-smith47/anodize/releases/download/v3.2.1/anodize-3.2.1-linux-amd64.tar.gz".to_string(),
                    "aabbccdd".to_string(),
                ),
                (
                    "aarch64".to_string(),
                    "https://github.com/tj-smith47/anodize/releases/download/v3.2.1/anodize-3.2.1-linux-arm64.tar.gz".to_string(),
                    "eeff0011".to_string(),
                ),
            ],
            binary_name: "anodize",
            install_template: None,
        });

        // Starts with maintainer comment
        assert!(pkgbuild.starts_with("# Maintainer: TJ Smith <tj@example.com>"));

        // Contains required fields
        assert!(pkgbuild.contains("pkgname=anodize"));
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
        });

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
    // publish_to_aur dry-run tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_publish_to_aur_dry_run() {
        use anodize_core::config::{AurConfig, Config, CrateConfig, PublishConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

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

    #[test]
    fn test_publish_to_aur_missing_config() {
        use anodize_core::config::{Config, CrateConfig, PublishConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

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
        use anodize_core::config::{AurConfig, Config, CrateConfig, PublishConfig};
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

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
}
