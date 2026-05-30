//! Root `flake.nix` generation for the Nix overlay repository.
//!
//! The per-crate derivations that [`super::publish_to_nix`] writes
//! (`pkgs/<name>/default.nix` by default, or an arbitrary `nix.path`
//! such as `nix/myapp.nix`) are overlay-style packages: each is a
//! function consumable via `pkgs.callPackage`. On their own they make
//! the repo usable as a Nixpkgs overlay, but NOT directly
//! flake-installable — `nix profile install github:<owner>/<repo>#<name>`,
//! `nix build .#<name>`, and `nix run github:<owner>/<repo>#<name>` all
//! require a root `flake.nix` that exposes `packages.<system>.<name>`.
//!
//! Every publish merges the package just written into the package set
//! recovered from the prior committed `flake.nix`, then regenerates the
//! flake from the merged set. Merging — rather than re-globbing a fixed
//! `pkgs/*` layout — is what makes the flake correct for custom
//! `nix.path` values AND idempotent/clobber-safe across the per-crate
//! re-clone loop: a sibling package published at `nix/foo.nix` survives
//! even though it does not live under `pkgs/`, and republishing one
//! crate never drops the others.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result};

use super::generate::nix_escape_string;

/// The standard Nix systems every published package is exposed for.
///
/// These are Nix system doubles (`<arch>-<os>`), NOT anodize's go-arch
/// asset names. The arch/os → asset mapping lives inside each package's
/// derivation (`urlMap`/`shaMap` keyed by these same doubles), so the
/// flake only ever speaks in doubles and the derivation resolves the
/// correct release asset per system.
pub(crate) const FLAKE_SYSTEMS: &[&str] = &[
    "x86_64-linux",
    "aarch64-linux",
    "x86_64-darwin",
    "aarch64-darwin",
];

/// Stable top-level `description` for the generated flake.
///
/// Deterministic by construction: it does NOT vary with which crate
/// published last, so a multi-crate publish commits a byte-stable
/// top-level value regardless of order. Per-package descriptions still
/// live inside each derivation's `meta.description`.
const FLAKE_DESCRIPTION: &str = "Nix flake for release artifacts published by anodize";

/// Indent prefix on each overlay `callPackage` line in the generated
/// flake. The line shape is fixed by this module, so re-parsing it to
/// recover the prior package set on the next publish is robust — we
/// control both the writer and the reader.
const OVERLAY_LINE_PREFIX: &str = "        ";

const FLAKE_TEMPLATE: &str = r#"{
  description = "{{ description }}";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [
{% for system in systems %}        "{{ system }}"
{% endfor %}      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ self.overlays.default ];
      };
    in
    {
      overlays.default = final: prev: {
{% for pkg in packages %}        {{ pkg.attr }} = final.callPackage ./{{ pkg.path }} { };
{% endfor %}      };

      packages = forAllSystems (system:
        let pkgs = pkgsFor system;
        in {
{% for pkg in packages %}          {{ pkg.attr }} = pkgs.{{ pkg.attr }};
{% endfor %}        });
    };
}
"#;

/// A package exposed by the flake: its attribute name (`attr`, always the
/// package/derivation name) and the repo-relative path of the derivation
/// file (`path`, e.g. `pkgs/foo/default.nix` or `nix/foo.nix`).
///
/// `attr` is the package name — never guessed from `path`, which for a
/// custom `nix.path` may not contain the name at all.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct FlakePackage {
    pub attr: String,
    pub path: String,
}

/// Recover the package set written by a prior publish from the existing
/// `flake.nix`, parsing the fixed-shape overlay `callPackage` lines this
/// module emits.
///
/// Returns the recovered `(attr, path)` entries keyed by `attr`. A
/// missing or unparseable flake yields an empty map — the caller always
/// merges the current package in afterward, so a fresh repo (no prior
/// flake) and a hand-edited flake both degrade to "at least the current
/// package is present" rather than erroring.
fn recover_prior_packages(repo_path: &Path) -> BTreeMap<String, FlakePackage> {
    let flake_path = repo_path.join("flake.nix");
    let Ok(contents) = std::fs::read_to_string(&flake_path) else {
        return BTreeMap::new();
    };
    let mut out: BTreeMap<String, FlakePackage> = BTreeMap::new();
    for line in contents.lines() {
        if let Some(pkg) = parse_overlay_line(line) {
            out.insert(pkg.attr.clone(), pkg);
        }
    }
    out
}

/// Parse one overlay `callPackage` line of the fixed shape
/// `        <attr> = final.callPackage ./<path> { };`, returning the
/// `(attr, path)` it carries. Returns `None` for any other line.
fn parse_overlay_line(line: &str) -> Option<FlakePackage> {
    let body = line.strip_prefix(OVERLAY_LINE_PREFIX)?;
    let (attr, rest) = body.split_once(" = final.callPackage ./")?;
    let attr = attr.trim();
    if attr.is_empty() || attr.contains(char::is_whitespace) {
        return None;
    }
    let path = rest.strip_suffix(" { };")?.trim();
    if path.is_empty() {
        return None;
    }
    Some(FlakePackage {
        attr: attr.to_string(),
        path: path.to_string(),
    })
}

/// Render the root `flake.nix` exposing every `package` for all
/// [`FLAKE_SYSTEMS`].
///
/// The top-level `description` is the stable [`FLAKE_DESCRIPTION`] (so
/// output is independent of publish order). The output is fully
/// determined by `packages` (already sorted by the caller), making
/// re-renders byte-identical.
pub(crate) fn generate_flake(packages: &[FlakePackage]) -> Result<String> {
    let tera = anodizer_core::template::parse_static("nix-flake", FLAKE_TEMPLATE)
        .context("nix: parse flake template")?;

    let mut ctx = tera::Context::new();
    // `description` is a controlled constant, but route it through the
    // same escape as free-text Nix-string inserts so the template can
    // never emit an invalid literal even if the constant changes.
    ctx.insert("description", &nix_escape_string(FLAKE_DESCRIPTION));
    ctx.insert("systems", FLAKE_SYSTEMS);
    ctx.insert("packages", packages);

    anodizer_core::template::render_static(&tera, "nix-flake", &ctx, "nix")
}

/// Merge the just-written package `(attr, path)` into the prior package
/// set and (re)write `<repo>/flake.nix`.
///
/// `attr` is the package/derivation name; `nix_path` is the
/// repo-relative path of the derivation file actually written
/// (honoring a custom `nix.path`). Returns the repo-relative path of the
/// flake (`"flake.nix"`) so the caller can stage it for commit alongside
/// the derivation. The current package always wins over any stale prior
/// entry for the same attr (it may now live at a new path or version);
/// siblings are preserved.
pub(super) fn write_flake(repo_path: &Path, attr: &str, nix_path: &str) -> Result<&'static str> {
    let mut set = recover_prior_packages(repo_path);
    set.insert(
        attr.to_string(),
        FlakePackage {
            attr: attr.to_string(),
            path: nix_path.to_string(),
        },
    );
    // BTreeMap iteration is sorted by attr → deterministic ordering.
    let packages: Vec<FlakePackage> = set.into_values().collect();

    let flake = generate_flake(&packages)?;
    let flake_path = repo_path.join("flake.nix");
    std::fs::write(&flake_path, &flake)
        .with_context(|| format!("nix: write {}", flake_path.display()))?;
    Ok("flake.nix")
}

/// Cheap structural validity check for a generated `flake.nix`: braces
/// balance AND every emitted overlay `callPackage` line round-trips
/// through [`parse_overlay_line`] (the same recovery parser the next
/// publish relies on). Used by the snapshot emission validator to fail
/// loud locally on a malformed flake rather than at `nix build` time on
/// the consumer's machine.
///
/// Returns the recovered package set on success so the caller can run
/// the system→asset cross-check; returns `Err` on imbalance or an
/// overlay line the recovery parser cannot read back.
pub(crate) fn flake_is_well_formed(flake: &str) -> Result<Vec<FlakePackage>> {
    let opens = flake.matches('{').count();
    let closes = flake.matches('}').count();
    if opens != closes {
        anyhow::bail!("generated flake.nix has unbalanced braces ({opens} '{{' vs {closes} '}}')");
    }
    let mut recovered: Vec<FlakePackage> = Vec::new();
    for line in flake.lines() {
        // Only lines shaped like the overlay callPackage emission are
        // parsed; the recovery parser returns None for everything else.
        if line.contains("final.callPackage") {
            let pkg = parse_overlay_line(line).with_context(|| {
                format!("generated flake.nix has an unparseable overlay line: {line:?}")
            })?;
            recovered.push(pkg);
        }
    }
    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flake_exposes_single_package_for_all_four_systems() {
        let pkgs = vec![FlakePackage {
            attr: "mytool".into(),
            path: "pkgs/mytool/default.nix".into(),
        }];
        let flake = generate_flake(&pkgs).unwrap();

        // Pins nixpkgs.
        assert!(
            flake.contains("nixpkgs.url = \"github:NixOS/nixpkgs/nixos-unstable\";"),
            "{flake}"
        );
        // Stable, order-independent description.
        assert!(
            flake.contains(&format!("description = \"{FLAKE_DESCRIPTION}\";")),
            "{flake}"
        );
        // All four standard systems present.
        for system in FLAKE_SYSTEMS {
            assert!(
                flake.contains(&format!("\"{system}\"")),
                "missing system {system} in:\n{flake}"
            );
        }
        // Package exposed via packages.<system>.<name> (genAttrs form) and
        // composed into the overlay.
        assert!(flake.contains("mytool = pkgs.mytool;"), "{flake}");
        assert!(
            flake.contains("mytool = final.callPackage ./pkgs/mytool/default.nix { };"),
            "{flake}"
        );
        assert!(flake.contains("overlays.default = final: prev:"), "{flake}");
    }

    #[test]
    fn empty_package_set_renders_valid_nix() {
        // Finding #1's edge state (and a legitimately-empty set) must
        // still emit syntactically valid Nix: empty overlay body + empty
        // packages attrset, with the structural braces balanced.
        let flake = generate_flake(&[]).unwrap();
        assert!(
            flake.contains("overlays.default = final: prev: {"),
            "{flake}"
        );
        assert!(flake.contains("packages = forAllSystems"), "{flake}");
        // No package lines leaked in.
        assert!(!flake.contains("callPackage"), "{flake}");
        // Brace balance — a crude but effective validity smoke test for a
        // template with no `${}` interpolation in the empty case.
        let opens = flake.matches('{').count();
        let closes = flake.matches('}').count();
        assert_eq!(opens, closes, "unbalanced braces:\n{flake}");
    }

    #[test]
    fn write_flake_default_path_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let rel = write_flake(tmp.path(), "alpha", "pkgs/alpha/default.nix").unwrap();
        assert_eq!(rel, "flake.nix");
        let flake = std::fs::read_to_string(tmp.path().join("flake.nix")).unwrap();
        assert!(flake.contains("alpha = pkgs.alpha;"), "{flake}");
        assert!(
            flake.contains("alpha = final.callPackage ./pkgs/alpha/default.nix { };"),
            "{flake}"
        );
    }

    #[test]
    fn write_flake_merges_custom_path_sibling_clobber_safe() {
        // Simulate the per-crate re-clone loop: crate A publishes at the
        // default path, then crate B (re-cloned, sees A's flake) publishes
        // at a CUSTOM path. Both must appear; A must NOT be dropped even
        // though B lives outside pkgs/.
        let tmp = tempfile::tempdir().unwrap();
        write_flake(tmp.path(), "alpha", "pkgs/alpha/default.nix").unwrap();
        write_flake(tmp.path(), "beta", "nix/beta.nix").unwrap();

        let flake = std::fs::read_to_string(tmp.path().join("flake.nix")).unwrap();
        // Custom-path sibling exposed by its real path, attr = name.
        assert!(flake.contains("beta = pkgs.beta;"), "{flake}");
        assert!(
            flake.contains("beta = final.callPackage ./nix/beta.nix { };"),
            "custom-path package missing or wrong path:\n{flake}"
        );
        // Prior default-path sibling preserved (clobber-safe).
        assert!(flake.contains("alpha = pkgs.alpha;"), "{flake}");
        assert!(
            flake.contains("alpha = final.callPackage ./pkgs/alpha/default.nix { };"),
            "default-path sibling dropped:\n{flake}"
        );
    }

    #[test]
    fn write_flake_republish_updates_path_not_duplicates() {
        // Republishing the same attr at a new path replaces the entry
        // rather than emitting two lines for one attr.
        let tmp = tempfile::tempdir().unwrap();
        write_flake(tmp.path(), "alpha", "pkgs/alpha/default.nix").unwrap();
        write_flake(tmp.path(), "alpha", "nix/alpha.nix").unwrap();
        let flake = std::fs::read_to_string(tmp.path().join("flake.nix")).unwrap();
        assert_eq!(
            flake.matches("alpha = pkgs.alpha;").count(),
            1,
            "duplicate attr lines:\n{flake}"
        );
        assert!(
            flake.contains("alpha = final.callPackage ./nix/alpha.nix { };"),
            "path not updated:\n{flake}"
        );
        assert!(
            !flake.contains("./pkgs/alpha/default.nix"),
            "stale path retained:\n{flake}"
        );
    }

    #[test]
    fn description_is_order_independent() {
        // Two crates, published in either order, must yield the same
        // top-level description (and the same package set ordering).
        let a = {
            let tmp = tempfile::tempdir().unwrap();
            write_flake(tmp.path(), "alpha", "pkgs/alpha/default.nix").unwrap();
            write_flake(tmp.path(), "beta", "pkgs/beta/default.nix").unwrap();
            std::fs::read_to_string(tmp.path().join("flake.nix")).unwrap()
        };
        let b = {
            let tmp = tempfile::tempdir().unwrap();
            write_flake(tmp.path(), "beta", "pkgs/beta/default.nix").unwrap();
            write_flake(tmp.path(), "alpha", "pkgs/alpha/default.nix").unwrap();
            std::fs::read_to_string(tmp.path().join("flake.nix")).unwrap()
        };
        assert_eq!(a, b, "flake output depends on publish order");
    }

    #[test]
    fn regenerating_with_same_inputs_is_byte_identical() {
        let pkgs = vec![
            FlakePackage {
                attr: "alpha".into(),
                path: "pkgs/alpha/default.nix".into(),
            },
            FlakePackage {
                attr: "beta".into(),
                path: "pkgs/beta/default.nix".into(),
            },
        ];
        assert_eq!(
            generate_flake(&pkgs).unwrap(),
            generate_flake(&pkgs).unwrap()
        );
    }

    #[test]
    fn parse_overlay_line_round_trips_what_we_emit() {
        // Defensive pin: the recovery parser must accept exactly the line
        // the template emits.
        let pkgs = vec![FlakePackage {
            attr: "tool".into(),
            path: "nix/tool.nix".into(),
        }];
        let flake = generate_flake(&pkgs).unwrap();
        let recovered: Vec<FlakePackage> = flake.lines().filter_map(parse_overlay_line).collect();
        assert_eq!(recovered, pkgs, "emitted overlay line did not round-trip");
    }
}
