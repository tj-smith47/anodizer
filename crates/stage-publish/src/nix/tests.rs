//! Tests for the Nix publisher submodules.

use super::generate::{NixParams, generate_nix_expression, nix_system};
use super::hashing::{hex_sha256_to_nix_base32, hex_sha256_to_sri};
use super::publish::publish_to_nix;
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
