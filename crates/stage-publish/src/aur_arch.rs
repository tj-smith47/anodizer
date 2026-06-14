//! Architecture-name mapping for AUR PKGBUILD generation.
//!
//! Arch Linux names CPU architectures differently from both Go's `GOARCH`
//! (which anodizer's artifact metadata carries) and Rust target triples. A
//! PKGBUILD's `arch=()` array, its `source_<arch>=` / `sha256sums_<arch>=`
//! suffixes, and the `.SRCINFO` `arch =` lines must all use the canonical
//! pacman architecture name, or makepkg downloads the wrong tarball for the
//! host and the installed binary will not run.
//!
//! Both mappers are *fallible*: an architecture this module does not know how
//! to name for Arch is returned as an [`UnknownArch`] error rather than
//! silently relabeled. Relabeling (the historical `_ => "x86_64"` bug) ships a
//! PKGBUILD that maps a non-x86 tarball under `source_x86_64`, so a user on
//! that architecture installs a binary that cannot execute — a silent
//! correctness corruption the caller must surface (hard-fail) or skip (loud
//! warn), never paper over.

/// An architecture name that has no known pacman equivalent.
///
/// Carries the offending input so the caller can build an actionable
/// hard-fail / skip-with-warning message naming the architecture that could
/// not be mapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownArch {
    /// The unmapped architecture token (a `GOARCH` value or a Rust target
    /// triple, depending on which mapper produced it).
    pub input: String,
}

impl std::fmt::Display for UnknownArch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no Arch Linux (pacman) architecture name is known for '{}'",
            self.input
        )
    }
}

impl std::error::Error for UnknownArch {}

/// Map an artifact's `GOARCH`-style architecture (the value anodizer's
/// artifact metadata carries — `amd64`, `arm64`, `386`, `armv7`, ...) to the
/// canonical pacman architecture name used in a PKGBUILD `arch=()` array.
///
/// | GOARCH input                        | pacman arch |
/// |-------------------------------------|-------------|
/// | `amd64`, `x86_64`                   | `x86_64`    |
/// | `arm64`, `aarch64`                  | `aarch64`   |
/// | `armv7`, `arm`, `armhf`, `armv6`    | `armv7h`    |
/// | `386`, `i686`, `i386`, `x86`        | `i686`      |
///
/// Any other input (`ppc64le`, `riscv64`, `s390x`, a typo, ...) returns
/// [`UnknownArch`] — never a silent fallthrough to `x86_64`.
pub fn goarch_to_pacman_arch(arch: &str) -> Result<&'static str, UnknownArch> {
    match arch {
        "amd64" | "x86_64" => Ok("x86_64"),
        "arm64" | "aarch64" => Ok("aarch64"),
        "armv7" | "arm" | "armhf" | "armv6" => Ok("armv7h"),
        "386" | "i686" | "i386" | "x86" => Ok("i686"),
        other => Err(UnknownArch {
            input: other.to_string(),
        }),
    }
}

/// Map a Rust target triple (`x86_64-unknown-linux-gnu`,
/// `aarch64-unknown-linux-gnu`, `armv7-unknown-linux-gnueabihf`,
/// `i686-unknown-linux-gnu`, ...) to the canonical pacman architecture name.
///
/// Used by the AUR *source* publisher, whose `arch=()` array must reflect the
/// architectures the upstream `cargo build` is configured to support rather
/// than a hardcoded constant. Matches on the leading arch token of the triple.
///
/// Returns [`UnknownArch`] for a triple whose architecture has no Arch name,
/// so a source build advertising an architecture it cannot serve is caught
/// rather than emitted.
pub fn triple_to_pacman_arch(triple: &str) -> Result<&'static str, UnknownArch> {
    // Match on the architecture token (the segment before the first `-`).
    let arch_token = triple.split('-').next().unwrap_or(triple);
    let mapped = match arch_token {
        "x86_64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        "i686" | "i386" | "i586" => Some("i686"),
        // armv7/armv6 triples carry a numeric suffix on the arch token
        // (`armv7`, `armv6`) — match by prefix so `armv7l` etc. also map; a
        // bare `arm` token maps to the same 32-bit hard-float Arch port.
        _ if arch_token.starts_with("armv7")
            || arch_token.starts_with("armv6")
            || arch_token == "arm" =>
        {
            Some("armv7h")
        }
        _ => None,
    };
    mapped.ok_or_else(|| UnknownArch {
        input: triple.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goarch_amd64_maps_to_x86_64() {
        assert_eq!(goarch_to_pacman_arch("amd64").unwrap(), "x86_64");
        assert_eq!(goarch_to_pacman_arch("x86_64").unwrap(), "x86_64");
    }

    #[test]
    fn goarch_arm64_maps_to_aarch64_not_x86_64() {
        // Regression guard for the `_ => "x86_64"` fallthrough that silently
        // relabeled aarch64 under the x86_64 source/sha arrays.
        assert_eq!(goarch_to_pacman_arch("arm64").unwrap(), "aarch64");
        assert_eq!(goarch_to_pacman_arch("aarch64").unwrap(), "aarch64");
        assert_ne!(goarch_to_pacman_arch("arm64").unwrap(), "x86_64");
    }

    #[test]
    fn goarch_armv7_maps_to_armv7h() {
        assert_eq!(goarch_to_pacman_arch("armv7").unwrap(), "armv7h");
        assert_eq!(goarch_to_pacman_arch("arm").unwrap(), "armv7h");
        assert_eq!(goarch_to_pacman_arch("armhf").unwrap(), "armv7h");
        assert_eq!(goarch_to_pacman_arch("armv6").unwrap(), "armv7h");
    }

    #[test]
    fn goarch_386_maps_to_i686() {
        assert_eq!(goarch_to_pacman_arch("386").unwrap(), "i686");
        assert_eq!(goarch_to_pacman_arch("i686").unwrap(), "i686");
        assert_eq!(goarch_to_pacman_arch("i386").unwrap(), "i686");
        assert_eq!(goarch_to_pacman_arch("x86").unwrap(), "i686");
    }

    #[test]
    fn goarch_unknown_errors_not_relabeled() {
        for unknown in ["ppc64le", "riscv64", "s390x", "mips64", "wat"] {
            let err = goarch_to_pacman_arch(unknown).unwrap_err();
            assert_eq!(err.input, unknown);
        }
    }

    #[test]
    fn triple_known_arches_map() {
        assert_eq!(
            triple_to_pacman_arch("x86_64-unknown-linux-gnu").unwrap(),
            "x86_64"
        );
        assert_eq!(
            triple_to_pacman_arch("aarch64-unknown-linux-gnu").unwrap(),
            "aarch64"
        );
        assert_eq!(
            triple_to_pacman_arch("armv7-unknown-linux-gnueabihf").unwrap(),
            "armv7h"
        );
        assert_eq!(
            triple_to_pacman_arch("i686-unknown-linux-gnu").unwrap(),
            "i686"
        );
    }

    #[test]
    fn triple_aarch64_not_relabeled_x86_64() {
        assert_ne!(
            triple_to_pacman_arch("aarch64-unknown-linux-gnu").unwrap(),
            "x86_64"
        );
    }

    #[test]
    fn triple_unknown_errors() {
        for unknown in [
            "riscv64gc-unknown-linux-gnu",
            "s390x-unknown-linux-gnu",
            "powerpc64le-unknown-linux-gnu",
        ] {
            let err = triple_to_pacman_arch(unknown).unwrap_err();
            assert_eq!(err.input, unknown);
        }
    }
}
