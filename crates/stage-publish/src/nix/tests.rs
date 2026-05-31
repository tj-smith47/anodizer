//! Tests for the Nix publisher submodules.

use super::binary::is_dynamically_linked;
use super::generate::{NixParams, generate_nix_expression, nix_system};
use super::hashing::{hex_sha256_to_nix_base32, hex_sha256_to_sri};
use super::publish::{publish_to_nix, render_nix_for_validation};
use super::validate_nix_license;

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
        main_program: "",
        archives: &archives,
        install_lines: &install_lines,
        post_install_lines: &[],
        needs_unzip: false,
        needs_make_wrapper: false,
        dep_args: &[],
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();

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
fn test_derivation_url_map_pairs_nix_double_with_go_arch_asset() {
    // The derivation's `urlMap` is keyed by Nix system doubles
    // (`<arch>-<os>`) but each value is the go-arch-named release asset
    // for that system. A wrong pairing is a 404 at `nix-build` time, the
    // same failure class as a binstall asset-name bug. Pin that each
    // standard system selects the correctly-named asset:
    //   x86_64-linux   -> ...-linux-amd64.tar.gz
    //   aarch64-linux  -> ...-linux-arm64.tar.gz
    //   x86_64-darwin  -> ...-darwin-amd64.tar.gz
    //   aarch64-darwin -> ...-darwin-arm64.tar.gz
    let cases = [
        ("linux", "amd64", "x86_64-linux", "tool-linux-amd64.tar.gz"),
        ("linux", "arm64", "aarch64-linux", "tool-linux-arm64.tar.gz"),
        (
            "darwin",
            "amd64",
            "x86_64-darwin",
            "tool-darwin-amd64.tar.gz",
        ),
        (
            "darwin",
            "arm64",
            "aarch64-darwin",
            "tool-darwin-arm64.tar.gz",
        ),
    ];

    // Build archives exactly the way publish.rs does: zip nix_system(os,
    // arch) with the per-artifact asset URL.
    let archives: Vec<(String, String, String)> = cases
        .iter()
        .map(|(os, arch, double, asset)| {
            assert_eq!(
                nix_system(os, arch).as_deref(),
                Some(*double),
                "nix_system({os},{arch}) must map to {double}"
            );
            (
                double.to_string(),
                format!("https://example.com/{asset}"),
                "deadbeef".to_string(),
            )
        })
        .collect();

    let expr = generate_nix_expression(&NixParams {
        name: "tool",
        version: "1.0.0",
        description: "",
        homepage: "",
        license: "mit",
        main_program: "",
        archives: &archives,
        install_lines: &["mkdir -p $out/bin".to_string()],
        post_install_lines: &[],
        needs_unzip: false,
        needs_make_wrapper: false,
        dep_args: &[],
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();

    for (_os, _arch, double, asset) in cases {
        assert!(
            expr.contains(&format!("{double} = \"https://example.com/{asset}\";")),
            "urlMap must pair nix double {double} with go-arch asset {asset}:\n{expr}"
        );
    }
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
        main_program: "",
        archives: &archives,
        install_lines: &install,
        post_install_lines: &[],
        needs_unzip: true,
        needs_make_wrapper: false,
        dep_args: &[],
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();

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
        main_program: "",
        archives: &archives,
        install_lines: &install,
        post_install_lines: &post,
        needs_unzip: false,
        needs_make_wrapper: false,
        dep_args: &[],
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();

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
        main_program: "",
        archives: &archives,
        install_lines: &install,
        post_install_lines: &[],
        needs_unzip: false,
        needs_make_wrapper: true,
        dep_args: &dep_args,
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();

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
        main_program: "",
        archives: &archives,
        install_lines: &install,
        post_install_lines: &[],
        needs_unzip: false,
        needs_make_wrapper: true,
        dep_args: &dep_args,
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();

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
        main_program: "",
        archives: &archives,
        install_lines: &install,
        post_install_lines: &[],
        needs_unzip: false,
        needs_make_wrapper: false,
        dep_args: &[],
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();

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
fn test_generate_nix_expression_with_main_program() {
    let archives = vec![(
        "x86_64-linux".to_string(),
        "https://example.com/tool.tar.gz".to_string(),
        "abc".to_string(),
    )];
    let install = vec!["mkdir -p $out/bin".to_string()];

    let expr = generate_nix_expression(&NixParams {
        name: "mytool",
        version: "1.2.1",
        description: "my test",
        homepage: "https://example.com",
        license: "mit",
        main_program: "drumroll",
        archives: &archives,
        install_lines: &install,
        post_install_lines: &[],
        needs_unzip: false,
        needs_make_wrapper: false,
        dep_args: &[],
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();

    assert!(
        expr.contains("mainProgram = \"drumroll\";"),
        "meta.mainProgram must be rendered; got:\n{expr}"
    );
}

#[test]
fn test_generate_nix_expression_omits_main_program_when_empty() {
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
        main_program: "",
        archives: &archives,
        install_lines: &install,
        post_install_lines: &[],
        needs_unzip: false,
        needs_make_wrapper: false,
        dep_args: &[],
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();

    assert!(
        !expr.contains("mainProgram"),
        "mainProgram attr must be omitted when value is empty; got:\n{expr}"
    );
}

/// `meta.mainProgram` is interpolated directly inside a Nix string literal.
/// A pathological value containing `"` / `\` / `${` would either close the
/// literal (yielding malformed Nix) or trigger antiquotation. The generator
/// must apply Nix string-escape rules so the rendered derivation parses.
#[test]
fn test_generate_nix_expression_escapes_main_program_quotes_backslashes_and_dollar_brace() {
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
        // Triple-hazard input: a quote, a backslash, and `${` (Nix
        // antiquotation start).
        main_program: r#"my"to\ol${X}"#,
        archives: &archives,
        install_lines: &install,
        post_install_lines: &[],
        needs_unzip: false,
        needs_make_wrapper: false,
        dep_args: &[],
        source_root: Some("."),
        source_root_map: None,
        dynamically_linked: false,
    })
    .unwrap();

    // The escaped form: `"` → `\"`, `\` → `\\`, `${` → `\${`.
    // Rust raw-string keeps the backslashes literal in the assertion.
    assert!(
        expr.contains(r#"mainProgram = "my\"to\\ol\${X}";"#),
        "main_program must be Nix-escaped; got:\n{expr}"
    );
}

#[test]
fn test_nix_escape_string_handles_backslash_quote_and_dollar_brace() {
    use super::generate::nix_escape_string;
    // Each rule in isolation, then composed.
    assert_eq!(nix_escape_string(""), "");
    assert_eq!(nix_escape_string("plain"), "plain");
    assert_eq!(nix_escape_string(r#"a"b"#), r#"a\"b"#);
    assert_eq!(nix_escape_string(r"a\b"), r"a\\b");
    assert_eq!(nix_escape_string("a${X}"), r"a\${X}");
    // Order matters: backslash escape must run first so the backslashes
    // introduced for `"` / `${` are not themselves doubled.
    assert_eq!(nix_escape_string(r#""${"#), r#"\"\${"#);
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
fn test_hex_sha256_to_nix_base32_valid() {
    // SHA256 of empty string = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    // nix base32 is 52 chars for SHA256 (256 bits / 5 bits per char = 52)
    let hash = hex_sha256_to_nix_base32(
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    )
    .unwrap();
    assert_eq!(hash.len(), 52, "nix base32 of SHA256 must be 52 chars");
    // Verify only valid nix base32 characters are used
    assert!(
        hash.chars()
            .all(|c| "0123456789abcdfghijklmnpqrsvwxyz".contains(c)),
        "output must use nix base32 alphabet"
    );
    // Cross-check: both conversions come from the same 32-byte hash
    let sri = hex_sha256_to_sri("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        .unwrap();
    assert_eq!(sri, "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=");
    // Both encode the same underlying hash, just in different formats
    assert_eq!(hash, "0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73");
}

#[test]
fn test_hex_sha256_to_nix_base32_invalid_hex() {
    assert!(hex_sha256_to_nix_base32("not-valid-hex").is_err());
}

#[test]
fn test_hex_sha256_to_nix_base32_wrong_length() {
    assert!(hex_sha256_to_nix_base32("abcd").is_err());
}

#[test]
fn test_hex_sha256_to_sri_valid() {
    let sri = hex_sha256_to_sri("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        .unwrap();
    assert_eq!(sri, "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=");
}

#[test]
fn test_publish_to_nix_dry_run() {
    use anodizer_core::config::{Config, CrateConfig, NixConfig, PublishConfig, RepositoryConfig};
    use anodizer_core::context::{Context, ContextOptions};
    use anodizer_core::log::{StageLogger, Verbosity};

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

    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: true,
            ..Default::default()
        },
    );
    let log = StageLogger::new("publish", Verbosity::Normal);
    assert!(publish_to_nix(&mut ctx, "mytool", &log).is_ok());
}

// ---------------------------------------------------------------------------
// `publish_to_nix` early-exit branches — every guard returns Ok(false)
// (no push happened) so the caller's rollback bookkeeping stays clean.
// ---------------------------------------------------------------------------

/// Helper: build a minimal Context with a `nix:` publish config.
fn nix_ctx(
    nix_cfg: anodizer_core::config::NixConfig,
    dry_run: bool,
) -> anodizer_core::context::Context {
    use anodizer_core::config::{Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                nix: Some(nix_cfg),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    Context::new(
        config,
        ContextOptions {
            dry_run,
            ..Default::default()
        },
    )
}

fn nix_log() -> anodizer_core::log::StageLogger {
    use anodizer_core::log::{StageLogger, Verbosity};
    StageLogger::new("publish", Verbosity::Quiet)
}

/// `nix:` config absent => actionable error citing the crate name.
#[test]
fn test_publish_to_nix_missing_config_errors() {
    use anodizer_core::config::{Config, CrateConfig, PublishConfig};
    use anodizer_core::context::{Context, ContextOptions};
    let config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: ".".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig::default()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(config, ContextOptions::default());
    let err = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no nix config"), "unexpected: {msg}");
    assert!(msg.contains("mytool"), "must name crate: {msg}");
}

/// `skip: true` bypasses everything and returns Ok(false).
#[test]
fn test_publish_to_nix_skip_true_returns_false() {
    use anodizer_core::config::{NixConfig, RepositoryConfig, StringOrBool};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        skip: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    let got = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap();
    assert!(!got, "skip=true must short-circuit before any push");
}

/// `skip_upload: true` bypasses the push and returns Ok(false).
#[test]
fn test_publish_to_nix_skip_upload_true_returns_false() {
    use anodizer_core::config::{NixConfig, RepositoryConfig, StringOrBool};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        skip_upload: Some(StringOrBool::Bool(true)),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    let got = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap();
    assert!(!got);
}

/// No `repository:` (and no top-level fallback) => error citing crate name.
#[test]
fn test_publish_to_nix_missing_repository_errors() {
    use anodizer_core::config::NixConfig;
    let cfg = NixConfig {
        repository: None,
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    let err = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no repository"), "unexpected: {msg}");
    assert!(msg.contains("mytool"), "{msg}");
}

/// Dry-run bypasses git work AND returns Ok(false) — push didn't happen.
#[test]
fn test_publish_to_nix_dry_run_returns_false() {
    use anodizer_core::config::{NixConfig, RepositoryConfig};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, true);
    let got = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap();
    assert!(!got, "dry-run must return Ok(false): no push happened");
}

/// Bad license identifier => validate_nix_license bails with a parseable
/// error. Pin the validator wiring (config-must-wire).
#[test]
fn test_publish_to_nix_invalid_license_errors() {
    use anodizer_core::config::{NixConfig, RepositoryConfig};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        license: Some("not-a-real-spdx-id".to_string()),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    let err = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap_err();
    assert!(format!("{err}").contains("not-a-real-spdx-id"));
}

/// No artifacts at all => `no Linux/Darwin archive artifacts found` bail
/// rather than a broken Nix expression with empty url/sha256.
#[test]
fn test_publish_to_nix_no_artifacts_errors() {
    use anodizer_core::config::{NixConfig, RepositoryConfig};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        license: Some("mit".to_string()),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    let err = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("no Linux/Darwin archive artifacts"),
        "unexpected: {msg}"
    );
    assert!(msg.contains("mytool"));
}

/// Building a Nix derivation for an artifact whose `sha256` metadata is
/// empty must bail with an actionable error. Defaulting to `""` would
/// embed an empty `sha256 = "";` in the rendered `fetchurl`
/// fixed-output derivation, which `nix-build` rejects. The bail
/// message must name the publisher, the field, the offending artifact
/// context, and a next-step hint.
#[test]
fn nix_sha256_empty_metadata_bails_with_actionable_error() {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    use anodizer_core::config::{NixConfig, RepositoryConfig};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        license: Some("mit".to_string()),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    ctx.artifacts.add(Artifact {
        kind: ArtifactKind::Archive,
        path: std::path::PathBuf::from("/tmp/mytool-linux-amd64.tar.gz"),
        name: "mytool-linux-amd64.tar.gz".to_string(),
        target: Some("x86_64-unknown-linux-gnu".to_string()),
        crate_name: "mytool".to_string(),
        metadata: {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "url".to_string(),
                "https://example.com/mytool-linux-amd64.tar.gz".to_string(),
            );
            m
        },
        size: None,
    });
    let err = publish_to_nix(&mut ctx, "mytool", &nix_log()).expect_err("missing sha256 must bail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("missing sha256 metadata"),
        "error must mention missing sha256; got: {msg}"
    );
    assert!(
        msg.contains("mytool"),
        "error must name the offending crate; got: {msg}"
    );
    assert!(
        msg.contains("checksum stage"),
        "error must mention the checksum stage; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// `is_dynamically_linked` — ELF PT_INTERP detection.
// ---------------------------------------------------------------------------

/// Non-existent path returns false (open() error). Pin the silent-degrade
/// contract used by the publish path's metadata-fallback inspection.
#[test]
fn is_dynamically_linked_missing_file_returns_false() {
    assert!(!is_dynamically_linked(std::path::Path::new(
        "/nonexistent/path/to/binary/that/cannot/exist"
    )));
}

/// File smaller than ELF header (52 bytes) returns false.
#[test]
fn is_dynamically_linked_short_file_returns_false() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("tiny");
    std::fs::write(&p, b"abc").unwrap();
    assert!(!is_dynamically_linked(&p));
}

/// File without ELF magic bytes (e.g. Mach-O / PE / random) returns false.
#[test]
fn is_dynamically_linked_non_elf_returns_false() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("not-elf");
    // 64 bytes of nonzero non-ELF data.
    let bytes: Vec<u8> = (0..64u8).collect();
    std::fs::write(&p, bytes).unwrap();
    assert!(!is_dynamically_linked(&p));
}

/// Hand-rolled minimal 64-bit ELF with a single PT_INTERP program header
/// returns true. Pins the "dynamically linked => emit autoPatchelfHook"
/// signal that the publish path uses to set `dynamically_linked`.
#[test]
fn is_dynamically_linked_elf64_with_pt_interp_returns_true() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("elf64-dyn");
    // 64-byte ELF header followed by one 56-byte program header with p_type=3.
    let phoff: u64 = 64;
    let phentsize: u16 = 56;
    let phnum: u16 = 1;
    let mut bytes = Vec::with_capacity(64 + phentsize as usize);
    bytes.extend_from_slice(b"\x7fELF"); // magic
    bytes.push(2); // 64-bit
    bytes.push(1); // little-endian
    bytes.push(1); // EI_VERSION
    bytes.extend_from_slice(&[0u8; 9]); // OSABI + padding
    bytes.extend_from_slice(&[0u8; 2]); // e_type
    bytes.extend_from_slice(&[0u8; 2]); // e_machine
    bytes.extend_from_slice(&[0u8; 4]); // e_version
    bytes.extend_from_slice(&[0u8; 8]); // e_entry
    bytes.extend_from_slice(&phoff.to_le_bytes()); // e_phoff (32..40)
    bytes.extend_from_slice(&[0u8; 8]); // e_shoff
    bytes.extend_from_slice(&[0u8; 4]); // e_flags
    bytes.extend_from_slice(&[0u8; 2]); // e_ehsize
    bytes.extend_from_slice(&phentsize.to_le_bytes()); // e_phentsize (54..56)
    bytes.extend_from_slice(&phnum.to_le_bytes()); // e_phnum (56..58)
    bytes.extend_from_slice(&[0u8; 6]); // remaining e_shentsize/e_shnum/e_shstrndx (pad to 64)
    debug_assert_eq!(bytes.len(), 64);
    // Program header: p_type=3 (PT_INTERP), 4-byte LE.
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(&vec![0u8; phentsize as usize - 4]);
    std::fs::write(&p, &bytes).unwrap();
    assert!(is_dynamically_linked(&p), "PT_INTERP must be detected");
}

/// 64-bit ELF whose only program header is PT_LOAD (1) returns false —
/// the file is statically linked.
#[test]
fn is_dynamically_linked_elf64_without_pt_interp_returns_false() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("elf64-static");
    let phoff: u64 = 64;
    let phentsize: u16 = 56;
    let phnum: u16 = 1;
    let mut bytes = Vec::with_capacity(64 + phentsize as usize);
    bytes.extend_from_slice(b"\x7fELF");
    bytes.push(2);
    bytes.push(1);
    bytes.push(1);
    bytes.extend_from_slice(&[0u8; 9]);
    bytes.extend_from_slice(&[0u8; 2]);
    bytes.extend_from_slice(&[0u8; 2]);
    bytes.extend_from_slice(&[0u8; 4]);
    bytes.extend_from_slice(&[0u8; 8]);
    bytes.extend_from_slice(&phoff.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 8]);
    bytes.extend_from_slice(&[0u8; 4]);
    bytes.extend_from_slice(&[0u8; 2]);
    bytes.extend_from_slice(&phentsize.to_le_bytes());
    bytes.extend_from_slice(&phnum.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 6]);
    debug_assert_eq!(bytes.len(), 64);
    // p_type = 1 (PT_LOAD), not 3.
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&vec![0u8; phentsize as usize - 4]);
    std::fs::write(&p, &bytes).unwrap();
    assert!(!is_dynamically_linked(&p));
}

/// 32-bit ELF with PT_INTERP returns true — pins the `is_64bit=false`
/// branch in the header parser (phoff/phnum read from 32-bit offsets).
#[test]
fn is_dynamically_linked_elf32_with_pt_interp_returns_true() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("elf32-dyn");
    // For 32-bit ELF: e_entry is 4 bytes (offset 24..28), e_phoff is 4 bytes
    // at offset 28..32, e_phentsize at 42..44, e_phnum at 44..46.
    let phoff: u32 = 52;
    let phentsize: u16 = 32;
    let phnum: u16 = 1;
    let mut bytes = Vec::with_capacity(52 + phentsize as usize);
    bytes.extend_from_slice(b"\x7fELF"); // 0..4
    bytes.push(1); // 32-bit class (4)
    bytes.push(1); // little-endian (5)
    bytes.push(1); // EI_VERSION (6)
    bytes.extend_from_slice(&[0u8; 9]); // osabi + padding (7..16)
    bytes.extend_from_slice(&[0u8; 2]); // e_type (16..18)
    bytes.extend_from_slice(&[0u8; 2]); // e_machine (18..20)
    bytes.extend_from_slice(&[0u8; 4]); // e_version (20..24)
    bytes.extend_from_slice(&[0u8; 4]); // e_entry — 32-bit is 4 bytes (24..28)
    bytes.extend_from_slice(&phoff.to_le_bytes()); // e_phoff (28..32)
    bytes.extend_from_slice(&[0u8; 4]); // e_shoff (32..36)
    bytes.extend_from_slice(&[0u8; 4]); // e_flags (36..40)
    bytes.extend_from_slice(&[0u8; 2]); // e_ehsize (40..42)
    bytes.extend_from_slice(&phentsize.to_le_bytes()); // e_phentsize (42..44)
    bytes.extend_from_slice(&phnum.to_le_bytes()); // e_phnum (44..46)
    bytes.extend_from_slice(&[0u8; 6]); // pad to 52
    debug_assert_eq!(bytes.len(), 52);
    bytes.extend_from_slice(&3u32.to_le_bytes()); // PT_INTERP
    bytes.extend_from_slice(&vec![0u8; phentsize as usize - 4]);
    std::fs::write(&p, &bytes).unwrap();
    assert!(is_dynamically_linked(&p));
}

/// Big-endian ELF with PT_INTERP returns true — exercises the `little=false`
/// branches of read_u16/read_u32/read_u64.
#[test]
fn is_dynamically_linked_elf64_big_endian_with_pt_interp_returns_true() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("elf64-be-dyn");
    let phoff: u64 = 64;
    let phentsize: u16 = 56;
    let phnum: u16 = 1;
    let mut bytes = Vec::with_capacity(64 + phentsize as usize);
    bytes.extend_from_slice(b"\x7fELF");
    bytes.push(2);
    bytes.push(2); // big-endian
    bytes.push(1);
    bytes.extend_from_slice(&[0u8; 9]);
    bytes.extend_from_slice(&[0u8; 2]);
    bytes.extend_from_slice(&[0u8; 2]);
    bytes.extend_from_slice(&[0u8; 4]);
    bytes.extend_from_slice(&[0u8; 8]);
    bytes.extend_from_slice(&phoff.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 8]);
    bytes.extend_from_slice(&[0u8; 4]);
    bytes.extend_from_slice(&[0u8; 2]);
    bytes.extend_from_slice(&phentsize.to_be_bytes());
    bytes.extend_from_slice(&phnum.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 6]);
    debug_assert_eq!(bytes.len(), 64);
    bytes.extend_from_slice(&3u32.to_be_bytes());
    bytes.extend_from_slice(&vec![0u8; phentsize as usize - 4]);
    std::fs::write(&p, &bytes).unwrap();
    assert!(is_dynamically_linked(&p));
}

// ---------------------------------------------------------------------------
// Orchestrator dry-run paths — exercise check_skip_guards template branches,
// resolve_repo_coords template rendering, and the early-exit log surface.
// These run with `dry_run: true` so no git work happens; the orchestrator
// dispatches helpers up to `is_dry_run()` and returns Ok(false).
// ---------------------------------------------------------------------------

/// `skip` is a template string that evaluates to `"true"` — must short-circuit
/// just like `Bool(true)`. Pins the template-render branch of `check_skip_guards`.
#[test]
fn test_publish_to_nix_skip_template_string_true_returns_false() {
    use anodizer_core::config::{NixConfig, RepositoryConfig, StringOrBool};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        skip: Some(StringOrBool::String("true".to_string())),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    let got = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap();
    assert!(!got);
}

/// `skip` template that renders to `"false"` does NOT short-circuit —
/// orchestration proceeds past the guard. With no artifacts present the
/// pipeline then bails on "no Linux/Darwin archive artifacts", confirming
/// the skip guard was actually evaluated and rejected.
#[test]
fn test_publish_to_nix_skip_template_false_proceeds_past_guard() {
    use anodizer_core::config::{NixConfig, RepositoryConfig, StringOrBool};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        skip: Some(StringOrBool::String("false".to_string())),
        license: Some("mit".to_string()),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    let err = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap_err();
    assert!(format!("{err}").contains("no Linux/Darwin archive"));
}

/// `repository.owner` / `repository.name` are template-rendered. A literal
/// `{{ .ProjectName }}` placeholder must resolve from `template_vars` AND
/// the rendered value must reach the dry-run log line — a regression that
/// silently dropped substitution would still pass an `unwrap()`-only check.
#[test]
fn test_publish_to_nix_repo_coords_render_templates() {
    use anodizer_core::config::{NixConfig, RepositoryConfig};
    use anodizer_core::log::{StageLogger, Verbosity};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("{{ .ProjectName }}-org".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, true);
    ctx.template_vars_mut().set("ProjectName", "myproj");
    let (log, capture) = StageLogger::with_capture("publish", Verbosity::Normal);
    assert!(!publish_to_nix(&mut ctx, "mytool", &log).unwrap());
    let msgs = capture.all_messages();
    let rendered_owner = msgs.iter().any(|(_, m)| m.contains("myproj-org"));
    assert!(
        rendered_owner,
        "rendered owner 'myproj-org' must appear in dry-run log; captured: {msgs:?}"
    );
    let raw_leaked = msgs.iter().any(|(_, m)| m.contains("{{ .ProjectName }}"));
    assert!(
        !raw_leaked,
        "raw template must not leak past render; captured: {msgs:?}"
    );
}

/// Repository `name:` field is also template-rendered (paired with the
/// `owner` render covered above). Both halves of the rendered destination
/// must appear in the dry-run log line — pins the `repo_name` branch of
/// `resolve_repo_coords` independently of `repo_owner`.
#[test]
fn test_publish_to_nix_repo_name_template_rendered_in_dry_run() {
    use anodizer_core::config::{NixConfig, RepositoryConfig};
    use anodizer_core::log::{StageLogger, Verbosity};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("static-owner".to_string()),
            name: Some("{{ .ProjectName }}-pkgs".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, true);
    ctx.template_vars_mut().set("ProjectName", "anodize");
    let (log, capture) = StageLogger::with_capture("publish", Verbosity::Normal);
    assert!(!publish_to_nix(&mut ctx, "mytool", &log).unwrap());
    let msgs = capture.all_messages();
    let dry_run_logged = msgs.iter().any(|(_, m)| {
        m.contains("(dry-run) would publish") && m.contains("static-owner/anodize-pkgs")
    });
    assert!(
        dry_run_logged,
        "rendered owner/repo 'static-owner/anodize-pkgs' must appear in dry-run log; captured: {msgs:?}"
    );
    let raw_leaked = msgs.iter().any(|(_, m)| m.contains("{{ .ProjectName }}"));
    assert!(
        !raw_leaked,
        "raw template must not leak past render; captured: {msgs:?}"
    );
}

/// Project-level `metadata.description` is the fallback when the per-crate
/// `nix.description` is unset. Pins `resolve_nix_metadata`'s
/// `or_else(|| ctx.config.meta_description())` chain.
#[test]
fn test_publish_to_nix_description_falls_back_to_project_metadata() {
    use anodizer_core::config::{
        Config, CrateConfig, MetadataConfig, NixConfig, PublishConfig, RepositoryConfig,
    };
    use anodizer_core::context::{Context, ContextOptions};
    let config = Config {
        metadata: Some(MetadataConfig {
            description: Some("project-level description".to_string()),
            license: Some("mit".to_string()),
            ..Default::default()
        }),
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
                    // description omitted → falls back to metadata.description
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut ctx = Context::new(
        config,
        ContextOptions {
            dry_run: false,
            ..Default::default()
        },
    );
    // No artifacts → bails on the no-archives guard after passing through
    // the description-fallback path. Confirms the fallback didn't panic
    // on a missing per-crate description.
    let err = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap_err();
    assert!(format!("{err}").contains("no Linux/Darwin archive"));
}

/// Bad homepage template (Tera syntax error) surfaces as a render error
/// with the crate name in the chain. Pins the
/// `.with_context(|| "render homepage template for '<crate>'")` plumbing.
#[test]
fn test_publish_to_nix_bad_homepage_template_errors_with_crate_name() {
    use anodizer_core::config::{NixConfig, RepositoryConfig};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        license: Some("mit".to_string()),
        // Unclosed Tera tag — render must fail loudly.
        homepage: Some("{{ broken".to_string()),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    let err = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("homepage"), "must name field: {msg}");
    assert!(msg.contains("mytool"), "must name crate: {msg}");
}

/// `formatter:` is wired but no artifacts present — orchestrator bails on
/// the no-archives guard BEFORE reaching `run_formatter`. Pins that the
/// formatter wiring doesn't fire prematurely (would attempt to spawn the
/// binary against a file that hasn't been written yet).
#[test]
fn test_publish_to_nix_formatter_not_invoked_before_archive_resolution() {
    use anodizer_core::config::{NixConfig, RepositoryConfig};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        license: Some("mit".to_string()),
        formatter: Some("nixfmt".to_string()),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    let err = publish_to_nix(&mut ctx, "mytool", &nix_log()).unwrap_err();
    // Confirms the failure was the archives guard, not a formatter spawn.
    assert!(format!("{err}").contains("no Linux/Darwin archive"));
}

// ---------------------------------------------------------------------------
// Derived-license resolution — the auto-derive (derive-don't-require) path
// feeds a Cargo SPDX id (e.g. `MIT`, `Apache-2.0`) into the nix emission,
// which must end up as a `lib.licenses.<attr>` nix attribute, NOT the raw
// SPDX id. Exercised through `render_nix_for_validation`, the in-memory twin
// of the publish render path, so the assertion is on the actual emitted
// derivation string.
// ---------------------------------------------------------------------------

/// Write a minimal `Cargo.toml` with the given `[package].license` to
/// `<dir>/<name>/Cargo.toml`, creating the crate dir.
fn write_crate_cargo(base: &std::path::Path, name: &str, license: &str) {
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\ndescription = \"the {name} crate\"\nlicense = \"{license}\"\n"
        ),
    )
    .unwrap();
}

/// Add a Linux+Darwin archive pair for `crate_name` so the nix render path
/// resolves at least one `lib.licenses`-bearing derivation.
fn add_linux_darwin_archives(ctx: &mut anodizer_core::context::Context, crate_name: &str) {
    use anodizer_core::artifact::{Artifact, ArtifactKind};
    for (target, url) in [
        (
            "x86_64-unknown-linux-gnu",
            format!("https://example.com/{crate_name}-linux-amd64.tar.gz"),
        ),
        (
            "aarch64-apple-darwin",
            format!("https://example.com/{crate_name}-darwin-arm64.tar.gz"),
        ),
    ] {
        let mut m = std::collections::HashMap::new();
        m.insert("url".to_string(), url.clone());
        m.insert(
            "sha256".to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
        );
        ctx.artifacts.add(Artifact {
            kind: ArtifactKind::Archive,
            name: url.rsplit('/').next().unwrap().to_string(),
            path: std::path::PathBuf::from(format!("dist/{}", url.rsplit('/').next().unwrap())),
            target: Some(target.to_string()),
            crate_name: crate_name.to_string(),
            metadata: m,
            size: None,
        });
    }
}

/// Render `crate_name`'s nix derivation in-memory from a single-crate config
/// whose license is *derived* from a Cargo.toml `[package].license` SPDX id
/// (no explicit `nix.license`). Returns the emitted expression.
fn render_with_derived_license(spdx: &str) -> anyhow::Result<String> {
    use anodizer_core::config::{Config, CrateConfig, NixConfig, PublishConfig, RepositoryConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let base = tempfile::tempdir().unwrap();
    write_crate_cargo(base.path(), "mytool", spdx);

    let mut config = Config {
        crates: vec![CrateConfig {
            name: "mytool".to_string(),
            path: "mytool".to_string(),
            tag_template: "v{{ .Version }}".to_string(),
            publish: Some(PublishConfig {
                nix: Some(NixConfig {
                    repository: Some(RepositoryConfig {
                        owner: Some("myorg".to_string()),
                        name: Some("nixpkgs-overlay".to_string()),
                        ..Default::default()
                    }),
                    // No explicit `license` — force the derived path.
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }],
        ..Default::default()
    };
    config.populate_derived_metadata(base.path());

    let mut ctx = Context::new(config, ContextOptions::default());
    add_linux_darwin_archives(&mut ctx, "mytool");

    render_nix_for_validation(&mut ctx, "mytool", &nix_log())
        .map(|r| r.expect("render should not skip").expr)
}

#[test]
fn derived_spdx_mit_emits_lib_licenses_mit() {
    let expr = render_with_derived_license("MIT").unwrap();
    assert!(
        expr.contains("license = lib.licenses.mit;"),
        "derived SPDX `MIT` must map to nix attr `mit`; got:\n{expr}"
    );
}

#[test]
fn derived_spdx_apache_emits_lib_licenses_asl20() {
    let expr = render_with_derived_license("Apache-2.0").unwrap();
    assert!(
        expr.contains("license = lib.licenses.asl20;"),
        "derived SPDX `Apache-2.0` must map to nix attr `asl20`; got:\n{expr}"
    );
}

#[test]
fn derived_spdx_nontrivial_mappings_emit_correct_attrs() {
    for (spdx, attr) in [
        ("BSD-3-Clause", "bsd3"),
        ("GPL-3.0-or-later", "gpl3Plus"),
        ("MPL-2.0", "mpl20"),
    ] {
        let expr = render_with_derived_license(spdx).unwrap();
        assert!(
            expr.contains(&format!("license = lib.licenses.{attr};")),
            "derived SPDX `{spdx}` must map to nix attr `{attr}`; got:\n{expr}"
        );
    }
}

#[test]
fn explicit_nix_attr_license_passes_through_unchanged() {
    // cfgd writes `nix.license: mit` (already a nix attr) — must not break.
    use anodizer_core::config::{NixConfig, RepositoryConfig};
    let cfg = NixConfig {
        repository: Some(RepositoryConfig {
            owner: Some("myorg".to_string()),
            name: Some("nixpkgs-overlay".to_string()),
            ..Default::default()
        }),
        license: Some("mit".to_string()),
        ..Default::default()
    };
    let mut ctx = nix_ctx(cfg, false);
    add_linux_darwin_archives(&mut ctx, "mytool");
    let expr = render_nix_for_validation(&mut ctx, "mytool", &nix_log())
        .unwrap()
        .expect("render should not skip")
        .expr;
    assert!(
        expr.contains("license = lib.licenses.mit;"),
        "explicit nix attr `mit` must pass through unchanged; got:\n{expr}"
    );
}

#[test]
fn derived_unknown_spdx_id_hard_errors() {
    let err = render_with_derived_license("Foo-1.0")
        .unwrap_err()
        .to_string();
    assert!(err.contains("Foo-1.0"), "must name the bad value: {err}");
    assert!(
        err.contains("neither a known SPDX"),
        "must explain neither SPDX nor nix attr: {err}"
    );
}

#[test]
fn derived_compound_spdx_expression_hard_errors() {
    let err = render_with_derived_license("MIT OR Apache-2.0")
        .unwrap_err()
        .to_string();
    assert!(err.contains("compound"), "must flag compound: {err}");
    assert!(
        err.contains("nix.license"),
        "must hint setting nix.license explicitly: {err}"
    );
}

/// Workspace per-crate mode: two crates with *different* Cargo SPDX ids each
/// resolve their own nix attribute. Pins that the derived-license path is
/// per-crate, not a single shared value.
#[test]
fn per_crate_workspace_each_crate_derives_its_own_nix_license() {
    use anodizer_core::config::{Config, CrateConfig, NixConfig, PublishConfig, RepositoryConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let base = tempfile::tempdir().unwrap();
    write_crate_cargo(base.path(), "alpha", "MIT");
    write_crate_cargo(base.path(), "beta", "Apache-2.0");

    let nix_cfg = || {
        Some(PublishConfig {
            nix: Some(NixConfig {
                repository: Some(RepositoryConfig {
                    owner: Some("myorg".to_string()),
                    name: Some("nixpkgs-overlay".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    };
    let mut config = Config {
        crates: vec![
            CrateConfig {
                name: "alpha".to_string(),
                path: "alpha".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: nix_cfg(),
                ..Default::default()
            },
            CrateConfig {
                name: "beta".to_string(),
                path: "beta".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: nix_cfg(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    config.populate_derived_metadata(base.path());

    let mut ctx = Context::new(config, ContextOptions::default());
    add_linux_darwin_archives(&mut ctx, "alpha");
    add_linux_darwin_archives(&mut ctx, "beta");

    let alpha = render_nix_for_validation(&mut ctx, "alpha", &nix_log())
        .unwrap()
        .expect("alpha render should not skip")
        .expr;
    let beta = render_nix_for_validation(&mut ctx, "beta", &nix_log())
        .unwrap()
        .expect("beta render should not skip")
        .expr;

    assert!(
        alpha.contains("license = lib.licenses.mit;"),
        "alpha (MIT) must emit `mit`; got:\n{alpha}"
    );
    assert!(
        beta.contains("license = lib.licenses.asl20;"),
        "beta (Apache-2.0) must emit `asl20`; got:\n{beta}"
    );
}

/// Workspace lockstep mode: two crates released at one shared workspace
/// version, each with its own Cargo SPDX id, both publishing nix. The
/// derived-license path keys off the per-crate `Cargo.toml` `license`, so a
/// shared version must NOT collapse the two crates onto one license — each
/// still resolves its own nix attribute. Mirrors the per-crate test but
/// with `version` fixed across both crates to model lockstep.
#[test]
fn lockstep_workspace_each_crate_derives_its_own_nix_license() {
    use anodizer_core::config::{Config, CrateConfig, NixConfig, PublishConfig, RepositoryConfig};
    use anodizer_core::context::{Context, ContextOptions};

    let base = tempfile::tempdir().unwrap();
    write_crate_cargo(base.path(), "alpha", "ISC");
    write_crate_cargo(base.path(), "beta", "MPL-2.0");

    let nix_cfg = || {
        Some(PublishConfig {
            nix: Some(NixConfig {
                repository: Some(RepositoryConfig {
                    owner: Some("myorg".to_string()),
                    name: Some("nixpkgs-overlay".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    };
    // Lockstep: both crates carry the SAME tag_template (one shared version
    // across the workspace), unlike per-crate independent tags.
    let mut config = Config {
        crates: vec![
            CrateConfig {
                name: "alpha".to_string(),
                path: "alpha".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: nix_cfg(),
                ..Default::default()
            },
            CrateConfig {
                name: "beta".to_string(),
                path: "beta".to_string(),
                tag_template: "v{{ .Version }}".to_string(),
                publish: nix_cfg(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    config.populate_derived_metadata(base.path());

    let mut ctx = Context::new(config, ContextOptions::default());
    // One shared release version for the whole workspace (lockstep).
    ctx.template_vars_mut().set("Version", "1.2.3");
    add_linux_darwin_archives(&mut ctx, "alpha");
    add_linux_darwin_archives(&mut ctx, "beta");

    let alpha = render_nix_for_validation(&mut ctx, "alpha", &nix_log())
        .unwrap()
        .expect("alpha render should not skip")
        .expr;
    let beta = render_nix_for_validation(&mut ctx, "beta", &nix_log())
        .unwrap()
        .expect("beta render should not skip")
        .expr;

    assert!(
        alpha.contains("license = lib.licenses.isc;"),
        "alpha (ISC) must emit `isc` even at the shared workspace version; got:\n{alpha}"
    );
    assert!(
        beta.contains("license = lib.licenses.mpl20;"),
        "beta (MPL-2.0) must emit `mpl20` even at the shared workspace version; got:\n{beta}"
    );
    // Both crates share the one workspace version (lockstep invariant).
    assert!(
        alpha.contains("version = \"1.2.3\";") && beta.contains("version = \"1.2.3\";"),
        "lockstep: both derivations must carry the shared version 1.2.3"
    );
}
