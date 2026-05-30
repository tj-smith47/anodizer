//! Nix derivation expression generation.
//!
//! The Tera template is embedded as a string literal so the generator
//! is self-contained — no template-file lookup at runtime. License
//! identifiers are validated against the canonical `lib.licenses`
//! attrset enumerated by GoReleaser's `internal/pipe/nix/licenses.go`.

use anyhow::{Context as _, Result};

// ---------------------------------------------------------------------------
// Nix expression template
// ---------------------------------------------------------------------------

const NIX_TEMPLATE: &str = r#"{ lib
, stdenvNoCC
, fetchurl
{% if needs_unzip %}, unzip
{% endif %}{% if needs_make_wrapper %}, makeWrapper
{% endif %}{% if dynamically_linked %}, stdenv
, autoPatchelfHook
{% endif %}, installShellFiles
{% for dep in dep_args %}, {{ dep }}
{% endfor %}}:

let
  selectSystem = attrs: attrs.${stdenvNoCC.hostPlatform.system} or (throw "Unsupported system: ${stdenvNoCC.hostPlatform.system}");
  urlMap = {
{% for key, archive in archives %}    {{ key }} = "{{ archive.url }}";
{% endfor %}  };
  shaMap = {
{% for key, archive in archives %}    {{ key }} = "{{ archive.sha }}";
{% endfor %}  };
{% if source_root_map %}  sourceRootMap = {
{% for entry in source_root_map %}    {{ entry.system }} = "{{ entry.root }}";
{% endfor %}  };
{% endif %}in
stdenvNoCC.mkDerivation {
  pname = "{{ name }}";
  version = "{{ version }}";

  src = fetchurl {
    url = selectSystem urlMap;
    sha256 = selectSystem shaMap;
  };

{% if source_root %}  sourceRoot = "{{ source_root }}";
{% elif source_root_map %}  sourceRoot = selectSystem sourceRootMap;
{% endif %}
  nativeBuildInputs = [
    installShellFiles
{% if needs_make_wrapper %}    makeWrapper
{% endif %}{% if needs_unzip %}    unzip
{% endif %}{% for dep in dep_args %}    {{ dep }}
{% endfor %}  ]{% if dynamically_linked %} ++ lib.optionals stdenvNoCC.isLinux [ autoPatchelfHook ]{% endif %};
{% if dynamically_linked %}
  buildInputs = lib.optionals stdenvNoCC.isLinux [
    stdenv.cc.cc.lib
  ];
{% endif %}
  installPhase = ''
{% for line in install_lines %}    {{ line }}
{% endfor %}  '';
{% if has_post_install %}
  postInstall = ''
{% for line in post_install_lines %}    {{ line }}
{% endfor %}  '';
{% endif %}
  meta = {
{% if description %}    description = "{{ description }}";
{% endif %}{% if homepage %}    homepage = "{{ homepage }}";
{% endif %}{% if license %}    license = lib.licenses.{{ license }};
{% endif %}{% if main_program %}    mainProgram = "{{ main_program }}";
{% endif %}    sourceProvenance = with lib.sourceTypes; [ binaryNativeCode ];
    platforms = [ {% for p in platforms %}"{{ p }}" {% endfor %}];
  };
}
"#;

// ---------------------------------------------------------------------------
// NixParams
// ---------------------------------------------------------------------------

/// Per-platform sourceRoot entry for when different archives have different
/// `wrap_in_directory` values.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceRootEntry {
    pub system: String,
    pub root: String,
}

/// Parameters for generating a Nix expression.
pub struct NixParams<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub description: &'a str,
    pub homepage: &'a str,
    pub license: &'a str,
    /// Value for `meta.mainProgram` in the rendered derivation.
    /// Empty string suppresses the attribute.
    pub main_program: &'a str,
    /// Per-platform archives: `(nix_system, url, sha256)`.
    pub archives: &'a [(String, String, String)],
    /// Install commands. If empty, auto-generates `cp` for each binary.
    pub install_lines: &'a [String],
    /// Post-install commands.
    pub post_install_lines: &'a [String],
    /// Whether any archive is a .zip (need unzip dep).
    pub needs_unzip: bool,
    /// Whether dependencies are configured (need makeWrapper).
    pub needs_make_wrapper: bool,
    /// Dependency package names to add as function arguments in the derivation.
    pub dep_args: &'a [String],
    /// Value for `sourceRoot` in the derivation. `None` when per-platform
    /// sourceRoots differ — use `source_root_map` instead.
    pub source_root: Option<&'a str>,
    /// Per-platform sourceRoot map, used when different archives have different
    /// `wrap_in_directory` values.
    pub source_root_map: Option<&'a [SourceRootEntry]>,
    /// Whether any binary in the release is dynamically linked (ELF with PT_INTERP).
    pub dynamically_linked: bool,
}

// ---------------------------------------------------------------------------
// generate_nix_expression
// ---------------------------------------------------------------------------

/// Generate a Nix derivation expression string.
pub fn generate_nix_expression(params: &NixParams<'_>) -> Result<String> {
    let tera = anodizer_core::template::parse_static("nix", NIX_TEMPLATE)
        .context("nix: parse template")?;

    let mut ctx = tera::Context::new();
    ctx.insert("name", params.name);
    ctx.insert("version", params.version);
    // `description` and `homepage` are free-text user input rendered
    // directly inside Nix string literals (`description = "{{ description }}";`,
    // `homepage = "{{ homepage }}";`). A value containing `"`, `\`, or
    // `${` would either break the literal or trigger antiquotation, so
    // escape both before insertion — same rationale as `main_program`
    // below. `license` is validated against the `lib.licenses` allow-list
    // upstream, so it needs no escaping.
    ctx.insert("description", &nix_escape_string(params.description));
    ctx.insert("homepage", &nix_escape_string(params.homepage));
    ctx.insert("license", params.license);
    // `main_program` is interpolated directly inside `"..."` in the rendered
    // Nix derivation (`meta.mainProgram = "{{ main_program }}";`). Nix string
    // literals interpret `\`, `"`, and `${...}` specially, so a value
    // containing any of those would either escape the literal (yielding
    // malformed Nix) or trigger antiquotation. Apply the Nix string-escape
    // rules before insertion. GoReleaser does not escape at
    // `internal/pipe/nix/tmpl.nix:135`; anodize escapes for robustness so
    // legitimate user-input main_program values (e.g. containing apostrophes
    // turned into curly-quote analogs, or interpolation-like substrings)
    // render as a valid Nix string rather than failing at `nix-build`.
    let main_program_escaped = nix_escape_string(params.main_program);
    ctx.insert("main_program", &main_program_escaped);
    if let Some(sr) = params.source_root {
        ctx.insert("source_root", sr);
    }
    if let Some(srm) = params.source_root_map {
        ctx.insert("source_root_map", srm);
    }
    ctx.insert("needs_unzip", &params.needs_unzip);
    ctx.insert("needs_make_wrapper", &params.needs_make_wrapper);
    ctx.insert("dynamically_linked", &params.dynamically_linked);
    ctx.insert("dep_args", &params.dep_args);

    // Archives map
    #[derive(serde::Serialize)]
    struct ArchiveEntry {
        url: String,
        sha: String,
    }
    let archives: std::collections::BTreeMap<String, ArchiveEntry> = params
        .archives
        .iter()
        .map(|(system, url, sha)| {
            (
                system.clone(),
                ArchiveEntry {
                    url: url.clone(),
                    sha: sha.clone(),
                },
            )
        })
        .collect();
    ctx.insert("archives", &archives);

    // Platforms list
    let platforms: Vec<&str> = params.archives.iter().map(|(s, _, _)| s.as_str()).collect();
    ctx.insert("platforms", &platforms);

    ctx.insert("install_lines", &params.install_lines);
    ctx.insert("has_post_install", &!params.post_install_lines.is_empty());
    ctx.insert("post_install_lines", &params.post_install_lines);

    anodizer_core::template::render_static(&tera, "nix", &ctx, "nix")
}

// ---------------------------------------------------------------------------
// Nix string escaping
// ---------------------------------------------------------------------------

/// Escape a value for inclusion inside a double-quoted Nix string literal.
///
/// Nix string-literal grammar (per
/// `https://nixos.org/manual/nix/stable/language/values#type-string`):
/// - `\\` escapes a literal backslash.
/// - `\"` escapes a literal double quote.
/// - `\${` escapes a literal dollar-brace so it is NOT interpreted as the
///   start of an antiquotation (string interpolation).
///
/// Apply replacements in this order: backslash first (so the backslashes
/// introduced for `"` and `${` are not themselves re-escaped), then quote,
/// then `${`.
pub(super) fn nix_escape_string(s: &str) -> String {
    let mut out = s.replace('\\', "\\\\");
    out = out.replace('"', "\\\"");
    out = out.replace("${", "\\${");
    out
}

// ---------------------------------------------------------------------------
// License validation
// ---------------------------------------------------------------------------

/// Known valid Nix license identifiers from `lib.licenses`.
/// Sourced from GoReleaser's internal/pipe/nix/licenses.go.
const VALID_NIX_LICENSES: &[&str] = &[
    "abstyles",
    "acsl14",
    "activision",
    "adobeDisplayPostScript",
    "adobeUtopia",
    "afl20",
    "afl21",
    "afl3",
    "agpl3Only",
    "agpl3Plus",
    "aladdin",
    "amazonsl",
    "amd",
    "aml",
    "ampas",
    "aom",
    "apple-psl10",
    "apple-psl20",
    "apsl10",
    "apsl20",
    "arphicpl",
    "artistic1",
    "artistic1-cl8",
    "artistic2",
    "asl11",
    "asl20",
    "baekmuk",
    "beerware",
    "bitstreamCharter",
    "bitstreamVera",
    "bitTorrent10",
    "bitTorrent11",
    "blueOak100",
    "boehmGC",
    "bola11",
    "boost",
    "bsd0",
    "bsd1",
    "bsd2",
    "bsd2Patent",
    "bsd2WithViews",
    "bsd3",
    "bsd3Clear",
    "bsd3ClauseTso",
    "bsd3Lbnl",
    "bsdAxisNoDisclaimerUnmodified",
    "bsdOriginal",
    "bsdOriginalShortened",
    "bsdOriginalUC",
    "bsdProtection",
    "bsdSourceCode",
    "bsl11",
    "bzip2",
    "cal10",
    "caldera",
    "capec",
    "cc-by-10",
    "cc-by-20",
    "cc-by-30",
    "cc-by-40",
    "cc-by-nc-30",
    "cc-by-nc-40",
    "cc-by-nc-nd-30",
    "cc-by-nc-nd-40",
    "cc-by-nc-sa-20",
    "cc-by-nc-sa-25",
    "cc-by-nc-sa-30",
    "cc-by-nc-sa-40",
    "cc-by-nd-30",
    "cc-by-nd-40",
    "cc-by-sa-10",
    "cc-by-sa-20",
    "cc-by-sa-25",
    "cc-by-sa-30",
    "cc-by-sa-40",
    "cc-sa-10",
    "cc0",
    "cddl",
    "cecill-b",
    "cecill-c",
    "cecill20",
    "cecill21",
    "clArtistic",
    "classpathException20",
    "cnri-python",
    "cockroachdb-community-license",
    "commons-clause",
    "cpal10",
    "cpl10",
    "cronyx",
    "curl",
    "databricks",
    "databricks-dbx",
    "databricks-license",
    "dec3Clause",
    "doc",
    "drl10",
    "dtoa",
    "eapl",
    "ecl20",
    "efl10",
    "efl20",
    "elastic20",
    "epl10",
    "epl20",
    "epson",
    "eupl11",
    "eupl12",
    "fair",
    "fairsource09",
    "fdl11Only",
    "fdl11Plus",
    "fdl12Only",
    "fdl12Plus",
    "fdl13Only",
    "fdl13Plus",
    "ffsl",
    "fontException",
    "fraunhofer-fdk",
    "free",
    "fsl11Asl20",
    "fsl11Mit",
    "ftl",
    "g4sl",
    "generaluser",
    "geogebra",
    "gfl",
    "gfsl",
    "giftware",
    "gnuplot",
    "gpl1Only",
    "gpl1Plus",
    "gpl2",
    "gpl2Only",
    "gpl2Plus",
    "gpl3",
    "gpl3Only",
    "gpl3Plus",
    "hl3",
    "hpnd",
    "hpndDec",
    "hpndDifferentDisclaimer",
    "hpndDoc",
    "hpndDocSell",
    "hpndSellVariant",
    "hpndSellVariantMitDisclaimerXserver",
    "hpndSellVariantSafetyClause",
    "hpndUc",
    "hyphenBulgarian",
    "iasl",
    "icu",
    "ijg",
    "imagemagick",
    "imlib2",
    "info-zip",
    "inria-compcert",
    "inria-icesl",
    "inria-zelus",
    "intel-eula",
    "interbase",
    "ipa",
    "ipl10",
    "isc",
    "issl",
    "knuth",
    "lal12",
    "lal13",
    "lens",
    "lgpl2",
    "lgpl2Only",
    "lgpl2Plus",
    "lgpl21",
    "lgpl21Only",
    "lgpl21Plus",
    "lgpl3",
    "lgpl3Only",
    "lgpl3Plus",
    "lgpllr",
    "libpng",
    "libpng2",
    "libtiff",
    "llgpl21",
    "llvm-exception",
    "lpl-102",
    "lppl1",
    "lppl12",
    "lppl13a",
    "lppl13c",
    "lsof",
    "miros",
    "mit",
    "mit-cmu",
    "mit-enna",
    "mit-feh",
    "mit-modern",
    "mit0",
    "mitAdvertising",
    "mitOpenGroup",
    "mpl10",
    "mpl11",
    "mpl20",
    "mplus",
    "mspl",
    "mulan-psl2",
    "naist-2003",
    "nasa13",
    "ncbiPd",
    "ncsa",
    "ncul1",
    "ngpl",
    "nistSoftware",
    "nlpl",
    "nposl3",
    "ntp",
    "nvidiaCuda",
    "nvidiaCudaRedist",
    "obsidian",
    "ocamlLgplLinkingException",
    "ocamlpro_nc",
    "odbl",
    "ofl",
    "oml",
    "openldap",
    "openssl",
    "opubl",
    "osl2",
    "osl21",
    "osl3",
    "paratype",
    "parity70",
    "php301",
    "postgresql",
    "postman",
    "prosperity30",
    "psfl",
    "publicDomain",
    "qhull",
    "qpl",
    "qwtException",
    "ruby",
    "sendmail",
    "sfl",
    "sgi-b-20",
    "sgmlug",
    "sissl11",
    "sleepycat",
    "smail",
    "smlnj",
    "sspl",
    "stk",
    "sudo",
    "sustainableUse",
    "tcltk",
    "tcpWrappers",
    "teamspeak",
    "tekHvcLicense",
    "torque11",
    "tost",
    "tsl",
    "ubdlException",
    "ucd",
    "ufl",
    "unfree",
    "unfreeRedistributable",
    "unfreeRedistributableFirmware",
    "unicode-30",
    "unicode-dfs-2015",
    "unicode-dfs-2016",
    "unicodeTOU",
    "unlicense",
    "upl",
    "vim",
    "virtualbox-puel",
    "vol-sl",
    "vsl10",
    "w3c",
    "wadalab",
    "watcom",
    "wtfpl",
    "wxWindowsException31",
    "x11",
    "x11BsdClause",
    "x11NoPermitPersons",
    "xerox",
    "xfig",
    "xinetd",
    "xskat",
    "zlib",
    "zpl20",
    "zpl21",
];

/// Validate that a license identifier is a known Nix license.
/// Returns `Ok(())` if valid, or `Err` with a descriptive message.
pub fn validate_nix_license(license: &str) -> Result<()> {
    if VALID_NIX_LICENSES.contains(&license) {
        Ok(())
    } else {
        anyhow::bail!(
            "nix: unknown license identifier '{}'. Valid values: {}",
            license,
            VALID_NIX_LICENSES.join(", ")
        )
    }
}

// ---------------------------------------------------------------------------
// Nix system mapping
// ---------------------------------------------------------------------------

/// Map canonical (os, arch) to Nix system string.
pub(crate) fn nix_system(os: &str, arch: &str) -> Option<String> {
    let nix_arch = match arch {
        "amd64" | "x86_64" => "x86_64",
        "arm64" | "aarch64" => "aarch64",
        "386" | "i686" => "i686",
        "arm" | "armv7l" => "armv7l",
        _ => return None,
    };
    let nix_os = match os {
        "linux" => "linux",
        "darwin" | "macos" => "darwin",
        _ => return None,
    };
    Some(format!("{}-{}", nix_arch, nix_os))
}
