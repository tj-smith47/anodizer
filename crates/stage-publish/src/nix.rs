use anodize_core::context::Context;
use anodize_core::log::StageLogger;
use anyhow::{Context as _, Result};
use base64::Engine as _;

use crate::util;

// ---------------------------------------------------------------------------
// SRI hash conversion
// ---------------------------------------------------------------------------

/// Convert a hex-encoded SHA256 hash to SRI format (`sha256-{base64}`).
///
/// Nix's `fetchurl` expects SRI hashes, not raw hex.  GoReleaser converts
/// hashes using `nix-hash --type sha256 --flat --base32`; the modern
/// equivalent is the SRI format that Nix also accepts.
pub fn hex_sha256_to_sri(hex: &str) -> Result<String> {
    let bytes = hex_to_bytes(hex)?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "nix: expected 32 bytes for SHA256 hash, got {} (hex: '{}')",
            bytes.len(),
            hex
        );
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("sha256-{}", b64))
}

/// Decode a hex string into raw bytes.
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        anyhow::bail!("nix: hex string has odd length: '{}'", hex);
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| anyhow::anyhow!("nix: invalid hex at offset {}: {}", i, e))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Nix expression template
// ---------------------------------------------------------------------------

const NIX_TEMPLATE: &str = r#"{ lib
, stdenvNoCC
, fetchurl
{% if needs_unzip %}, unzip
{% endif %}{% if needs_make_wrapper %}, makeWrapper
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
in
stdenvNoCC.mkDerivation {
  pname = "{{ name }}";
  version = "{{ version }}";

  src = fetchurl {
    url = selectSystem urlMap;
    sha256 = selectSystem shaMap;
  };

  sourceRoot = "{{ source_root }}";

  nativeBuildInputs = [
    installShellFiles
{% if needs_make_wrapper %}    makeWrapper
{% endif %}{% if needs_unzip %}    unzip
{% endif %}{% for dep in dep_args %}    {{ dep }}
{% endfor %}  ];

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
{% endif %}    sourceProvenance = with lib.sourceTypes; [ binaryNativeCode ];
    platforms = [ {% for p in platforms %}"{{ p }}" {% endfor %}];
  };
}
"#;

// ---------------------------------------------------------------------------
// NixParams
// ---------------------------------------------------------------------------

/// Parameters for generating a Nix expression.
pub struct NixParams<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub description: &'a str,
    pub homepage: &'a str,
    pub license: &'a str,
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
    /// Value for `sourceRoot` in the derivation. Defaults to `"."`.
    pub source_root: &'a str,
}

// ---------------------------------------------------------------------------
// generate_nix_expression
// ---------------------------------------------------------------------------

/// Generate a Nix derivation expression string.
pub fn generate_nix_expression(params: &NixParams<'_>) -> String {
    let mut tera = tera::Tera::default();
    tera.add_raw_template("nix", NIX_TEMPLATE)
        .expect("nix: parse template");
    tera.autoescape_on(vec![]);

    let mut ctx = tera::Context::new();
    ctx.insert("name", params.name);
    ctx.insert("version", params.version);
    ctx.insert("description", params.description);
    ctx.insert("homepage", params.homepage);
    ctx.insert("license", params.license);
    ctx.insert("source_root", params.source_root);
    ctx.insert("needs_unzip", &params.needs_unzip);
    ctx.insert("needs_make_wrapper", &params.needs_make_wrapper);
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

    tera.render("nix", &ctx).expect("nix: render expression")
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
fn nix_system(os: &str, arch: &str) -> Option<String> {
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

// ---------------------------------------------------------------------------
// publish_to_nix
// ---------------------------------------------------------------------------

pub fn publish_to_nix(ctx: &Context, crate_name: &str, log: &StageLogger) -> Result<()> {
    let (_crate_cfg, publish) = crate::util::get_publish_config(ctx, crate_name, "nix")?;

    let nix_cfg = publish
        .nix
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("nix: no nix config for '{}'", crate_name))?;

    // Check skip_upload before doing any work.
    if crate::homebrew::should_skip_upload(nix_cfg.skip_upload.as_ref(), ctx) {
        log.status(&format!(
            "nix: skipping upload for '{}' (skip_upload={})",
            crate_name,
            nix_cfg.skip_upload.as_ref().map(|v| v.as_str()).unwrap_or("")
        ));
        return Ok(());
    }

    // Resolve repository config.
    // GoReleaser applies TemplateRef() to repository fields (nix.go:159-162).
    let (repo_owner_raw, repo_name_raw) =
        crate::util::resolve_repo_owner_name(nix_cfg.repository.as_ref(), None, None)
            .ok_or_else(|| anyhow::anyhow!("nix: no repository config for '{}'", crate_name))?;
    let repo_owner = ctx.render_template(&repo_owner_raw).unwrap_or(repo_owner_raw);
    let repo_name = ctx.render_template(&repo_name_raw).unwrap_or(repo_name_raw);

    let name_raw = nix_cfg.name.as_deref().unwrap_or(crate_name);
    let name_rendered = ctx
        .render_template(name_raw)
        .unwrap_or_else(|_| name_raw.to_string());
    let name = name_rendered.as_str();

    if ctx.is_dry_run() {
        log.status(&format!(
            "(dry-run) would publish Nix expression for '{}' to {}/{}",
            crate_name, repo_owner, repo_name
        ));
        return Ok(());
    }

    let version = ctx.version();
    let description_raw = nix_cfg.description.as_deref().unwrap_or("");
    let description_rendered = ctx
        .render_template(description_raw)
        .unwrap_or_else(|_| description_raw.to_string());
    let description = description_rendered.as_str();
    let homepage_raw = nix_cfg.homepage.as_deref().unwrap_or("");
    let homepage_rendered = ctx
        .render_template(homepage_raw)
        .unwrap_or_else(|_| homepage_raw.to_string());
    let homepage = homepage_rendered.as_str();
    let license = nix_cfg.license.as_deref().unwrap_or("");

    // Validate license identifier against known Nix licenses (skip if empty).
    if !license.is_empty() {
        validate_nix_license(license)?;
    }

    // Find artifacts for Linux and Darwin platforms, applying IDs + goamd64 filter.
    let ids_filter = nix_cfg.ids.as_deref();
    let goamd64 = nix_cfg.goamd64.as_deref().or(Some("v1"));
    let all_artifacts = util::find_all_platform_artifacts_with_goarch(
        ctx, crate_name, ids_filter, goamd64, None,
    );

    let url_template = nix_cfg.url_template.as_deref();

    let archives: Vec<(String, String, String)> = all_artifacts
        .iter()
        .filter_map(|a| {
            let system = nix_system(&a.os, &a.arch)?;
            let download_url = if let Some(tmpl) = url_template {
                util::render_url_template(tmpl, crate_name, &version, &a.arch, &a.os)
            } else {
                a.url.clone()
            };
            // Convert hex SHA256 to SRI format for Nix's fetchurl.
            let sri_hash = if a.sha256.is_empty() {
                a.sha256.clone()
            } else {
                match hex_sha256_to_sri(&a.sha256) {
                    Ok(sri) => sri,
                    Err(e) => {
                        log.warn(&format!(
                            "nix: failed to convert SHA256 to SRI for {}: {}; using raw hex",
                            a.url, e
                        ));
                        a.sha256.clone()
                    }
                }
            };
            Some((system, download_url, sri_hash))
        })
        .collect();

    if archives.is_empty() {
        anyhow::bail!(
            "nix: no Linux/Darwin archive artifacts found for '{}'",
            crate_name
        );
    }

    // Check if any archive is a zip (needs unzip dep)
    let needs_unzip = all_artifacts.iter().any(|a| a.url.ends_with(".zip"));

    // Check if dependencies are configured (needs makeWrapper)
    let deps = nix_cfg.dependencies.as_deref().unwrap_or(&[]);
    let needs_make_wrapper = !deps.is_empty();

    // Collect unique dependency package names for the derivation function arguments.
    let dep_args: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        deps.iter()
            .filter(|d| seen.insert(d.name.clone()))
            .map(|d| d.name.clone())
            .collect()
    };

    // Build install lines
    let install_lines: Vec<String> = if let Some(ref custom_install) = nix_cfg.install {
        let mut lines: Vec<String> = custom_install.lines().map(|l| l.to_string()).collect();
        if let Some(ref extra) = nix_cfg.extra_install {
            lines.extend(extra.lines().map(|l| l.to_string()));
        }
        lines
    } else {
        let mut lines = vec!["mkdir -p $out/bin".to_string()];
        lines.push(format!("cp -vr ./{name} $out/bin/{name}"));
        if let Some(ref extra) = nix_cfg.extra_install {
            lines.extend(extra.lines().map(|l| l.to_string()));
        }
        // Generate wrapProgram invocations from dependencies with OS filtering.
        if needs_make_wrapper {
            // Partition deps by OS for conditional wrapping.
            let all_os_deps: Vec<&str> = deps
                .iter()
                .filter(|d| d.os.is_none())
                .map(|d| d.name.as_str())
                .collect();
            let darwin_deps: Vec<&str> = deps
                .iter()
                .filter(|d| d.os.as_deref() == Some("darwin"))
                .map(|d| d.name.as_str())
                .collect();
            let linux_deps: Vec<&str> = deps
                .iter()
                .filter(|d| d.os.as_deref() == Some("linux"))
                .map(|d| d.name.as_str())
                .collect();

            // Build lib.makeBinPath argument list with optional platform guards.
            let mut list_parts: Vec<String> = Vec::new();
            if !darwin_deps.is_empty() {
                let items = darwin_deps
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                list_parts.push(format!("lib.optionals stdenvNoCC.isDarwin [ {items} ]"));
            }
            if !linux_deps.is_empty() {
                let items = linux_deps
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                list_parts.push(format!("lib.optionals stdenvNoCC.isLinux [ {items} ]"));
            }
            if !all_os_deps.is_empty() {
                let items = all_os_deps
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                list_parts.push(format!("[ {items} ]"));
            }

            if !list_parts.is_empty() {
                let joined = list_parts.join(" ++\n      ");
                lines.push(format!(
                    "wrapProgram $out/bin/{name} --prefix PATH : ${{lib.makeBinPath (\n      {joined}\n    )}}"
                ));
            }
        }
        lines
    };

    let post_install_lines: Vec<String> = nix_cfg
        .post_install
        .as_ref()
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default();

    // Determine sourceRoot from the archive config's wrap_in_directory setting.
    // When an archive wraps contents in a directory, Nix needs to know the
    // extraction root.  We use a placeholder default name since the exact
    // archive stem is not available here; the template in wrap_in_directory
    // is typically a string like "myapp-1.0.0".
    let source_root = {
        let wrap_dir = match &_crate_cfg.archives {
            anodize_core::config::ArchivesConfig::Configs(cfgs) => cfgs.first().and_then(|c| {
                c.wrap_in_directory
                    .as_ref()
                    .and_then(|w| w.directory_name(&format!("{}-{}", name, version)))
            }),
            anodize_core::config::ArchivesConfig::Disabled => None,
        };
        wrap_dir.unwrap_or_else(|| ".".to_string())
    };

    let nix_expr = generate_nix_expression(&NixParams {
        name,
        version: &version,
        description,
        homepage,
        license,
        archives: &archives,
        install_lines: &install_lines,
        post_install_lines: &post_install_lines,
        needs_unzip,
        needs_make_wrapper,
        dep_args: &dep_args,
        source_root: &source_root,
    });

    // Optionally format with alejandra or nixfmt
    // (only if the formatter binary is available)

    // Clone repo (SSH-aware), write nix expression, commit, push.
    let token = util::resolve_repo_token(ctx, nix_cfg.repository.as_ref(), None);

    let tmp_dir = tempfile::tempdir().context("nix: create temp dir")?;
    let repo_path = tmp_dir.path();
    util::clone_repo(
        nix_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        token.as_deref(),
        repo_path,
        "nix",
        log,
    )?;

    // Write nix file at configured path or default
    let nix_path = nix_cfg
        .path
        .as_deref()
        .map(|p| p.to_string())
        .unwrap_or_else(|| format!("pkgs/{}/default.nix", name));
    let nix_file = repo_path.join(&nix_path);

    if let Some(parent) = nix_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("nix: create dir {}", parent.display()))?;
    }

    std::fs::write(&nix_file, &nix_expr)
        .with_context(|| format!("nix: write {}", nix_file.display()))?;

    // Run formatter if configured
    if let Some(ref formatter) = nix_cfg.formatter {
        let nix_file_str = nix_file.to_string_lossy();
        match formatter.as_str() {
            "alejandra" | "nixfmt" => {
                if let Ok(output) = std::process::Command::new(formatter)
                    .arg(&*nix_file_str)
                    .output()
                {
                    if !output.status.success() {
                        log.warn(&format!("nix: {} formatting failed", formatter));
                    }
                } else {
                    log.warn(&format!(
                        "nix: {} not available, skipping format",
                        formatter
                    ));
                }
            }
            _ => {
                log.warn(&format!("nix: unknown formatter '{}', skipping", formatter));
            }
        }
    }

    log.status(&format!("wrote Nix expression: {}", nix_file.display()));

    let commit_msg = crate::homebrew::render_commit_msg(
        nix_cfg.commit_msg_template.as_deref(),
        name,
        &version,
        "package",
    );
    let commit_opts = util::resolve_commit_opts(nix_cfg.commit_author.as_ref(), None, None);
    let branch = util::resolve_branch(nix_cfg.repository.as_ref());
    util::commit_and_push_with_opts(
        repo_path,
        &[&nix_path],
        &commit_msg,
        branch,
        "nix",
        &commit_opts,
    )?;

    // Submit PR if configured.
    util::maybe_submit_pr(
        repo_path,
        nix_cfg.repository.as_ref(),
        &repo_owner,
        &repo_name,
        branch.unwrap_or("main"),
        &format!("Update {} to {}", name, version),
        &format!(
            "## Package\n- **Name**: {}\n- **Version**: {}\n\nAutomatically submitted by anodize.",
            name, version
        ),
        "nix",
        log,
    );

    log.status(&format!(
        "Nix expression pushed to {}/{} for '{}'",
        repo_owner, repo_name, crate_name
    ));

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nix_system_mapping() {
        assert_eq!(
            nix_system("linux", "amd64"),
            Some("x86_64-linux".to_string())
        );
        assert_eq!(
            nix_system("linux", "arm64"),
            Some("aarch64-linux".to_string())
        );
        assert_eq!(
            nix_system("darwin", "amd64"),
            Some("x86_64-darwin".to_string())
        );
        assert_eq!(
            nix_system("darwin", "arm64"),
            Some("aarch64-darwin".to_string())
        );
        assert_eq!(nix_system("linux", "386"), Some("i686-linux".to_string()));
        assert_eq!(nix_system("windows", "amd64"), None);
    }

    #[test]
    fn test_generate_nix_expression_basic() {
        let archives = vec![
            (
                "x86_64-linux".to_string(),
                "https://example.com/tool-linux-amd64.tar.gz".to_string(),
                "abc123".to_string(),
            ),
            (
                "aarch64-darwin".to_string(),
                "https://example.com/tool-darwin-arm64.tar.gz".to_string(),
                "def456".to_string(),
            ),
        ];
        let install_lines = vec![
            "mkdir -p $out/bin".to_string(),
            "cp -vr ./mytool $out/bin/mytool".to_string(),
        ];

        let expr = generate_nix_expression(&NixParams {
            name: "mytool",
            version: "1.0.0",
            description: "A great tool",
            homepage: "https://example.com",
            license: "mit",
            archives: &archives,
            install_lines: &install_lines,
            post_install_lines: &[],
            needs_unzip: false,
            needs_make_wrapper: false,
            dep_args: &[],
            source_root: ".",
        });

        assert!(expr.contains("pname = \"mytool\""));
        assert!(expr.contains("version = \"1.0.0\""));
        assert!(expr.contains("description = \"A great tool\""));
        assert!(expr.contains("homepage = \"https://example.com\""));
        assert!(expr.contains("lib.licenses.mit"));
        assert!(expr.contains("x86_64-linux"));
        assert!(expr.contains("aarch64-darwin"));
        assert!(expr.contains("abc123"));
        assert!(expr.contains("def456"));
        assert!(expr.contains("mkdir -p $out/bin"));
    }

    #[test]
    fn test_generate_nix_expression_with_unzip() {
        let archives = vec![(
            "x86_64-linux".to_string(),
            "https://example.com/tool.zip".to_string(),
            "abc".to_string(),
        )];
        let install = vec!["mkdir -p $out/bin".to_string()];

        let expr = generate_nix_expression(&NixParams {
            name: "mytool",
            version: "1.0.0",
            description: "",
            homepage: "",
            license: "mit",
            archives: &archives,
            install_lines: &install,
            post_install_lines: &[],
            needs_unzip: true,
            needs_make_wrapper: false,
            dep_args: &[],
            source_root: ".",
        });

        assert!(expr.contains(", unzip"));
    }

    #[test]
    fn test_generate_nix_expression_with_post_install() {
        let archives = vec![(
            "x86_64-linux".to_string(),
            "https://example.com/tool.tar.gz".to_string(),
            "abc".to_string(),
        )];
        let install = vec!["mkdir -p $out/bin".to_string()];
        let post = vec!["installShellCompletion --bash comp.bash".to_string()];

        let expr = generate_nix_expression(&NixParams {
            name: "mytool",
            version: "1.0.0",
            description: "",
            homepage: "",
            license: "mit",
            archives: &archives,
            install_lines: &install,
            post_install_lines: &post,
            needs_unzip: false,
            needs_make_wrapper: false,
            dep_args: &[],
            source_root: ".",
        });

        assert!(expr.contains("postInstall"));
        assert!(expr.contains("installShellCompletion"));
    }

    #[test]
    fn test_generate_nix_expression_with_deps_uses_make_bin_path() {
        let archives = vec![
            (
                "x86_64-linux".to_string(),
                "https://example.com/tool.tar.gz".to_string(),
                "abc".to_string(),
            ),
            (
                "aarch64-darwin".to_string(),
                "https://example.com/tool-darwin.tar.gz".to_string(),
                "def".to_string(),
            ),
        ];
        // Simulate install lines that publish_to_nix would generate with deps.
        let install = vec![
            "mkdir -p $out/bin".to_string(),
            "cp -vr ./mytool $out/bin/mytool".to_string(),
            "wrapProgram $out/bin/mytool --prefix PATH : ${lib.makeBinPath (\n      lib.optionals stdenvNoCC.isDarwin [ darwin_dep ] ++\n      lib.optionals stdenvNoCC.isLinux [ linux_dep ] ++\n      [ git ]\n    )}".to_string(),
        ];
        let dep_args = vec![
            "darwin_dep".to_string(),
            "linux_dep".to_string(),
            "git".to_string(),
        ];

        let expr = generate_nix_expression(&NixParams {
            name: "mytool",
            version: "1.0.0",
            description: "A tool with deps",
            homepage: "",
            license: "mit",
            archives: &archives,
            install_lines: &install,
            post_install_lines: &[],
            needs_unzip: false,
            needs_make_wrapper: true,
            dep_args: &dep_args,
            source_root: ".",
        });

        // Verify lib.makeBinPath pattern is used (not lib.getBin)
        assert!(
            expr.contains("lib.makeBinPath"),
            "should use lib.makeBinPath"
        );
        assert!(!expr.contains("lib.getBin"), "should not use lib.getBin");
        // Verify platform-conditional lists
        assert!(expr.contains("lib.optionals stdenvNoCC.isDarwin [ darwin_dep ]"));
        assert!(expr.contains("lib.optionals stdenvNoCC.isLinux [ linux_dep ]"));
        // Verify makeWrapper is listed as a function arg
        assert!(expr.contains(", makeWrapper"));
    }

    #[test]
    fn test_generate_nix_expression_deps_in_native_build_inputs() {
        let archives = vec![(
            "x86_64-linux".to_string(),
            "https://example.com/tool.tar.gz".to_string(),
            "abc".to_string(),
        )];
        let install = vec!["mkdir -p $out/bin".to_string()];
        let dep_args = vec!["git".to_string(), "curl".to_string()];

        let expr = generate_nix_expression(&NixParams {
            name: "mytool",
            version: "1.0.0",
            description: "",
            homepage: "",
            license: "mit",
            archives: &archives,
            install_lines: &install,
            post_install_lines: &[],
            needs_unzip: false,
            needs_make_wrapper: true,
            dep_args: &dep_args,
            source_root: ".",
        });

        // Verify dep_args appear in nativeBuildInputs
        assert!(
            expr.contains("nativeBuildInputs"),
            "should have nativeBuildInputs"
        );
        // The deps should appear inside the nativeBuildInputs block
        let nbi_start = expr.find("nativeBuildInputs").unwrap();
        let nbi_section = &expr[nbi_start..];
        let bracket_end = nbi_section.find("];").unwrap();
        let nbi_block = &nbi_section[..bracket_end];
        assert!(
            nbi_block.contains("git"),
            "nativeBuildInputs should contain git"
        );
        assert!(
            nbi_block.contains("curl"),
            "nativeBuildInputs should contain curl"
        );
        assert!(
            nbi_block.contains("makeWrapper"),
            "nativeBuildInputs should contain makeWrapper"
        );
    }

    #[test]
    fn test_generate_nix_expression_no_rec() {
        let archives = vec![(
            "x86_64-linux".to_string(),
            "https://example.com/tool.tar.gz".to_string(),
            "abc".to_string(),
        )];
        let install = vec!["mkdir -p $out/bin".to_string()];

        let expr = generate_nix_expression(&NixParams {
            name: "mytool",
            version: "1.0.0",
            description: "",
            homepage: "",
            license: "mit",
            archives: &archives,
            install_lines: &install,
            post_install_lines: &[],
            needs_unzip: false,
            needs_make_wrapper: false,
            dep_args: &[],
            source_root: ".",
        });

        assert!(
            !expr.contains("mkDerivation rec"),
            "should not contain 'rec'"
        );
        assert!(
            expr.contains("mkDerivation {"),
            "should contain mkDerivation without rec"
        );
    }

    #[test]
    fn test_validate_nix_license_valid() {
        // Common licenses should all pass
        assert!(validate_nix_license("mit").is_ok());
        assert!(validate_nix_license("asl20").is_ok());
        assert!(validate_nix_license("gpl3Only").is_ok());
        assert!(validate_nix_license("bsd2").is_ok());
        assert!(validate_nix_license("bsd3").is_ok());
        assert!(validate_nix_license("mpl20").is_ok());
        assert!(validate_nix_license("isc").is_ok());
        assert!(validate_nix_license("unlicense").is_ok());
        assert!(validate_nix_license("cc0").is_ok());
        assert!(validate_nix_license("agpl3Only").is_ok());
        assert!(validate_nix_license("eupl12").is_ok());
        assert!(validate_nix_license("boost").is_ok());
        assert!(validate_nix_license("publicDomain").is_ok());
        assert!(validate_nix_license("unfree").is_ok());
        assert!(validate_nix_license("unfreeRedistributable").is_ok());
        assert!(validate_nix_license("wtfpl").is_ok());
        assert!(validate_nix_license("zlib").is_ok());
        assert!(validate_nix_license("artistic2").is_ok());
    }

    #[test]
    fn test_validate_nix_license_invalid() {
        let result = validate_nix_license("not-a-real-license");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not-a-real-license"),
            "error should contain the bad license name"
        );
        assert!(
            msg.contains("unknown license"),
            "error should say unknown license"
        );
    }

    #[test]
    fn test_hex_sha256_to_sri_valid() {
        // SHA256 of empty string: e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let sri =
            hex_sha256_to_sri("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
                .unwrap();
        assert!(
            sri.starts_with("sha256-"),
            "SRI hash should start with 'sha256-'"
        );
        assert_eq!(sri, "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=");
    }

    #[test]
    fn test_hex_sha256_to_sri_invalid_hex() {
        assert!(hex_sha256_to_sri("not-valid-hex").is_err());
    }

    #[test]
    fn test_hex_sha256_to_sri_wrong_length() {
        // Valid hex but not 32 bytes
        assert!(hex_sha256_to_sri("abcd").is_err());
    }

    #[test]
    fn test_publish_to_nix_dry_run() {
        use anodize_core::config::{
            Config, CrateConfig, NixConfig, PublishConfig, RepositoryConfig,
        };
        use anodize_core::context::{Context, ContextOptions};
        use anodize_core::log::{StageLogger, Verbosity};

        let config = Config {
            crates: vec![CrateConfig {
                name: "mytool".to_string(),
                path: ".".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: Some(PublishConfig {
                    nix: Some(NixConfig {
                        repository: Some(RepositoryConfig {
                            owner: Some("myorg".to_string()),
                            name: Some("nixpkgs-overlay".to_string()),
                            ..Default::default()
                        }),
                        description: Some("My tool".to_string()),
                        license: Some("mit".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };

        let ctx = Context::new(
            config,
            ContextOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let log = StageLogger::new("publish", Verbosity::Normal);
        assert!(publish_to_nix(&ctx, "mytool", &log).is_ok());
    }
}
