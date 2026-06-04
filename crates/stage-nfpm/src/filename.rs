//! Per-packager `ConventionalFileName` implementations, matching
//! nfpm v2.46.3 output.
//!
//! Before this module, the `ConventionalFileName` template variable was
//! hand-rolled as `{name}_{version}_{os}_{arch}{ext}` — wrong for every
//! format except deb. Users who referenced `{{ .ConventionalFileName }}`
//! in a `file_name_template` got a deb-shaped path even for rpm
//! (expects `{name}-{version}-{release}.{arch}.rpm`), apk
//! (expects `{name}_{pkgver}_{arch}.apk`), archlinux
//! (expects `{name}-{version}-{pkgrel}-{arch}.pkg.tar.zst`), ipk
//! (same shape as deb but with ipk-specific arch translation).
//!
//! The conventional file-name shape per format:
//!
//! - deb: `{name}_{version}_{arch}.deb`
//! - rpm: `{name}-{version}-{release}.{arch}.rpm`
//! - apk: `{name}_{pkgver}_{arch}.apk`
//! - archlinux: `{name}-{version}-{pkgrel}-{arch}.pkg.tar.zst`
//! - ipk: deb-shaped, with ipk-specific arch translation.
//!
//! Arch translation tables match nfpm output byte-for-byte.

use anodizer_core::config::NfpmConfig;

/// Input to the per-packager filename builders. Carries the full set of
/// fields the upstream `ConventionalFileName` methods read from
/// `nfpm.Info`, already resolved from the anodizer `NfpmConfig`.
///
/// `arch` uses anodizer's Go-style naming (`amd64`, `arm64`, `armv7`,
/// `armv6`, `386`, `mipsle`, `mips64le`, `ppc64le`, `s390`, `all`, ...)
/// because the upstream translation tables are keyed by those strings.
/// The per-format helpers translate to the packager-native arch inline.
pub struct FileNameInfo<'a> {
    pub name: &'a str,
    pub version: &'a str,
    /// Go-style arch (`amd64`, `arm64`, `armv7`, `386`, …).
    pub arch: &'a str,
    pub release: &'a str,
    pub prerelease: &'a str,
    pub version_metadata: &'a str,
    /// Packager-native arch override (e.g. `deb.arch`, `rpm.arch`).
    pub arch_override: Option<&'a str>,
}

impl<'a> FileNameInfo<'a> {
    /// Build from an `NfpmConfig` and resolved per-target/per-format
    /// values. `pkg_name` and `version` are resolved upstream (template
    /// rendering is already applied).
    pub fn from_config(
        cfg: &'a NfpmConfig,
        pkg_name: &'a str,
        version: &'a str,
        arch: &'a str,
    ) -> Self {
        Self {
            name: pkg_name,
            version,
            arch,
            release: cfg.release.as_deref().unwrap_or(""),
            prerelease: cfg.prerelease.as_deref().unwrap_or(""),
            version_metadata: cfg.version_metadata.as_deref().unwrap_or(""),
            arch_override: None,
        }
    }
}

/// Return the conventional filename for the given packager format, or
/// `None` when the format is unknown (callers fall back to the
/// hand-rolled default, preserving old behaviour for unrecognised
/// entries instead of returning a misleading empty string).
pub fn conventional_filename(format: &str, info: &FileNameInfo<'_>) -> Option<String> {
    match format {
        "deb" => Some(deb_filename(info)),
        "termux.deb" => Some(deb_filename(info)),
        "rpm" => Some(rpm_filename(info)),
        "apk" => Some(apk_filename(info)),
        "archlinux" => Some(archlinux_filename(info)),
        "ipk" => Some(ipk_filename(info)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// deb
// ---------------------------------------------------------------------------

/// Debian arch translation, keyed on Go-style arch (matching nfpm
/// v2.46.3's `archToDebian`).
///
/// Rows mirror nfpm exactly, plus anodizer's `armv5`/`armv6`/`armv7`
/// aliases for the `arm5`/`arm6`/`arm7` keys. Unmapped archs (`amd64`,
/// `ppc64`, `riscv64`, …) pass through, matching nfpm's fall-through.
fn debian_arch(arch: &str) -> &str {
    match arch {
        "386" => "i386",
        "arm64" => "arm64",
        "arm5" | "armv5" => "armel",
        "arm6" | "armv6" => "armhf",
        "arm7" | "armv7" => "armhf",
        "mips64le" => "mips64el",
        "mipsle" => "mipsel",
        "ppc64le" => "ppc64el",
        "s390" => "s390x",
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

fn deb_filename(info: &FileNameInfo<'_>) -> String {
    // deb version composition:
    //   {version}[~prerelease][+metadata][-release]
    let mut version = info.version.to_string();
    if !info.prerelease.is_empty() {
        version.push('~');
        version.push_str(info.prerelease);
    }
    if !info.version_metadata.is_empty() {
        version.push('+');
        version.push_str(info.version_metadata);
    }
    if !info.release.is_empty() {
        version.push('-');
        version.push_str(info.release);
    }
    let arch = info.arch_override.unwrap_or_else(|| debian_arch(info.arch));
    format!("{}_{}_{}.deb", info.name, version, arch)
}

// ---------------------------------------------------------------------------
// rpm
// ---------------------------------------------------------------------------

/// RPM arch translation, keyed on Go-style arch (matching nfpm
/// v2.46.3's `archToRPM`).
fn rpm_arch(arch: &str) -> &str {
    match arch {
        "all" => "noarch",
        "amd64" => "x86_64",
        "386" => "i386",
        "arm64" => "aarch64",
        "arm5" | "armv5" => "armv5tel",
        "arm6" | "armv6" => "armv6hl",
        "arm7" | "armv7" => "armv7hl",
        "mips64le" => "mips64el",
        "mipsle" => "mipsel",
        "mips" => "mips",
        "loong64" => "loongarch64",
        other => other,
    }
}

fn rpm_filename(info: &FileNameInfo<'_>) -> String {
    // rpm version composition:
    //   {version}[~sanitized_prerelease][+metadata]
    // Prerelease dashes are replaced with underscores to stay within the
    // RPM version grammar.
    let mut version = info.version.to_string();
    if !info.prerelease.is_empty() {
        version.push('~');
        version.push_str(&info.prerelease.replace('-', "_"));
    }
    if !info.version_metadata.is_empty() {
        version.push('+');
        version.push_str(info.version_metadata);
    }
    // Release defaults to "1" when empty.
    let release = if info.release.is_empty() {
        "1"
    } else {
        info.release
    };
    let arch = info.arch_override.unwrap_or_else(|| rpm_arch(info.arch));
    format!("{}-{}-{}.{}.rpm", info.name, version, release, arch)
}

// ---------------------------------------------------------------------------
// apk
// ---------------------------------------------------------------------------

/// Alpine arch translation, keyed on Go-style arch (matching nfpm
/// v2.46.3's `archToAlpine`).
///
/// The identity rows (`aarch64`, `x86_64`) and the `i386`/`i686 => x86`
/// rows are part of nfpm's map, so they're kept verbatim for
/// byte-for-byte parity even though `other => other` would already
/// cover the identity cases — they are an intentional replica, not
/// deletable cruft.
fn apk_arch(arch: &str) -> &str {
    match arch {
        "all" => "noarch",
        "386" => "x86",
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        "arm6" | "armv6" => "armhf",
        "arm7" | "armv7" => "armv7",
        "ppc64le" => "ppc64le",
        "s390" => "s390x",
        "loong64" => "loongarch64",
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        "i386" => "x86",
        "i686" => "x86",
        other => other,
    }
}

fn apk_filename(info: &FileNameInfo<'_>) -> String {
    let version = apk_pkgver(info);
    let arch = info.arch_override.unwrap_or_else(|| apk_arch(info.arch));
    format!("{}_{}_{}.apk", info.name, version, arch)
}

/// apk version composition:
///   {version}[_prerelease]["-r"+release][-{p,cvs,svn,git,hg}prefix+metadata]
/// Release always gets an `r` prefix if the user omitted it; metadata
/// gets a `p` prefix unless it already starts with a known VCS tag.
fn apk_pkgver(info: &FileNameInfo<'_>) -> String {
    let mut version = info.version.to_string();
    if !info.prerelease.is_empty() {
        version.push('_');
        version.push_str(info.prerelease);
    }
    if !info.release.is_empty() {
        let rel = if info.release.starts_with('r') {
            info.release.to_string()
        } else {
            format!("r{}", info.release)
        };
        version.push('-');
        version.push_str(&rel);
    }
    if !info.version_metadata.is_empty() {
        let meta = &info.version_metadata;
        let prefixed = if meta.starts_with('p')
            || meta.starts_with("cvs")
            || meta.starts_with("svn")
            || meta.starts_with("git")
            || meta.starts_with("hg")
        {
            meta.to_string()
        } else {
            format!("p{}", meta)
        };
        version.push('-');
        version.push_str(&prefixed);
    }
    version
}

// ---------------------------------------------------------------------------
// archlinux
// ---------------------------------------------------------------------------

/// Arch Linux arch translation, keyed on Go-style arch (matching nfpm
/// v2.46.3's `archToArchLinux`).
///
/// The identity rows (`x86_64`, `aarch64`) are part of nfpm's map, so
/// they're kept verbatim for byte-for-byte parity even though
/// `other => other` would already cover them — an intentional replica,
/// not deletable cruft.
fn archlinux_arch(arch: &str) -> &str {
    match arch {
        "all" => "any",
        "amd64" => "x86_64",
        "386" => "i686",
        "arm64" => "aarch64",
        "arm7" | "armv7" => "armv7h",
        "arm6" | "armv6" => "armv6h",
        "arm5" | "armv5" => "arm",
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "i386" => "i686",
        other => other,
    }
}

fn archlinux_filename(info: &FileNameInfo<'_>) -> String {
    // Archlinux: {name}-{version}{_sanitized_prerelease}-{pkgrel}-{arch}.pkg.tar.zst
    // pkgrel is parsed as int, defaults to 1 on parse failure (matches
    // an unparseable pkgrel falls back to 1).
    let pkgrel: i64 = info.release.parse().unwrap_or(1);
    let prerelease_sanitized = info.prerelease.replace('-', "_");
    let arch = info
        .arch_override
        .unwrap_or_else(|| archlinux_arch(info.arch));
    let raw = format!(
        "{}-{}{}-{}-{}.pkg.tar.zst",
        info.name, info.version, prerelease_sanitized, pkgrel, arch,
    );
    valid_pkg_name(&raw)
}

/// Arch-Linux package-name sanitisation: keep only
/// `[A-Za-z0-9]`, `.`, `_`, `+`, `-`; then strip leading `-` / `.`.
fn valid_pkg_name(s: &str) -> String {
    let filtered: String = s
        .chars()
        .filter(|&r| r.is_ascii_alphanumeric() || matches!(r, '.' | '_' | '+' | '-'))
        .collect();
    filtered.trim_start_matches(['-', '.']).to_string()
}

// ---------------------------------------------------------------------------
// ipk
// ---------------------------------------------------------------------------

/// Translate a generic nfpm architecture (`amd64`, `arm64`, …) into the
/// control-field nomenclature a given packager stamps into the built package
/// (deb keeps `arm64`, rpm uses `aarch64`, apk uses `aarch64`, …).
///
/// This is the same per-format mapping the conventional filename uses, exposed
/// so a built package's `Architecture` control field can be cross-checked
/// against the architecture anodizer resolved. An unknown format passes the
/// generic arch through unchanged.
pub fn control_arch(format: &str, arch: &str) -> String {
    let mapped = match format {
        "deb" | "termux.deb" => debian_arch(arch),
        "rpm" => rpm_arch(arch),
        "apk" => apk_arch(arch),
        "archlinux" => archlinux_arch(arch),
        "ipk" => ipk_arch(arch),
        _ => arch,
    };
    mapped.to_string()
}

/// IPK arch translation, keyed on Go-style arch (matching nfpm
/// v2.46.3's `archToIPK`).
///
/// The identity rows (`x86_64`, `i386`) are part of nfpm's map, so
/// they're kept verbatim for byte-for-byte parity even though
/// `other => other` would already cover them — an intentional replica,
/// not deletable cruft.
fn ipk_arch(arch: &str) -> &str {
    match arch {
        "386" => "i386",
        "amd64" => "x86_64",
        "arm64" => "arm64",
        "arm5" | "armv5" => "armel",
        "arm6" | "armv6" => "armhf",
        "arm7" | "armv7" => "armhf",
        "mips64le" => "mips64el",
        "mipsle" => "mipsel",
        "ppc64le" => "ppc64el",
        "s390" => "s390x",
        "x86_64" => "x86_64",
        "aarch64" => "arm64",
        "i386" => "i386",
        other => other,
    }
}

fn ipk_filename(info: &FileNameInfo<'_>) -> String {
    // ipk version composition matches deb exactly — upstream shares the
    // same format as the ipk conventional file name.
    let mut version = info.version.to_string();
    if !info.prerelease.is_empty() {
        version.push('~');
        version.push_str(info.prerelease);
    }
    if !info.version_metadata.is_empty() {
        version.push('+');
        version.push_str(info.version_metadata);
    }
    if !info.release.is_empty() {
        version.push('-');
        version.push_str(info.release);
    }
    let arch = info.arch_override.unwrap_or_else(|| ipk_arch(info.arch));
    format!("{}_{}_{}.ipk", info.name, version, arch)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base(arch: &str) -> FileNameInfo<'static> {
        // Leaked strings keep lifetimes simple for table-driven tests.
        // All tests drop references at end of fn; Box::leak is fine here.
        FileNameInfo {
            name: "myapp",
            version: "1.2.3",
            arch: Box::leak(arch.to_string().into_boxed_str()),
            release: "",
            prerelease: "",
            version_metadata: "",
            arch_override: None,
        }
    }

    #[test]
    fn deb_amd64_basic() {
        // amd64 passes through unmapped under archToDebian — upstream does
        // the same.
        assert_eq!(deb_filename(&base("amd64")), "myapp_1.2.3_amd64.deb");
    }

    #[test]
    fn deb_armv7_is_armhf() {
        // anodizer "armv7" and upstream "arm7" both map to "armhf".
        assert_eq!(deb_filename(&base("armv7")), "myapp_1.2.3_armhf.deb");
        assert_eq!(deb_filename(&base("arm7")), "myapp_1.2.3_armhf.deb");
    }

    #[test]
    fn deb_armv6_is_armhf() {
        assert_eq!(deb_filename(&base("armv6")), "myapp_1.2.3_armhf.deb");
    }

    #[test]
    fn deb_i386_from_386() {
        assert_eq!(deb_filename(&base("386")), "myapp_1.2.3_i386.deb");
    }

    #[test]
    fn deb_ppc64le_is_ppc64el() {
        assert_eq!(deb_filename(&base("ppc64le")), "myapp_1.2.3_ppc64el.deb");
    }

    #[test]
    fn deb_with_release_prerelease_metadata() {
        // deb ordering: ~prerelease, +metadata, -release.
        let info = FileNameInfo {
            name: "myapp",
            version: "1.2.3",
            arch: "amd64",
            release: "2",
            prerelease: "beta1",
            version_metadata: "g1234abcd",
            arch_override: None,
        };
        assert_eq!(
            deb_filename(&info),
            "myapp_1.2.3~beta1+g1234abcd-2_amd64.deb"
        );
    }

    #[test]
    fn rpm_amd64_is_x86_64() {
        assert_eq!(rpm_filename(&base("amd64")), "myapp-1.2.3-1.x86_64.rpm");
    }

    #[test]
    fn rpm_arm64_is_aarch64() {
        assert_eq!(rpm_filename(&base("arm64")), "myapp-1.2.3-1.aarch64.rpm");
    }

    #[test]
    fn rpm_armv7_is_armv7hl() {
        // RPM's conventional Linux arm7 name; different from deb's armhf.
        assert_eq!(rpm_filename(&base("armv7")), "myapp-1.2.3-1.armv7hl.rpm");
    }

    #[test]
    fn rpm_all_is_noarch() {
        assert_eq!(rpm_filename(&base("all")), "myapp-1.2.3-1.noarch.rpm");
    }

    #[test]
    fn rpm_default_release_is_1() {
        // empty release → "1".
        assert_eq!(rpm_filename(&base("amd64")), "myapp-1.2.3-1.x86_64.rpm");
    }

    #[test]
    fn rpm_prerelease_dash_becomes_underscore() {
        // rpm version formatting replaces `-` with `_` in prerelease strings
        // so the version stays within the RPM version grammar.
        let info = FileNameInfo {
            name: "myapp",
            version: "1.2.3",
            arch: "amd64",
            release: "1",
            prerelease: "rc-1",
            version_metadata: "",
            arch_override: None,
        };
        assert_eq!(rpm_filename(&info), "myapp-1.2.3~rc_1-1.x86_64.rpm");
    }

    #[test]
    fn apk_amd64_is_x86_64() {
        assert_eq!(apk_filename(&base("amd64")), "myapp_1.2.3_x86_64.apk");
    }

    #[test]
    fn apk_386_is_x86() {
        // Alpine uses "x86" not "i386" — distinct from rpm/deb.
        assert_eq!(apk_filename(&base("386")), "myapp_1.2.3_x86.apk");
    }

    #[test]
    fn apk_armv6_is_armhf() {
        assert_eq!(apk_filename(&base("armv6")), "myapp_1.2.3_armhf.apk");
    }

    #[test]
    fn apk_armv7_unchanged() {
        // apk keeps "armv7" verbatim (deb would call this armhf).
        assert_eq!(apk_filename(&base("armv7")), "myapp_1.2.3_armv7.apk");
    }

    #[test]
    fn apk_release_prefixed_r_once() {
        // apk pkgver: if release already starts with "r",
        // don't double-prefix.
        let info = FileNameInfo {
            release: "r5",
            ..base("amd64")
        };
        assert_eq!(apk_filename(&info), "myapp_1.2.3-r5_x86_64.apk");
        let info2 = FileNameInfo {
            release: "5",
            ..base("amd64")
        };
        assert_eq!(apk_filename(&info2), "myapp_1.2.3-r5_x86_64.apk");
    }

    #[test]
    fn apk_metadata_p_prefix_unless_vcs() {
        // apk pkgver prefixes metadata with "p" UNLESS it already
        // starts with one of p/cvs/svn/git/hg.
        let a = FileNameInfo {
            version_metadata: "abc123",
            ..base("amd64")
        };
        assert_eq!(apk_filename(&a), "myapp_1.2.3-pabc123_x86_64.apk");

        for vcs in ["git", "hg", "cvs", "svn", "p"] {
            let info = FileNameInfo {
                version_metadata: Box::leak(format!("{vcs}-1234").into_boxed_str()),
                ..base("amd64")
            };
            assert_eq!(
                apk_filename(&info),
                format!("myapp_1.2.3-{vcs}-1234_x86_64.apk"),
                "vcs prefix {} should pass through",
                vcs
            );
        }
    }

    #[test]
    fn archlinux_amd64_is_x86_64() {
        assert_eq!(
            archlinux_filename(&base("amd64")),
            "myapp-1.2.3-1-x86_64.pkg.tar.zst"
        );
    }

    #[test]
    fn archlinux_386_is_i686() {
        // Arch Linux uses "i686" — the classic Pentium Pro target.
        assert_eq!(
            archlinux_filename(&base("386")),
            "myapp-1.2.3-1-i686.pkg.tar.zst"
        );
    }

    #[test]
    fn archlinux_armv7_is_armv7h() {
        // Distinct from deb's "armhf" and apk's "armv7".
        assert_eq!(
            archlinux_filename(&base("armv7")),
            "myapp-1.2.3-1-armv7h.pkg.tar.zst"
        );
    }

    #[test]
    fn archlinux_all_is_any() {
        assert_eq!(
            archlinux_filename(&base("all")),
            "myapp-1.2.3-1-any.pkg.tar.zst"
        );
    }

    #[test]
    fn archlinux_release_non_numeric_defaults_to_1() {
        // pkgrel is parsed as an integer; on error falls back to 1.
        let info = FileNameInfo {
            release: "not-a-number",
            ..base("amd64")
        };
        assert_eq!(
            archlinux_filename(&info),
            "myapp-1.2.3-1-x86_64.pkg.tar.zst"
        );
    }

    #[test]
    fn archlinux_prerelease_dash_becomes_underscore() {
        let info = FileNameInfo {
            prerelease: "rc-1",
            ..base("amd64")
        };
        assert_eq!(
            archlinux_filename(&info),
            "myapp-1.2.3rc_1-1-x86_64.pkg.tar.zst"
        );
    }

    #[test]
    fn archlinux_strips_invalid_chars() {
        // valid_pkg_name keeps [A-Za-z0-9._+-] only.
        assert_eq!(valid_pkg_name("my@app"), "myapp");
        assert_eq!(valid_pkg_name("-leading.dash"), "leading.dash");
        assert_eq!(valid_pkg_name(".leading"), "leading");
    }

    #[test]
    fn ipk_amd64_is_x86_64() {
        // ipk's arch table uses x86_64 for amd64 (different from deb which
        // keeps amd64).
        assert_eq!(ipk_filename(&base("amd64")), "myapp_1.2.3_x86_64.ipk");
    }

    #[test]
    fn ipk_armv7_is_armhf() {
        assert_eq!(ipk_filename(&base("armv7")), "myapp_1.2.3_armhf.ipk");
    }

    #[test]
    fn ipk_with_release() {
        let info = FileNameInfo {
            release: "3",
            ..base("amd64")
        };
        assert_eq!(ipk_filename(&info), "myapp_1.2.3-3_x86_64.ipk");
    }

    #[test]
    fn conventional_filename_dispatches_by_format() {
        let info = base("amd64");
        assert_eq!(
            conventional_filename("deb", &info).as_deref(),
            Some("myapp_1.2.3_amd64.deb")
        );
        assert_eq!(
            conventional_filename("rpm", &info).as_deref(),
            Some("myapp-1.2.3-1.x86_64.rpm")
        );
        assert_eq!(
            conventional_filename("apk", &info).as_deref(),
            Some("myapp_1.2.3_x86_64.apk")
        );
        assert_eq!(
            conventional_filename("archlinux", &info).as_deref(),
            Some("myapp-1.2.3-1-x86_64.pkg.tar.zst")
        );
        assert_eq!(
            conventional_filename("ipk", &info).as_deref(),
            Some("myapp_1.2.3_x86_64.ipk")
        );
        assert_eq!(
            conventional_filename("termux.deb", &info).as_deref(),
            Some("myapp_1.2.3_amd64.deb")
        );
        // Unknown formats return None so callers can fall back.
        assert_eq!(conventional_filename("snap", &info), None);
    }

    #[test]
    fn apk_all_is_noarch() {
        // An arch-independent apk must be named `..._noarch.apk`, not
        // `..._all.apk`. The `all` token reaches apk via the synthetic
        // `darwin-universal` arch and the `control_arch` / arch-override
        // surfaces; before nfpm v2.46.3 parity it leaked through as `all`.
        assert_eq!(apk_arch("all"), "noarch");
        assert_eq!(apk_filename(&base("all")), "myapp_1.2.3_noarch.apk");
        assert_eq!(control_arch("apk", "all"), "noarch");
    }

    #[test]
    fn loong64_translates_per_format() {
        // anodizer's `map_target` feeds `loong64` for loongarch64 triples;
        // rpm/apk must name it `loongarch64`.
        assert_eq!(rpm_arch("loong64"), "loongarch64");
        assert_eq!(apk_arch("loong64"), "loongarch64");
    }

    /// nfpm v2.46.3 arch maps, verbatim from `deb/deb.go`, `rpm/rpm.go`,
    /// `apk/apk.go`, `arch/arch.go`, `ipk/ipk.go` at tag v2.46.3. Each
    /// per-format helper must reproduce its map key-for-key (plus
    /// anodizer's `armv5`/`armv6`/`armv7` superset aliases, asserted
    /// separately below).
    #[test]
    fn tables_replicate_nfpm_2_46_3() {
        let arch_to_debian = [
            ("386", "i386"),
            ("arm64", "arm64"),
            ("arm5", "armel"),
            ("arm6", "armhf"),
            ("arm7", "armhf"),
            ("mips64le", "mips64el"),
            ("mipsle", "mipsel"),
            ("ppc64le", "ppc64el"),
            ("s390", "s390x"),
            ("x86_64", "amd64"),
            ("aarch64", "arm64"),
        ];
        for (k, v) in arch_to_debian {
            assert_eq!(debian_arch(k), v, "debian_arch({k})");
        }

        let arch_to_rpm = [
            ("all", "noarch"),
            ("amd64", "x86_64"),
            ("386", "i386"),
            ("arm64", "aarch64"),
            ("arm5", "armv5tel"),
            ("arm6", "armv6hl"),
            ("arm7", "armv7hl"),
            ("mips64le", "mips64el"),
            ("mipsle", "mipsel"),
            ("mips", "mips"),
            ("loong64", "loongarch64"),
        ];
        for (k, v) in arch_to_rpm {
            assert_eq!(rpm_arch(k), v, "rpm_arch({k})");
        }

        let arch_to_alpine = [
            ("all", "noarch"),
            ("386", "x86"),
            ("amd64", "x86_64"),
            ("arm64", "aarch64"),
            ("arm6", "armhf"),
            ("arm7", "armv7"),
            ("ppc64le", "ppc64le"),
            ("s390", "s390x"),
            ("loong64", "loongarch64"),
            ("aarch64", "aarch64"),
            ("x86_64", "x86_64"),
            ("i386", "x86"),
            ("i686", "x86"),
        ];
        for (k, v) in arch_to_alpine {
            assert_eq!(apk_arch(k), v, "apk_arch({k})");
        }

        let arch_to_archlinux = [
            ("all", "any"),
            ("amd64", "x86_64"),
            ("386", "i686"),
            ("arm64", "aarch64"),
            ("arm7", "armv7h"),
            ("arm6", "armv6h"),
            ("arm5", "arm"),
            ("x86_64", "x86_64"),
            ("aarch64", "aarch64"),
            ("i386", "i686"),
        ];
        for (k, v) in arch_to_archlinux {
            assert_eq!(archlinux_arch(k), v, "archlinux_arch({k})");
        }

        let arch_to_ipk = [
            ("386", "i386"),
            ("amd64", "x86_64"),
            ("arm64", "arm64"),
            ("arm5", "armel"),
            ("arm6", "armhf"),
            ("arm7", "armhf"),
            ("mips64le", "mips64el"),
            ("mipsle", "mipsel"),
            ("ppc64le", "ppc64el"),
            ("s390", "s390x"),
            ("x86_64", "x86_64"),
            ("aarch64", "arm64"),
            ("i386", "i386"),
        ];
        for (k, v) in arch_to_ipk {
            assert_eq!(ipk_arch(k), v, "ipk_arch({k})");
        }
    }

    #[test]
    fn armv_aliases_mirror_arm_keys() {
        // anodizer's documented superset: `armvN` aliases resolve
        // identically to nfpm's `armN` keys, per format.
        for (a, n) in [("armv5", "arm5"), ("armv6", "arm6"), ("armv7", "arm7")] {
            assert_eq!(debian_arch(a), debian_arch(n), "debian {a}/{n}");
            assert_eq!(rpm_arch(a), rpm_arch(n), "rpm {a}/{n}");
            assert_eq!(archlinux_arch(a), archlinux_arch(n), "archlinux {a}/{n}");
            assert_eq!(ipk_arch(a), ipk_arch(n), "ipk {a}/{n}");
        }
        // apk has no arm5 key; arm6/arm7 only.
        for (a, n) in [("armv6", "arm6"), ("armv7", "arm7")] {
            assert_eq!(apk_arch(a), apk_arch(n), "apk {a}/{n}");
        }
    }

    /// `control_arch` is the surface the post-build cross-check uses to compare
    /// a built package's `Architecture` control field against the resolved arch.
    /// A built `.deb` keeps the generic `amd64`/`arm64` names, while the same
    /// generic arch renders as `x86_64`/`aarch64` in a `.rpm` header — so the
    /// expected value MUST be derived per-format, never compared cross-format.
    /// This locks the deb-vs-rpm nomenclature matrix and the FALSE-match guards
    /// that prove a deb expectation cannot validate an rpm-resolved package (or
    /// the reverse) for the same logical arch.
    #[test]
    fn control_arch_matrix_deb_vs_rpm_nomenclature() {
        // Generic nfpm arch -> (deb control name, rpm control name).
        let matrix = [
            ("amd64", "amd64", "x86_64"),
            ("arm64", "arm64", "aarch64"),
            ("386", "i386", "i386"),
        ];
        for (generic, deb, rpm) in matrix {
            assert_eq!(control_arch("deb", generic), deb, "deb {generic}");
            assert_eq!(control_arch("rpm", generic), rpm, "rpm {generic}");
        }

        // FALSE-match guards: the two 64-bit arches differ across formats, so a
        // deb `amd64` control field must NOT equal the rpm-resolved expectation
        // for the SAME logical arch — comparing raw, without per-format
        // derivation, would mislabel one as the other.
        assert_ne!(
            control_arch("deb", "amd64"),
            control_arch("rpm", "amd64"),
            "deb amd64 must not equal rpm x86_64 for the same logical arch"
        );
        assert_ne!(
            control_arch("deb", "arm64"),
            control_arch("rpm", "arm64"),
            "deb arm64 must not equal rpm aarch64 for the same logical arch"
        );
        // And a deb amd64 expectation must reject an arm64-resolved package.
        assert_ne!(
            control_arch("deb", "amd64"),
            control_arch("deb", "arm64"),
            "deb amd64 must not equal deb arm64"
        );
        assert_ne!(
            control_arch("rpm", "amd64"),
            control_arch("rpm", "arm64"),
            "rpm x86_64 must not equal rpm aarch64"
        );

        // Unmapped passthrough is documented, not a silent surprise: an arch no
        // per-format table maps (and an unknown FORMAT) returns the generic
        // string unchanged, so the cross-check compares it literally.
        assert_eq!(
            control_arch("deb", "riscv64"),
            "riscv64",
            "an unmapped deb arch passes through unchanged"
        );
        assert_eq!(
            control_arch("rpm", "riscv64"),
            "riscv64",
            "an unmapped rpm arch passes through unchanged"
        );
        assert_eq!(
            control_arch("snap", "amd64"),
            "amd64",
            "an unknown format passes the generic arch through unchanged"
        );
    }
}
