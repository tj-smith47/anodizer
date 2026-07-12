//! PEP 440 / PEP 503 / PEP 427 name and version forms.
//!
//! PyPI accepts only PEP 440 versions and normalizes project names per
//! PEP 503; wheel filenames escape the distribution name per PEP 427.
//! Cargo versions are semver, so the prerelease grammar differs:
//! `1.2.3-rc.1` (semver) must upload as `1.2.3rc1` (PEP 440). The mapping
//! here mirrors what maturin applies to `bindings = "bin"` crates so a
//! project migrating from maturin-built wheels keeps identical versions
//! on the index.

use anyhow::{Result, bail};

/// Normalize a project name per PEP 503: runs of `-`, `_`, `.` collapse to a
/// single `-`, lowercased. This is the name PyPI's simple index and the
/// upload API compare against.
pub(crate) fn normalize_project_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut in_sep = false;
    for c in name.chars() {
        if c == '-' || c == '_' || c == '.' {
            in_sep = true;
            continue;
        }
        if in_sep && !out.is_empty() {
            out.push('-');
        }
        in_sep = false;
        out.extend(c.to_lowercase());
    }
    out
}

/// Escape a distribution name for a wheel/sdist filename per PEP 427: any
/// run of characters outside `[A-Za-z0-9.]` becomes a single `_`.
pub(crate) fn escape_distribution_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut in_sep = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            if in_sep && !out.is_empty() {
                out.push('_');
            }
            in_sep = false;
            out.push(c);
        } else {
            in_sep = true;
        }
    }
    out
}

/// Validate that a project name is legal for PyPI (PEP 508 name grammar):
/// ASCII letters/digits/`-`/`_`/`.`, starting and ending alphanumeric.
pub(crate) fn validate_project_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!(
            "pypi: project name '{name}' is not a legal PyPI name \
             (ASCII letters, digits, '-', '_', '.'; must start and end alphanumeric)"
        );
    }
    Ok(())
}

/// Convert a semver version string to its PEP 440 form.
///
/// | semver | PEP 440 |
/// |---|---|
/// | `1.2.3` | `1.2.3` |
/// | `1.2.3-alpha.4` / `-alpha4` / `-a.4` | `1.2.3a4` |
/// | `1.2.3-beta.4` / `-b.4` | `1.2.3b4` |
/// | `1.2.3-rc.4` / `-pre.4` / `-preview.4` | `1.2.3rc4` |
/// | bare `-alpha` / `-beta` / `-rc` | number defaults to `0` |
/// | `1.2.3-dev.4` | `1.2.3.dev4` |
/// | trailing `+build.7` | `+build.7` local segment (dots preserved) |
///
/// Any other prerelease form has no faithful PEP 440 equivalent and errors
/// rather than uploading a version pip would order differently than cargo.
pub(crate) fn semver_to_pep440(version: &str) -> Result<String> {
    let (core_and_pre, local) = match version.split_once('+') {
        Some((v, l)) => (v, Some(l)),
        None => (version, None),
    };
    let (core, pre) = match core_and_pre.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (core_and_pre, None),
    };
    if core.is_empty()
        || !core
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    {
        bail!("pypi: version '{version}' does not have a numeric X.Y.Z core");
    }

    let mut out = core.to_string();
    if let Some(pre) = pre {
        let (label, number) = split_prerelease(pre);
        let seg = match label {
            "alpha" | "a" => "a",
            "beta" | "b" => "b",
            "rc" | "pre" | "preview" | "c" => "rc",
            "dev" => ".dev",
            _ => bail!(
                "pypi: prerelease '-{pre}' in version '{version}' has no PEP 440 \
                 equivalent (supported: alpha/beta/rc/pre/preview/dev)"
            ),
        };
        out.push_str(seg);
        out.push_str(number.unwrap_or("0"));
    }
    if let Some(local) = local {
        let legal = !local.is_empty()
            && local
                .split('.')
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric()));
        if !legal {
            bail!(
                "pypi: build metadata '+{local}' in version '{version}' is not a \
                 legal PEP 440 local segment (dot-separated alphanumerics)"
            );
        }
        out.push('+');
        out.push_str(&local.to_lowercase());
    }
    Ok(out)
}

/// Split a semver prerelease like `rc.1`, `rc1`, or `alpha` into its label
/// and optional trailing number.
fn split_prerelease(pre: &str) -> (&str, Option<&str>) {
    if let Some((label, num)) = pre.split_once('.') {
        if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
            return (label, Some(num));
        }
        return (pre, None);
    }
    let digits_at = pre.find(|c: char| c.is_ascii_digit());
    match digits_at {
        Some(i) if i > 0 && pre[i..].chars().all(|c| c.is_ascii_digit()) => {
            (&pre[..i], Some(&pre[i..]))
        }
        _ => (pre, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_project_names_per_pep503() {
        assert_eq!(normalize_project_name("git-cliff"), "git-cliff");
        assert_eq!(normalize_project_name("Git__Cliff"), "git-cliff");
        assert_eq!(normalize_project_name("a.b-c_d"), "a-b-c-d");
        assert_eq!(normalize_project_name("A...B"), "a-b");
    }

    #[test]
    fn escapes_distribution_names_per_pep427() {
        assert_eq!(escape_distribution_name("git-cliff"), "git_cliff");
        assert_eq!(escape_distribution_name("a-b_c"), "a_b_c");
        assert_eq!(escape_distribution_name("a.b"), "a.b");
    }

    #[test]
    fn validates_project_names() {
        assert!(validate_project_name("git-cliff").is_ok());
        assert!(validate_project_name("a").is_ok());
        assert!(validate_project_name("-abc").is_err());
        assert!(validate_project_name("abc-").is_err());
        assert!(validate_project_name("a b").is_err());
        assert!(validate_project_name("").is_err());
    }

    #[test]
    fn release_versions_pass_through() {
        assert_eq!(semver_to_pep440("1.2.3").unwrap(), "1.2.3");
        assert_eq!(semver_to_pep440("0.16.1").unwrap(), "0.16.1");
    }

    #[test]
    fn prerelease_forms_map_to_pep440() {
        assert_eq!(semver_to_pep440("1.2.3-rc.1").unwrap(), "1.2.3rc1");
        assert_eq!(semver_to_pep440("1.2.3-rc1").unwrap(), "1.2.3rc1");
        assert_eq!(semver_to_pep440("1.2.3-alpha.4").unwrap(), "1.2.3a4");
        assert_eq!(semver_to_pep440("1.2.3-beta").unwrap(), "1.2.3b0");
        assert_eq!(semver_to_pep440("1.2.3-pre.2").unwrap(), "1.2.3rc2");
        assert_eq!(semver_to_pep440("1.2.3-dev.9").unwrap(), "1.2.3.dev9");
    }

    #[test]
    fn build_metadata_becomes_local_segment() {
        assert_eq!(semver_to_pep440("1.2.3+Build.7").unwrap(), "1.2.3+build.7");
        assert_eq!(semver_to_pep440("1.2.3-rc.1+abc").unwrap(), "1.2.3rc1+abc");
    }

    #[test]
    fn unmappable_forms_error() {
        assert!(semver_to_pep440("1.2.3-nightly.20260712").is_err());
        assert!(semver_to_pep440("1.2").is_ok());
        assert!(semver_to_pep440("1.x.3").is_err());
        assert!(semver_to_pep440("1.2.3+bad..seg").is_err());
    }
}
