use anodizer_core::config::AurSourceConfig;
use anodizer_core::context::Context;
use anodizer_core::log::StageLogger;
use anyhow::{Context as _, Result};

use crate::util;

/// Shared core logic for publishing a single AUR source entry.
///
/// Both per-crate (`publish_to_aur_source`) and top-level (`publish_top_level_aur_sources`)
/// delegate here after resolving which `AurSourceConfig` to use.
fn publish_aur_source_entry(
    ctx: &mut Context,
    cfg: &AurSourceConfig,
    default_name: &str,
    strip_bin_suffix: bool,
    label: &str,
    log: &StageLogger,
) -> Result<()> {
    let version = ctx
        .template_vars()
        .get("Version")
        .cloned()
        .unwrap_or_else(|| "0.0.0".to_string())
        .replace('-', "_");

    let raw_name = cfg.name.as_deref().unwrap_or(default_name);
    let pkg_name = if strip_bin_suffix {
        raw_name
            .strip_suffix("-bin")
            .unwrap_or(raw_name)
            .to_string()
    } else {
        raw_name.to_string()
    };

    let description = cfg.description.as_deref().unwrap_or(default_name);
    let homepage = cfg.homepage.as_deref().unwrap_or("");
    let license = cfg.license.as_deref().unwrap_or("MIT");

    let pkgrel: u32 = cfg.rel.as_deref().and_then(|r| r.parse().ok()).unwrap_or(1);

    // Source URL — use url_template or default release URL
    let tag = ctx.template_vars().get("Tag").cloned().unwrap_or_default();

    let source_url = if let Some(ref tmpl) = cfg.url_template {
        ctx.render_template(tmpl)
            .with_context(|| format!("{}: render url_template", label))?
    } else {
        let git_url = ctx
            .template_vars()
            .get("GitURL")
            .cloned()
            .unwrap_or_default();
        let owner = if git_url.contains("://") {
            git_url.split('/').nth(3).unwrap_or("").to_string()
        } else if git_url.contains(':') {
            git_url
                .split(':')
                .nth(1)
                .unwrap_or("")
                .split('/')
                .next()
                .unwrap_or("")
                .to_string()
        } else {
            String::new()
        };
        let project = ctx
            .template_vars()
            .get("ProjectName")
            .cloned()
            .unwrap_or_default();
        if owner.is_empty() {
            log.warn(&format!(
                "{}: could not extract owner from GitURL; set url_template explicitly",
                label
            ));
        }
        format!("https://github.com/{owner}/{project}/archive/refs/tags/{tag}.tar.gz",)
    };

    let maintainers = cfg.maintainers.clone().unwrap_or_default();
    let contributors = cfg.contributors.clone().unwrap_or_default();
    let depends = cfg.depends.clone().unwrap_or_default();
    let optdepends = cfg.optdepends.clone().unwrap_or_default();
    let conflicts = cfg
        .conflicts
        .clone()
        .unwrap_or_else(|| vec![format!("{}-bin", pkg_name)]);
    let provides = cfg
        .provides
        .clone()
        .unwrap_or_else(|| vec![pkg_name.clone()]);
    let backup = cfg.backup.clone().unwrap_or_default();
    let makedepends = cfg
        .makedepends
        .clone()
        .unwrap_or_else(|| vec!["rust".to_string(), "cargo".to_string()]);

    let pkgbuild = generate_source_pkgbuild(
        &pkg_name,
        &version,
        pkgrel,
        description,
        homepage,
        license,
        &maintainers,
        &contributors,
        &depends,
        &makedepends,
        &optdepends,
        &conflicts,
        &provides,
        &backup,
        &source_url,
        cfg.prepare.as_deref(),
        cfg.build.as_deref(),
        cfg.package.as_deref(),
        default_name,
    );

    let srcinfo = generate_source_srcinfo(
        &pkg_name,
        &version,
        pkgrel,
        description,
        homepage,
        license,
        &depends,
        &makedepends,
        &optdepends,
        &conflicts,
        &provides,
        &source_url,
    );

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would publish AUR source package '{}' ({})",
            pkg_name, label
        ));
        log.verbose(&format!("PKGBUILD:\n{}", pkgbuild));
        return Ok(());
    }

    // Write files to dist
    let dist = ctx.config.dist.clone();
    let aur_dir = dist.join("aur_source").join(&pkg_name);
    std::fs::create_dir_all(&aur_dir)
        .with_context(|| format!("{}: create dir {}", label, aur_dir.display()))?;

    std::fs::write(aur_dir.join("PKGBUILD"), &pkgbuild)
        .with_context(|| format!("{}: write PKGBUILD", label))?;
    std::fs::write(aur_dir.join(".SRCINFO"), &srcinfo)
        .with_context(|| format!("{}: write .SRCINFO", label))?;

    // Register artifacts
    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::SourcePkgBuild,
        name: "PKGBUILD".to_string(),
        path: aur_dir.join("PKGBUILD"),
        target: None,
        crate_name: pkg_name.clone(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), pkg_name.clone());
            m.insert("format".to_string(), "aur_source".to_string());
            m
        },
        size: None,
    });

    ctx.artifacts.add(anodizer_core::artifact::Artifact {
        kind: anodizer_core::artifact::ArtifactKind::SourceSrcInfo,
        name: ".SRCINFO".to_string(),
        path: aur_dir.join(".SRCINFO"),
        target: None,
        crate_name: pkg_name.clone(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert("id".to_string(), pkg_name.clone());
            m
        },
        size: None,
    });

    // Push to AUR git repo if configured
    if let Some(ref git_url) = cfg.git_url {
        let tmp_dir = tempfile::tempdir().context(format!("{}: create temp dir", label))?;
        let repo_path = tmp_dir.path();

        if cfg.private_key.is_some() || cfg.git_ssh_command.is_some() {
            util::clone_repo_ssh(
                git_url,
                cfg.private_key.as_deref(),
                cfg.git_ssh_command.as_deref(),
                repo_path,
                label,
                log,
            )?;
        } else {
            util::clone_repo_with_auth(git_url, None, repo_path, label, log)?;
        }

        let output_dir = if let Some(ref dir) = cfg.directory {
            let rendered_dir = ctx.render_template(dir).unwrap_or_else(|_| dir.clone());
            let d = repo_path.join(&rendered_dir);
            std::fs::create_dir_all(&d)?;
            d
        } else {
            repo_path.to_path_buf()
        };

        std::fs::copy(aur_dir.join("PKGBUILD"), output_dir.join("PKGBUILD"))?;
        std::fs::copy(aur_dir.join(".SRCINFO"), output_dir.join(".SRCINFO"))?;

        let commit_msg = crate::homebrew::render_commit_msg(
            cfg.commit_msg_template.as_deref(),
            &pkg_name,
            &version,
            "package",
        );
        let commit_opts = util::resolve_commit_opts(cfg.commit_author.as_ref(), None, None);
        util::commit_and_push_with_opts(repo_path, &["."], &commit_msg, None, label, &commit_opts)?;

        log.status(&format!(
            "{}: package '{}' pushed to {}",
            label, pkg_name, git_url
        ));
    }

    log.status(&format!("{}: published '{}'", label, pkg_name));
    Ok(())
}

/// Publish AUR source packages for a crate (per-crate config path).
pub fn publish_to_aur_source(ctx: &mut Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let crate_cfg = ctx
        .config
        .crates
        .iter()
        .find(|c| c.name == crate_name)
        .ok_or_else(|| anyhow::anyhow!("aur_source: crate '{}' not found", crate_name))?;
    let publish_cfg = crate_cfg
        .publish
        .as_ref()
        .and_then(|p| p.aur_source.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "aur_source: no aur_source config for crate '{}'",
                crate_name
            )
        })?
        .clone();

    let label = format!("aur_source: crate '{crate_name}'");
    if crate::util::is_publisher_disabled(
        ctx,
        publish_cfg.skip.as_ref(),
        publish_cfg.skip_upload.as_ref(),
        &label,
        log,
    )? {
        return Ok(());
    }

    publish_aur_source_entry(ctx, &publish_cfg, crate_name, false, "aur_source", log)
}

/// Publish top-level `aur_sources` entries (not tied to a specific crate).
///
/// GoReleaser's `internal/pipe/aursources/aursources.go` reads `ctx.Config.AURSources`
/// as a project-wide array. Each entry generates a source PKGBUILD and .SRCINFO,
/// then pushes them to the configured AUR git repo.
pub fn publish_top_level_aur_sources(ctx: &mut Context, log: &StageLogger) -> Result<()> {
    let entries = match ctx.config.aur_sources {
        Some(ref v) if !v.is_empty() => v.clone(),
        _ => return Ok(()),
    };

    let project_name = ctx
        .template_vars()
        .get("ProjectName")
        .cloned()
        .unwrap_or_default();

    for (i, cfg) in entries.iter().enumerate() {
        let label = format!("aur_sources[{}]", i);
        if crate::util::is_publisher_disabled(
            ctx,
            cfg.skip.as_ref(),
            cfg.skip_upload.as_ref(),
            &label,
            log,
        )? {
            continue;
        }

        publish_aur_source_entry(ctx, cfg, &project_name, true, &label, log)?;
    }

    Ok(())
}

/// Generate a .SRCINFO file for a source AUR package.
#[allow(clippy::too_many_arguments)]
fn generate_source_srcinfo(
    name: &str,
    version: &str,
    pkgrel: u32,
    description: &str,
    homepage: &str,
    license: &str,
    depends: &[String],
    makedepends: &[String],
    optdepends: &[String],
    conflicts: &[String],
    provides: &[String],
    source_url: &str,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("pkgbase = {}", name));
    lines.push(format!("\tpkgdesc = {}", description));
    lines.push(format!("\tpkgver = {}", version));
    lines.push(format!("\tpkgrel = {}", pkgrel));
    if !homepage.is_empty() {
        lines.push(format!("\turl = {}", homepage));
    }
    lines.push("\tarch = x86_64".to_string());
    lines.push("\tarch = aarch64".to_string());
    lines.push(format!("\tlicense = {}", license));
    for d in makedepends {
        lines.push(format!("\tmakedepends = {}", d));
    }
    for d in depends {
        lines.push(format!("\tdepends = {}", d));
    }
    for d in optdepends {
        lines.push(format!("\toptdepends = {}", d));
    }
    for c in conflicts {
        lines.push(format!("\tconflicts = {}", c));
    }
    for p in provides {
        lines.push(format!("\tprovides = {}", p));
    }
    lines.push(format!("\tsource = {}", source_url));
    lines.push("\tsha256sums = SKIP".to_string());
    lines.push(String::new());
    lines.push(format!("pkgname = {}", name));
    lines.join("\n")
}

/// Generate a source-only PKGBUILD that builds from source using cargo.
#[allow(clippy::too_many_arguments)]
fn generate_source_pkgbuild(
    name: &str,
    version: &str,
    pkgrel: u32,
    description: &str,
    homepage: &str,
    license: &str,
    maintainers: &[String],
    contributors: &[String],
    depends: &[String],
    makedepends: &[String],
    optdepends: &[String],
    conflicts: &[String],
    provides: &[String],
    backup: &[String],
    source_url: &str,
    prepare: Option<&str>,
    build: Option<&str>,
    package: Option<&str>,
    binary_name: &str,
) -> String {
    let mut lines = Vec::new();

    // Header comments
    for m in maintainers {
        lines.push(format!("# Maintainer: {}", m));
    }
    for c in contributors {
        lines.push(format!("# Contributor: {}", c));
    }
    if !maintainers.is_empty() || !contributors.is_empty() {
        lines.push(String::new());
    }

    lines.push(format!("pkgname='{}'", name));
    lines.push(format!("pkgver='{}'", version));
    lines.push(format!("pkgrel={}", pkgrel));
    lines.push(format!("pkgdesc=\"{}\"", description));
    lines.push("arch=('x86_64' 'aarch64')".to_string());
    if !homepage.is_empty() {
        lines.push(format!("url='{}'", homepage));
    }
    lines.push(format!("license=('{}')", license));

    if !depends.is_empty() {
        let d: Vec<String> = depends.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("depends=({})", d.join(" ")));
    }
    if !makedepends.is_empty() {
        let d: Vec<String> = makedepends.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("makedepends=({})", d.join(" ")));
    }
    if !optdepends.is_empty() {
        let d: Vec<String> = optdepends.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("optdepends=({})", d.join(" ")));
    }
    if !conflicts.is_empty() {
        let d: Vec<String> = conflicts.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("conflicts=({})", d.join(" ")));
    }
    if !provides.is_empty() {
        let d: Vec<String> = provides.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("provides=({})", d.join(" ")));
    }
    if !backup.is_empty() {
        let d: Vec<String> = backup.iter().map(|s| format!("'{}'", s)).collect();
        lines.push(format!("backup=({})", d.join(" ")));
    }

    lines.push(format!("source=(\"{}\")", source_url));
    lines.push("sha256sums=('SKIP')".to_string());

    lines.push(String::new());

    // prepare() function
    if let Some(prep) = prepare {
        lines.push("prepare() {".to_string());
        for line in prep.lines() {
            lines.push(format!("  {}", line));
        }
        lines.push("}".to_string());
        lines.push(String::new());
    }

    // build() function
    lines.push("build() {".to_string());
    if let Some(b) = build {
        for line in b.lines() {
            lines.push(format!("  {}", line));
        }
    } else {
        lines.push(format!("  cd \"$srcdir/{}-$pkgver\"", binary_name));
        lines.push("  cargo build --release --locked".to_string());
    }
    lines.push("}".to_string());
    lines.push(String::new());

    // package() function
    lines.push("package() {".to_string());
    if let Some(pkg) = package {
        for line in pkg.lines() {
            lines.push(format!("  {}", line));
        }
    } else {
        lines.push(format!("  cd \"$srcdir/{}-$pkgver\"", binary_name));
        lines.push(format!(
            "  install -Dm755 \"target/release/{}\" \"$pkgdir/usr/bin/{}\"",
            binary_name, binary_name
        ));
    }
    lines.push("}".to_string());

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_source_pkgbuild() {
        let pkgbuild = generate_source_pkgbuild(
            "myapp",
            "1.0.0",
            1,
            "A test application",
            "https://example.com",
            "MIT",
            &["Test <test@example.com>".to_string()],
            &[],
            &["openssl".to_string()],
            &["rust".to_string(), "cargo".to_string()],
            &[],
            &["myapp-bin".to_string()],
            &["myapp".to_string()],
            &[],
            "https://github.com/user/myapp/archive/refs/tags/v1.0.0.tar.gz",
            None,
            None,
            None,
            "myapp",
        );

        assert!(pkgbuild.contains("pkgname='myapp'"));
        assert!(pkgbuild.contains("pkgver='1.0.0'"));
        assert!(pkgbuild.contains("pkgrel=1"));
        assert!(pkgbuild.contains("arch=('x86_64' 'aarch64')"));
        assert!(pkgbuild.contains("makedepends=('rust' 'cargo')"));
        assert!(pkgbuild.contains("conflicts=('myapp-bin')"));
        assert!(pkgbuild.contains("cargo build --release --locked"));
        assert!(pkgbuild.contains("install -Dm755"));
        assert!(pkgbuild.contains("# Maintainer: Test <test@example.com>"));
    }

    #[test]
    fn test_generate_source_pkgbuild_custom_build() {
        let pkgbuild = generate_source_pkgbuild(
            "myapp",
            "1.0.0",
            1,
            "Test",
            "",
            "MIT",
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            "https://example.com/source.tar.gz",
            Some("cd myapp\npatch -p1 < fix.patch"),
            Some("make"),
            Some("make install DESTDIR=\"$pkgdir\""),
            "myapp",
        );

        assert!(pkgbuild.contains("prepare() {"));
        assert!(pkgbuild.contains("patch -p1 < fix.patch"));
        assert!(pkgbuild.contains("make\n}"));
        assert!(pkgbuild.contains("make install DESTDIR=\"$pkgdir\""));
    }

    #[test]
    fn test_generate_source_srcinfo() {
        let srcinfo = generate_source_srcinfo(
            "myapp",
            "1.0.0",
            1,
            "A test application",
            "https://example.com",
            "MIT",
            &["openssl".to_string()],
            &["rust".to_string(), "cargo".to_string()],
            &[],
            &["myapp-bin".to_string()],
            &["myapp".to_string()],
            "https://github.com/user/myapp/archive/refs/tags/v1.0.0.tar.gz",
        );

        assert!(srcinfo.contains("pkgbase = myapp"));
        assert!(srcinfo.contains("\tpkgver = 1.0.0"));
        assert!(srcinfo.contains("\tmakedepends = rust"));
        assert!(srcinfo.contains("\tdepends = openssl"));
        assert!(srcinfo.contains("\tconflicts = myapp-bin"));
        assert!(srcinfo.contains("\tprovides = myapp"));
        assert!(srcinfo.contains("pkgname = myapp"));
    }

    #[test]
    fn test_top_level_aur_sources_config_parsing() {
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
aur_sources:
  - name: myapp
    description: "My application"
    license: MIT
    makedepends:
      - rust
      - cargo
    git_url: "ssh://aur@aur.archlinux.org/myapp.git"
  - name: myapp-extra
    description: "Extra package"
    license: MIT
    git_url: "ssh://aur@aur.archlinux.org/myapp-extra.git"
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur_sources = config.aur_sources.as_ref().unwrap();
        assert_eq!(aur_sources.len(), 2);
        assert_eq!(aur_sources[0].name.as_deref(), Some("myapp"));
        assert_eq!(
            aur_sources[0].makedepends.as_ref().unwrap(),
            &["rust", "cargo"]
        );
        assert_eq!(aur_sources[1].name.as_deref(), Some("myapp-extra"));
    }

    #[test]
    fn test_aur_source_config_parsing() {
        use anodizer_core::config::Config;

        let yaml = r#"
project_name: test
crates:
  - name: myapp
    path: "."
    tag_template: "v{{ .Version }}"
    publish:
      aur_source:
        name: myapp
        description: "My application"
        license: MIT
        makedepends:
          - rust
          - cargo
        depends:
          - openssl
        git_url: "ssh://aur@aur.archlinux.org/myapp.git"
"#;
        let config: Config = serde_yaml_ng::from_str(yaml).unwrap();
        let aur_src = config.crates[0]
            .publish
            .as_ref()
            .unwrap()
            .aur_source
            .as_ref()
            .unwrap();
        assert_eq!(aur_src.name.as_deref(), Some("myapp"));
        assert_eq!(aur_src.makedepends.as_ref().unwrap(), &["rust", "cargo"]);
        assert_eq!(aur_src.depends.as_ref().unwrap(), &["openssl"]);
    }
}
