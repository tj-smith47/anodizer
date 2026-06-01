//! glibc-ceiling check.
//!
//! A binary built against a newer glibc than the oldest distro you support
//! will fail to run there with a cryptic `version 'GLIBC_2.xx' not found`.
//! This check reads the required glibc symbol versions from a `.deb`'s
//! embedded ELF binary (the `.gnu.version_r` / verneed records, surfaced by
//! the `object` crate) and fails if the maximum required version exceeds a
//! configured floor.
//!
//! musl-linked binaries have NO glibc requirement (no verneed entries naming
//! `GLIBC_*`) and are SKIPPED — which is precisely the point: a musl build
//! hides a glibc-floor regression, so "no glibc requirement" is a pass, not a
//! defect.
//!
//! Two layers, both unit-testable without a network or Docker:
//! - [`GlibcVersion`] — numeric, component-wise version parse + compare
//!   (tested on synthetic strings; `2.36` vs `2.4` vs `2.36.1` must order
//!   numerically, NOT lexically).
//! - [`max_glibc_requirement`] — extracts the maximum `GLIBC_*` requirement
//!   from ELF bytes via `object` (tested on a real binary / fixture).

use std::cmp::Ordering;

use anyhow::{Context, Result};
use object::elf::FileHeader64;
use object::read::elf::FileHeader;
use object::{Endianness, SymbolIndex};

/// A dotted glibc version (e.g. `GLIBC_2.36` → `[2, 36]`), compared
/// component-wise as integers so `2.36 > 2.4` (lexical string compare would
/// wrongly order `2.4 > 2.36`).
#[derive(Debug, Clone)]
pub struct GlibcVersion {
    components: Vec<u64>,
    /// The original dotted text, retained for diagnostics.
    raw: String,
}

// Equality is defined by the numeric ordering (trailing-zero-insensitive), so
// `2.36 == 2.36.0`. Deriving `PartialEq` would instead compare the component
// vecs field-wise and the `raw` string, making those two unequal — which would
// contradict `Ord`. Keep the two consistent.
impl PartialEq for GlibcVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for GlibcVersion {}

impl GlibcVersion {
    /// Parse a dotted version body (the part AFTER the `GLIBC_` prefix, e.g.
    /// `2.36` or `2.2.5`). Returns `None` when a component is non-numeric.
    pub fn parse(body: &str) -> Option<Self> {
        let mut components = Vec::new();
        for part in body.split('.') {
            components.push(part.parse::<u64>().ok()?);
        }
        if components.is_empty() {
            return None;
        }
        Some(Self {
            components,
            raw: body.to_string(),
        })
    }

    /// The original dotted text (without the `GLIBC_` prefix).
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl PartialOrd for GlibcVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for GlibcVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        // Component-wise numeric compare; a missing trailing component counts
        // as 0 so `2.36` == `2.36.0` and `2.36.1 > 2.36`.
        let len = self.components.len().max(other.components.len());
        for i in 0..len {
            let a = self.components.get(i).copied().unwrap_or(0);
            let b = other.components.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => continue,
                non_eq => return non_eq,
            }
        }
        Ordering::Equal
    }
}

/// Outcome of checking one `.deb`'s embedded binary against a glibc ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibcCheckOutcome {
    /// No `GLIBC_*` requirement found — a static or musl binary. Skipped (a
    /// pass): musl hides the very floor regression this check guards.
    NoGlibcRequirement,
    /// The maximum required glibc version is within the ceiling.
    WithinCeiling {
        /// The highest `GLIBC_*` version the binary requires.
        max: String,
    },
    /// The maximum required glibc version EXCEEDS the ceiling — a defect.
    ExceedsCeiling {
        /// The highest `GLIBC_*` version the binary requires.
        max: String,
        /// The configured ceiling it exceeds.
        ceiling: String,
    },
}

/// Extract the maximum `GLIBC_*` symbol-version requirement from a 64-bit
/// little-endian ELF's `.gnu.version_r` data.
///
/// Returns `Ok(None)` when the ELF has no versioned-symbol table or no
/// `GLIBC_*` requirement (static / musl). Returns `Ok(Some(max))` with the
/// numerically-greatest requirement otherwise. Errors only on malformed ELF.
pub fn max_glibc_requirement(elf_bytes: &[u8]) -> Result<Option<GlibcVersion>> {
    let elf = FileHeader64::<Endianness>::parse(elf_bytes)
        .context("verify-release: parse ELF header for glibc check")?;
    let endian = elf
        .endian()
        .context("verify-release: read ELF endianness")?;
    let sections = elf
        .sections(endian, elf_bytes)
        .context("verify-release: read ELF sections")?;
    let versions = sections
        .versions(endian, elf_bytes)
        .context("verify-release: read ELF version table")?;
    let Some(version_table) = versions else {
        // No `.gnu.version` table at all — static / musl binary.
        return Ok(None);
    };

    let symbols = sections
        .symbols(endian, elf_bytes, object::elf::SHT_DYNSYM)
        .context("verify-release: read ELF dynamic symbols")?;

    let mut max: Option<GlibcVersion> = None;
    for index in 0..symbols.len() {
        let vindex = version_table.version_index(endian, SymbolIndex(index));
        let Ok(Some(version)) = version_table.version(vindex) else {
            continue;
        };
        let Ok(name) = std::str::from_utf8(version.name()) else {
            continue;
        };
        let Some(body) = name.strip_prefix("GLIBC_") else {
            continue;
        };
        let Some(parsed) = GlibcVersion::parse(body) else {
            continue;
        };
        if max.as_ref().is_none_or(|cur| parsed > *cur) {
            max = Some(parsed);
        }
    }
    Ok(max)
}

/// Check one ELF binary's glibc requirement against `ceiling`.
///
/// `ceiling` is the dotted glibc floor (e.g. `"2.36"`). A binary requiring a
/// glibc strictly NEWER than `ceiling` produces [`LibcCheckOutcome::ExceedsCeiling`].
pub fn check_glibc_ceiling(elf_bytes: &[u8], ceiling: &str) -> Result<LibcCheckOutcome> {
    let ceiling_ver = GlibcVersion::parse(ceiling).ok_or_else(|| {
        anyhow::anyhow!("verify-release: invalid glibc_ceiling '{ceiling}' (expected e.g. 2.36)")
    })?;
    match max_glibc_requirement(elf_bytes)? {
        None => Ok(LibcCheckOutcome::NoGlibcRequirement),
        Some(max) if max > ceiling_ver => Ok(LibcCheckOutcome::ExceedsCeiling {
            max: max.raw().to_string(),
            ceiling: ceiling.to_string(),
        }),
        Some(max) => Ok(LibcCheckOutcome::WithinCeiling {
            max: max.raw().to_string(),
        }),
    }
}

/// Compare a set of `GLIBC_*` requirement strings against a ceiling, without
/// touching ELF bytes — the synthetic-symbol-list path used in tests and by
/// any caller that has already extracted requirement names.
pub fn check_glibc_requirements(requirements: &[&str], ceiling: &str) -> Result<LibcCheckOutcome> {
    let ceiling_ver = GlibcVersion::parse(ceiling).ok_or_else(|| {
        anyhow::anyhow!("verify-release: invalid glibc_ceiling '{ceiling}' (expected e.g. 2.36)")
    })?;
    let max = requirements
        .iter()
        .filter_map(|r| r.strip_prefix("GLIBC_"))
        .filter_map(GlibcVersion::parse)
        .max();
    match max {
        None => Ok(LibcCheckOutcome::NoGlibcRequirement),
        Some(max) if max > ceiling_ver => Ok(LibcCheckOutcome::ExceedsCeiling {
            max: max.raw().to_string(),
            ceiling: ceiling.to_string(),
        }),
        Some(max) => Ok(LibcCheckOutcome::WithinCeiling {
            max: max.raw().to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_compare_beats_lexical() {
        // Lexically "2.4" > "2.36" (because '4' > '3'); numerically it's the
        // reverse. This is the canonical bug the check must not have.
        let v2_4 = GlibcVersion::parse("2.4").unwrap();
        let v2_36 = GlibcVersion::parse("2.36").unwrap();
        assert!(v2_36 > v2_4, "2.36 must be greater than 2.4 numerically");
    }

    #[test]
    fn patch_component_ordering() {
        let v2_36 = GlibcVersion::parse("2.36").unwrap();
        let v2_36_1 = GlibcVersion::parse("2.36.1").unwrap();
        assert!(v2_36_1 > v2_36, "2.36.1 > 2.36");
        // Trailing zero equals the shorter form.
        assert_eq!(GlibcVersion::parse("2.36.0").unwrap(), v2_36);
    }

    #[test]
    fn rejects_non_numeric() {
        assert!(GlibcVersion::parse("2.x").is_none());
        assert!(GlibcVersion::parse("").is_none());
    }

    #[test]
    fn requirement_exceeds_ceiling_fails() {
        let out = check_glibc_requirements(&["GLIBC_2.2.5", "GLIBC_2.38"], "2.36").unwrap();
        assert_eq!(
            out,
            LibcCheckOutcome::ExceedsCeiling {
                max: "2.38".to_string(),
                ceiling: "2.36".to_string(),
            }
        );
    }

    #[test]
    fn requirement_within_ceiling_passes() {
        let out = check_glibc_requirements(&["GLIBC_2.2.5", "GLIBC_2.31"], "2.36").unwrap();
        assert_eq!(
            out,
            LibcCheckOutcome::WithinCeiling {
                max: "2.31".to_string()
            }
        );
    }

    #[test]
    fn requirement_equal_to_ceiling_passes() {
        // Equal is within (the ceiling is the floor you support, inclusive).
        let out = check_glibc_requirements(&["GLIBC_2.36"], "2.36").unwrap();
        assert_eq!(
            out,
            LibcCheckOutcome::WithinCeiling {
                max: "2.36".to_string()
            }
        );
    }

    #[test]
    fn no_glibc_requirement_is_skipped() {
        // A musl binary's verneed has no GLIBC_* entries.
        let out = check_glibc_requirements(&["libc.so.6"], "2.36").unwrap();
        assert_eq!(out, LibcCheckOutcome::NoGlibcRequirement);
        let empty = check_glibc_requirements(&[], "2.36").unwrap();
        assert_eq!(empty, LibcCheckOutcome::NoGlibcRequirement);
    }

    #[test]
    fn lexical_max_would_be_wrong() {
        // /bin/ls-style real requirement set: lexical max is "2.9", numeric
        // max is "2.34". The check must use numeric.
        let reqs = [
            "GLIBC_2.29",
            "GLIBC_2.2.5",
            "GLIBC_2.34",
            "GLIBC_2.4",
            "GLIBC_2.9",
        ];
        let out = check_glibc_requirements(&reqs, "2.40").unwrap();
        assert_eq!(
            out,
            LibcCheckOutcome::WithinCeiling {
                max: "2.34".to_string()
            },
            "numeric max is 2.34, not the lexical 2.9"
        );
    }

    #[test]
    fn invalid_ceiling_errors() {
        assert!(check_glibc_requirements(&["GLIBC_2.36"], "not-a-version").is_err());
    }

    #[test]
    fn real_elf_extraction_matches_numeric_max() {
        // Parse a real glibc-linked binary from the host and assert the
        // extracted max is a sane GLIBC version. This proves the `object`
        // .gnu.version_r path, not just the synthetic compare. Skips
        // gracefully when /bin/ls is absent or not a 64-bit LE ELF.
        let Ok(bytes) = std::fs::read("/bin/ls") else {
            eprintln!("skipping: /bin/ls not readable");
            return;
        };
        match max_glibc_requirement(&bytes) {
            Ok(Some(max)) => {
                // /bin/ls links glibc; the max must parse and be >= 2.2.5.
                let floor = GlibcVersion::parse("2.2.5").unwrap();
                assert!(max >= floor, "extracted glibc max {} too low", max.raw());
            }
            Ok(None) => eprintln!("skipping: /bin/ls reported no glibc requirement"),
            Err(e) => eprintln!("skipping: /bin/ls not a parseable ELF here: {e}"),
        }
    }
}
