//! Nix `lib.licenses` license resolution.
//!
//! A nix derivation's `meta.license` must reference a `lib.licenses`
//! attribute *name* (lowercase camelCase like `mit`, `asl20`, `bsd3`) —
//! NOT an SPDX identifier. Two value shapes reach this resolver:
//!
//! - A value a user wrote directly in `nix.license` — by convention the
//!   nix attribute name (GoReleaser only ever accepts this form), e.g.
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
/// Source of truth for a refresh: nixpkgs `lib/licenses.nix` — each
/// license attribute carries an `spdxId` field; this table is the
/// inverse (`spdxId` -> attribute name) for every entry whose attribute
/// is also present in the [`super::generate`] `VALID_NIX_LICENSES`
/// allow-list (so a mapped value always passes validation). SPDX ids are
/// matched case-insensitively (SPDX treats them as case-insensitive),
/// so keys here are lowercased at lookup time.
///
/// Entries are sorted by SPDX id for readability; lookup is linear over a
/// small table and case-folds the needle.
static SPDX_TO_NIX: &[(&str, &str)] = &[
    ("AFL-2.0", "afl20"),
    ("AFL-2.1", "afl21"),
    ("AFL-3.0", "afl3"),
    ("AGPL-3.0-only", "agpl3Only"),
    ("AGPL-3.0-or-later", "agpl3Plus"),
    ("AMD", "amd"),
    ("AML", "aml"),
    ("AOM", "aom"),
    ("APSL-1.0", "apsl10"),
    ("APSL-2.0", "apsl20"),
    ("Apache-2.0", "asl20"),
    ("Artistic-1.0", "artistic1"),
    ("Artistic-1.0-cl8", "artistic1-cl8"),
    ("Artistic-2.0", "artistic2"),
    ("BSD-1-Clause", "bsd1"),
    ("BSD-2-Clause", "bsd2"),
    ("BSD-2-Clause-Patent", "bsd2Patent"),
    ("BSD-2-Clause-Views", "bsd2WithViews"),
    ("BSD-3-Clause", "bsd3"),
    ("BSD-3-Clause-Clear", "bsd3Clear"),
    ("BSD-3-Clause-LBNL", "bsd3Lbnl"),
    ("BSD-4-Clause", "bsdOriginal"),
    ("BSD-4-Clause-Shortened", "bsdOriginalShortened"),
    ("BSD-4-Clause-UC", "bsdOriginalUC"),
    ("BSD-Protection", "bsdProtection"),
    ("BSD-Source-Code", "bsdSourceCode"),
    ("BSL-1.0", "boost"),
    ("BUSL-1.1", "bsl11"),
    ("Beerware", "beerware"),
    ("BlueOak-1.0.0", "blueOak100"),
    ("Bitstream-Vera", "bitstreamVera"),
    ("CAL-1.0", "cal10"),
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
    ("CC0-1.0", "cc0"),
    ("CDDL-1.0", "cddl"),
    ("CECILL-2.0", "cecill20"),
    ("CECILL-2.1", "cecill21"),
    ("CECILL-B", "cecill-b"),
    ("CECILL-C", "cecill-c"),
    ("CPAL-1.0", "cpal10"),
    ("CPL-1.0", "cpl10"),
    ("ClArtistic", "clArtistic"),
    ("Classpath-exception-2.0", "classpathException20"),
    ("Curl", "curl"),
    ("ECL-2.0", "ecl20"),
    ("EFL-1.0", "efl10"),
    ("EFL-2.0", "efl20"),
    ("EPL-1.0", "epl10"),
    ("EPL-2.0", "epl20"),
    ("EUPL-1.1", "eupl11"),
    ("EUPL-1.2", "eupl12"),
    ("Elastic-2.0", "elastic20"),
    ("FTL", "ftl"),
    ("Fair", "fair"),
    ("GFDL-1.1-only", "fdl11Only"),
    ("GFDL-1.1-or-later", "fdl11Plus"),
    ("GFDL-1.2-only", "fdl12Only"),
    ("GFDL-1.2-or-later", "fdl12Plus"),
    ("GFDL-1.3-only", "fdl13Only"),
    ("GFDL-1.3-or-later", "fdl13Plus"),
    ("GPL-1.0-only", "gpl1Only"),
    ("GPL-1.0-or-later", "gpl1Plus"),
    ("GPL-2.0-only", "gpl2Only"),
    ("GPL-2.0-or-later", "gpl2Plus"),
    ("GPL-3.0-only", "gpl3Only"),
    ("GPL-3.0-or-later", "gpl3Plus"),
    ("Giftware", "giftware"),
    ("Gnuplot", "gnuplot"),
    ("HPND", "hpnd"),
    ("HPND-sell-variant", "hpndSellVariant"),
    ("ICU", "icu"),
    ("IJG", "ijg"),
    ("IPA", "ipa"),
    ("IPL-1.0", "ipl10"),
    ("ISC", "isc"),
    ("ImageMagick", "imagemagick"),
    ("Imlib2", "imlib2"),
    ("Info-ZIP", "info-zip"),
    ("Intel", "intel-eula"),
    ("Interbase-1.0", "interbase"),
    ("Knuth-CTAN", "knuth"),
    ("LAL-1.2", "lal12"),
    ("LAL-1.3", "lal13"),
    ("LGPL-2.0-only", "lgpl2Only"),
    ("LGPL-2.0-or-later", "lgpl2Plus"),
    ("LGPL-2.1-only", "lgpl21Only"),
    ("LGPL-2.1-or-later", "lgpl21Plus"),
    ("LGPL-3.0-only", "lgpl3Only"),
    ("LGPL-3.0-or-later", "lgpl3Plus"),
    ("LLGPL", "llgpl21"),
    ("LLVM-exception", "llvm-exception"),
    ("LPL-1.02", "lpl-102"),
    ("LPPL-1.0", "lppl1"),
    ("LPPL-1.2", "lppl12"),
    ("LPPL-1.3a", "lppl13a"),
    ("LPPL-1.3c", "lppl13c"),
    ("Libpng", "libpng"),
    ("Linux-OpenIB", "bsd3"),
    ("MIT", "mit"),
    ("MIT-0", "mit0"),
    ("MIT-CMU", "mit-cmu"),
    ("MIT-advertising", "mitAdvertising"),
    ("MIT-enna", "mit-enna"),
    ("MIT-feh", "mit-feh"),
    ("MITNFA", "mitAdvertising"),
    ("MPL-1.0", "mpl10"),
    ("MPL-1.1", "mpl11"),
    ("MPL-2.0", "mpl20"),
    ("MS-PL", "mspl"),
    ("MulanPSL-2.0", "mulan-psl2"),
    ("NASA-1.3", "nasa13"),
    ("NCSA", "ncsa"),
    ("NLPL", "nlpl"),
    ("NPOSL-3.0", "nposl3"),
    ("NTP", "ntp"),
    ("OCaml-LGPL-linking-exception", "ocamlLgplLinkingException"),
    ("ODbL-1.0", "odbl"),
    ("OFL-1.1", "ofl"),
    ("OLDAP-2.8", "openldap"),
    ("OML", "oml"),
    ("OpenSSL", "openssl"),
    ("OSL-2.0", "osl2"),
    ("OSL-2.1", "osl21"),
    ("OSL-3.0", "osl3"),
    ("PHP-3.01", "php301"),
    ("PostgreSQL", "postgresql"),
    ("Python-2.0", "psfl"),
    ("QPL-1.0", "qpl"),
    ("Qhull", "qhull"),
    ("Ruby", "ruby"),
    ("SGI-B-2.0", "sgi-b-20"),
    ("SISSL", "sissl11"),
    ("SMLNJ", "smlnj"),
    ("SSPL-1.0", "sspl"),
    ("Sendmail", "sendmail"),
    ("Sleepycat", "sleepycat"),
    ("TCL", "tcltk"),
    ("UPL-1.0", "upl"),
    ("Unicode-DFS-2015", "unicode-dfs-2015"),
    ("Unicode-DFS-2016", "unicode-dfs-2016"),
    ("Unlicense", "unlicense"),
    ("VSL-1.0", "vsl10"),
    ("Vim", "vim"),
    ("W3C", "w3c"),
    ("WTFPL", "wtfpl"),
    ("Watcom-1.0", "watcom"),
    ("X11", "x11"),
    ("XFree86-1.1", "x11"),
    ("Xerox", "xerox"),
    ("Zlib", "zlib"),
    ("ZPL-2.0", "zpl20"),
    ("ZPL-2.1", "zpl21"),
    ("xinetd", "xinetd"),
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
///    `mit`, `asl20`) is returned verbatim — GoReleaser-style direct
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
        // GoReleaser-style direct nix attrs (cfgd writes `mit`) must not break.
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
