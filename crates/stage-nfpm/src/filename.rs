//! Per-packager `ConventionalFileName` implementations, ported from
//! upstream nfpm v2.44.0.
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
//! The code here mirrors the upstream Go implementations:
//!
//! - deb: `ConventionalFileName` in `/tmp/nfpm/deb/deb.go` (see
//!   <https://github.com/goreleaser/nfpm/blob/v2.44.0/deb/deb.go>).
//! - rpm: `ConventionalFileName` + `formatVersion` + `setDefaults` in
//!   `/tmp/nfpm/rpm/rpm.go`.
//! - apk: `ConventionalFileName` + `pkgver` in `/tmp/nfpm/apk/apk.go`.
//! - archlinux: `ConventionalFileName` + `validPkgName` in
//!   `/tmp/nfpm/arch/arch.go`.
//! - ipk: `ConventionalFileName` in `/tmp/nfpm/ipk/ipk.go`.
//!
//! Arch translation tables match upstream byte-for-byte.

use anodize_core::config::NfpmConfig;

/// Input to the per-packager filename builders. Carries the full set of
/// fields the upstream `ConventionalFileName` methods read from
/// `nfpm.Info`, already resolved from the anodize `NfpmConfig`.
///
/// `arch` uses anodize's Go-style naming (`amd64`, `arm64`, `armv7`,
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
        format: &str,
    ) -> Self {
        // nfpm exposes per-format arch overrides (e.g. `deb.arch`); we
        // only support `deb.arch_variant` today, which is a different
        // concept (microarch label, not an arch rename). Leave the
        // override slot open for future per-packager arch fields.
        let _ = format; // suppress unused-var while we plumb overrides
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

/// Debian arch translation. Keyed on Go-style arch.
/// Source: `archToDebian` in nfpm v2.44.0 `deb/deb.go`.
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
        // Unmapped archs (including amd64, ppc64, riscv64, s390x already)
        // pass through — upstream does the same.
        other => other,
    }
}

fn deb_filename(info: &FileNameInfo<'_>) -> String {
    // deb version composition (from nfpm deb.go::ConventionalFileName):
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

/// RPM arch translation. Source: `archToRPM` in nfpm v2.44.0 `rpm/rpm.go`.
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
        other => other,
    }
}

fn rpm_filename(info: &FileNameInfo<'_>) -> String {
    // rpm version composition (from nfpm rpm.go::formatVersion):
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
    // Release defaults to "1" when empty (nfpm rpm.go::setDefaults).
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

/// Alpine arch translation. Source: `archToAlpine` in nfpm v2.44.0 `apk/apk.go`.
fn apk_arch(arch: &str) -> &str {
    match arch {
        "386" => "x86",
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        "arm6" | "armv6" => "armhf",
        "arm7" | "armv7" => "armv7",
        "ppc64le" => "ppc64le",
        "s390" => "s390x",
        other => other,
    }
}

fn apk_filename(info: &FileNameInfo<'_>) -> String {
    let version = apk_pkgver(info);
    let arch = info.arch_override.unwrap_or_else(|| apk_arch(info.arch));
    format!("{}_{}_{}.apk", info.name, version, arch)
}

/// apk version composition (from nfpm apk.go::pkgver):
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

/// Arch Linux arch translation. Source: `archToArchLinux` in nfpm v2.44.0 `arch/arch.go`.
fn archlinux_arch(arch: &str) -> &str {
    match arch {
        "all" => "any",
        "amd64" => "x86_64",
        "386" => "i686",
        "arm64" => "aarch64",
        "arm7" | "armv7" => "armv7h",
        "arm6" | "armv6" => "armv6h",
        "arm5" | "armv5" => "arm",
        other => other,
    }
}

fn archlinux_filename(info: &FileNameInfo<'_>) -> String {
    // Archlinux: {name}-{version}{_sanitized_prerelease}-{pkgrel}-{arch}.pkg.tar.zst
    // pkgrel is parsed as int, defaults to 1 on parse failure (matches
    // nfpm arch.go where strconv.Atoi errors fall back to 1).
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

/// Mirror of `validPkgName` + `mapValidChar` in nfpm arch.go: keep only
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

/// IPK arch translation. Source: `archToIPK` in nfpm v2.44.0 `ipk/ipk.go`.
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
        other => other,
    }
}

fn ipk_filename(info: &FileNameInfo<'_>) -> String {
    // ipk version composition matches deb exactly — upstream shares the
    // same format in `ipk/ipk.go::ConventionalFileName`.
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
        // the same. See nfpm v2.44 deb/deb.go archToDebian.
        assert_eq!(deb_filename(&base("amd64")), "myapp_1.2.3_amd64.deb");
    }

    #[test]
    fn deb_armv7_is_armhf() {
        // anodize "armv7" and upstream "arm7" both map to "armhf".
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
        // Matches deb.go ordering: ~prerelease, +metadata, -release.
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
        // Matches nfpm rpm.go::setDefaults: empty release → "1".
        assert_eq!(rpm_filename(&base("amd64")), "myapp-1.2.3-1.x86_64.rpm");
    }

    #[test]
    fn rpm_prerelease_dash_becomes_underscore() {
        // rpm.go::formatVersion replaces `-` with `_` in prerelease strings
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
        // Matches apk.go::pkgver: if release already starts with "r",
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
        // apk.go::pkgver prefixes metadata with "p" UNLESS it already
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
        // arch.go parses pkgrel via strconv.Atoi; on error falls back to 1.
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
}
