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
//! [`resolve_nix_license`] reconciles both: a value already valid as a
//! nix attribute is preserved verbatim; otherwise a known SPDX id is
//! mapped to its nix attribute; anything else is a hard error.

use anyhow::Result;

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

/// True when a value looks like a compound SPDX expression — a license
/// expression joined by an operator (`OR` / `AND` / `WITH`), a deprecated
/// `+` suffix (`Apache-2.0+`), or a parenthesised group. nixpkgs
/// `lib.licenses` has no single attribute for a compound expression, so
/// these cannot be auto-mapped and require an explicit `nix.license`.
fn is_compound_spdx(value: &str) -> bool {
    let upper = format!(" {} ", value.to_ascii_uppercase());
    upper.contains(" OR ")
        || upper.contains(" AND ")
        || upper.contains(" WITH ")
        || value.contains('(')
        || value.ends_with('+')
}

/// Resolve a license value to a nix `lib.licenses` attribute name.
///
/// Precedence:
/// 1. A value already valid as a nix `lib.licenses` attribute (e.g.
///    `mit`, `asl20`) is returned verbatim — a direct
///    nix-attr config keeps working unchanged.
/// 2. A known single SPDX id (e.g. `MIT`, `Apache-2.0`, case-insensitive)
///    is mapped to its nix attribute.
/// 3. A compound SPDX expression (`MIT OR Apache-2.0`,
///    `Apache-2.0 WITH LLVM-exception`, `Apache-2.0+`) is rejected with a
///    message pointing the user at an explicit `nix.license`, because
///    nixpkgs has no single attribute for a compound expression.
/// 4. Anything else is rejected as neither a known SPDX id nor a nix
///    attribute.
pub fn resolve_nix_license(value: &str) -> Result<String> {
    if validate_nix_license(value).is_ok() {
        return Ok(value.to_string());
    }
    if let Some(attr) = spdx_to_nix_attr(value) {
        return Ok(attr.to_string());
    }
    if is_compound_spdx(value) {
        anyhow::bail!(
            "nix: license '{}' is a compound SPDX expression, which has no \
             single `lib.licenses` attribute. Set `nix.license` explicitly to \
             the nix attribute name (e.g. `asl20`) for the license you want in \
             the derivation's `meta.license`.",
            value
        );
    }
    anyhow::bail!(
        "nix: license '{}' is neither a known SPDX identifier nor a nix \
         `lib.licenses` attribute name. Set `nix.license` to a valid nix \
         attribute (e.g. `mit`, `asl20`, `bsd3`) — see `lib.licenses` in \
         nixpkgs for the full set.",
        value
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_valid_nix_attr_unchanged() {
        // Direct nix attrs (cfgd writes `mit`) must not break.
        assert_eq!(resolve_nix_license("mit").unwrap(), "mit");
        assert_eq!(resolve_nix_license("asl20").unwrap(), "asl20");
        assert_eq!(resolve_nix_license("gpl3Plus").unwrap(), "gpl3Plus");
    }

    #[test]
    fn maps_common_spdx_ids() {
        assert_eq!(resolve_nix_license("MIT").unwrap(), "mit");
        assert_eq!(resolve_nix_license("Apache-2.0").unwrap(), "asl20");
        assert_eq!(resolve_nix_license("BSD-3-Clause").unwrap(), "bsd3");
        assert_eq!(resolve_nix_license("GPL-3.0-or-later").unwrap(), "gpl3Plus");
        assert_eq!(resolve_nix_license("MPL-2.0").unwrap(), "mpl20");
        assert_eq!(resolve_nix_license("ISC").unwrap(), "isc");
    }

    #[test]
    fn spdx_lookup_is_case_insensitive() {
        // SPDX treats identifiers as case-insensitive.
        assert_eq!(resolve_nix_license("apache-2.0").unwrap(), "asl20");
        assert_eq!(resolve_nix_license("mit").unwrap(), "mit");
        assert_eq!(resolve_nix_license("Bsd-3-Clause").unwrap(), "bsd3");
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
        assert_eq!(resolve_nix_license("0BSD").unwrap(), "bsd0");
        assert_eq!(resolve_nix_license("GPL-2.0").unwrap(), "gpl2");
        assert_eq!(resolve_nix_license("GPL-3.0").unwrap(), "gpl3");
        assert_eq!(resolve_nix_license("LGPL-2.1").unwrap(), "lgpl21");
        assert_eq!(resolve_nix_license("Apache-1.1").unwrap(), "asl11");
        assert_eq!(resolve_nix_license("Unicode-3.0").unwrap(), "unicode-30");
    }

    #[test]
    fn distinct_licenses_are_not_aliased_onto_a_neighbor_attr() {
        // These SPDX ids name licenses that are NOT the nixpkgs attr a naive
        // recall-built table reached for, which silently mislabeled them:
        //   MITNFA       != MIT-advertising (mitAdvertising)
        //   Linux-OpenIB != BSD-3-Clause    (bsd3)
        //   XFree86-1.1  != X11             (x11)
        //   Intel        != intel-eula      (the unrelated unfree Intel EULA)
        // nixpkgs has no attr for them, so they must hard-error rather than
        // silently mislabel the release as a different license.
        for id in ["MITNFA", "Linux-OpenIB", "XFree86-1.1", "Intel"] {
            let err = resolve_nix_license(id).unwrap_err().to_string();
            assert!(
                err.contains(id) && err.contains("neither a known SPDX"),
                "`{id}` must hard-error, not alias onto a wrong attr; got: {err}"
            );
        }
        // The legitimately-mapped neighbors still resolve.
        assert_eq!(
            resolve_nix_license("MIT-advertising").unwrap(),
            "mitAdvertising"
        );
        assert_eq!(resolve_nix_license("BSD-3-Clause").unwrap(), "bsd3");
        assert_eq!(resolve_nix_license("X11").unwrap(), "x11");
    }

    #[test]
    fn unknown_id_is_clear_error() {
        let err = resolve_nix_license("Foo-1.0").unwrap_err().to_string();
        assert!(err.contains("Foo-1.0"), "must name the value: {err}");
        assert!(
            err.contains("neither a known SPDX") && err.contains("nix"),
            "must say neither SPDX nor nix attr: {err}"
        );
    }

    #[test]
    fn compound_or_expression_is_rejected_with_hint() {
        let err = resolve_nix_license("MIT OR Apache-2.0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("compound"), "must flag compound: {err}");
        assert!(
            err.contains("nix.license"),
            "must hint explicit nix.license: {err}"
        );
    }

    #[test]
    fn compound_with_and_plus_rejected() {
        assert!(resolve_nix_license("Apache-2.0 WITH LLVM-exception").is_err());
        assert!(resolve_nix_license("GPL-2.0-only AND MIT").is_err());
        assert!(resolve_nix_license("Apache-2.0+").is_err());
        assert!(resolve_nix_license("(MIT OR Apache-2.0)").is_err());
    }
}
