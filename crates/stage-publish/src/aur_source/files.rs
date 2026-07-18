use anyhow::{Context as _, Result};

// ---------------------------------------------------------------------------
// AUR source render specs
// ---------------------------------------------------------------------------
//
// The original `generate_source_srcinfo` and `generate_source_pkgbuild`
// functions took 12 and 19 positional arguments respectively. Bundle them
// so each public entry point lands well under clippy's threshold and so
// fields that are truly identical between the two render paths
// (`AurMeta`, `AurDeps`) are guaranteed to stay in lock-step.

/// Package identity — name, version, pkgrel, description, homepage, license.
/// Shared by [`generate_source_srcinfo`] and [`generate_source_pkgbuild`].
#[derive(Clone, Copy)]
pub(super) struct AurMeta<'a> {
    pub(super) name: &'a str,
    pub(super) version: &'a str,
    pub(super) pkgrel: u32,
    pub(super) description: &'a str,
    pub(super) homepage: &'a str,
    /// Rendered pacman `license=()` entries (SPDX-id-split for dual-licensed
    /// crates). Empty when no license configured.
    pub(super) license: &'a [String],
    /// Pacman `arch=()` entries derived from the linux build targets the
    /// source package supports (not a hardcoded constant).
    pub(super) arches: &'a [String],
}

/// Dependency lists — the five `depends`/`makedepends`/`optdepends`/
/// `conflicts`/`provides` arrays. Shared by both renderers.
#[derive(Clone, Copy)]
pub(super) struct AurDeps<'a> {
    pub(super) depends: &'a [String],
    pub(super) makedepends: &'a [String],
    pub(super) optdepends: &'a [String],
    pub(super) conflicts: &'a [String],
    pub(super) provides: &'a [String],
}

/// People credits — `# Maintainer:` / `# Contributor:` comment lines.
/// PKGBUILD-only (.SRCINFO does not surface these).
#[derive(Clone, Copy)]
pub(super) struct AurPeople<'a> {
    pub(super) maintainers: &'a [String],
    pub(super) contributors: &'a [String],
}

/// User-supplied PKGBUILD function bodies. Each is opt-in; when `None`,
/// the renderer emits the default cargo-based body.
#[derive(Clone, Copy)]
pub(super) struct AurHooks<'a> {
    pub(super) prepare: Option<&'a str>,
    pub(super) build: Option<&'a str>,
    pub(super) package: Option<&'a str>,
}

/// Everything PKGBUILD-only beyond `meta` + `deps` + `source_url`.
/// Bundles people, hooks, backup file list, and the binary name used by
/// the default build/package bodies.
#[derive(Clone, Copy)]
pub(super) struct AurExtras<'a> {
    pub(super) people: AurPeople<'a>,
    pub(super) hooks: AurHooks<'a>,
    pub(super) backup: &'a [String],
    pub(super) binary_name: &'a str,
    /// When set, the PKGBUILD emits `install=<name>.install` and the
    /// `.install` file (post-install/pre-remove scripts) is written
    /// alongside it (the `install:` field).
    pub(super) install_file: Option<&'a str>,
}

/// Write `PKGBUILD`, `.SRCINFO`, and the optional `.install` file into
/// `aur_dir`. The `.install` file (`<install_filename>`) is only written when
/// `install_content` is `Some`, mirroring the `-bin` AUR publisher's
/// `aur_write_package_files`.
pub(super) fn write_aur_source_files(
    aur_dir: &std::path::Path,
    pkgbuild: &str,
    srcinfo: &str,
    install_filename: &str,
    install_content: Option<&str>,
    label: &str,
) -> Result<()> {
    std::fs::write(aur_dir.join("PKGBUILD"), pkgbuild)
        .with_context(|| format!("{}: write PKGBUILD", label))?;
    std::fs::write(aur_dir.join(".SRCINFO"), srcinfo)
        .with_context(|| format!("{}: write .SRCINFO", label))?;
    if let Some(content) = install_content {
        std::fs::write(aur_dir.join(install_filename), content)
            .with_context(|| format!("{}: write {}", label, install_filename))?;
    }
    Ok(())
}

/// Generate a .SRCINFO file for a source AUR package.
pub(super) fn generate_source_srcinfo(
    meta: &AurMeta<'_>,
    deps: &AurDeps<'_>,
    source_url: &str,
) -> String {
    let AurMeta {
        name,
        version,
        pkgrel,
        description,
        homepage,
        license,
        arches,
    } = *meta;
    let AurDeps {
        depends,
        makedepends,
        optdepends,
        conflicts,
        provides,
    } = *deps;

    let mut lines = Vec::new();
    lines.push(format!("pkgbase = {}", name));
    lines.push(format!("\tpkgdesc = {}", description));
    lines.push(format!("\tpkgver = {}", version));
    lines.push(format!("\tpkgrel = {}", pkgrel));
    if !homepage.is_empty() {
        lines.push(format!("\turl = {}", homepage));
    }
    for a in arches {
        lines.push(format!("\tarch = {}", a));
    }
    for l in license {
        lines.push(format!("\tlicense = {}", l));
    }
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
pub(super) fn generate_source_pkgbuild(
    meta: &AurMeta<'_>,
    deps: &AurDeps<'_>,
    extras: &AurExtras<'_>,
    source_url: &str,
) -> String {
    let AurMeta {
        name,
        version,
        pkgrel,
        description,
        homepage,
        license,
        arches,
    } = *meta;
    let AurDeps {
        depends,
        makedepends,
        optdepends,
        conflicts,
        provides,
    } = *deps;
    let AurExtras {
        people: AurPeople {
            maintainers,
            contributors,
        },
        hooks: AurHooks {
            prepare,
            build,
            package,
        },
        backup,
        binary_name,
        install_file,
    } = *extras;

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
    let arch_entries: Vec<String> = arches.iter().map(|a| format!("'{}'", a)).collect();
    lines.push(format!("arch=({})", arch_entries.join(" ")));
    if !homepage.is_empty() {
        lines.push(format!("url='{}'", homepage));
    }
    let license_entries: Vec<String> = license.iter().map(|l| format!("'{}'", l)).collect();
    lines.push(format!("license=({})", license_entries.join(" ")));

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

    if let Some(install_file) = install_file {
        lines.push(format!("install={}", install_file));
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
        // LICENSE — REQUIRED for non-common licenses; install any LICENSE* the
        // upstream source tree carries. Existence-gated so a tree without one
        // does not fail the build.
        lines.push(
            "  for _l in LICENSE*; do [ -e \"$_l\" ] && \
             install -Dm644 \"$_l\" \"$pkgdir/usr/share/licenses/$pkgname/$_l\"; done"
                .to_string(),
        );
    }
    lines.push("}".to_string());

    lines.join("\n")
}
