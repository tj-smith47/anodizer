//! Nix `lib.licenses` license resolution.
//!
//! A nix derivation's `meta.license` must reference a `lib.licenses`
//! attribute *name* (lowercase camelCase like `mit`, `asl20`, `bsd3`) —
//! NOT an SPDX identifier. Two value shapes reach this resolver:
//!
//! - A value a user wrote directly in `nix.license` — by convention the
//!   nix attribute name, e.g.
//!   `mit`.
//! - A value derived from `Cargo.toml` `[package].license`, which the
//!   Cargo book defines as an **SPDX 2.1 license expression** (e.g.
//!   `MIT`, `Apache-2.0`, `BSD-3-Clause`). Feeding that straight into the
//!   derivation's `lib.licenses.<attr>` lookup would reference a
//!   nonexistent attribute and fail at evaluation.
//!
//! [`resolve_nix_license_meta`] reconciles both: a value already valid as a
//! nix attribute is preserved verbatim; a known SPDX id (or `OR`/`AND` list of
//! known ids) is mapped to its nix attribute(s); anything unmappable — an
//! unknown id or an unparseable compound expression — degrades to a verbatim
//! quoted-string `meta.license` (always valid in Nix `meta`) rather than emit
//! a bogus `lib.licenses.<id>` attr-path or abort the release.

use anodizer_core::license::parse_spdx_expression;

use super::generate::validate_nix_license;

/// Map from SPDX license identifier to the corresponding nix
/// `lib.licenses` attribute name.
///
/// Source of truth for a refresh: nixpkgs `lib/licenses/licenses.nix` —
/// each license attribute carries an `spdxId` field, and this table is the
/// inverse (`spdxId` -> attribute name) for every entry whose attribute is
/// also present in the [`super::generate`] `VALID_NIX_LICENSES` allow-list
/// (so a mapped value always passes validation). Every key here is a real
/// SPDX *license* identifier (per the SPDX license list) — SPDX *exception*
/// identifiers (e.g. `LLVM-exception`, `Classpath-exception-2.0`) are
/// deliberately excluded: an exception never appears as a bare
/// `Cargo.toml` `[package].license` value (it only follows `WITH` in a
/// compound expression, which is rejected), and the corresponding nix
/// attribute is still reachable via direct `nix.license` passthrough.
///
/// One key (`CC-PDDC`) maps to `publicDomain`, which nixpkgs does not tag
/// with an `spdxId`; it is kept as the honest nixpkgs public-domain
/// equivalent for that SPDX id.
///
/// SPDX ids are matched case-insensitively (SPDX treats them as
/// case-insensitive), so keys here are lowercased at lookup time.
///
/// Entries are sorted by SPDX id for readability; lookup is linear over a
/// small table and case-folds the needle.
static SPDX_TO_NIX: &[(&str, &str)] = &[
    ("0BSD", "bsd0"),
    ("Abstyles", "abstyles"),
    ("Adobe-Display-PostScript", "adobeDisplayPostScript"),
    ("Adobe-Utopia", "adobeUtopia"),
    ("AFL-2.0", "afl20"),
    ("AFL-2.1", "afl21"),
    ("AFL-3.0", "afl3"),
    ("AGPL-3.0-only", "agpl3Only"),
    ("AGPL-3.0-or-later", "agpl3Plus"),
    ("Aladdin", "aladdin"),
    ("AML", "aml"),
    ("AMPAS", "ampas"),
    ("Apache-1.1", "asl11"),
    ("Apache-2.0", "asl20"),
    ("APSL-1.0", "apsl10"),
    ("APSL-2.0", "apsl20"),
    ("Arphic-1999", "arphicpl"),
    ("Artistic-1.0", "artistic1"),
    ("Artistic-1.0-cl8", "artistic1-cl8"),
    ("Artistic-2.0", "artistic2"),
    ("Baekmuk", "baekmuk"),
    ("Beerware", "beerware"),
    ("Bitstream-Charter", "bitstreamCharter"),
    ("Bitstream-Vera", "bitstreamVera"),
    ("BitTorrent-1.0", "bitTorrent10"),
    ("BitTorrent-1.1", "bitTorrent11"),
    ("BlueOak-1.0.0", "blueOak100"),
    ("Boehm-GC", "boehmGC"),
    ("BOLA-1.1", "bola11"),
    ("BSD-1-Clause", "bsd1"),
    ("BSD-2-Clause", "bsd2"),
    ("BSD-2-Clause-Patent", "bsd2Patent"),
    ("BSD-2-Clause-Views", "bsd2WithViews"),
    ("BSD-3-Clause", "bsd3"),
    ("BSD-3-Clause-Clear", "bsd3Clear"),
    ("BSD-3-Clause-LBNL", "bsd3Lbnl"),
    ("BSD-3-Clause-Tso", "bsd3ClauseTso"),
    ("BSD-4-Clause", "bsdOriginal"),
    ("BSD-4-Clause-Shortened", "bsdOriginalShortened"),
    ("BSD-4-Clause-UC", "bsdOriginalUC"),
    ("BSD-Protection", "bsdProtection"),
    ("BSD-Source-Code", "bsdSourceCode"),
    ("BSL-1.0", "boost"),
    ("BUSL-1.1", "bsl11"),
    ("bzip2-1.0.6", "bzip2"),
    ("CAL-1.0", "cal10"),
    ("Caldera", "caldera"),
    ("CAPEC-tou", "capec"),
    ("CC-BY-1.0", "cc-by-10"),
    ("CC-BY-2.0", "cc-by-20"),
    ("CC-BY-3.0", "cc-by-30"),
    ("CC-BY-4.0", "cc-by-40"),
    ("CC-BY-NC-3.0", "cc-by-nc-30"),
    ("CC-BY-NC-4.0", "cc-by-nc-40"),
    ("CC-BY-NC-ND-3.0", "cc-by-nc-nd-30"),
    ("CC-BY-NC-ND-4.0", "cc-by-nc-nd-40"),
    ("CC-BY-NC-SA-2.0", "cc-by-nc-sa-20"),
    ("CC-BY-NC-SA-2.5", "cc-by-nc-sa-25"),
    ("CC-BY-NC-SA-3.0", "cc-by-nc-sa-30"),
    ("CC-BY-NC-SA-4.0", "cc-by-nc-sa-40"),
    ("CC-BY-ND-3.0", "cc-by-nd-30"),
    ("CC-BY-ND-4.0", "cc-by-nd-40"),
    ("CC-BY-SA-1.0", "cc-by-sa-10"),
    ("CC-BY-SA-2.0", "cc-by-sa-20"),
    ("CC-BY-SA-2.5", "cc-by-sa-25"),
    ("CC-BY-SA-3.0", "cc-by-sa-30"),
    ("CC-BY-SA-4.0", "cc-by-sa-40"),
    ("CC-PDDC", "publicDomain"),
    ("CC-SA-1.0", "cc-sa-10"),
    ("CC0-1.0", "cc0"),
    ("CDDL-1.0", "cddl"),
    ("CECILL-2.0", "cecill20"),
    ("CECILL-2.1", "cecill21"),
    ("CECILL-B", "cecill-b"),
    ("CECILL-C", "cecill-c"),
    ("ClArtistic", "clArtistic"),
    ("CNRI-Python", "cnri-python"),
    ("CPAL-1.0", "cpal10"),
    ("CPL-1.0", "cpl10"),
    ("Cronyx", "cronyx"),
    ("Curl", "curl"),
    ("DEC-3-Clause", "dec3Clause"),
    ("DOC", "doc"),
    ("DRL-1.0", "drl10"),
    ("dtoa", "dtoa"),
    ("ECL-2.0", "ecl20"),
    ("EFL-1.0", "efl10"),
    ("EFL-2.0", "efl20"),
    ("Elastic-2.0", "elastic20"),
    ("EPL-1.0", "epl10"),
    ("EPL-2.0", "epl20"),
    ("EUPL-1.1", "eupl11"),
    ("EUPL-1.2", "eupl12"),
    ("Fair", "fair"),
    ("FDK-AAC", "fraunhofer-fdk"),
    ("FSL-1.1-ALv2", "fsl11Asl20"),
    ("FSL-1.1-MIT", "fsl11Mit"),
    ("FTL", "ftl"),
    ("GFDL-1.1-only", "fdl11Only"),
    ("GFDL-1.1-or-later", "fdl11Plus"),
    ("GFDL-1.2-only", "fdl12Only"),
    ("GFDL-1.2-or-later", "fdl12Plus"),
    ("GFDL-1.3-only", "fdl13Only"),
    ("GFDL-1.3-or-later", "fdl13Plus"),
    ("Giftware", "giftware"),
    ("Gnuplot", "gnuplot"),
    ("GPL-1.0-only", "gpl1Only"),
    ("GPL-1.0-or-later", "gpl1Plus"),
    ("GPL-2.0", "gpl2"),
    ("GPL-2.0-only", "gpl2Only"),
    ("GPL-2.0-or-later", "gpl2Plus"),
    ("GPL-3.0", "gpl3"),
    ("GPL-3.0-only", "gpl3Only"),
    ("GPL-3.0-or-later", "gpl3Plus"),
    ("HPND", "hpnd"),
    ("HPND-DEC", "hpndDec"),
    ("HPND-doc", "hpndDoc"),
    ("HPND-doc-sell", "hpndDocSell"),
    (
        "HPND-sell-MIT-disclaimer-xserver",
        "hpndSellVariantMitDisclaimerXserver",
    ),
    ("HPND-sell-variant", "hpndSellVariant"),
    (
        "HPND-sell-variant-critical-systems",
        "hpndSellVariantSafetyClause",
    ),
    ("HPND-UC", "hpndUc"),
    ("hyphen-bulgarian", "hyphenBulgarian"),
    ("ICU", "icu"),
    ("IJG", "ijg"),
    ("ImageMagick", "imagemagick"),
    ("Imlib2", "imlib2"),
    ("Info-ZIP", "info-zip"),
    ("Intel-ACPI", "iasl"),
    ("Interbase-1.0", "interbase"),
    ("IPA", "ipa"),
    ("IPL-1.0", "ipl10"),
    ("ISC", "isc"),
    ("Knuth-CTAN", "knuth"),
    ("LAL-1.2", "lal12"),
    ("LAL-1.3", "lal13"),
    ("LGPL-2.0", "lgpl2"),
    ("LGPL-2.0-only", "lgpl2Only"),
    ("LGPL-2.0-or-later", "lgpl2Plus"),
    ("LGPL-2.1", "lgpl21"),
    ("LGPL-2.1-only", "lgpl21Only"),
    ("LGPL-2.1-or-later", "lgpl21Plus"),
    ("LGPL-3.0", "lgpl3"),
    ("LGPL-3.0-only", "lgpl3Only"),
    ("LGPL-3.0-or-later", "lgpl3Plus"),
    ("LGPLLR", "lgpllr"),
    ("Libpng", "libpng"),
    ("libpng-2.0", "libpng2"),
    ("libtiff", "libtiff"),
    ("LPL-1.02", "lpl-102"),
    ("LPPL-1.0", "lppl1"),
    ("LPPL-1.2", "lppl12"),
    ("LPPL-1.3a", "lppl13a"),
    ("LPPL-1.3c", "lppl13c"),
    ("lsof", "lsof"),
    ("MirOS", "miros"),
    ("MIT", "mit"),
    ("MIT-0", "mit0"),
    ("MIT-advertising", "mitAdvertising"),
    ("MIT-CMU", "mit-cmu"),
    ("MIT-enna", "mit-enna"),
    ("MIT-feh", "mit-feh"),
    ("MIT-Modern-Variant", "mit-modern"),
    ("MIT-open-group", "mitOpenGroup"),
    ("MPL-1.0", "mpl10"),
    ("MPL-1.1", "mpl11"),
    ("MPL-2.0", "mpl20"),
    ("mplus", "mplus"),
    ("MS-PL", "mspl"),
    ("MulanPSL-2.0", "mulan-psl2"),
    ("NAIST-2003", "naist-2003"),
    ("NASA-1.3", "nasa13"),
    ("NCBI-PD", "ncbiPd"),
    ("NCSA", "ncsa"),
    ("NGPL", "ngpl"),
    ("NIST-Software", "nistSoftware"),
    ("NLPL", "nlpl"),
    ("NPOSL-3.0", "nposl3"),
    ("NTP", "ntp"),
    ("ODbL-1.0", "odbl"),
    ("OFL-1.1", "ofl"),
    ("OLDAP-2.8", "openldap"),
    ("OML", "oml"),
    ("OpenSSL", "openssl"),
    ("OPUBL-1.0", "opubl"),
    ("OSL-2.0", "osl2"),
    ("OSL-2.1", "osl21"),
    ("OSL-3.0", "osl3"),
    ("ParaType-Free-Font-1.3", "paratype"),
    ("Parity-7.0.0", "parity70"),
    ("PHP-3.01", "php301"),
    ("Pixar", "tost"),
    ("PostgreSQL", "postgresql"),
    ("Python-2.0", "psfl"),
    ("Qhull", "qhull"),
    ("QPL-1.0", "qpl"),
    ("Ruby", "ruby"),
    ("Sendmail", "sendmail"),
    ("SGI-B-2.0", "sgi-b-20"),
    ("SGMLUG-PM", "sgmlug"),
    ("SISSL", "sissl11"),
    ("Sleepycat", "sleepycat"),
    ("SMAIL-GPL", "smail"),
    ("SMLNJ", "smlnj"),
    ("SSPL-1.0", "sspl"),
    ("SUL-1.0", "sustainableUse"),
    ("TCL", "tcltk"),
    ("TCP-wrappers", "tcpWrappers"),
    ("TekHVC", "tekHvcLicense"),
    ("TORQUE-1.1", "torque11"),
    ("Ubuntu-font-1.0", "ufl"),
    ("Unicode-3.0", "unicode-30"),
    ("Unicode-DFS-2015", "unicode-dfs-2015"),
    ("Unicode-DFS-2016", "unicode-dfs-2016"),
    ("Unicode-TOU", "unicodeTOU"),
    ("Unlicense", "unlicense"),
    ("UPL-1.0", "upl"),
    ("Vim", "vim"),
    ("VSL-1.0", "vsl10"),
    ("W3C", "w3c"),
    ("Watcom-1.0", "watcom"),
    ("WTFPL", "wtfpl"),
    ("X11", "x11"),
    ("X11-no-permit-persons", "x11NoPermitPersons"),
    ("Xerox", "xerox"),
    ("Xfig", "xfig"),
    ("xinetd", "xinetd"),
    ("XSkat", "xskat"),
    ("Zlib", "zlib"),
    ("ZPL-2.0", "zpl20"),
    ("ZPL-2.1", "zpl21"),
];

/// Look up the nix `lib.licenses` attribute name for an SPDX id.
/// SPDX ids are case-insensitive, so the needle is folded to lowercase.
fn spdx_to_nix_attr(spdx: &str) -> Option<&'static str> {
    let needle = spdx.to_ascii_lowercase();
    SPDX_TO_NIX
        .iter()
        .find(|(id, _)| id.to_ascii_lowercase() == needle)
        .map(|(_, attr)| *attr)
}

/// The resolved shape of a derivation's `meta.license`, ready to render.
///
/// A nix `meta.license` may be a single attribute (`lib.licenses.mit`), a
/// list (`with lib.licenses; [ mit asl20 ]` for an `A OR B` dual license),
/// or — when an id cannot be mapped to a known `lib.licenses` attribute — a
/// plain string (`license = "<original SPDX string>";`), which Nix always
/// accepts in `meta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NixLicense {
    /// `license = lib.licenses.<attr>;` — exactly one known attribute.
    Single(String),
    /// `license = with lib.licenses; [ <attr> <attr> … ];` — a list of two or
    /// more known attributes (a dual/multi license expression).
    List(Vec<String>),
    /// `license = "<original>";` — the original SPDX/literal string verbatim,
    /// used whenever ANY id is unknown or the expression is a compound the
    /// parser kept literal (`WITH`, mixed connectives, parenthesised). A bare
    /// string is always valid in `meta`, so this never emits an invalid
    /// `lib.licenses` attr-path.
    Str(String),
}

/// Resolve a single license id (an SPDX id OR a direct nix attribute) to its
/// nix `lib.licenses` attribute name, or `None` when it maps to neither.
///
/// Applies two precedence rules — direct nix-attr passthrough, then SPDX→attr
/// mapping — and returns `None` on a miss, so the list/single resolver can fall
/// back to the verbatim string form rather than abort the release.
fn resolve_single_id(id: &str) -> Option<String> {
    if validate_nix_license(id).is_ok() {
        return Some(id.to_string());
    }
    spdx_to_nix_attr(id).map(|attr| attr.to_string())
}

/// Resolve a license value into a renderable [`NixLicense`].
///
/// Uses the shared SPDX parser so a dual-license expression (`MIT OR
/// Apache-2.0`) becomes a `lib.licenses` LIST, matching how nixpkgs renders
/// dual-licensed Rust crates (e.g. ripgrep's `[ unlicense mit ]`). The guard
/// against the homebrew code review applies: the parser may hand back a
/// compound/unparseable `Single` (e.g. `Apache-2.0 WITH LLVM-exception`), and
/// any individual id may not map to a known attribute — in EITHER case the
/// resolver degrades to the verbatim-string form rather than emit a bogus
/// `lib.licenses.<id>` attr-path. An empty value yields `None`.
///
/// Precedence:
/// 1. Empty → `None` (suppresses `meta.license`).
/// 2. Single id that maps to a known attr → [`NixLicense::Single`].
/// 3. `OR`/`AND` list whose ids ALL map to known attrs →
///    [`NixLicense::List`] (rendered `with lib.licenses; [ … ]`).
/// 4. Anything else (a compound `Single` the parser kept literal, an unknown
///    id, or a list with any unmappable id) → [`NixLicense::Str`] carrying the
///    original verbatim string.
pub fn resolve_nix_license_meta(value: &str) -> Option<NixLicense> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expr = parse_spdx_expression(trimmed);
    if expr.is_single() {
        // One literal: map it, or fall back to the verbatim string. A
        // compound the parser declined to split (a `WITH` exception, a mixed
        // connective) lands here as a `Single` carrying the whole expression,
        // which `resolve_single_id` correctly fails to map → string fallback.
        return Some(match resolve_single_id(expr.ids()[0].as_str()) {
            Some(attr) => NixLicense::Single(attr),
            None => NixLicense::Str(trimmed.to_string()),
        });
    }
    // A connective expression (OR / AND): resolve every id. If ANY id is
    // unknown, the whole expression degrades to the verbatim string rather
    // than emit a list with a bogus attr.
    let mut attrs = Vec::with_capacity(expr.ids().len());
    for id in expr.ids() {
        match resolve_single_id(id) {
            Some(attr) => attrs.push(attr),
            None => return Some(NixLicense::Str(trimmed.to_string())),
        }
    }
    Some(NixLicense::List(attrs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert that `value` resolves to exactly the single nix attr `attr`
    /// through the production resolver. Covers the SPDX→attr map + direct
    /// nix-attr passthrough that `resolve_nix_license_meta` shares.
    fn assert_single_attr(value: &str, attr: &str) {
        assert_eq!(
            resolve_nix_license_meta(value),
            Some(NixLicense::Single(attr.to_string())),
            "`{value}` must resolve to lib.licenses.{attr}"
        );
    }

    #[test]
    fn passthrough_valid_nix_attr_unchanged() {
        // Direct nix attrs (cfgd writes `mit`) must not break.
        assert_single_attr("mit", "mit");
        assert_single_attr("asl20", "asl20");
        assert_single_attr("gpl3Plus", "gpl3Plus");
    }

    #[test]
    fn maps_common_spdx_ids() {
        assert_single_attr("MIT", "mit");
        assert_single_attr("Apache-2.0", "asl20");
        assert_single_attr("BSD-3-Clause", "bsd3");
        assert_single_attr("GPL-3.0-or-later", "gpl3Plus");
        assert_single_attr("MPL-2.0", "mpl20");
        assert_single_attr("ISC", "isc");
    }

    #[test]
    fn spdx_lookup_is_case_insensitive() {
        // SPDX treats identifiers as case-insensitive.
        assert_single_attr("apache-2.0", "asl20");
        assert_single_attr("mit", "mit");
        assert_single_attr("Bsd-3-Clause", "bsd3");
    }

    #[test]
    fn every_mapped_attr_validates() {
        // Invariant: every nix attr in the map must pass validation, else a
        // derived license would map to a value that then fails validation.
        for (spdx, attr) in SPDX_TO_NIX {
            assert!(
                validate_nix_license(attr).is_ok(),
                "mapped attr `{attr}` for SPDX `{spdx}` is not a valid nix license"
            );
        }
    }

    #[test]
    fn map_keys_are_unique_under_case_folding() {
        // SPDX lookup case-folds the needle; two keys that collide when
        // lowercased would make the resolved attr order-dependent. The map
        // must have no such collision.
        let mut seen: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
        for (spdx, attr) in SPDX_TO_NIX {
            let key = spdx.to_ascii_lowercase();
            if let Some(prev) = seen.insert(key.clone(), attr) {
                assert_eq!(
                    prev, *attr,
                    "case-folded key `{key}` maps to two different attrs (`{prev}` vs `{attr}`)"
                );
            }
        }
    }

    #[test]
    fn maps_common_legacy_and_added_spdx_ids() {
        // Regression for the source-of-truth refresh: common ids a Cargo.toml
        // `license` field carries must each resolve to nixpkgs' attribute.
        // `0BSD` and the SPDX-deprecated-but-still-used bare `GPL-*`/`LGPL-*`
        // legacy ids appear in real crates.
        assert_single_attr("0BSD", "bsd0");
        assert_single_attr("GPL-2.0", "gpl2");
        assert_single_attr("GPL-3.0", "gpl3");
        assert_single_attr("LGPL-2.1", "lgpl21");
        assert_single_attr("Apache-1.1", "asl11");
        assert_single_attr("Unicode-3.0", "unicode-30");
    }

    #[test]
    fn distinct_licenses_are_not_aliased_onto_a_neighbor_attr() {
        // These SPDX ids name licenses that are NOT the nixpkgs attr a naive
        // recall-built table reached for, which would silently mislabel them:
        //   MITNFA       != MIT-advertising (mitAdvertising)
        //   Linux-OpenIB != BSD-3-Clause    (bsd3)
        //   XFree86-1.1  != X11             (x11)
        //   Intel        != intel-eula      (the unrelated unfree Intel EULA)
        // nixpkgs has no attr for them, so they must degrade to a verbatim
        // string literal rather than silently alias onto a wrong attr.
        for id in ["MITNFA", "Linux-OpenIB", "XFree86-1.1", "Intel"] {
            assert_eq!(
                resolve_nix_license_meta(id),
                Some(NixLicense::Str(id.to_string())),
                "`{id}` must degrade to a string literal, not alias onto a wrong attr"
            );
        }
        // The legitimately-mapped neighbors still resolve to their attr.
        assert_single_attr("MIT-advertising", "mitAdvertising");
        assert_single_attr("BSD-3-Clause", "bsd3");
        assert_single_attr("X11", "x11");
    }

    #[test]
    fn unknown_id_degrades_to_string_literal() {
        assert_eq!(
            resolve_nix_license_meta("Foo-1.0"),
            Some(NixLicense::Str("Foo-1.0".to_string())),
            "an unknown id must degrade to a verbatim string, never a bogus attr"
        );
    }

    // -----------------------------------------------------------------
    // resolve_nix_license_meta — single attr / list / string fallback.
    // -----------------------------------------------------------------

    #[test]
    fn meta_single_known_spdx_maps_to_attr() {
        assert_eq!(
            resolve_nix_license_meta("MIT"),
            Some(NixLicense::Single("mit".to_string()))
        );
        assert_eq!(
            resolve_nix_license_meta("Apache-2.0"),
            Some(NixLicense::Single("asl20".to_string()))
        );
    }

    #[test]
    fn meta_direct_nix_attr_passes_through() {
        assert_eq!(
            resolve_nix_license_meta("gpl3Plus"),
            Some(NixLicense::Single("gpl3Plus".to_string()))
        );
    }

    #[test]
    fn meta_dual_or_becomes_list_of_attrs() {
        // The canonical Rust dual license — a `lib.licenses` LIST.
        assert_eq!(
            resolve_nix_license_meta("MIT OR Apache-2.0"),
            Some(NixLicense::List(vec![
                "mit".to_string(),
                "asl20".to_string()
            ]))
        );
        // Order preserved; the legacy slash form is equivalent.
        assert_eq!(
            resolve_nix_license_meta("Apache-2.0/MIT"),
            Some(NixLicense::List(vec![
                "asl20".to_string(),
                "mit".to_string()
            ]))
        );
    }

    #[test]
    fn meta_and_expression_also_becomes_list() {
        // Nix `meta.license` has no AND/OR distinction — both render as a list.
        assert_eq!(
            resolve_nix_license_meta("Apache-2.0 AND MIT"),
            Some(NixLicense::List(vec![
                "asl20".to_string(),
                "mit".to_string()
            ]))
        );
    }

    #[test]
    fn meta_unknown_single_id_falls_back_to_string() {
        assert_eq!(
            resolve_nix_license_meta("Foo-1.0"),
            Some(NixLicense::Str("Foo-1.0".to_string()))
        );
    }

    #[test]
    fn meta_compound_with_exception_falls_back_to_string() {
        // A `WITH` exception is kept literal by the parser → string fallback,
        // never a bogus `lib.licenses` attr or a half-resolved list.
        assert_eq!(
            resolve_nix_license_meta("Apache-2.0 WITH LLVM-exception"),
            Some(NixLicense::Str(
                "Apache-2.0 WITH LLVM-exception".to_string()
            ))
        );
    }

    #[test]
    fn meta_list_with_one_unknown_id_falls_back_to_whole_string() {
        // If ANY id in an OR list is unmappable, the WHOLE expression degrades
        // to the verbatim string rather than emit a list with a bogus attr.
        assert_eq!(
            resolve_nix_license_meta("MIT OR Bogus-9.9"),
            Some(NixLicense::Str("MIT OR Bogus-9.9".to_string()))
        );
    }

    #[test]
    fn meta_empty_is_none() {
        assert_eq!(resolve_nix_license_meta(""), None);
        assert_eq!(resolve_nix_license_meta("   "), None);
    }

    #[test]
    fn meta_never_emits_bogus_attr_for_compound_single() {
        // The homebrew-review guard: a compound `Single` must never produce a
        // `List`/`Single` carrying the unparseable literal as an "attr".
        for expr in [
            "Apache-2.0 WITH LLVM-exception",
            "(MIT OR Apache-2.0) AND BSD-3-Clause",
            "Apache-2.0 AND MIT OR BSD-3-Clause",
        ] {
            assert!(
                matches!(resolve_nix_license_meta(expr), Some(NixLicense::Str(_))),
                "`{expr}` must degrade to a string literal, got {:?}",
                resolve_nix_license_meta(expr)
            );
        }
    }
}
